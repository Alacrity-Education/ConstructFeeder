//! Library tables, library classification and library item loading.
//! OWNER: agent "libs".

use crate::project::{Project, build_excludes};
use crate::sexpr::Node;
use crate::types::{KicadVersion, LibSource};
use crate::vfs::{Vfs, path};
use anyhow::{Result, anyhow};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibTableEntry {
    pub name: String,
    pub kind: String,
    pub uri: String,
    pub options: String,
    pub descr: String,
}

/// Parse a `sym_lib_table` / `fp_lib_table` file.
pub fn parse_lib_table(text: &str) -> Result<Vec<LibTableEntry>> {
    let root = crate::sexpr::parse(text).map_err(|e| anyhow!("lib table: {e}"))?;
    let mut out = Vec::new();
    for lib in root.find_all("lib") {
        let get = |k: &str| lib.value(k).unwrap_or("").to_string();
        out.push(LibTableEntry {
            name: get("name"),
            kind: get("type"),
            uri: get("uri"),
            options: get("options"),
            descr: get("descr"),
        });
    }
    Ok(out)
}

/// Stock KiCad libraries of one major version on the real file system.
/// Layout: `<root>/symbols/<Nick>.kicad_sym` and `<root>/footprints/<Nick>.pretty/<Name>.kicad_mod`.
/// (A symbol library may also be a directory `<root>/symbols/<Nick>.kicad_symdir/<Name>.kicad_sym`;
/// support it if present.)
#[derive(Clone, Debug)]
pub struct StockLibs {
    pub version: KicadVersion,
    pub root: PathBuf,
}

impl StockLibs {
    fn symbol_path(&self, nick: &str) -> Option<PathBuf> {
        let f = self.root.join("symbols").join(format!("{nick}.kicad_sym"));
        if f.is_file() {
            return Some(f);
        }
        let d = self
            .root
            .join("symbols")
            .join(format!("{nick}.kicad_symdir"));
        d.is_dir().then_some(d)
    }
    fn footprint_path(&self, nick: &str) -> Option<PathBuf> {
        let d = self.root.join("footprints").join(format!("{nick}.pretty"));
        d.is_dir().then_some(d)
    }
    pub fn has_symbol_lib(&self, nick: &str) -> bool {
        self.symbol_path(nick).is_some()
    }
    pub fn has_footprint_lib(&self, nick: &str) -> bool {
        self.footprint_path(nick).is_some()
    }
}

/// Where a library's files are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibLocation {
    /// Repo-relative path inside the VFS (a .kicad_sym file, a .kicad_symdir dir or a .pretty dir).
    Vfs(String),
    /// Real file system path (stock libraries).
    Fs(PathBuf),
    /// Not available.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLib {
    pub nickname: String,
    pub source: LibSource,
    pub location: LibLocation,
    /// URI from the lib table if the nickname came from one.
    pub uri: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Symbol,
    Footprint,
}

/// Classifies library nicknames for one project.
///
/// Rules (first match wins):
/// 1. Nickname in the project's table:
///    - URI uses `${KICAD<n>_SYMBOL_DIR}` / `${KICAD<n>_FOOTPRINT_DIR}` (any n) -> Stock (location in StockLibs).
///    - URI uses `${KIPRJMOD}` or is relative -> resolved inside the repo -> Project
///      (if the target doesn't exist in the VFS -> Unresolved, uri kept).
///    - Anything else (absolute path, other env vars) -> Unresolved.
/// 2. Nickname exists in StockLibs -> Stock.
/// 3. A `<nick>.kicad_sym` (symbols) / `<nick>.pretty` (footprints) exists anywhere under the
///    project dir (excluding backup/.history dirs) -> ProjectUnregistered (shallowest path wins).
/// 4. Otherwise Unresolved.
pub struct Resolver {
    project_dir: String,
    stock: StockLibs,
    sym_table: Vec<LibTableEntry>,
    fp_table: Vec<LibTableEntry>,
    /// Repo paths referenced by table entries that exist in the VFS (file or directory).
    existing: HashSet<String>,
    /// nick -> repo path of loose (unregistered) libraries.
    loose_sym: HashMap<String, String>,
    loose_fp: HashMap<String, String>,
}

