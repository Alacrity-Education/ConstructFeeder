//! Virtual file system over either a working tree or a git ref.
//!
//! All paths are repo-relative, '/'-separated, no leading './' or '/'.
//! OWNER: agent "vfs" implements `FsVfs`, `GitVfs`, `list_branches`, `repo_info`.
//! The `path` helpers below are shared and already implemented.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait Vfs {
    /// All regular files in the tree, repo-relative, '/'-separated, sorted ascending.
    /// Must not include anything under `.git/`.
    fn files(&self) -> &[String];

    /// Raw file contents.
    fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// File contents as UTF-8 (lossy).
    fn read_string(&self, path: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.read(path)?).into_owned())
    }

    fn exists(&self, path: &str) -> bool {
        self.files()
            .binary_search_by(|p| p.as_str().cmp(path))
            .is_ok()
    }

    /// Does any file live under `dir/` (i.e. is `dir` a directory)?
    fn is_dir(&self, dir: &str) -> bool {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let files = self.files();
        let i = files.partition_point(|p| p.as_str() < prefix.as_str());
        files.get(i).is_some_and(|p| p.starts_with(&prefix))
    }

    /// Files directly or transitively under `dir/`.
    fn files_under(&self, dir: &str) -> Vec<&str> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        self.files()
            .iter()
            .filter(|p| p.starts_with(&prefix))
            .map(|s| s.as_str())
            .collect()
    }

    /// "worktree" for the file system, otherwise the git ref name (e.g. "main").
    fn label(&self) -> &str;

    /// Commit SHA if known.
    fn commit(&self) -> Option<&str>;

    /// For the worktree: whether it has uncommitted changes (None if not a git repo).
    /// For git refs: Some(false).
    fn dirty(&self) -> Option<bool>;
}

/// Working tree (plain directory, git or not).
pub struct FsVfs {
    root: PathBuf,
    files: Vec<String>,
    commit: Option<String>,
    dirty: Option<bool>,
}

impl FsVfs {
    /// Walk `root` recursively (skipping `.git`, `target`, `node_modules`).
    /// Symlinks are not followed. Populate commit/dirty via `repo_info` if `root` is in a git repo.
    pub fn new(root: &Path) -> Result<FsVfs> {
        let meta = std::fs::metadata(root)
            .with_context(|| format!("cannot access root directory {}", root.display()))?;
        if !meta.is_dir() {
            bail!("{} is not a directory", root.display());
        }
        let mut files = Vec::new();
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() > 0 && e.file_type().is_dir() {
                    let n = e.file_name();
                    return n != ".git" && n != "target" && n != "node_modules";
                }
                true
            });
        for entry in walker {
            let entry = entry.with_context(|| format!("walking {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let parts: Vec<_> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            files.push(parts.join("/"));
        }
        files.sort();
        files.dedup();
        let info = repo_info(root);
        Ok(FsVfs {
            root: root.to_path_buf(),
            files,
            commit: info.as_ref().map(|i| i.commit.clone()),
            dirty: info.map(|i| i.dirty),
        })
    }
}

impl Vfs for FsVfs {
    fn files(&self) -> &[String] {
        &self.files
    }
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        if !self.exists(path) {
            bail!("file not found in worktree: {path}");
        }
        let full = self.root.join(path);
        std::fs::read(&full).with_context(|| format!("reading {}", full.display()))
    }
    fn label(&self) -> &str {
        "worktree"
    }
    fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }
    fn dirty(&self) -> Option<bool> {
        self.dirty
    }
}

/// Read-only view of a git ref (branch, tag or SHA) without checking it out.
/// Implementation shells out to the `git` CLI (`git ls-tree -r -z`, `git cat-file`).
/// Must never modify the repo, index, worktrees or HEAD.
pub struct GitVfs {
    repo: PathBuf,
    refname: String,
    sha: String,
    files: Vec<String>,
    /// Blob oids, parallel to `files`.
    oids: Vec<String>,
}

fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--no-optional-locks")
        .arg("-c")
        .arg("safe.directory=*")
        .arg("-C")
        .arg(repo);
    c
}

fn run_git(mut cmd: Command, what: &str) -> Result<Vec<u8>> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to run git ({what}); is git installed?"))?;
    if !out.status.success() {
        bail!(
            "git {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

impl GitVfs {
    /// `repo_root` is the top level of the git repo. `refname` is e.g. "main", "origin/v3-ltem", a SHA.
    pub fn new(repo_root: &Path, refname: &str) -> Result<GitVfs> {
        let mut c = git(repo_root);
        c.args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{refname}^{{commit}}"));
        let sha = run_git(c, &format!("rev-parse {refname}")).with_context(|| {
            format!("cannot resolve ref '{refname}' in {}", repo_root.display())
        })?;
        let sha = String::from_utf8_lossy(&sha).trim().to_string();

        let mut c = git(repo_root);
        c.args(["ls-tree", "-r", "-z", "--full-tree", &sha]);
        let out = run_git(c, &format!("ls-tree {sha}")).with_context(|| {
            format!("cannot list tree of '{refname}' in {}", repo_root.display())
        })?;

        let mut entries: Vec<(String, String)> = Vec::new();
        for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&rec[..tab]);
            let mut it = meta.split(' ');
            let (mode, ty, oid) = (
                it.next().unwrap_or(""),
                it.next().unwrap_or(""),
                it.next().unwrap_or(""),
            );
            if ty != "blob" || !(mode == "100644" || mode == "100755") {
                continue;
            }
            entries.push((
                String::from_utf8_lossy(&rec[tab + 1..]).into_owned(),
                oid.to_string(),
            ));
        }
        entries.sort();
        entries.dedup_by(|a, b| a.0 == b.0);
        let (files, oids) = entries.into_iter().unzip();
        Ok(GitVfs {
            repo: repo_root.to_path_buf(),
            refname: refname.to_string(),
            sha,
            files,
            oids,
        })
    }
}

impl Vfs for GitVfs {
    fn files(&self) -> &[String] {
        &self.files
    }
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let i = self
            .files
            .binary_search_by(|p| p.as_str().cmp(path))
            .map_err(|_| anyhow::anyhow!("file not found in {}: {path}", self.refname))?;
        let mut c = git(&self.repo);
        c.args(["cat-file", "blob", &self.oids[i]]);
        run_git(c, "cat-file blob").with_context(|| format!("reading {path} at {}", self.refname))
    }
    fn label(&self) -> &str {
        &self.refname
    }
    fn commit(&self) -> Option<&str> {
        Some(&self.sha)
    }
    fn dirty(&self) -> Option<bool> {
        Some(false)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// `git rev-parse --show-toplevel`
    pub toplevel: std::path::PathBuf,
    /// Current branch name or None if detached.
    pub head_ref: Option<String>,
    pub commit: String,
    pub dirty: bool,
}

fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    let mut c = git(dir);
    c.args(args);
    let out = c.output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string()
    })
}

