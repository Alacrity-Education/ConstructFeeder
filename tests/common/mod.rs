#![allow(dead_code)]
use anyhow::Result;
use construct_feeder::project::Project;
use construct_feeder::vfs::Vfs;
use std::path::{Path, PathBuf};

/// Walkdir-based test Vfs over a real directory (skips .git, .history, backups).
pub struct DirVfs {
    root: PathBuf,
    files: Vec<String>,
}

impl DirVfs {
    pub fn new(root: impl AsRef<Path>) -> DirVfs {
        let root = root.as_ref();
        let mut files = Vec::new();
        let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !(n == ".git" || n == ".history" || n.contains("backup"))
        });
        for e in walker.flatten() {
            if e.file_type().is_file() {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(rel);
            }
        }
        files.sort();
        DirVfs {
            root: root.to_path_buf(),
            files,
        }
    }
}

impl Vfs for DirVfs {
    fn files(&self) -> &[String] {
        &self.files
    }
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.root.join(path))?)
    }
    fn label(&self) -> &str {
        "worktree"
    }
    fn commit(&self) -> Option<&str> {
        None
    }
    fn dirty(&self) -> Option<bool> {
        None
    }
}

/// Hand-built Project: root schematic `<name>.kicad_sch` plus sheet files found via DFS.
pub fn project(vfs: &dyn Vfs, name: &str) -> Project {
    let root = format!("{name}.kicad_sch");
    let mut schematics = vec![root];
    let mut i = 0;
    while i < schematics.len() {
        let f = schematics[i].clone();
        i += 1;
        let text = vfs.read_string(&f).unwrap();
        let n = construct_feeder::sexpr::parse(&text).unwrap();
        for sh in n.find_all("sheet") {
            if let Some(sf) = sh.property("Sheetfile") {
                let p =
                    construct_feeder::vfs::path::join(construct_feeder::vfs::path::parent(&f), sf)
                        .unwrap();
                if vfs.exists(&p) && !schematics.contains(&p) {
                    schematics.push(p);
                }
            }
        }
    }
    Project {
        name: name.into(),
        dir: "".into(),
        pro: format!("{name}.kicad_pro"),
        schematics,
        pcb: Some(format!("{name}.kicad_pcb")),
        sym_lib_table: None,
        fp_lib_table: None,
        versions: Default::default(),
        unsupported: None,
        warnings: vec![],
    }
}