/// Replace `${VAR}` / `$(VAR)` references by nothing; return the remaining text and the variable names.
fn expand(uri: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut vars = Vec::new();
    let b = uri.as_bytes();
    let mut i = 0;
    let mut last = 0;
    while i + 1 < b.len() {
        if b[i] == b'$' && (b[i + 1] == b'{' || b[i + 1] == b'(') {
            let close = if b[i + 1] == b'{' { '}' } else { ')' };
            if let Some(j) = uri[i + 2..].find(close) {
                out.push_str(&uri[last..i]);
                vars.push(uri[i + 2..i + 2 + j].to_string());
                i += 2 + j + 1;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&uri[last..]);
    (out, vars)
}

/// If `var` is `KICAD<n>_<SYMBOL|FOOTPRINT>_DIR`, return which kind.
fn stock_var(var: &str) -> Option<Kind> {
    let rest = var.strip_prefix("KICAD")?;
    match rest.trim_start_matches(|c: char| c.is_ascii_digit()) {
        "_SYMBOL_DIR" => Some(Kind::Symbol),
        "_FOOTPRINT_DIR" => Some(Kind::Footprint),
        _ => None,
    }
}

/// Repo-relative target of a table URI if it is project-local (`${KIPRJMOD}` or relative).
/// Outer None: not project-local (stock / absolute / other variable). Inner None: escapes the repo.
fn repo_target(project_dir: &str, uri: &str) -> Option<Option<String>> {
    let (expanded, vars) = expand(uri);
    let expanded = expanded.replace('\\', "/");
    match vars.as_slice() {
        [] => Some(path::join(project_dir, expanded.trim_end_matches('/'))),
        [v] if v == "KIPRJMOD" => Some(path::join(project_dir, expanded.trim_matches('/'))),
        _ => None,
    }
}

fn is_excluded_component(p: &str) -> bool {
    p.split('/').any(|c| {
        let l = c.to_ascii_lowercase();
        l.contains("backup") || l == ".history" || l == ".git" || l == "target"
    })
}

impl Resolver {
    pub fn new(vfs: &dyn Vfs, project: &Project, stock: &StockLibs) -> Result<Resolver> {
        let load = |p: &Option<String>| -> Result<Vec<LibTableEntry>> {
            match p {
                Some(p) => parse_lib_table(&vfs.read_string(p)?).map_err(|e| anyhow!("{p}: {e}")),
                None => Ok(Vec::new()),
            }
        };
        let sym_table = load(&project.sym_lib_table)?;
        let fp_table = load(&project.fp_lib_table)?;

        let mut existing = HashSet::new();
        for e in sym_table.iter().chain(fp_table.iter()) {
            if let Some(Some(p)) = repo_target(&project.dir, &e.uri)
                && (vfs.exists(&p) || vfs.is_dir(&p))
            {
                existing.insert(p);
            }
        }

        let excl = build_excludes(&[])?;
        let mut loose_sym: HashMap<String, String> = HashMap::new();
        let mut loose_fp: HashMap<String, String> = HashMap::new();
        let better = |old: Option<&String>, new: &str| match old {
            None => true,
            Some(o) => (new.matches('/').count(), new) < (o.matches('/').count(), o.as_str()),
        };
        let files: Vec<&str> = if project.dir.is_empty() {
            vfs.files().iter().map(|s| s.as_str()).collect()
        } else {
            vfs.files_under(&project.dir)
        };
        for f in files {
            let below = f.strip_prefix(project.dir.as_str()).unwrap_or(f);
            if excl.is_match(f) || is_excluded_component(below) {
                continue;
            }
            let comps: Vec<&str> = f.split('/').collect();
            for (idx, c) in comps.iter().enumerate() {
                let is_last = idx + 1 == comps.len();
                let (map, nick) = if is_last && c.ends_with(".kicad_sym") {
                    (&mut loose_sym, &c[..c.len() - ".kicad_sym".len()])
                } else if !is_last && c.ends_with(".kicad_symdir") {
                    (&mut loose_sym, &c[..c.len() - ".kicad_symdir".len()])
                } else if !is_last && c.ends_with(".pretty") {
                    (&mut loose_fp, &c[..c.len() - ".pretty".len()])
                } else {
                    continue;
                };
                let sub = comps[..=idx].join("/");
                if better(map.get(nick), &sub) {
                    map.insert(nick.to_string(), sub);
                }
            }
        }
        Ok(Resolver {
            project_dir: project.dir.clone(),
            stock: stock.clone(),
            sym_table,
            fp_table,
            existing,
            loose_sym,
            loose_fp,
        })
    }

    fn resolve(&self, nick: &str, kind: Kind) -> ResolvedLib {
        let (table, loose) = match kind {
            Kind::Symbol => (&self.sym_table, &self.loose_sym),
            Kind::Footprint => (&self.fp_table, &self.loose_fp),
        };
        let mk = |source, location, uri: Option<String>| ResolvedLib {
            nickname: nick.to_string(),
            source,
            location,
            uri,
        };
        let stock_loc = |n: &str| {
            let p = match kind {
                Kind::Symbol => self.stock.symbol_path(n),
                Kind::Footprint => self.stock.footprint_path(n),
            };
            p.map_or(LibLocation::None, LibLocation::Fs)
        };
        if let Some(e) = table.iter().find(|e| e.name == nick) {
            let uri = Some(e.uri.clone());
            let (expanded, vars) = expand(&e.uri);
            if let [v] = vars.as_slice()
                && stock_var(v) == Some(kind)
            {
                let rest = expanded.replace('\\', "/");
                let rest = rest.trim_matches('/');
                let base = self.stock.root.join(if kind == Kind::Symbol {
                    "symbols"
                } else {
                    "footprints"
                });
                let direct = base.join(rest);
                let loc = if !rest.is_empty() && direct.exists() {
                    LibLocation::Fs(direct)
                } else {
                    stock_loc(nick)
                };
                return mk(LibSource::Stock, loc, uri);
            }
            return match repo_target(&self.project_dir, &e.uri) {
                Some(Some(p)) if self.existing.contains(&p) => {
                    mk(LibSource::Project, LibLocation::Vfs(p), uri)
                }
                _ => mk(LibSource::Unresolved, LibLocation::None, uri),
            };
        }
        if let LibLocation::Fs(p) = stock_loc(nick) {
            return mk(LibSource::Stock, LibLocation::Fs(p), None);
        }
        if let Some(p) = loose.get(nick) {
            return mk(
                LibSource::ProjectUnregistered,
                LibLocation::Vfs(p.clone()),
                None,
            );
        }
        mk(LibSource::Unresolved, LibLocation::None, None)
    }

    pub fn symbol_lib(&self, nick: &str) -> ResolvedLib {
        self.resolve(nick, Kind::Symbol)
    }
    pub fn footprint_lib(&self, nick: &str) -> ResolvedLib {
        self.resolve(nick, Kind::Footprint)
    }
    /// The project's table entries (for the report).
    pub fn sym_table(&self) -> &[LibTableEntry] {
        &self.sym_table
    }
    pub fn fp_table(&self) -> &[LibTableEntry] {
        &self.fp_table
    }
}

/// Loads and caches library contents.
pub struct LibStore<'a> {
    vfs: &'a dyn Vfs,
    /// Parsed files keyed by "vfs:<path>" / "fs:<path>". None = missing.
    cache: RefCell<HashMap<String, Option<Rc<Node>>>>,
}

impl<'a> LibStore<'a> {
    pub fn new(vfs: &'a dyn Vfs) -> LibStore<'a> {
        LibStore {
            vfs,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Parse (cached) one file: `loc` itself, or `<loc>/<child>` when `child` is given.
    /// Ok(None) if it does not exist.
    fn load(&self, loc: &LibLocation, child: Option<&str>) -> Result<Option<Rc<Node>>> {
        let (key, text) = match loc {
            LibLocation::None => return Ok(None),
            LibLocation::Vfs(p) => {
                let full = child.map_or_else(|| p.clone(), |f| format!("{p}/{f}"));
                let key = format!("vfs:{full}");
                if let Some(c) = self.cache.borrow().get(&key) {
                    return Ok(c.clone());
                }
                let t = if self.vfs.exists(&full) {
                    Some(self.vfs.read_string(&full)?)
                } else {
                    None
                };
                (key, t)
            }
            LibLocation::Fs(p) => {
                let full = child.map_or_else(|| p.clone(), |f| p.join(f));
                let key = format!("fs:{}", full.display());
                if let Some(c) = self.cache.borrow().get(&key) {
                    return Ok(c.clone());
                }
                let t = if full.is_file() {
                    Some(String::from_utf8_lossy(&std::fs::read(&full)?).into_owned())
                } else {
                    None
                };
                (key, t)
            }
        };
        let node = match text {
            Some(t) => Some(Rc::new(
                crate::sexpr::parse(&t).map_err(|e| anyhow!("{key}: {e}"))?,
            )),
            None => None,
        };
        self.cache.borrow_mut().insert(key, node.clone());
        Ok(node)
    }

    fn is_symdir(loc: &LibLocation) -> bool {
        match loc {
            LibLocation::Vfs(p) => p.ends_with(".kicad_symdir"),
            LibLocation::Fs(p) => p.is_dir(),
            LibLocation::None => false,
        }
    }

    /// Top-level `(symbol "Name" ...)` from the library, with `(extends "Parent")` flattened:
    /// the result has the child's name and properties but the parent's unit sub-symbols
    /// (renamed `Parent_u_s` -> `Name_u_s`) and pin/graphics flags; no `extends` node remains.
    /// Ok(None) if the library or the symbol does not exist. Parsed files are cached.
    pub fn symbol(&self, lib: &ResolvedLib, name: &str) -> Result<Option<Node>> {
        self.symbol_depth(lib, name, 0)
    }

    fn symbol_depth(&self, lib: &ResolvedLib, name: &str, depth: usize) -> Result<Option<Node>> {
        if depth > 16 {
            return Err(anyhow!("symbol '{name}': extends chain too deep"));
        }
        let loc = &lib.location;
        let file = if Self::is_symdir(loc) {
            self.load(loc, Some(&format!("{name}.kicad_sym")))?
        } else {
            self.load(loc, None)?
        };
        let sym = file.as_ref().and_then(|r| {
            r.find_all("symbol")
                .find(|s| s.first_arg() == Some(name))
                .cloned()
        });
        let Some(sym) = sym else { return Ok(None) };
        let Some(parent_name) = sym.value("extends").map(str::to_string) else {
            return Ok(Some(sym));
        };
        let Some(parent) = self.symbol_depth(lib, &parent_name, depth + 1)? else {
            return Err(anyhow!(
                "symbol '{name}' extends missing parent '{parent_name}'"
            ));
        };
        Ok(Some(flatten(&sym, &parent, &parent_name, name)))
    }

    /// `(footprint "Name" ...)` parsed from `<lib>.pretty/<Name>.kicad_mod`. Ok(None) if missing.
    pub fn footprint(&self, lib: &ResolvedLib, name: &str) -> Result<Option<Node>> {
        Ok(self
            .load(&lib.location, Some(&format!("{name}.kicad_mod")))?
            .map(|n| (*n).clone()))
    }
}

fn flatten(child: &Node, parent: &Node, parent_name: &str, name: &str) -> Node {
    let mut out = vec![Node::sym("symbol"), Node::str(name)];
    let child_props: Vec<&Node> = child.find_all("property").collect();
    let mut props_done = false;
    let old_prefix = format!("{parent_name}_");
    let new_prefix = format!("{name}_");
    // args() starts at the name atom; skip it.
    for n in parent.args().iter().skip(1) {
        if n.is("property") {
            if !props_done {
                out.extend(child_props.iter().map(|p| (*p).clone()));
                props_done = true;
            }
        } else if n.is("extends") {
            // dropped
        } else if n.is("symbol") {
            let mut u = n.clone();
            if let Some(Node::Str(s)) = u.items_mut().and_then(|v| v.get_mut(1))
                && let Some(rest) = s.strip_prefix(&old_prefix)
            {
                *s = format!("{new_prefix}{rest}");
            }
            out.push(u);
        } else if let Some(h) = n.head() {
            // Flag-like children: the child's own value wins.
            out.push(child.find(h).cloned().unwrap_or_else(|| n.clone()));
        } else {
            out.push(n.clone());
        }
    }
    if !props_done {
        out.extend(child_props.iter().map(|p| (*p).clone()));
    }
    // Child flags the parent lacks.
    for n in child.args().iter().skip(1) {
        if let Some(h) = n.head()
            && !matches!(h, "property" | "extends" | "symbol")
            && parent.find(h).is_none()
        {
            out.push(n.clone());
        }
    }
    Node::List(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
                .ok_or_else(|| anyhow!("no {p}"))
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

    const SYM_TABLE: &str = r#"(sym_lib_table
  (version 7)
  (lib (name "Mine")(type "KiCad")(uri "${KIPRJMOD}/libraries/Mine.kicad_sym")(options "")(descr "d"))
  (lib (name "Win")(type "KiCad")(uri "${KIPRJMOD}\\libs\\Win.kicad_sym")(options "")(descr ""))
  (lib (name "Rel")(type "KiCad")(uri "libraries/Rel.kicad_sym")(options "")(descr ""))
  (lib (name "Gone")(type "KiCad")(uri "${KIPRJMOD}/nope.kicad_sym")(options "")(descr ""))
  (lib (name "Glob")(type "KiCad")(uri "/home/u/Glob.kicad_sym")(options "")(descr ""))
  (lib (name "Env")(type "KiCad")(uri "$(MYVAR)/Env.kicad_sym")(options "")(descr ""))
  (lib (name "Dev")(type "KiCad")(uri "${KICAD9_SYMBOL_DIR}/Dev.kicad_sym")(options "")(descr ""))
)"#;

    #[test]
    fn table_parse() {
        let t = parse_lib_table(SYM_TABLE).unwrap();
        assert_eq!(t.len(), 7);
        assert_eq!(
            t[0],
            LibTableEntry {
                name: "Mine".into(),
                kind: "KiCad".into(),
                uri: "${KIPRJMOD}/libraries/Mine.kicad_sym".into(),
                options: "".into(),
                descr: "d".into()
            }
        );
    }

    fn stock() -> StockLibs {
        StockLibs {
            version: KicadVersion::V9,
            root: PathBuf::from("/nonexistent-stock"),
        }
    }

    #[test]
    fn resolver_classification() {
        let vfs = Mem::new(&[
            ("proj/a.kicad_pro", "{}"),
            ("proj/sym-lib-table", SYM_TABLE),
            (
                "proj/fp-lib-table",
                r#"(fp_lib_table (lib (name "FP")(type "KiCad")(uri "${KIPRJMOD}/fp/FP.pretty")(options "")(descr "")))"#,
            ),
            ("proj/libraries/Mine.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/libs/Win.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/libraries/Rel.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/fp/FP.pretty/X.kicad_mod", "(footprint \"X\")"),
            ("proj/Loose.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/deep/er/Loose.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/zz/Tie.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/aa/Tie.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/x-backups/Bk.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/.history/Hist.kicad_sym", "(kicad_symbol_lib)"),
            ("proj/Foot.pretty/Y.kicad_mod", "(footprint \"Y\")"),
        ]);
        let p = Project {
            name: "a".into(),
            dir: "proj".into(),
            pro: "proj/a.kicad_pro".into(),
            schematics: vec![],
            pcb: None,
            sym_lib_table: Some("proj/sym-lib-table".into()),
            fp_lib_table: Some("proj/fp-lib-table".into()),
            versions: BTreeMap::new(),
            unsupported: None,
            warnings: vec![],
        };
        let r = Resolver::new(&vfs, &p, &stock()).unwrap();
        let s = |n: &str| r.symbol_lib(n);
        assert_eq!(s("Mine").source, LibSource::Project);
        assert_eq!(
            s("Mine").location,
            LibLocation::Vfs("proj/libraries/Mine.kicad_sym".into())
        );
        assert_eq!(
            s("Win").location,
            LibLocation::Vfs("proj/libs/Win.kicad_sym".into())
        );
        assert_eq!(s("Rel").source, LibSource::Project);
        assert_eq!(s("Gone").source, LibSource::Unresolved);
        assert_eq!(s("Gone").uri.as_deref(), Some("${KIPRJMOD}/nope.kicad_sym"));
        assert_eq!(s("Glob").source, LibSource::Unresolved);
        assert_eq!(s("Env").source, LibSource::Unresolved);
        assert_eq!(s("Dev").source, LibSource::Stock);
        assert_eq!(s("Loose").source, LibSource::ProjectUnregistered);
        assert_eq!(
            s("Loose").location,
            LibLocation::Vfs("proj/Loose.kicad_sym".into())
        );
        assert_eq!(
            s("Tie").location,
            LibLocation::Vfs("proj/aa/Tie.kicad_sym".into())
        );
        assert_eq!(s("Bk").source, LibSource::Unresolved);
        assert_eq!(s("Hist").source, LibSource::Unresolved);
        assert_eq!(r.footprint_lib("FP").source, LibSource::Project);
        assert_eq!(
            r.footprint_lib("Foot").source,
            LibSource::ProjectUnregistered
        );
        assert_eq!(r.footprint_lib("Mine").source, LibSource::Unresolved);
        assert_eq!(r.sym_table().len(), 7);
        assert_eq!(r.fp_table().len(), 1);

        let store = LibStore::new(&vfs);
        let fp = store
            .footprint(&r.footprint_lib("FP"), "X")
            .unwrap()
            .unwrap();
        assert!(fp.is("footprint"));
        assert!(
            store
                .footprint(&r.footprint_lib("FP"), "Nope")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn extends_flatten() {
        let lib = r#"(kicad_symbol_lib (version 20241209)
 (symbol "Base" (pin_names (offset 0)) (in_bom yes) (on_board yes)
   (property "Reference" "R" (at 0 0 0))
   (property "Value" "Base" (at 0 0 0))
   (symbol "Base_0_1" (rectangle (start 0 0) (end 1 1)))
   (symbol "Base_1_1" (pin passive line (at 0 0 0) (length 1) (name "~") (number "1"))))
 (symbol "Kid" (extends "Base")
   (property "Reference" "R" (at 1 1 0))
   (property "Value" "Kid" (at 2 2 0))))"#;
        let vfs = Mem::new(&[("l/X.kicad_sym", lib)]);
        let store = LibStore::new(&vfs);
        let rl = ResolvedLib {
            nickname: "X".into(),
            source: LibSource::Project,
            location: LibLocation::Vfs("l/X.kicad_sym".into()),
            uri: None,
        };
        let base = store.symbol(&rl, "Base").unwrap().unwrap();
        assert_eq!(base.find_all("symbol").count(), 2);
        let kid = store.symbol(&rl, "Kid").unwrap().unwrap();
        assert_eq!(kid.first_arg(), Some("Kid"));
        assert!(kid.find("extends").is_none());
        assert_eq!(kid.property("Value"), Some("Kid"));
        let units: Vec<_> = kid
            .find_all("symbol")
            .map(|s| s.first_arg().unwrap().to_string())
            .collect();
        assert_eq!(units, ["Kid_0_1", "Kid_1_1"]);
        assert!(kid.find("in_bom").is_some());
        assert!(store.symbol(&rl, "Missing").unwrap().is_none());
    }
}