/// Info about the git repo containing `dir`, or None if not in a git repo.
pub fn repo_info(dir: &Path) -> Option<RepoInfo> {
    let toplevel = git_text(dir, &["rev-parse", "--show-toplevel"])?;
    let commit = git_text(dir, &["rev-parse", "HEAD"]).unwrap_or_default();
    let head_ref =
        git_text(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).filter(|s| !s.is_empty());
    let dirty = git_text(dir, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some(RepoInfo {
        toplevel: PathBuf::from(toplevel),
        head_ref,
        commit,
        dirty,
    })
}

/// All branch names: local branches plus remote-tracking branches that have no local
/// counterpart (returned as e.g. "origin/cezar"). Excludes "origin/HEAD". Sorted, deduped.
pub fn list_branches(repo_root: &Path) -> Result<Vec<String>> {
    let mut c = git(repo_root);
    c.args([
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads",
        "refs/remotes",
    ]);
    let text = run_git(c, "for-each-ref")
        .with_context(|| format!("cannot list branches in {}", repo_root.display()))?;
    let text = String::from_utf8_lossy(&text);
    let mut locals = BTreeSet::new();
    let mut remotes = BTreeSet::new();
    for line in text.lines() {
        if let Some(b) = line.strip_prefix("refs/heads/") {
            locals.insert(b.to_string());
        } else if let Some(r) = line.strip_prefix("refs/remotes/")
            && !r.ends_with("/HEAD")
        {
            remotes.insert(r.to_string());
        }
    }
    let mut out = locals.clone();
    for r in remotes {
        let short = r.split_once('/').map_or(r.as_str(), |(_, b)| b);
        if !locals.contains(short) {
            out.insert(r);
        }
    }
    Ok(out.into_iter().collect())
}

/// Shared path helpers for repo-relative '/'-separated paths.
pub mod path {
    /// Parent directory ("" for top-level files).
    pub fn parent(p: &str) -> &str {
        match p.rfind('/') {
            Some(i) => &p[..i],
            None => "",
        }
    }

    /// Final component.
    pub fn file_name(p: &str) -> &str {
        match p.rfind('/') {
            Some(i) => &p[i + 1..],
            None => p,
        }
    }

    /// File stem (name without the last extension).
    pub fn stem(p: &str) -> &str {
        let f = file_name(p);
        match f.rfind('.') {
            Some(0) | None => f,
            Some(i) => &f[..i],
        }
    }

    /// Join `rel` onto `base` and normalize `.`/`..`, accepting '\' as separator too.
    /// Returns None if the result escapes the repo root or `rel` is absolute.
    pub fn join(base: &str, rel: &str) -> Option<String> {
        let rel = rel.replace('\\', "/");
        if rel.starts_with('/') || (rel.len() > 1 && rel.as_bytes()[1] == b':') {
            return None;
        }
        let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
        for seg in rel.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                s => parts.push(s),
            }
        }
        Some(parts.join("/"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn helpers() {
            assert_eq!(parent("a/b/c.txt"), "a/b");
            assert_eq!(parent("c.txt"), "");
            assert_eq!(stem("a/My Board.kicad_pcb"), "My Board");
            assert_eq!(join("a/b", "../c/./d").as_deref(), Some("a/c/d"));
            assert_eq!(join("", "x\\y").as_deref(), Some("x/y"));
            assert_eq!(join("a", "../.."), None);
            assert_eq!(join("a", "/abs"), None);
        }
    }
}

/// View of a subdirectory of another Vfs (used when `--root` is below the git top level).
pub struct PrefixVfs<V: Vfs> {
    inner: V,
    prefix: String,
    files: Vec<String>,
}

impl<V: Vfs> PrefixVfs<V> {
    pub fn new(inner: V, prefix: &str) -> PrefixVfs<V> {
        let prefix = prefix.trim_matches('/').to_string();
        let p = format!("{prefix}/");
        let files = inner
            .files()
            .iter()
            .filter_map(|f| f.strip_prefix(&p).map(str::to_string))
            .collect();
        PrefixVfs {
            inner,
            prefix,
            files,
        }
    }
}

impl<V: Vfs> Vfs for PrefixVfs<V> {
    fn files(&self) -> &[String] {
        &self.files
    }
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.inner.read(&format!("{}/{path}", self.prefix))
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn commit(&self) -> Option<&str> {
        self.inner.commit()
    }
    fn dirty(&self) -> Option<bool> {
        self.inner.dirty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn g(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    const SPECIAL: &str = "lib/DF12NB(3.0)-10DP-0.5V(51).step";

    fn make_repo() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        g(d, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("a.txt"), "hello").unwrap();
        std::fs::write(d.join(SPECIAL), "step data").unwrap();
        std::fs::write(d.join("bin.dat"), [0u8, 159, 146, 150, 255, 0]).unwrap();
        std::fs::write(d.join("target/ignored"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", d.join("link")).unwrap();
        g(d, &["add", "-A", "-f"]);
        g(d, &["commit", "-q", "-m", "one"]);
        g(d, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(d.join("extra.txt"), "extra").unwrap();
        g(d, &["add", "-A"]);
        g(d, &["commit", "-q", "-m", "two"]);
        g(d, &["checkout", "-q", "main"]);
        t
    }

    #[test]
    fn fs_vfs() {
        let t = make_repo();
        let v = FsVfs::new(t.path()).unwrap();
        assert_eq!(v.label(), "worktree");
        assert!(v.exists("a.txt") && v.exists(SPECIAL));
        assert!(!v.exists("extra.txt"));
        assert!(
            !v.files()
                .iter()
                .any(|f| f.starts_with(".git/") || f.starts_with("target/"))
        );
        let mut sorted = v.files().to_vec();
        sorted.sort();
        assert_eq!(sorted, v.files());
        assert_eq!(v.dirty(), Some(false));
        assert_eq!(v.commit().map(str::len), Some(40));
        assert_eq!(v.read_string("a.txt").unwrap(), "hello");
        assert!(v.is_dir("lib"));
        assert!(v.read("nope").is_err());
    }

    #[test]
    fn fs_subdir_and_dirty() {
        let t = make_repo();
        std::fs::write(t.path().join("lib/new.txt"), "n").unwrap();
        let v = FsVfs::new(&t.path().join("lib")).unwrap();
        assert!(v.exists("new.txt") && v.exists("DF12NB(3.0)-10DP-0.5V(51).step"));
        assert_eq!(v.dirty(), Some(true));
        let info = repo_info(t.path()).unwrap();
        assert!(info.dirty);
        assert_eq!(info.head_ref.as_deref(), Some("main"));
    }

    #[test]
    fn not_a_repo() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("x"), "1").unwrap();
        let v = FsVfs::new(t.path()).unwrap();
        assert_eq!(v.commit(), None);
        assert_eq!(v.dirty(), None);
        assert!(repo_info(t.path()).is_none());
        assert!(FsVfs::new(&t.path().join("missing")).is_err());
    }

    #[test]
    fn git_vfs_branches() {
        let t = make_repo();
        let m = GitVfs::new(t.path(), "main").unwrap();
        let f = GitVfs::new(t.path(), "feat").unwrap();
        assert_eq!(m.label(), "main");
        assert_eq!(m.dirty(), Some(false));
        assert_eq!(m.commit().map(str::len), Some(40));
        assert_ne!(m.commit(), f.commit());
        assert!(!m.exists("extra.txt") && f.exists("extra.txt"));
        assert!(m.exists(SPECIAL));
        assert!(!m.exists("link"), "symlinks skipped");
        assert_eq!(m.read_string(SPECIAL).unwrap(), "step data");
        assert_eq!(m.read("bin.dat").unwrap(), vec![0u8, 159, 146, 150, 255, 0]);
        assert!(m.read("extra.txt").is_err());
        assert_eq!(f.read_string("extra.txt").unwrap(), "extra");
        let sha = m.commit().unwrap().to_string();
        assert!(GitVfs::new(t.path(), &sha).unwrap().exists("a.txt"));
        let err = format!("{:#}", GitVfs::new(t.path(), "nonexistent").err().unwrap());
        assert!(err.contains("nonexistent"), "{err}");
        // working tree untouched
        assert!(!t.path().join("extra.txt").exists());
    }

    #[test]
    fn branches() {
        let t = make_repo();
        assert_eq!(list_branches(t.path()).unwrap(), vec!["feat", "main"]);
        // fake remote-tracking refs
        g(
            t.path(),
            &["update-ref", "refs/remotes/origin/main", "main"],
        );
        g(
            t.path(),
            &["update-ref", "refs/remotes/origin/cezar", "feat"],
        );
        g(
            t.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        assert_eq!(
            list_branches(t.path()).unwrap(),
            vec!["feat", "main", "origin/cezar"]
        );
    }
}
