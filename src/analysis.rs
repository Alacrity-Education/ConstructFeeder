//! Per-project analysis: classify every component instance and compute the five flags.

use crate::board::{self, BoardFootprint};
use crate::compare::{self, FootprintDiff, SymbolDiff};
use crate::libtable::{LibStore, ResolvedLib, Resolver, StockLibs};
use crate::models::{self, EmbeddedFile, ModelCtx, ModelOrigin, ResolvedModel};
use crate::project::{self, Project};
use crate::schematic::{self, SchSymbol};
use crate::sexpr::Node;
use crate::types::{KicadVersion, LibId, LibSource};
use crate::vfs::Vfs;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The five customization flags (spec).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Flags {
    pub symbol: bool,
    pub symbol_local: bool,
    pub footprint: bool,
    pub footprint_local: bool,
    pub model: bool,
}

impl Flags {
    pub fn any(&self) -> bool {
        self.symbol || self.symbol_local || self.footprint || self.footprint_local || self.model
    }
}

/// Where an instance was found.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRef {
    /// "worktree" or git ref.
    pub source: String,
    pub project: String,
    pub project_path: String,
    pub reference: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub dnp: bool,
    pub on_schematic: bool,
    pub on_board: bool,
}

/// One analysed component instance (symbol and/or footprint), before grouping.
#[derive(Clone, Debug)]
pub struct Finding {
    pub flags: Flags,
    pub version: KicadVersion,
    pub lib_id: Option<String>,
    pub lib_name: Option<String>,
    pub footprint_id: Option<String>,
    pub symbol_source: Option<LibSource>,
    pub footprint_source: Option<LibSource>,
    pub symbol_diff: Option<SymbolDiff>,
    pub footprint_diff: Option<FootprintDiff>,
    pub models_diff: Vec<String>,
    pub models: Vec<ResolvedModel>,
    pub reasons: Vec<String>,
    pub notes: Vec<String>,
    pub instance: InstanceRef,
    /// Embedded (as-used) symbol, if any.
    pub symbol_node: Option<Node>,
    /// As-used footprint (board copy, or library copy for schematic-only parts).
    pub footprint_node: Option<Node>,
    /// True if footprint_node is a board copy (needs footprint_to_library on export).
    pub footprint_from_board: bool,
    /// Model bytes for export: (file name, bytes).
    pub model_files: Vec<(String, Vec<u8>)>,
    pub symbol_hash: Option<String>,
    pub footprint_hash: Option<String>,
    pub model_hashes: Vec<String>,
}

