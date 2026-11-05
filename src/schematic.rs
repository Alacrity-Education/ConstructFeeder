//! Schematic extraction.
//! OWNER: agent "extract".

use crate::project::Project;
use crate::sexpr::Node;
use crate::vfs::Vfs;
use anyhow::{Context, Result};
use std::collections::BTreeMap;

/// One placed symbol *instance* (a symbol inside a sheet used N times yields N instances).
#[derive(Clone, Debug, PartialEq)]
pub struct SchSymbol {
    /// Repo-relative .kicad_sch the symbol is drawn in.
    pub sheet_file: String,
    /// Human readable hierarchical sheet path, e.g. "/" or "/CHARGER/".
    pub sheet_name_path: String,
    /// Full instance path of the *containing sheet* as written in `(instances (project .. (path "...")))`,
    /// e.g. "/<root-uuid>/<sheet-uuid>".
    pub instance_path: String,
    /// Symbol uuid.
    pub uuid: String,
    /// Path used by the board to link footprints: instance_path without the leading root uuid,
    /// plus "/<uuid>". e.g. "/<sheet-uuid>/<symbol-uuid>" or "/<symbol-uuid>" on the root sheet.
    pub board_path: String,
    pub lib_id: String,
    /// `(lib_name "X")` if present: the embedded symbol key is then X instead of lib_id.
    pub lib_name: Option<String>,
    pub reference: String,
    pub value: String,
    /// "Footprint" property (may be empty).
    pub footprint: String,
    pub unit: u32,
    pub dnp: bool,
    pub in_bom: bool,
    pub on_board: bool,
}

impl SchSymbol {
    /// Key into the sheet's `(lib_symbols ...)` cache.
    pub fn embedded_key(&self) -> &str {
        self.lib_name.as_deref().unwrap_or(&self.lib_id)
    }
}

#[derive(Debug, Default)]
pub struct SchematicSet {
    /// (sheet_file, embedded symbol name) -> the `(symbol "Name" ...)` node from that sheet's lib_symbols.
    pub lib_symbols: BTreeMap<(String, String), Node>,
    /// All instances across the hierarchy, ordered by (sheet_name_path, reference).
    pub symbols: Vec<SchSymbol>,
    pub warnings: Vec<String>,
}

impl SchematicSet {
    pub fn embedded(&self, s: &SchSymbol) -> Option<&Node> {
        self.lib_symbols
            .get(&(s.sheet_file.clone(), s.embedded_key().to_string()))
    }
}

/// Load every schematic in `project.schematics` (root first). Instances are taken from
/// each symbol's `(instances (project "<name>" (path ...)))` entries; prefer entries whose project
/// name equals `project.name`, else use all entries. Only paths that are reachable from the root
/// sheet's hierarchy are kept (stale instance data from copied sheets must be ignored).
/// Symbols with lib_id starting with "power:" are still returned (filtering happens later).
pub fn load(vfs: &dyn Vfs, project: &Project) -> Result<SchematicSet> {
    let mut set = SchematicSet::default();
    let Some(root_file) = project.schematics.first() else {
        return Ok(set);
    };
    let mut cache: BTreeMap<String, Node> = BTreeMap::new();
    for f in &project.schematics {
        if cache.contains_key(f) {
            continue;
        }
        match parse_file(vfs, f) {
            Ok(n) => {
                collect_lib_symbols(f, &n, &mut set);
                cache.insert(f.clone(), n);
            }
            Err(e) if f == root_file => return Err(e),
            Err(e) => set.warnings.push(format!("{f}: {e:#}")),
        }
    }
    let root_uuid = cache
        .get(root_file)
        .and_then(|n| n.value("uuid"))
        .unwrap_or("")
        .to_string();
    let mut stack = vec![root_file.clone()];
    let root_path = format!("/{root_uuid}");
    walk(
        vfs, project, &mut cache, &mut set, &mut stack, root_file, &root_path, "/",
    );
    set.symbols.sort_by(|a, b| {
        (&a.sheet_name_path, &a.reference).cmp(&(&b.sheet_name_path, &b.reference))
    });
    Ok(set)
}

