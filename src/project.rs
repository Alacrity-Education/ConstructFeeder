//! Project discovery and KiCad version detection.
//! OWNER: agent "libs".

use crate::sexpr;
use crate::types::KicadVersion;
use crate::vfs::{Vfs, path};
use anyhow::Result;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, HashSet};

/// Default exclude globs (matched against repo-relative paths, any component).
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "**/*backup*/**",
    "**/*-backups/**",
    "**/.history/**",
    "**/.git/**",
    "**/target/**",
    "**/.claude/**",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    /// File stem of the .kicad_pro, e.g. "airbox".
    pub name: String,
    /// Repo-relative directory of the .kicad_pro ("" if at repo root).
    pub dir: String,
    /// Repo-relative path of the .kicad_pro.
    pub pro: String,
    /// Root schematic first, then every sheet file reachable through the hierarchy
    /// (`(sheet ... (property "Sheetfile" "x.kicad_sch"))`, paths relative to the
    /// containing sheet). Unique, in discovery (DFS) order. Missing sheet files are skipped
    /// and reported in `warnings`.
    pub schematics: Vec<String>,
    /// `<dir>/<name>.kicad_pcb` if it exists.
    pub pcb: Option<String>,
    /// `<dir>/sym-lib-table` if it exists.
    pub sym_lib_table: Option<String>,
    /// `<dir>/fp-lib-table` if it exists.
    pub fp_lib_table: Option<String>,
    /// Detected version per schematic/board file (only files that parsed as supported).
    pub versions: BTreeMap<String, KicadVersion>,
    /// Set if any schematic/board file is not KiCad 9/10 (message names the file + detected version).
    pub unsupported: Option<String>,
    pub warnings: Vec<String>,
}

impl Project {
    /// Version used for a given file, falling back to the project's highest version, then V10.
    pub fn version_of(&self, file: &str) -> KicadVersion {
        self.versions
            .get(file)
            .copied()
            .or_else(|| self.versions.values().max().copied())
            .unwrap_or(KicadVersion::V10)
    }
}

/// Build a glob set from the default excludes plus caller-provided ones.
pub fn build_excludes(extra: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for g in DEFAULT_EXCLUDES
        .iter()
        .copied()
        .chain(extra.iter().map(|s| s.as_str()))
    {
        b.add(
            GlobBuilder::new(g)
                .case_insensitive(true)
                .literal_separator(false)
                .build()?,
        );
    }
    Ok(b.build()?)
}

