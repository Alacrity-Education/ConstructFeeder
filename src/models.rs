//! 3D model references, resolution and embedded files.
//! OWNER: agent "models".

use crate::sexpr::{Node, num};
use crate::types::KicadVersion;
use crate::vfs::{Vfs, path};
use anyhow::Result;
use base64::Engine as _;
use serde::Serialize;
use std::io::Read as _;

/// One `(model "path" (offset (xyz ..)) (scale (xyz ..)) (rotate (xyz ..)) (hide yes) (opacity x))`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelRef {
    pub path: String,
    pub offset: [f64; 3],
    pub scale: [f64; 3],
    pub rotate: [f64; 3],
    pub hide: bool,
    pub opacity: Option<f64>,
}

/// All `(model ...)` children of a footprint node, in order.
/// Missing offset/rotate default to 0, missing scale to 1.
pub fn model_refs(footprint: &Node) -> Vec<ModelRef> {
    footprint
        .find_all("model")
        .map(|m| ModelRef {
            path: m.first_arg().unwrap_or("").to_string(),
            offset: xyz(m, &["offset", "at"], 0.0),
            scale: xyz(m, &["scale"], 1.0),
            rotate: xyz(m, &["rotate", "rotation"], 0.0),
            hide: m.flag("hide").unwrap_or(false),
            opacity: m
                .find("opacity")
                .and_then(|o| o.first_arg())
                .and_then(|v| v.parse::<f64>().ok()),
        })
        .collect()
}

fn xyz(model: &Node, heads: &[&str], default: f64) -> [f64; 3] {
    for h in heads {
        if let Some(v) = model.find(h).and_then(|n| n.find("xyz")) {
            return [num(v.str_at(1)), num(v.str_at(2)), num(v.str_at(3))];
        }
    }
    [default; 3]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOrigin {
    /// `${KICAD<n>_3DMODEL_DIR}/...` or legacy `${KISYS3DMOD}/...`.
    Stock,
    /// Inside the repo: `${KIPRJMOD}/...` or a relative path (relative to the project dir).
    Project,
    /// `kicad-embed://name` resolved from footprint- or board-level embedded_files.
    Embedded,
    /// Absolute path, unknown env var, or `:alias:` path.
    External,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResolvedModel {
    pub reference: ModelRef,
    pub origin: ModelOrigin,
    /// Repo-relative path when origin == Project and the file exists.
    pub vfs_path: Option<String>,
    /// Name to use when exporting (file name part of the path / embedded name).
    pub file_name: String,
    /// Whether the content is available (file found / embedded data decoded).
    pub found: bool,
}

/// One decoded embedded file.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedFile {
    pub name: String,
    /// `(type model)` etc.
    pub kind: String,
    /// Decompressed bytes. KiCad stores `(data |base64...|)` (possibly split over several atoms)
    /// as base64 of zstd-compressed content.
    pub data: Vec<u8>,
}

/// Decode an `(embedded_files (file (name ..) (type ..) (data ..) (checksum ..)) ...)` node.
/// Entries that fail to decode are skipped with an Err-free warning (return them in the second vec).
pub fn parse_embedded_files(node: &Node) -> (Vec<EmbeddedFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for f in node.find_all("file") {
        let name = f.value("name").unwrap_or("").to_string();
        let kind = f.value("type").unwrap_or("").to_string();
        let Some(data) = f.find("data") else {
            warnings.push(format!("embedded file '{name}': no data"));
            continue;
        };
        match decode_embedded(data) {
            Ok(data) => files.push(EmbeddedFile { name, kind, data }),
            Err(e) => warnings.push(format!("embedded file '{name}': {e}")),
        }
    }
    (files, warnings)
}

/// The data may be split over several atoms (whitespace separated lines) and
/// wrapped in `|` delimiters, which end up inside the first/last atoms.
fn decode_embedded(data: &Node) -> Result<Vec<u8>, String> {
    let mut b64: String = data
        .args()
        .iter()
        .filter_map(Node::atom)
        .flat_map(str::chars)
        .filter(|c| *c != '|' && !c.is_whitespace())
        .collect();
    while b64.ends_with('=') {
        b64.pop();
    }
    let compressed = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(b64.as_bytes())
        .map_err(|e| format!("base64: {e}"))?;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(compressed.as_slice())
        .map_err(|e| format!("zstd: {e}"))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| format!("zstd: {e}"))?;
    Ok(out)
}

