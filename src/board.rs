//! Board extraction.
//! OWNER: agent "extract".

use crate::sexpr::Node;
use crate::vfs::Vfs;
use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct BoardFootprint {
    /// `(footprint "Lib:Name" ...)` identifier.
    pub fpid: String,
    pub reference: String,
    pub value: String,
    /// "F.Cu" or "B.Cu".
    pub layer: String,
    pub uuid: String,
    /// `(path "/.../...")` link to the schematic symbol (SchSymbol::board_path), if any.
    pub path: Option<String>,
    /// `(attr ... dnp)` / `(dnp yes)`
    pub dnp: bool,
    /// `(attr ... board_only)`
    pub board_only: bool,
    /// The full footprint node as stored on the board.
    pub node: Node,
}

#[derive(Debug, Default)]
pub struct Board {
    pub file: String,
    pub footprints: Vec<BoardFootprint>,
    /// Board-level `(embedded_files ...)` node, if any (decoded by `models`).
    pub embedded_files: Option<Node>,
}

/// Parse a .kicad_pcb from the VFS. Footprints are in file order.
pub fn load(vfs: &dyn Vfs, pcb: &str) -> Result<Board> {
    let text = vfs
        .read_string(pcb)
        .with_context(|| format!("reading {pcb}"))?;
    let root = crate::sexpr::parse(&text).map_err(|e| anyhow::anyhow!("parsing {pcb}: {e}"))?;
    Ok(from_node(pcb, &root))
}

fn from_node(file: &str, root: &Node) -> Board {
    let footprints = root.find_all("footprint").map(footprint).collect();
    Board {
        file: file.to_string(),
        footprints,
        embedded_files: root.find("embedded_files").cloned(),
    }
}

fn footprint(n: &Node) -> BoardFootprint {
    let fp_text = |kind: &str| {
        n.find_all("fp_text")
            .find(|t| t.str_at(1) == Some(kind))
            .and_then(|t| t.str_at(2))
    };
    let attr = n.find("attr");
    let has_attr = |a: &str| attr.is_some_and(|x| x.args().iter().any(|v| v.atom() == Some(a)));
    BoardFootprint {
        fpid: n.first_arg().unwrap_or("").to_string(),
        reference: n
            .property("Reference")
            .or_else(|| fp_text("reference"))
            .unwrap_or("")
            .to_string(),
        value: n
            .property("Value")
            .or_else(|| fp_text("value"))
            .unwrap_or("")
            .to_string(),
        layer: n.value("layer").unwrap_or("").to_string(),
        uuid: n
            .value("uuid")
            .or_else(|| n.value("tstamp"))
            .unwrap_or("")
            .to_string(),
        path: n.value("path").map(str::to_string),
        dnp: has_attr("dnp") || n.flag("dnp").unwrap_or(false),
        board_only: has_attr("board_only"),
        node: n.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_footprints() {
        let root = crate::sexpr::parse(
            r#"(kicad_pcb (footprint "L:A" (layer "B.Cu") (uuid "u1") (at 1 2) (property "Reference" "R1") (property "Value" "10k")
                 (path "/s/x") (attr smd board_only dnp))
               (footprint "L:B" (layer "F.Cu") (tstamp "u2") (fp_text reference "C1" (at 0 0)) (fp_text value "1u" (at 0 0)) (dnp yes))
               (embedded_files (file (name "a"))))"#,
        )
        .unwrap();
        let b = from_node("x.kicad_pcb", &root);
        assert_eq!(b.footprints.len(), 2);
        let a = &b.footprints[0];
        assert_eq!(
            (
                a.reference.as_str(),
                a.value.as_str(),
                a.layer.as_str(),
                a.uuid.as_str()
            ),
            ("R1", "10k", "B.Cu", "u1")
        );
        assert_eq!(a.path.as_deref(), Some("/s/x"));
        assert!(a.dnp && a.board_only);
        let c = &b.footprints[1];
        assert_eq!(
            (c.reference.as_str(), c.value.as_str(), c.uuid.as_str()),
            ("C1", "1u", "u2")
        );
        assert!(c.dnp && !c.board_only && c.path.is_none());
        assert!(b.embedded_files.is_some());
    }
}
