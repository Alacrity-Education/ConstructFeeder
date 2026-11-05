//! Normalization, diffing and hashing of symbols and footprints, plus conversion
//! of embedded copies back into library form.
//! OWNER: agent "compare".
//!
//! Spec: property/field edits are IGNORED entirely (values, positions, visibility, added/removed
//! fields). Only electrical/graphical content counts. Placement (at/rotation/side), uuids,
//! nets and timestamps never count. KiCad 9 vs 10 formatting differences (e.g. `hide` vs
//! `(hide yes)`, defaulted flags, number formatting like `0` vs `0.0`) must not produce diffs.

use crate::sexpr::Node;
use serde::Serialize;

mod canon;
mod footprint;
mod symbol;

pub use footprint::{flip_layer, normalize_model_path, to_lib_frame};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PinDiff {
    /// Pin number ("3"), or name if unnumbered.
    pub number: String,
    /// Unit the pin belongs to (sub-symbol index), 0 = common to all units.
    pub unit: u32,
    /// "added" | "removed" | "changed"
    pub change: String,
    /// Which attributes changed: any of "position", "orientation", "length", "type", "shape",
    /// "name", "hidden", "alternates".
    pub fields: Vec<String>,
    /// Compact description of the library pin / local pin, e.g. {"at":[-5.08,-2.54,0],"type":"passive",...}
    pub library: Option<serde_json::Value>,
    pub local: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SymbolDiff {
    pub pins: Vec<PinDiff>,
    /// Human-readable graphics differences per unit ("unit 1: 2 rectangles changed").
    pub graphics: Vec<String>,
    /// Structural differences: unit count, de-morgan styles, power flag, pin_names/pin_numbers
    /// visibility/offset, in_bom/on_board defaults, etc.
    pub other: Vec<String>,
}

impl SymbolDiff {
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty() && self.graphics.is_empty() && self.other.is_empty()
    }
}

/// Compare an embedded schematic symbol (`lib_symbols` entry, name "Lib:Name" or variant "Name_1")
/// against the (flattened) library symbol. Top-level and unit sub-symbol names are ignored
/// (only the trailing `_<unit>_<style>` matters).
pub fn diff_symbol(local: &Node, library: &Node) -> SymbolDiff {
    symbol::diff(local, library)
}

/// Stable content hash (hex sha256) of a symbol ignoring names and properties.
pub fn symbol_hash(sym: &Node) -> String {
    symbol::hash(sym)
}

/// `(power)` / `(power global|local)` symbols, or legacy: all pins power_in and hidden with reference "#PWR".
pub fn is_power(sym: &Node) -> bool {
    symbol::is_power(sym)
}

/// No pins at all (logos, graphics, title blocks).
pub fn is_graphic_only(sym: &Node) -> bool {
    symbol::is_graphic_only(sym)
}

/// Rename an embedded symbol to `name` for a standalone library: top-level name and all unit
/// sub-symbols (`Old_1_1` -> `name_1_1`). Properties are kept as embedded.
pub fn symbol_to_library(embedded: &Node, name: &str) -> Node {
    symbol::to_library(embedded, name)
}

