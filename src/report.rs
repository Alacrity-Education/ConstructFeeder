//! Grouping of findings into component folders, report JSON and zip export.

use crate::analysis::{Finding, Flags, InstanceRef, ProjectReport};
use crate::compare::{self, FootprintDiff, SymbolDiff};
use crate::models::ModelOrigin;
use crate::types::{KicadVersion, LibId, LibSource};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub path: String,
    pub origin: ModelOrigin,
    pub found: bool,
    pub offset: [f64; 3],
    pub scale: [f64; 3],
    pub rotate: [f64; 3],
    pub hide: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// Path inside the zip, if the file was exported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported_as: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint: Option<String>,
    pub models: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    /// Folder name inside the zip (`components/<folder>/`).
    pub folder: String,
    pub name: String,
    #[serde(flatten)]
    pub flags: Flags,
    pub kicad_version: KicadVersion,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lib_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lib_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_source: Option<LibSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint_source: Option<LibSource>,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_diff: Option<SymbolDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint_diff: Option<FootprintDiff>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models_diff: Vec<String>,
    pub models: Vec<ModelEntry>,
    pub hashes: Hashes,
    /// Files of this component inside the zip.
    pub files: Vec<String>,
    pub instance_count: usize,
    pub instances: Vec<InstanceRef>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub projects: usize,
    pub projects_unsupported: usize,
    pub projects_error: usize,
    pub components: usize,
    pub instances: usize,
    pub symbol: usize,
    pub symbol_local: usize,
    pub footprint: usize,
    pub footprint_local: usize,
    pub model: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub tool: BTreeMap<&'static str, &'static str>,
    pub generated_at: String,
    pub repository: String,
    pub sources: Vec<SourceInfo>,
    pub stock_libraries: BTreeMap<String, String>,
    pub summary: Summary,
    pub projects: Vec<ProjectReport>,
    pub components: Vec<Component>,
    pub warnings: Vec<String>,
}

/// A component folder plus the representative finding used for export.
pub struct Group {
    pub component: Component,
    pub rep: Finding,
}

fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let t = out.trim().trim_matches('.').to_string();
    if t.is_empty() { "unnamed".into() } else { t }
}

fn project_slug(i: &InstanceRef) -> String {
    let dir = crate::vfs::path::parent(&i.project_path);
    if dir.is_empty() {
        i.project.clone()
    } else {
        dir.replace('/', "-")
    }
}

/// Group findings by content and assign folder names.
pub fn group(findings: Vec<Finding>) -> Vec<Group> {
    // content hash -> findings, keeping first-seen order
    let mut order: Vec<String> = Vec::new();
    let mut by_hash: BTreeMap<String, Vec<Finding>> = BTreeMap::new();
    for f in findings {
        let h = f.content_hash();
        if !by_hash.contains_key(&h) {
            order.push(h.clone());
        }
        by_hash.entry(h).or_default().push(f);
    }
    // base name -> variant hashes
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for h in &order {
        let name = by_hash[h][0].base_name();
        by_name.entry(name).or_default().push(h.clone());
    }

    let mut folder_of: BTreeMap<String, String> = BTreeMap::new();
    for (name, hashes) in &by_name {
        let base = sanitize(name);
        if hashes.len() == 1 {
            folder_of.insert(hashes[0].clone(), base);
            continue;
        }
        let label = |h: &String, level: usize| -> String {
            let i = &by_hash[h][0].instance;
            // Shortest unique: board, board dir, board + reference, + branch, + both.
            match level {
                0 => i.project.clone(),
                1 => project_slug(i),
                2 => format!("{}_{}", project_slug(i), i.reference),
                3 => format!("{}@{}", project_slug(i), i.source),
                _ => format!("{}@{}_{}", project_slug(i), i.source, i.reference),
            }
        };
        let mut chosen = 4;
        for level in 0..4 {
            let labels: BTreeSet<String> = hashes.iter().map(|h| label(h, level)).collect();
            if labels.len() == hashes.len() {
                chosen = level;
                break;
            }
        }
        for h in hashes {
            folder_of.insert(h.clone(), format!("{base}_{}", sanitize(&label(h, chosen))));
        }
    }

    // Final uniqueness guard (case-insensitive file systems).
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut groups = Vec::new();
    for h in order {
        let mut folder = folder_of.remove(&h).unwrap();
        if !used.insert(folder.to_lowercase()) {
            let mut n = 2;
            while !used.insert(format!("{folder}_{n}").to_lowercase()) {
                n += 1;
            }
            folder = format!("{folder}_{n}");
        }
        let fs = by_hash.remove(&h).unwrap();
        groups.push(make_group(folder, h, fs));
    }
    groups.sort_by_key(|a| a.component.folder.to_lowercase());
    groups
}