fn parse_file(vfs: &dyn Vfs, f: &str) -> Result<Node> {
    let text = vfs.read_string(f).with_context(|| format!("reading {f}"))?;
    crate::sexpr::parse(&text).map_err(|e| anyhow::anyhow!("parsing {f}: {e}"))
}

fn collect_lib_symbols(file: &str, root: &Node, set: &mut SchematicSet) {
    if let Some(ls) = root.find("lib_symbols") {
        for s in ls.find_all("symbol") {
            if let Some(name) = s.first_arg() {
                set.lib_symbols
                    .insert((file.to_string(), name.to_string()), s.clone());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    vfs: &dyn Vfs,
    project: &Project,
    cache: &mut BTreeMap<String, Node>,
    set: &mut SchematicSet,
    stack: &mut Vec<String>,
    file: &str,
    inst_path: &str,
    name_path: &str,
) {
    let Some(root) = cache.get(file).cloned() else {
        return;
    };
    // board path prefix: instance path without the leading root uuid component
    let board_prefix = {
        let rest = inst_path.strip_prefix('/').unwrap_or(inst_path);
        match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => String::new(),
        }
    };
    for sym in root.find_all("symbol") {
        let uuid = sym.value("uuid").unwrap_or("").to_string();
        let entry = find_instance(sym, inst_path, &project.name);
        let reference = entry
            .and_then(|e| e.value("reference"))
            .or_else(|| sym.property("Reference"))
            .unwrap_or("")
            .to_string();
        let unit = entry
            .and_then(|e| e.value("unit"))
            .or_else(|| sym.value("unit"))
            .and_then(|u| u.parse().ok())
            .unwrap_or(1);
        set.symbols.push(SchSymbol {
            sheet_file: file.to_string(),
            sheet_name_path: name_path.to_string(),
            instance_path: inst_path.to_string(),
            board_path: format!("{board_prefix}/{uuid}"),
            uuid,
            lib_id: sym.value("lib_id").unwrap_or("").to_string(),
            lib_name: sym.value("lib_name").map(str::to_string),
            reference,
            value: sym.property("Value").unwrap_or("").to_string(),
            footprint: sym.property("Footprint").unwrap_or("").to_string(),
            unit,
            dnp: sym.flag("dnp").unwrap_or(false),
            in_bom: sym.flag("in_bom").unwrap_or(true),
            on_board: sym.flag("on_board").unwrap_or(true),
        });
    }
    let dir = crate::vfs::path::parent(file).to_string();
    for sh in root.find_all("sheet") {
        let Some(suuid) = sh.value("uuid") else {
            continue;
        };
        let Some(sfile) = sh
            .property("Sheetfile")
            .or_else(|| sh.property("Sheet file"))
        else {
            continue;
        };
        let sname = sh
            .property("Sheetname")
            .or_else(|| sh.property("Sheet name"))
            .unwrap_or("");
        let Some(child) = crate::vfs::path::join(&dir, sfile) else {
            set.warnings
                .push(format!("{file}: bad sheet file path {sfile:?}"));
            continue;
        };
        if stack.contains(&child) {
            set.warnings
                .push(format!("{file}: recursive sheet reference to {child}"));
            continue;
        }
        if !cache.contains_key(&child) {
            if !vfs.exists(&child) {
                set.warnings
                    .push(format!("{file}: missing sheet file {child}"));
                continue;
            }
            match parse_file(vfs, &child) {
                Ok(n) => {
                    collect_lib_symbols(&child, &n, set);
                    cache.insert(child.clone(), n);
                }
                Err(e) => {
                    set.warnings.push(format!("{child}: {e:#}"));
                    continue;
                }
            }
        }
        stack.push(child.clone());
        let cpath = format!("{inst_path}/{suuid}");
        let cname = format!("{name_path}{sname}/");
        walk(vfs, project, cache, set, stack, &child, &cpath, &cname);
        stack.pop();
    }
}

/// The `(path ..)` entry of the symbol's instances matching `inst_path`, preferring `project_name`.
fn find_instance<'a>(sym: &'a Node, inst_path: &str, project_name: &str) -> Option<&'a Node> {
    let inst = sym.find("instances")?;
    let mut any = None;
    for p in inst.find_all("project") {
        let Some(e) = p
            .find_all("path")
            .find(|e| e.first_arg() == Some(inst_path))
        else {
            continue;
        };
        if p.first_arg() == Some(project_name) {
            return Some(e);
        }
        any.get_or_insert(e);
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MemVfs {
        files: Vec<String>,
        data: HashMap<String, String>,
    }
    impl MemVfs {
        fn new(items: &[(&str, &str)]) -> Self {
            let mut files: Vec<String> = items.iter().map(|(k, _)| k.to_string()).collect();
            files.sort();
            MemVfs {
                files,
                data: items
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        }
    }
    impl Vfs for MemVfs {
        fn files(&self) -> &[String] {
            &self.files
        }
        fn read(&self, p: &str) -> Result<Vec<u8>> {
            self.data
                .get(p)
                .map(|s| s.clone().into_bytes())
                .ok_or_else(|| anyhow::anyhow!("nf"))
        }
        fn label(&self) -> &str {
            "mem"
        }
        fn commit(&self) -> Option<&str> {
            None
        }
        fn dirty(&self) -> Option<bool> {
            None
        }
    }

    const ROOT: &str = r#"(kicad_sch (uuid "root")
      (lib_symbols (symbol "Device:R" (symbol "R_1_1")) (symbol "R_1"))
      (symbol (lib_id "Device:R") (lib_name "R_1") (uuid "sym-root") (unit 1) (dnp yes) (on_board no) (in_bom no)
        (property "Reference" "R9") (property "Value" "1k") (property "Footprint" "FP:X")
        (instances (project "P" (path "/root" (reference "R1") (unit 1)))))
      (sheet (uuid "sa") (property "Sheetname" "A") (property "Sheetfile" "sub/s.kicad_sch")
        (instances (project "P" (path "/root" (page "2")))))
      (sheet (uuid "sb") (property "Sheetname" "B") (property "Sheetfile" "sub/s.kicad_sch")))"#;
    const SUB: &str = r#"(kicad_sch (uuid "subfile")
      (lib_symbols (symbol "Device:C"))
      (symbol (lib_id "Device:C") (uuid "c1") (property "Reference" "C?") (property "Value" "1u")
        (instances (project "P"
          (path "/root/sa" (reference "C1") (unit 1))
          (path "/root/sb" (reference "C2") (unit 1))
          (path "/root/stale" (reference "C3") (unit 1))))))"#;

    #[test]
    fn hierarchy() {
        let vfs = MemVfs::new(&[("p.kicad_sch", ROOT), ("sub/s.kicad_sch", SUB)]);
        let proj = Project {
            name: "P".into(),
            dir: "".into(),
            pro: "p.kicad_pro".into(),
            schematics: vec!["p.kicad_sch".into(), "sub/s.kicad_sch".into()],
            pcb: None,
            sym_lib_table: None,
            fp_lib_table: None,
            versions: Default::default(),
            unsupported: None,
            warnings: vec![],
        };
        let set = load(&vfs, &proj).unwrap();
        assert_eq!(set.symbols.len(), 3, "{:?}", set.symbols);
        let r1 = &set.symbols[0];
        assert_eq!(
            (r1.reference.as_str(), r1.sheet_name_path.as_str()),
            ("R1", "/")
        );
        assert_eq!(r1.board_path, "/sym-root");
        assert_eq!(r1.instance_path, "/root");
        assert!(r1.dnp && !r1.on_board && !r1.in_bom);
        assert_eq!(r1.embedded_key(), "R_1");
        assert!(set.embedded(r1).is_some());
        let c1 = &set.symbols[1];
        assert_eq!(
            (c1.reference.as_str(), c1.sheet_name_path.as_str()),
            ("C1", "/A/")
        );
        assert_eq!(c1.board_path, "/sa/c1");
        assert_eq!(c1.sheet_file, "sub/s.kicad_sch");
        assert!(!c1.dnp && c1.in_bom && c1.on_board);
        let c2 = &set.symbols[2];
        assert_eq!(
            (
                c2.reference.as_str(),
                c2.board_path.as_str(),
                c2.sheet_name_path.as_str()
            ),
            ("C2", "/sb/c1", "/B/")
        );
    }
}