pub struct ModelCtx<'a> {
    pub vfs: &'a dyn Vfs,
    pub version: KicadVersion,
    /// Repo-relative project dir (value of ${KIPRJMOD}).
    pub project_dir: &'a str,
    /// Footprint-level embedded files followed by board-level ones.
    pub embedded: &'a [EmbeddedFile],
}

/// Resolve one model reference. Env vars `${VAR}` and `$(VAR)` are both accepted.
pub fn resolve(model: &ModelRef, ctx: &ModelCtx) -> ResolvedModel {
    let raw = model.path.trim();
    let norm = raw.replace('\\', "/");
    let file_name = path::file_name(&norm).to_string();
    let mk = |origin, vfs_path: Option<String>, found, file_name: String| ResolvedModel {
        reference: model.clone(),
        origin,
        vfs_path,
        file_name,
        found,
    };

    if let Some(name) = raw.strip_prefix("kicad-embed://") {
        let found = ctx.embedded.iter().any(|e| e.name == name);
        return mk(ModelOrigin::Embedded, None, found, name.to_string());
    }
    let external = || mk(ModelOrigin::External, None, false, file_name.clone());
    if norm.starts_with(':') {
        return external();
    }

    let project = |rel: &str| -> ResolvedModel {
        let Some(full) = path::join(ctx.project_dir, rel) else {
            return external();
        };
        match find_file(ctx.vfs, &full) {
            Some(real) => mk(ModelOrigin::Project, Some(real), true, file_name.clone()),
            None => mk(ModelOrigin::Project, None, false, file_name.clone()),
        }
    };

    if let Some((var, rest)) = split_env(&norm) {
        return if var == "KIPRJMOD" {
            project(rest.trim_start_matches('/'))
        } else if is_stock_var(var) {
            mk(ModelOrigin::Stock, None, false, file_name)
        } else {
            external()
        };
    }
    if norm.contains("${") || norm.contains("$(") {
        return external();
    }
    project(&norm)
}

/// Split a leading `${VAR}` / `$(VAR)` off the path.
fn split_env(p: &str) -> Option<(&str, &str)> {
    let close = match p.get(..2)? {
        "${" => '}',
        "$(" => ')',
        _ => return None,
    };
    let end = p.find(close)?;
    Some((&p[2..end], &p[end + 1..]))
}