fn make_group(folder: String, content: String, fs: Vec<Finding>) -> Group {
    let mut flags = Flags::default();
    let mut reasons: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut instances = Vec::new();
    for f in &fs {
        flags.symbol |= f.flags.symbol;
        flags.symbol_local |= f.flags.symbol_local;
        flags.footprint |= f.flags.footprint;
        flags.footprint_local |= f.flags.footprint_local;
        flags.model |= f.flags.model;
        for r in &f.reasons {
            if !reasons.contains(r) {
                reasons.push(r.clone());
            }
        }
        for n in &f.notes {
            if !notes.contains(n) {
                notes.push(n.clone());
            }
        }
        instances.push(f.instance.clone());
    }
    let rep = fs.into_iter().next().unwrap();
    let component = Component {
        folder,
        name: rep.base_name(),
        flags,
        kicad_version: rep.version,
        lib_id: rep.lib_id.clone(),
        lib_name: rep.lib_name.clone(),
        footprint_id: rep.footprint_id.clone(),
        symbol_source: rep.symbol_source,
        footprint_source: rep.footprint_source,
        reasons,
        notes,
        symbol_diff: rep.symbol_diff.clone(),
        footprint_diff: rep.footprint_diff.clone(),
        models_diff: rep.models_diff.clone(),
        models: rep
            .models
            .iter()
            .map(|m| ModelEntry {
                path: m.reference.path.clone(),
                origin: m.origin,
                found: m.found,
                offset: m.reference.offset,
                scale: m.reference.scale,
                rotate: m.reference.rotate,
                hide: m.reference.hide,
                repo_path: m.vfs_path.clone(),
                exported_as: None,
            })
            .collect(),
        hashes: Hashes {
            content,
            symbol: rep.symbol_hash.clone(),
            footprint: rep.footprint_hash.clone(),
            models: rep.model_hashes.clone(),
        },
        files: vec![],
        instance_count: instances.len(),
        instances,
    };
    Group { component, rep }
}

/// Render the files of one component: (zip path, bytes). Also fills `component.files` and
/// `models[].exported_as`.
pub fn component_files(g: &mut Group) -> Vec<(String, Vec<u8>)> {
    let c = &mut g.component;
    let f = &g.rep;
    let dir = format!("components/{}", c.folder);
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();

    if let Some(sym) = &f.symbol_node {
        let item = f
            .lib_id
            .as_deref()
            .map(|s| LibId::parse(s).item)
            .unwrap_or_else(|| c.name.clone());
        let lib = compare::symbol_to_library(sym, &item);
        out.push((
            format!("{dir}/{}.kicad_sym", c.folder),
            compare::symbol_library_file(&[lib], f.version).into_bytes(),
        ));
    }
    if let Some(fp) = &f.footprint_node {
        let item = f
            .footprint_id
            .as_deref()
            .map(|s| LibId::parse(s).item)
            .unwrap_or_else(|| c.name.clone());
        let lib = if f.footprint_from_board {
            compare::footprint_to_library(fp, &item, &[])
        } else {
            fp.clone()
        };
        out.push((
            format!("{dir}/{}.pretty/{}.kicad_mod", c.folder, sanitize(&item)),
            compare::footprint_file(&lib, f.version).into_bytes(),
        ));
    }
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut file_iter = f.model_files.iter();
    for m in c.models.iter_mut() {
        let exportable = m.origin != ModelOrigin::Stock && m.found;
        if !exportable {
            continue;
        }
        let Some((name, bytes)) = file_iter.next() else {
            break;
        };
        let mut n = sanitize(name);
        if !names.insert(n.clone()) {
            continue; // same file referenced twice
        }
        if n.is_empty() {
            n = "model".into();
        }
        let p = format!("{dir}/3d/{n}");
        m.exported_as = Some(p.clone());
        out.push((p, bytes.clone()));
    }
    c.files = out.iter().map(|(p, _)| p.clone()).collect();
    out
}

pub fn summarize(projects: &[ProjectReport], comps: &[Component]) -> Summary {
    Summary {
        projects: projects.len(),
        projects_unsupported: projects
            .iter()
            .filter(|p| p.status == "unsupported")
            .count(),
        projects_error: projects.iter().filter(|p| p.status == "error").count(),
        components: comps.len(),
        instances: comps.iter().map(|c| c.instance_count).sum(),
        symbol: comps.iter().filter(|c| c.flags.symbol).count(),
        symbol_local: comps.iter().filter(|c| c.flags.symbol_local).count(),
        footprint: comps.iter().filter(|c| c.flags.footprint).count(),
        footprint_local: comps.iter().filter(|c| c.flags.footprint_local).count(),
        model: comps.iter().filter(|c| c.flags.model).count(),
    }
}

/// Write the zip: report.json + component files.
pub fn write_zip(path: &Path, report: &Report, files: &[(String, Vec<u8>)]) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    zw.start_file("report.json", opts)?;
    zw.write_all(serde_json::to_string_pretty(report)?.as_bytes())?;
    zw.write_all(b"\n")?;
    for (p, b) in files {
        zw.start_file(p.as_str(), opts)?;
        zw.write_all(b)?;
    }
    zw.finish()?;
    Ok(())
}

/// RFC 3339 UTC timestamp without external crates.
pub fn now_rfc3339() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts() {
        // SAFETY: test-only, single-threaded use of env.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1730764800") };
        assert_eq!(now_rfc3339(), "2024-11-05T00:00:00Z");
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }
    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("DF12NB(3.0)/x:y"), "DF12NB(3.0)_x_y");
        assert_eq!(sanitize(""), "unnamed");
    }
}