impl Finding {
    /// Content identity used for de-duplication.
    pub fn content_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.symbol_hash.as_deref().unwrap_or("-"));
        h.update("|");
        h.update(self.footprint_hash.as_deref().unwrap_or("-"));
        for m in &self.model_hashes {
            h.update("|");
            h.update(m);
        }
        hex::encode(h.finalize())
    }

    /// Base component name: named after what is customized.
    pub fn base_name(&self) -> String {
        let sym = self.lib_id.as_deref().map(|s| LibId::parse(s).item);
        let fp = self.footprint_id.as_deref().map(|s| LibId::parse(s).item);
        let sym_custom = self.flags.symbol || self.flags.symbol_local;
        match (sym, fp) {
            (Some(s), _) if sym_custom => s,
            (_, Some(f)) => f,
            (Some(s), None) => s,
            (None, None) => "unnamed".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibUse {
    pub nickname: String,
    pub source: LibSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectReport {
    pub source: String,
    pub name: String,
    pub dir: String,
    pub pro: String,
    pub schematics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcb: Option<String>,
    pub versions: BTreeMap<String, KicadVersion>,
    /// "ok" | "unsupported" | "error"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub symbol_libraries: Vec<LibUse>,
    pub footprint_libraries: Vec<LibUse>,
    pub symbol_count: usize,
    pub footprint_count: usize,
    pub custom_instance_count: usize,
    pub warnings: Vec<String>,
}

fn lib_use(r: &ResolvedLib) -> LibUse {
    use crate::libtable::LibLocation;
    LibUse {
        nickname: r.nickname.clone(),
        source: r.source,
        uri: r.uri.clone(),
        location: match &r.location {
            LibLocation::Vfs(p) => Some(p.clone()),
            LibLocation::Fs(p) => Some(p.display().to_string()),
            LibLocation::None => None,
        },
    }
}

fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// Analyse one project. Returns its report plus the findings that have at least one flag set.
pub fn analyse_project(
    vfs: &dyn Vfs,
    project: &Project,
    stock: &BTreeMap<KicadVersion, StockLibs>,
) -> (ProjectReport, Vec<Finding>) {
    let mut rep = ProjectReport {
        source: vfs.label().to_string(),
        name: project.name.clone(),
        dir: project.dir.clone(),
        pro: project.pro.clone(),
        schematics: project.schematics.clone(),
        pcb: project.pcb.clone(),
        versions: project.versions.clone(),
        status: "ok".into(),
        error: None,
        symbol_libraries: vec![],
        footprint_libraries: vec![],
        symbol_count: 0,
        footprint_count: 0,
        custom_instance_count: 0,
        warnings: project.warnings.clone(),
    };
    if let Some(u) = &project.unsupported {
        rep.status = "unsupported".into();
        rep.error = Some(u.clone());
        return (rep, vec![]);
    }
    match analyse_inner(vfs, project, stock, &mut rep) {
        Ok(f) => {
            rep.custom_instance_count = f.len();
            (rep, f)
        }
        Err(e) => {
            rep.status = "error".into();
            rep.error = Some(format!("{e:#}"));
            (rep, vec![])
        }
    }
}

fn analyse_inner(
    vfs: &dyn Vfs,
    project: &Project,
    stock_all: &BTreeMap<KicadVersion, StockLibs>,
    rep: &mut ProjectReport,
) -> Result<Vec<Finding>> {
    // Libraries are resolved against the stock set of the project's (highest) version.
    let pver = project
        .versions
        .values()
        .max()
        .copied()
        .unwrap_or(KicadVersion::V10);
    let stock = stock_all
        .get(&pver)
        .with_context(|| format!("no stock libraries configured for KiCad {}", pver.major()))?;
    let resolver = Resolver::new(vfs, project, stock)?;
    let store = LibStore::new(vfs);
    // Other supported stock versions, used to recognise library drift (a stock part that matches
    // an older/newer official library instead of the one of the file's version).
    let mut alt: Vec<(KicadVersion, Resolver)> = Vec::new();
    for (v, st) in stock_all {
        if *v != pver {
            alt.push((*v, Resolver::new(vfs, project, st)?));
        }
    }

    let sch = if project.schematics.is_empty() {
        schematic::SchematicSet::default()
    } else {
        schematic::load(vfs, project)?
    };
    rep.warnings.extend(sch.warnings.iter().cloned());
    let board = match &project.pcb {
        Some(p) => Some(board::load(vfs, p)?),
        None => None,
    };

    let (board_embedded, w) = match board.as_ref().and_then(|b| b.embedded_files.as_ref()) {
        Some(n) => models::parse_embedded_files(n),
        None => (vec![], vec![]),
    };
    rep.warnings.extend(w);

    let mut sym_libs: BTreeMap<String, ResolvedLib> = BTreeMap::new();
    let mut fp_libs: BTreeMap<String, ResolvedLib> = BTreeMap::new();

    // board_path -> footprint index
    let mut by_path: HashMap<&str, usize> = HashMap::new();
    if let Some(b) = &board {
        for (i, fp) in b.footprints.iter().enumerate() {
            if let Some(p) = &fp.path {
                by_path.insert(p.as_str(), i);
            }
        }
        rep.footprint_count = b.footprints.len();
    }
    let mut used_fp: BTreeSet<usize> = BTreeSet::new();
    let mut findings = Vec::new();
    let source = vfs.label().to_string();

    // Multi-unit symbols appear once per unit with the same reference; treat them as one part.
    let unit_key = |s: &SchSymbol| -> Option<(String, String, String)> {
        (!s.reference.is_empty() && !s.reference.ends_with('?')).then(|| {
            (
                s.instance_path.split('/').nth(1).unwrap_or("").to_string(),
                s.reference.clone(),
                s.lib_id.clone(),
            )
        })
    };
    let mut units: HashMap<(String, String, String), Vec<&str>> = HashMap::new();
    for s in &sch.symbols {
        if let Some(k) = unit_key(s) {
            units.entry(k).or_default().push(s.board_path.as_str());
        }
    }
    let mut seen_units: BTreeSet<(String, String, String)> = BTreeSet::new();

    for s in &sch.symbols {
        let key = unit_key(s);
        if let Some(k) = &key
            && !seen_units.insert(k.clone())
        {
            continue;
        }
        let embedded = sch.embedded(s);
        let lid = LibId::parse(&s.lib_id);
        let slib = sym_libs
            .entry(lid.nickname.clone())
            .or_insert_with(|| resolver.symbol_lib(&lid.nickname))
            .clone();
        let lib_sym = store.symbol(&slib, &lid.item).unwrap_or_else(|e| {
            rep.warnings.push(format!(
                "{}: reading symbol {}: {e:#}",
                s.reference, s.lib_id
            ));
            None
        });
        let Some(sym_node) = embedded.cloned().or_else(|| lib_sym.clone()) else {
            rep.warnings.push(format!(
                "{} ({}): symbol not embedded and not found in library",
                s.reference, s.lib_id
            ));
            continue;
        };
        if compare::is_power(&sym_node) || compare::is_graphic_only(&sym_node) {
            continue;
        }
        rep.symbol_count += 1;
        let fp_idx = match key.as_ref().and_then(|k| units.get(k)) {
            Some(paths) => paths.iter().find_map(|p| by_path.get(p).copied()),
            None => by_path.get(s.board_path.as_str()).copied(),
        };
        if let Some(i) = fp_idx {
            used_fp.insert(i);
        }
        let fp = fp_idx.and_then(|i| board.as_ref().map(|b| &b.footprints[i]));
        let ver = project.version_of(&s.sheet_file);
        let mut f = new_finding(&source, project, ver);
        f.instance.reference = s.reference.clone();
        f.instance.value = s.value.clone();
        f.instance.sheet = Some(s.sheet_name_path.clone());
        f.instance.sheet_file = Some(s.sheet_file.clone());
        f.instance.symbol_uuid = Some(s.uuid.clone());
        f.instance.dnp = s.dnp;
        f.instance.on_schematic = true;

        analyse_symbol(
            &mut f,
            s,
            &slib,
            embedded,
            lib_sym.as_ref(),
            &sym_node,
            &alt,
            &store,
        );

        match fp {
            Some(bf) => analyse_footprint(
                &mut f,
                vfs,
                project,
                &resolver,
                &store,
                &mut fp_libs,
                bf,
                &board_embedded,
                rep,
                &alt,
            ),
            None if !s.footprint.is_empty() => analyse_schematic_only_footprint(
                &mut f,
                &resolver,
                &store,
                &mut fp_libs,
                &s.footprint,
                vfs,
                project,
                rep,
            ),
            None => {}
        }
        if f.flags.any() {
            findings.push(f);
        }
    }

    // Board footprints without a schematic symbol.
    if let Some(b) = &board {
        for (i, bf) in b.footprints.iter().enumerate() {
            if used_fp.contains(&i) {
                continue;
            }
            let has_pads = bf.node.find("pad").is_some();
            if !has_pads {
                continue; // graphics-only board items (logos etc.)
            }
            let ver = project.version_of(&b.file);
            let mut f = new_finding(&source, project, ver);
            f.instance.reference = bf.reference.clone();
            f.instance.value = bf.value.clone();
            f.instance.dnp = bf.dnp;
            analyse_footprint(
                &mut f,
                vfs,
                project,
                &resolver,
                &store,
                &mut fp_libs,
                bf,
                &board_embedded,
                rep,
                &alt,
            );
            if bf.path.is_some() {
                f.notes
                    .push("footprint links to a schematic symbol that was not found".into());
            }
            if f.flags.any() {
                findings.push(f);
            }
        }
    }

    rep.symbol_libraries = sym_libs.values().map(lib_use).collect();
    rep.footprint_libraries = fp_libs.values().map(lib_use).collect();
    Ok(findings)
}

fn new_finding(source: &str, project: &Project, version: KicadVersion) -> Finding {
    Finding {
        flags: Flags::default(),
        version,
        lib_id: None,
        lib_name: None,
        footprint_id: None,
        symbol_source: None,
        footprint_source: None,
        symbol_diff: None,
        footprint_diff: None,
        models_diff: vec![],
        models: vec![],
        reasons: vec![],
        notes: vec![],
        instance: InstanceRef {
            source: source.to_string(),
            project: project.name.clone(),
            project_path: project.pro.clone(),
            reference: String::new(),
            value: String::new(),
            sheet: None,
            sheet_file: None,
            symbol_uuid: None,
            footprint_uuid: None,
            layer: None,
            dnp: false,
            on_schematic: false,
            on_board: false,
        },
        symbol_node: None,
        footprint_node: None,
        footprint_from_board: false,
        model_files: vec![],
        symbol_hash: None,
        footprint_hash: None,
        model_hashes: vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn analyse_symbol(
    f: &mut Finding,
    s: &SchSymbol,
    slib: &ResolvedLib,
    embedded: Option<&Node>,
    lib_sym: Option<&Node>,
    sym_node: &Node,
    alt: &[(KicadVersion, Resolver)],
    store: &LibStore,
) {
    f.lib_id = Some(s.lib_id.clone());
    f.lib_name = s.lib_name.clone();
    f.symbol_source = Some(slib.source);
    if slib.source.is_custom() {
        f.flags.symbol = true;
        f.reasons.push(format!(
            "symbol library '{}' is {}",
            slib.nickname,
            source_word(slib.source)
        ));
    }
    match (embedded, lib_sym) {
        (Some(e), Some(l)) => {
            let d = compare::diff_symbol(e, l);
            let drift = (!d.is_empty() && slib.source == LibSource::Stock)
                .then(|| {
                    let item = LibId::parse(&s.lib_id).item;
                    alt.iter().find_map(|(v, r)| {
                        let rl = r.symbol_lib(&slib.nickname);
                        let l2 = store.symbol(&rl, &item).ok().flatten()?;
                        (rl.source == LibSource::Stock && compare::diff_symbol(e, &l2).is_empty())
                            .then_some(*v)
                    })
                })
                .flatten();
            if let Some(v) = drift {
                f.notes.push(format!(
                    "embedded symbol matches the KiCad {} stock library (library drift, not a local edit)",
                    v.major()
                ));
            } else if !d.is_empty() {
                f.flags.symbol_local = true;
                f.reasons.push(match &s.lib_name {
                    Some(n) => {
                        format!("symbol edited on schematic (variant '{n}' differs from library)")
                    }
                    None => "embedded symbol differs from library".into(),
                });
                f.symbol_diff = Some(d);
            }
        }
        (Some(_), None) => {
            if s.lib_name.is_some() {
                f.notes.push(format!(
                    "symbol variant '{}' present but library symbol unavailable; cannot compare",
                    s.lib_name.as_deref().unwrap_or_default()
                ));
            } else {
                f.notes
                    .push("library symbol unavailable; only the embedded copy exists".into());
            }
        }
        (None, _) => f
            .notes
            .push("symbol not embedded in schematic; library copy used".into()),
    }
    f.symbol_hash = Some(compare::symbol_hash(sym_node));
    f.symbol_node = Some(sym_node.clone());
}

fn source_word(s: LibSource) -> &'static str {
    match s {
        LibSource::Stock => "stock",
        LibSource::Project => "a project library",
        LibSource::ProjectUnregistered => "an unregistered project library",
        LibSource::Unresolved => "unresolved (not found in any library)",
    }
}

#[allow(clippy::too_many_arguments)]
fn analyse_footprint(
    f: &mut Finding,
    vfs: &dyn Vfs,
    project: &Project,
    resolver: &Resolver,
    store: &LibStore,
    fp_libs: &mut BTreeMap<String, ResolvedLib>,
    bf: &BoardFootprint,
    board_embedded: &[EmbeddedFile],
    rep: &mut ProjectReport,
    alt: &[(KicadVersion, Resolver)],
) {
    let fid = LibId::parse(&bf.fpid);
    let flib = fp_libs
        .entry(fid.nickname.clone())
        .or_insert_with(|| resolver.footprint_lib(&fid.nickname))
        .clone();
    f.footprint_id = Some(bf.fpid.clone());
    f.footprint_source = Some(flib.source);
    f.instance.footprint_uuid = Some(bf.uuid.clone());
    f.instance.layer = Some(bf.layer.clone());
    f.instance.on_board = true;
    f.instance.dnp |= bf.dnp;
    if f.instance.reference.is_empty() {
        f.instance.reference = bf.reference.clone();
    }
    if flib.source.is_custom() {
        f.flags.footprint = true;
        f.reasons.push(format!(
            "footprint library '{}' is {}",
            flib.nickname,
            source_word(flib.source)
        ));
    }
    let lib_fp = store.footprint(&flib, &fid.item).unwrap_or_else(|e| {
        rep.warnings.push(format!(
            "{}: reading footprint {}: {e:#}",
            bf.reference, bf.fpid
        ));
        None
    });
    match &lib_fp {
        Some(l) => {
            let alt_fp = || -> Vec<(KicadVersion, Node)> {
                if flib.source != LibSource::Stock {
                    return vec![];
                }
                alt.iter()
                    .filter_map(|(v, r)| {
                        let rl = r.footprint_lib(&flib.nickname);
                        (rl.source == LibSource::Stock).then_some(())?;
                        store
                            .footprint(&rl, &fid.item)
                            .ok()
                            .flatten()
                            .map(|n| (*v, n))
                    })
                    .collect()
            };
            let d = compare::diff_footprint(&bf.node, l);
            let fp_drift = if d.is_empty() {
                None
            } else {
                alt_fp()
                    .into_iter()
                    .find(|(_, n)| compare::diff_footprint(&bf.node, n).is_empty())
                    .map(|(v, _)| v)
            };
            if let Some(v) = fp_drift {
                f.notes.push(format!(
                    "board footprint matches the KiCad {} stock library (library drift, not a local edit)",
                    v.major()
                ));
            } else if !d.is_empty() {
                f.flags.footprint_local = true;
                f.reasons
                    .push("footprint on board differs from library".into());
                f.footprint_diff = Some(d);
            }
            let mut md = compare::models_diff(&bf.node, l);
            if !md.is_empty()
                && alt_fp()
                    .iter()
                    .any(|(_, n)| compare::models_diff(&bf.node, n).is_empty())
            {
                f.notes.push(
                    "3D models match another KiCad stock library version (library drift)".into(),
                );
                md.clear();
            }
            if !md.is_empty() {
                f.flags.model = true;
                f.reasons
                    .push("3D model list/transform differs from library footprint".into());
                f.models_diff = md;
            }
        }
        None => f
            .notes
            .push("library footprint unavailable; only the board copy exists".into()),
    }

    // 3D models
    let (fp_emb, w) = match bf.node.find("embedded_files") {
        Some(n) => models::parse_embedded_files(n),
        None => (vec![], vec![]),
    };
    rep.warnings.extend(w);
    let mut emb = fp_emb;
    emb.extend(board_embedded.iter().cloned());
    let ctx = ModelCtx {
        vfs,
        version: f.version,
        project_dir: &project.dir,
        embedded: &emb,
    };
    collect_models(f, &bf.node, &ctx, rep);

    f.footprint_hash = Some(compare::footprint_hash(&bf.node));
    f.footprint_node = Some(bf.node.clone());
    f.footprint_from_board = true;
}

fn collect_models(f: &mut Finding, fp: &Node, ctx: &ModelCtx, rep: &mut ProjectReport) {
    for mr in models::model_refs(fp) {
        let rm = models::resolve(&mr, ctx);
        if rm.origin != ModelOrigin::Stock {
            f.flags.model = true;
            f.reasons.push(format!(
                "3D model '{}' is not a stock model ({})",
                mr.path,
                format!("{:?}", rm.origin).to_lowercase()
            ));
            if !rm.found {
                rep.warnings.push(format!(
                    "{}: 3D model '{}' not found",
                    f.instance.reference, mr.path
                ));
            }
        }
        let bytes = if rm.origin != ModelOrigin::Stock && rm.found {
            models::load_bytes(&rm, ctx).unwrap_or_else(|e| {
                rep.warnings.push(format!(
                    "{}: reading model '{}': {e:#}",
                    f.instance.reference, mr.path
                ));
                None
            })
        } else {
            None
        };
        match bytes {
            Some(b) => {
                f.model_hashes.push(sha_hex(&b));
                f.model_files.push((rm.file_name.clone(), b));
            }
            None => f.model_hashes.push(format!(
                "path:{}:{:?}:{:?}:{:?}",
                mr.path, mr.offset, mr.scale, mr.rotate
            )),
        }
        f.models.push(rm);
    }
}

#[allow(clippy::too_many_arguments)]
fn analyse_schematic_only_footprint(
    f: &mut Finding,
    resolver: &Resolver,
    store: &LibStore,
    fp_libs: &mut BTreeMap<String, ResolvedLib>,
    fpid: &str,
    vfs: &dyn Vfs,
    project: &Project,
    rep: &mut ProjectReport,
) {
    let fid = LibId::parse(fpid);
    let flib = fp_libs
        .entry(fid.nickname.clone())
        .or_insert_with(|| resolver.footprint_lib(&fid.nickname))
        .clone();
    f.footprint_id = Some(fpid.to_string());
    f.footprint_source = Some(flib.source);
    if flib.source.is_custom() {
        f.flags.footprint = true;
        f.reasons.push(format!(
            "footprint library '{}' (from the symbol's Footprint field) is {}",
            flib.nickname,
            source_word(flib.source)
        ));
    }
    if let Ok(Some(l)) = store.footprint(&flib, &fid.item) {
        let ctx = ModelCtx {
            vfs,
            version: f.version,
            project_dir: &project.dir,
            embedded: &[],
        };
        collect_models(f, &l, &ctx, rep);
        f.footprint_hash = Some(compare::footprint_hash(&l));
        f.footprint_node = Some(l);
    }
}

/// Discover and analyse all projects of one source.
pub fn analyse_source(
    vfs: &dyn Vfs,
    excludes: &[String],
    stock: &BTreeMap<KicadVersion, StockLibs>,
) -> Result<(Vec<ProjectReport>, Vec<Finding>)> {
    let projects = project::discover(vfs, excludes)?;
    let mut reps = Vec::new();
    let mut all = Vec::new();
    for p in &projects {
        let (r, f) = analyse_project(vfs, p, stock);
        reps.push(r);
        all.extend(f);
    }
    Ok((reps, all))
}