/// Find all projects: every `*.kicad_pro` not matching `excludes` (globs, see DEFAULT_EXCLUDES).
/// A project without a root schematic (`<dir>/<name>.kicad_sch`) AND without a board is skipped.
/// Sorted by `pro` path.
pub fn discover(vfs: &dyn Vfs, excludes: &[String]) -> Result<Vec<Project>> {
    let set = build_excludes(excludes)?;
    let mut out = Vec::new();
    for f in vfs.files() {
        if !f.ends_with(".kicad_pro") || set.is_match(f.as_str()) {
            continue;
        }
        if let Some(p) = load_project(vfs, f) {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.pro.cmp(&b.pro));
    Ok(out)
}

fn load_project(vfs: &dyn Vfs, pro: &str) -> Option<Project> {
    let dir = path::parent(pro).to_string();
    let name = path::stem(pro).to_string();
    let sch = path::join(&dir, &format!("{name}.kicad_sch"))?;
    let pcb = path::join(&dir, &format!("{name}.kicad_pcb"))?;
    let has_sch = vfs.exists(&sch);
    let has_pcb = vfs.exists(&pcb);
    if !has_sch && !has_pcb {
        return None;
    }
    let opt = |n: &str| path::join(&dir, n).filter(|p| vfs.exists(p));
    let mut proj = Project {
        name,
        dir: dir.clone(),
        pro: pro.to_string(),
        schematics: Vec::new(),
        pcb: has_pcb.then(|| pcb.clone()),
        sym_lib_table: opt("sym-lib-table"),
        fp_lib_table: opt("fp-lib-table"),
        versions: BTreeMap::new(),
        unsupported: None,
        warnings: Vec::new(),
    };
    if has_sch {
        let mut seen = HashSet::new();
        walk_sheets(vfs, &sch, &mut seen, &mut proj);
    }
    if has_pcb {
        match vfs.read_string(&pcb) {
            Ok(text) => record_version(&mut proj, &pcb, &text),
            Err(e) => proj.warnings.push(format!("{pcb}: {e}")),
        }
    }
    Some(proj)
}

fn record_version(proj: &mut Project, file: &str, text: &str) {
    match detect_version(text) {
        Ok(v) => {
            proj.versions.insert(file.to_string(), v);
        }
        Err(e) => {
            if proj.unsupported.is_none() {
                proj.unsupported = Some(format!("{file}: {e}"));
            }
        }
    }
}

fn walk_sheets(vfs: &dyn Vfs, file: &str, seen: &mut HashSet<String>, proj: &mut Project) {
    if !seen.insert(file.to_string()) {
        return;
    }
    let text = match vfs.read_string(file) {
        Ok(t) => t,
        Err(e) => {
            proj.warnings.push(format!("{file}: {e}"));
            return;
        }
    };
    proj.schematics.push(file.to_string());
    record_version(proj, file, &text);
    let root = match sexpr::parse(&text) {
        Ok(r) => r,
        Err(e) => {
            proj.warnings.push(format!("{file}: parse error: {e}"));
            return;
        }
    };
    for sheet in root.find_all("sheet") {
        let Some(rel) = sheet
            .property("Sheetfile")
            .or_else(|| sheet.property("Sheet file"))
        else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let Some(target) = path::join(path::parent(file), rel) else {
            proj.warnings
                .push(format!("{file}: sheet path '{rel}' escapes the repository"));
            continue;
        };
        if !vfs.exists(&target) {
            proj.warnings
                .push(format!("{file}: missing sheet file '{target}'"));
            continue;
        }
        walk_sheets(vfs, &target, seen, proj);
    }
}

/// Detect the KiCad version from the head of a .kicad_sch / .kicad_pcb / .kicad_sym / .kicad_mod.
/// Uses `(generator_version "9.0")`-style major; files without generator_version
/// (KiCad <= 7) or with major < 9 return Err("KiCad <major or 'unknown'> is not supported").
/// Majors > 10 are also Err. Only needs to look at the first ~2KB.
pub fn detect_version(text: &str) -> Result<KicadVersion, String> {
    let mut end = text.len().min(2048);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let unknown = || "KiCad unknown is not supported".to_string();
    let Some(i) = head.find("(generator_version") else {
        return Err(unknown());
    };
    let rest = head[i + "(generator_version".len()..]
        .trim_start()
        .trim_start_matches('"');
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let Ok(major) = digits.parse::<u32>() else {
        return Err(unknown());
    };
    KicadVersion::from_major(major).ok_or_else(|| format!("KiCad {major} is not supported"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<String>, BTreeMap<String, String>);
    impl Mem {
        fn new(files: &[(&str, &str)]) -> Mem {
            let mut v: Vec<String> = files.iter().map(|f| f.0.to_string()).collect();
            v.sort();
            Mem(
                v,
                files
                    .iter()
                    .map(|(a, b)| (a.to_string(), b.to_string()))
                    .collect(),
            )
        }
    }
    impl Vfs for Mem {
        fn files(&self) -> &[String] {
            &self.0
        }
        fn read(&self, p: &str) -> Result<Vec<u8>> {
            self.1
                .get(p)
                .map(|s| s.clone().into_bytes())
                .ok_or_else(|| anyhow::anyhow!("no {p}"))
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

    fn sch(ver: &str, sheets: &[&str]) -> String {
        let mut s = format!(
            "(kicad_sch (version 20250114) (generator \"eeschema\") (generator_version \"{ver}\")\n"
        );
        for f in sheets {
            s += &format!(
                "(sheet (at 0 0) (property \"Sheetname\" \"x\") (property \"Sheetfile\" \"{f}\"))\n"
            );
        }
        s + ")"
    }

    #[test]
    fn versions() {
        assert_eq!(
            detect_version("(kicad_sch (generator_version \"9.0\"))"),
            Ok(KicadVersion::V9)
        );
        assert_eq!(
            detect_version("(kicad_pcb (generator_version \"10.0\"))"),
            Ok(KicadVersion::V10)
        );
        assert!(
            detect_version("(kicad_sch (version 20211123))")
                .unwrap_err()
                .contains("unknown")
        );
        assert!(
            detect_version("(x (generator_version \"8.0\"))")
                .unwrap_err()
                .contains("8")
        );
        assert!(detect_version("(x (generator_version \"11.0\"))").is_err());
    }

    #[test]
    fn discovery_hierarchy_cycles_excludes() {
        let root = sch(
            "9.0",
            &["sub/a.kicad_sch", "b.kicad_sch", "missing.kicad_sch"],
        );
        let a = sch("9.0", &["../b.kicad_sch", "../top.kicad_sch"]);
        let b = sch("8.0", &[]);
        let vfs = Mem::new(&[
            ("p/top.kicad_pro", "{}"),
            ("p/top.kicad_sch", &root),
            ("p/sub/a.kicad_sch", &a),
            ("p/b.kicad_sch", &b),
            (
                "p/top.kicad_pcb",
                "(kicad_pcb (generator_version \"10.0\"))",
            ),
            ("p/sym-lib-table", "(sym_lib_table)"),
            ("p-backups/top.kicad_pro", "{}"),
            ("p-backups/top.kicad_sch", &root),
            ("q/.history/x.kicad_pro", "{}"),
            ("q/x.kicad_pro", "{}"),
            ("r/nosch.kicad_pro", "{}"),
        ]);
        let ps = discover(&vfs, &[]).unwrap();
        assert_eq!(ps.len(), 1, "{ps:?}");
        let p = &ps[0];
        assert_eq!(
            p.schematics,
            ["p/top.kicad_sch", "p/sub/a.kicad_sch", "p/b.kicad_sch"]
        );
        assert_eq!(p.pcb.as_deref(), Some("p/top.kicad_pcb"));
        assert_eq!(p.sym_lib_table.as_deref(), Some("p/sym-lib-table"));
        assert_eq!(p.fp_lib_table, None);
        assert_eq!(p.versions.len(), 3);
        assert!(p.unsupported.as_ref().unwrap().contains("p/b.kicad_sch"));
        assert!(p.warnings.iter().any(|w| w.contains("missing.kicad_sch")));
        assert_eq!(p.version_of("p/top.kicad_pcb"), KicadVersion::V10);
        let ps = discover(&vfs, &["p/**".to_string()]).unwrap();
        assert!(ps.is_empty());
    }
}