fn is_stock_var(v: &str) -> bool {
    if v == "KISYS3DMOD" {
        return true;
    }
    v.strip_prefix("KICAD")
        .and_then(|r| r.strip_suffix("_3DMODEL_DIR"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Exact match first, then a case-insensitive match; returns the real path.
fn find_file(vfs: &dyn Vfs, full: &str) -> Option<String> {
    if full.is_empty() {
        return None;
    }
    if vfs.exists(full) {
        return Some(full.to_string());
    }
    vfs.files()
        .iter()
        .find(|f| f.eq_ignore_ascii_case(full))
        .cloned()
}

/// Bytes of a resolved model (None if not found / stock / external).
pub fn load_bytes(m: &ResolvedModel, ctx: &ModelCtx) -> Result<Option<Vec<u8>>> {
    if !m.found {
        return Ok(None);
    }
    match m.origin {
        ModelOrigin::Project => match &m.vfs_path {
            Some(p) => Ok(Some(ctx.vfs.read(p)?)),
            None => Ok(None),
        },
        ModelOrigin::Embedded => Ok(ctx
            .embedded
            .iter()
            .find(|e| e.name == m.file_name)
            .map(|e| e.data.clone())),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexpr::parse;

    struct MemVfs(Vec<String>);
    impl MemVfs {
        fn new(files: &[&str]) -> Self {
            let mut v: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            v.sort();
            MemVfs(v)
        }
    }
    impl Vfs for MemVfs {
        fn files(&self) -> &[String] {
            &self.0
        }
        /// Content of each file is its own path.
        fn read(&self, p: &str) -> Result<Vec<u8>> {
            if self.exists(p) {
                Ok(p.as_bytes().to_vec())
            } else {
                anyhow::bail!("not found")
            }
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

    fn mr(p: &str) -> ModelRef {
        ModelRef {
            path: p.into(),
            offset: [0.0; 3],
            scale: [1.0; 3],
            rotate: [0.0; 3],
            hide: false,
            opacity: None,
        }
    }

    #[test]
    fn refs_defaults_and_values() {
        let n = parse(
            r#"(footprint "X"
              (model "${KIPRJMOD}/a.step")
              (model "b.wrl" (offset (xyz 1 2 3)) (scale (xyz 2 2 2)) (rotate (xyz -90 0 180)) (hide yes) (opacity 0.5))
              (model "c.step" hide))"#,
        )
        .unwrap();
        let r = model_refs(&n);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], mr("${KIPRJMOD}/a.step"));
        assert_eq!(r[1].offset, [1.0, 2.0, 3.0]);
        assert_eq!(r[1].scale, [2.0; 3]);
        assert_eq!(r[1].rotate, [-90.0, 0.0, 180.0]);
        assert!(r[1].hide);
        assert_eq!(r[1].opacity, Some(0.5));
        assert!(r[2].hide);
        assert_eq!(r[2].opacity, None);
    }

    #[test]
    fn origins() {
        let vfs = MemVfs::new(&[
            "proj/libs/m.step",
            "proj/Lib/Case.STEP",
            "top.step",
            "proj/x.step",
        ]);
        let emb = [EmbeddedFile {
            name: "e.step".into(),
            kind: "model".into(),
            data: vec![1, 2],
        }];
        let ctx = ModelCtx {
            vfs: &vfs,
            version: KicadVersion::V9,
            project_dir: "proj",
            embedded: &emb,
        };
        let r = |p: &str| resolve(&mr(p), &ctx);

        let a = r("${KIPRJMOD}/libs/m.step");
        assert_eq!((a.origin, a.found), (ModelOrigin::Project, true));
        assert_eq!(a.vfs_path.as_deref(), Some("proj/libs/m.step"));
        assert_eq!(a.file_name, "m.step");
        assert_eq!(
            r("$(KIPRJMOD)\\libs\\m.step").vfs_path.as_deref(),
            Some("proj/libs/m.step")
        );
        assert_eq!(r("x.step").vfs_path.as_deref(), Some("proj/x.step"));
        assert_eq!(r("../top.step").vfs_path.as_deref(), Some("top.step"));
        let missing = r("${KIPRJMOD}/nope.step");
        assert_eq!(
            (missing.origin, missing.found, missing.vfs_path.clone()),
            (ModelOrigin::Project, false, None)
        );

        // case-insensitive fallback
        let c = r("${KIPRJMOD}/lib/case.step");
        assert!(c.found);
        assert_eq!(c.vfs_path.as_deref(), Some("proj/Lib/Case.STEP"));

        for s in [
            "${KICAD9_3DMODEL_DIR}/a.3dshapes/b.step",
            "${KICAD10_3DMODEL_DIR}/x.wrl",
            "${KISYS3DMOD}/x.wrl",
        ] {
            let m = r(s);
            assert_eq!((m.origin, m.found), (ModelOrigin::Stock, false), "{s}");
        }
        assert_eq!(r("${KICAD10_3DMODEL_DIR}/a/b.step").file_name, "b.step");

        for s in [
            "/abs/x.step",
            "C:\\x\\y.step",
            "${FOO}/x.step",
            ":alias:x.step",
            "${KIPRJMOD}/../../x.step",
            "../../x.step",
        ] {
            let m = r(s);
            assert_eq!((m.origin, m.found), (ModelOrigin::External, false), "{s}");
        }

        let e = r("kicad-embed://e.step");
        assert_eq!(
            (e.origin, e.found, e.file_name.as_str()),
            (ModelOrigin::Embedded, true, "e.step")
        );
        assert_eq!(load_bytes(&e, &ctx).unwrap(), Some(vec![1, 2]));
        assert!(!r("kicad-embed://zz.step").found);

        assert_eq!(
            load_bytes(&a, &ctx).unwrap(),
            Some(b"proj/libs/m.step".to_vec())
        );
        assert_eq!(load_bytes(&missing, &ctx).unwrap(), None);
        assert_eq!(
            load_bytes(&r("${KICAD9_3DMODEL_DIR}/a.step"), &ctx).unwrap(),
            None
        );
    }

    #[test]
    fn embedded_decode() {
        let payload = b"ISO-10303-21; hello step data hello step data hello step data".to_vec();
        let z = ruzstd::encoding::compress_to_vec(
            payload.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(&z);
        let (a, b) = b64.split_at(b64.len() / 2);
        let text = format!(
            "(embedded_files (file (name \"x.step\") (type model) (data |{a}\n\t\t{b}|) (checksum \"abc\")) (file (name \"bad.step\") (type model) (data |!!!|)))"
        );
        let n = parse(&text).unwrap();
        let (files, warns) = parse_embedded_files(&n);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "x.step");
        assert_eq!(files[0].kind, "model");
        assert_eq!(files[0].data, payload);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("bad.step"));
    }
}