/// Full text of a `.kicad_sym` file containing the given symbols.
/// `version` is the KiCad major written into `(generator_version ..)`; the file `(version N)` must
/// be a value that KiCad of that major accepts (use 20241209 for 9, 20251024 for 10).
pub fn symbol_library_file(symbols: &[Node], version: crate::types::KicadVersion) -> String {
    symbol::library_file(symbols, version)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PadDiff {
    pub number: String,
    /// "added" | "removed" | "changed"
    pub change: String,
    /// e.g. "position", "size", "shape", "type", "layers", "drill", "roundrect_rratio",
    /// "solder_mask_margin", "paste_margin", "primitives", "orientation"
    pub fields: Vec<String>,
    pub library: Option<serde_json::Value>,
    pub local: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FootprintDiff {
    pub pads: Vec<PadDiff>,
    /// Graphics on fab/silk/courtyard/user layers (fp_line, fp_rect, fp_circle, fp_arc, fp_poly,
    /// fp_text (non-field), fp_curve, dimension, image) — summarized per layer.
    pub graphics: Vec<String>,
    /// `(attr smd|through_hole ...)` type flags, exclude_from_*, allow_missing_courtyard,
    /// clearance/margins at footprint level, net ties, private layers, zones.
    pub other: Vec<String>,
}

impl FootprintDiff {
    pub fn is_empty(&self) -> bool {
        self.pads.is_empty() && self.graphics.is_empty() && self.other.is_empty()
    }
}

/// Compare a board footprint against its library footprint. The board copy must first be
/// brought back to library frame: undo placement (at x y rot) and, for B.Cu footprints, undo the
/// flip (mirror Y about the footprint origin per KiCad's flip convention, swap F./B. layers).
/// Ignored: property/field text (Reference, Value, user fields, `fp_text reference/value`), uuids,
/// nets, pinfunction/pintype, `locked`, `path`, `sheetname`, `sheetfile`, `dnp`/board-level attrs,
/// `(model ...)` entries (handled by `models_diff`), embedded_files, teardrop settings.
pub fn diff_footprint(board_fp: &Node, library_fp: &Node) -> FootprintDiff {
    footprint::diff(board_fp, library_fp)
}

/// Stable content hash of a footprint in library frame (same ignores as diff_footprint,
/// models excluded).
pub fn footprint_hash(board_fp: &Node) -> String {
    footprint::hash(board_fp)
}

/// Differences in the model lists (path, offset, scale, rotate, hide; opacity ignored) between
/// a board footprint and its library footprint. Empty = identical.
pub fn models_diff(board_fp: &Node, library_fp: &Node) -> Vec<String> {
    footprint::models_diff(board_fp, library_fp)
}

/// Convert a board footprint into a standalone library footprint named `name`: library frame
/// (unplaced, unflipped), strip uuid/path/sheet info/nets/pinfunction/pintype/locked, keep
/// properties and models. Optionally rewrite model paths via `model_path_map` (old -> new),
/// e.g. `${KIPRJMOD}/libraries/3dmodels/X.step` -> `${KIPRJMOD}/3d/X.step`.
pub fn footprint_to_library(
    board_fp: &Node,
    name: &str,
    model_path_map: &[(String, String)],
) -> Node {
    footprint::to_library(board_fp, name, model_path_map)
}

/// Full text of a `.kicad_mod` file for the footprint (adds `(version ..)`, `(generator ..)`,
/// `(generator_version ..)` like KiCad does for the given major).
pub fn footprint_file(fp: &Node, version: crate::types::KicadVersion) -> String {
    footprint::file(fp, version)
}

/// Debug helper: canonical (library-frame) form of a footprint, one item per line.
#[doc(hidden)]
pub fn debug_footprint_canonical(fp: &Node) -> String {
    footprint::debug_dump(fp)
}

/// Debug helper: canonical form of a symbol, one item per line.
#[doc(hidden)]
pub fn debug_symbol_canonical(sym: &Node) -> String {
    symbol::debug_dump(sym)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexpr::parse;
    use crate::types::KicadVersion;

    const SYM_V9: &str = r#"(symbol "Device:R"
        (pin_numbers hide) (pin_names (offset 0) hide)
        (exclude_from_sim no) (in_bom yes) (on_board yes)
        (property "Reference" "R" (at 2.032 0 90) (effects (font (size 1.27 1.27))))
        (property "Value" "R" (at 0 0 90) (effects (font (size 1.27 1.27))))
        (symbol "R_0_1"
            (rectangle (start -1.016 -2.54) (end 1.016 2.54) (stroke (width 0.254) (type default)) (fill (type none))))
        (symbol "R_1_1"
            (pin passive line (at 0 3.81 270) (length 1.27) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
            (pin passive line (at 0 -3.81 90) (length 1.27) (name "~" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27))))))
        (embedded_fonts no))"#;

    // Same symbol in KiCad 10 formatting, different name / properties, reordered items.
    const SYM_V10: &str = r#"(symbol "R_Custom"
        (pin_numbers (hide yes)) (pin_names (offset 0.0) (hide yes))
        (exclude_from_sim no) (in_bom yes) (on_board yes) (in_pos_files yes)
        (duplicate_pin_numbers_are_jumpers no)
        (property "Reference" "R" (at 5 5 0) (show_name no) (do_not_autoplace no) (effects (font (size 1 1))))
        (property "Extra" "x" (at 0 0 0) (effects (font (size 1.27 1.27))))
        (symbol "R_Custom_1_1"
            (pin passive line (at 0 -3.81 90.0) (length 1.270) (name "" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))
            (pin passive line (at 0 3.81 -90) (length 1.27) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27))))))
        (symbol "R_Custom_0_1"
            (rectangle (start 1.016 2.54) (end -1.016 -2.54) (stroke (width 0.254) (type default)) (fill (type none))))
        (embedded_fonts no))"#;

    #[test]
    fn symbol_version_formatting_ignored() {
        let a = parse(SYM_V9).unwrap();
        let b = parse(SYM_V10).unwrap();
        let d = diff_symbol(&a, &b);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(symbol_hash(&a), symbol_hash(&b));
        assert!(!is_power(&a));
        assert!(!is_graphic_only(&a));
    }

    #[test]
    fn symbol_pin_change_detected() {
        let a = parse(SYM_V9).unwrap();
        let b = parse(&SYM_V9.replace(
            "(at 0 3.81 270) (length 1.27)",
            "(at 0 5.08 270) (length 2.54)",
        ))
        .unwrap();
        let d = diff_symbol(&b, &a);
        assert_eq!(d.pins.len(), 1);
        assert_eq!(d.pins[0].number, "1");
        assert_eq!(d.pins[0].change, "changed");
        assert_eq!(d.pins[0].fields, ["position", "length"]);
        assert_ne!(symbol_hash(&a), symbol_hash(&b));

        let c = parse(&SYM_V9.replace("(in_bom yes)", "(in_bom no)")).unwrap();
        let d = diff_symbol(&c, &a);
        assert_eq!(d.other.len(), 1, "{d:?}");
        let e = parse(&SYM_V9.replace("(width 0.254)", "(width 0.3)")).unwrap();
        assert_eq!(
            diff_symbol(&e, &a).graphics,
            ["unit 1: 1 rectangle changed"]
        );
    }

    #[test]
    fn symbol_power_graphic_rename() {
        let p = parse(r#"(symbol "power:GND" (power) (symbol "GND_1_1" (pin power_in line (at 0 0 270) (length 0) hide (name "GND") (number "1"))))"#).unwrap();
        assert!(is_power(&p));
        let legacy = parse(r##"(symbol "x:GND" (property "Reference" "#PWR" (at 0 0 0)) (symbol "GND_0_1" (pin power_in line (at 0 0 270) (length 0) hide (name "GND") (number "1"))))"##).unwrap();
        assert!(is_power(&legacy));
        let logo = parse(
            r#"(symbol "Graphic:Logo" (symbol "Logo_0_1" (circle (center 0 0) (radius 1))))"#,
        )
        .unwrap();
        assert!(is_graphic_only(&logo));
        let r = symbol_to_library(&parse(SYM_V9).unwrap(), "MyR");
        assert_eq!(r.first_arg(), Some("MyR"));
        let subs: Vec<&str> = r.find_all("symbol").filter_map(|s| s.first_arg()).collect();
        assert_eq!(subs, ["MyR_0_1", "MyR_1_1"]);
        let text = symbol_library_file(&[r], KicadVersion::V9);
        let lib = parse(&text).unwrap();
        assert_eq!(lib.value("version"), Some("20241209"));
        assert_eq!(lib.value("generator_version"), Some("9.0"));
    }

    const FP_LIB: &str = r#"(footprint "SOT-23"
        (layer "F.Cu")
        (property "Reference" "REF**" (at 0 -2.4 0) (layer "F.SilkS") (effects (font (size 1 1) (thickness 0.15))))
        (attr smd)
        (fp_line (start 0 -1.56) (end 0.65 -1.56) (stroke (width 0.12) (type solid)) (layer "F.SilkS"))
        (fp_rect (start -1.4 -1.7) (end 1.4 1.7) (stroke (width 0.05) (type solid)) (fill no) (layer "F.CrtYd"))
        (fp_text user "${REFERENCE}" (at 0 0 0) (layer "F.Fab") (effects (font (size 0.32 0.32) (thickness 0.05))))
        (pad "1" smd roundrect (at -0.9375 -0.95) (size 1.475 0.6) (layers "F.Cu" "F.Mask" "F.Paste") (roundrect_rratio 0.25))
        (pad "2" smd roundrect (at -0.9375 0.95 90) (size 1.475 0.6) (layers "F.Cu" "F.Mask" "F.Paste") (roundrect_rratio 0.25))
        (pad "3" thru_hole oval (at 0.9375 0) (size 1.475 0.6) (drill oval 0.6 0.3 (offset 0.1 0.2)) (layers "*.Cu" "*.Mask") (remove_unused_layers no))
        (model "${KICAD9_3DMODEL_DIR}/Package_TO_SOT_SMD.3dshapes/SOT-23.step" (offset (xyz 0 0 0)) (scale (xyz 1 1 1)) (rotate (xyz 0 0 0))))"#;

    // FP_LIB placed on B.Cu at 90 degrees (KiCad conventions: local y mirrored, absolute angles).
    const FP_BOARD_BACK: &str = r#"(footprint "Package_TO_SOT_SMD:SOT-23"
        (layer "B.Cu") (uuid "u0") (at 100 50 90)
        (property "Reference" "Q7" (at 0 2.4 90) (layer "B.SilkS") (uuid "u1") (effects (font (size 1 1) (thickness 0.15)) (justify mirror)))
        (path "/abc") (sheetname "/") (sheetfile "x.kicad_sch")
        (attr smd)
        (fp_line (start 0 1.56) (end 0.65 1.56) (stroke (width 0.12) (type solid)) (layer "B.SilkS") (uuid "u2"))
        (fp_rect (start -1.4 1.7) (end 1.4 -1.7) (stroke (width 0.05) (type solid)) (fill no) (layer "B.CrtYd") (uuid "u3"))
        (fp_text user "${REFERENCE}" (at 0 0 270) (layer "B.Fab") (uuid "u4") (effects (font (size 0.32 0.32) (thickness 0.05)) (justify mirror)))
        (pad "1" smd roundrect (at -0.9375 0.95 90) (size 1.475 0.6) (layers "B.Cu" "B.Mask" "B.Paste") (roundrect_rratio 0.25) (net "GND") (pinfunction "G") (pintype "input") (uuid "u5"))
        (pad "2" smd roundrect (at -0.9375 -0.95 0) (size 1.475 0.6) (layers "B.Cu" "B.Mask" "B.Paste") (roundrect_rratio 0.25) (net "X") (uuid "u6"))
        (pad "3" thru_hole oval (at 0.9375 0 90) (size 1.475 0.6) (drill oval 0.6 0.3 (offset 0.1 -0.2)) (layers "*.Cu" "*.Mask") (net "Y") (uuid "u7"))
        (embedded_fonts no)
        (model "${KICAD10_3DMODEL_DIR}/Package_TO_SOT_SMD.3dshapes/SOT-23.step" (offset (xyz 0 0 0)) (scale (xyz 1 1 1)) (rotate (xyz 0 0 0))))"#;

    #[test]
    fn footprint_back_rotated_matches_library() {
        let lib = parse(FP_LIB).unwrap();
        let board = parse(FP_BOARD_BACK).unwrap();
        let d = diff_footprint(&board, &lib);
        assert!(d.is_empty(), "{d:?}");
        assert!(models_diff(&board, &lib).is_empty());
        assert_eq!(footprint_hash(&board), footprint_hash(&lib));

        // library form round trip
        let conv = footprint_to_library(&board, "MySOT", &[]);
        assert_eq!(conv.first_arg(), Some("MySOT"));
        assert_eq!(conv.value("layer"), Some("F.Cu"));
        assert!(conv.find("at").is_none() && conv.find("path").is_none());
        let mut has_uuid = false;
        conv.walk(&mut |n| has_uuid |= n.is("uuid") || n.is("net"));
        assert!(!has_uuid);
        let text = footprint_file(&conv, KicadVersion::V10);
        let back = parse(&text).unwrap();
        assert_eq!(back.value("version"), Some("20260206"));
        assert!(diff_footprint(&board, &back).is_empty());
        assert!(diff_footprint(&back, &lib).is_empty());
    }

    #[test]
    fn footprint_changes_detected() {
        let lib = parse(FP_LIB).unwrap();
        let moved = parse(&FP_LIB.replace("(at -0.9375 -0.95)", "(at -1 -0.95)")).unwrap();
        let d = diff_footprint(&moved, &lib);
        assert_eq!(d.pads.len(), 1);
        assert_eq!(d.pads[0].fields, ["position"]);
        let drill = parse(&FP_LIB.replace("(offset 0.1 0.2)", "(offset 0.1 0.3)")).unwrap();
        assert_eq!(diff_footprint(&drill, &lib).pads[0].fields, ["drill"]);
        let attr = parse(&FP_LIB.replace("(attr smd)", "(attr through_hole)")).unwrap();
        assert_eq!(diff_footprint(&attr, &lib).other.len(), 1);
        let silk = parse(&FP_LIB.replace("(end 0.65 -1.56)", "(end 0.7 -1.56)")).unwrap();
        assert_eq!(
            diff_footprint(&silk, &lib).graphics,
            ["F.SilkS: 1 line changed"]
        );
        // property-only edits never count
        let props =
            parse(&FP_LIB.replace("\"REF**\" (at 0 -2.4 0)", "\"Q1\" (at 3 3 45)")).unwrap();
        assert!(diff_footprint(&props, &lib).is_empty());
        // dnp / exclude flags are board-level
        let dnp = parse(&FP_LIB.replace("(attr smd)", "(attr smd dnp exclude_from_bom)")).unwrap();
        assert!(diff_footprint(&dnp, &lib).is_empty());
    }

    #[test]
    fn models() {
        let lib = parse(FP_LIB).unwrap();
        let off = parse(&FP_LIB.replace("(offset (xyz 0 0 0))", "(offset (xyz 0 0 1))")).unwrap();
        assert_eq!(models_diff(&off, &lib).len(), 1);
        let hidden = parse(&FP_LIB.replace(
            "(rotate (xyz 0 0 0))",
            "(rotate (xyz 0 0 360)) (hide yes) (opacity 0.5)",
        ))
        .unwrap();
        // Visibility (hide) and opacity are ignored by design; 360° rotation equals 0°.
        assert!(models_diff(&hidden, &lib).is_empty());
        let mapped = footprint_to_library(
            &lib,
            "X",
            &[(
                "${KICAD9_3DMODEL_DIR}/Package_TO_SOT_SMD.3dshapes/SOT-23.step".into(),
                "${KIPRJMOD}/3d/SOT-23.step".into(),
            )],
        );
        assert_eq!(
            mapped.find("model").and_then(Node::first_arg),
            Some("${KIPRJMOD}/3d/SOT-23.step")
        );
        assert_eq!(
            models_diff(&mapped, &lib),
            [
                "model added: ${KIPRJMOD}/3d/SOT-23.step",
                "model removed: ${KICAD_3DMODEL_DIR}/Package_TO_SOT_SMD.3dshapes/SOT-23.step"
            ]
        );
    }
}
