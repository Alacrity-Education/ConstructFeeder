//! Footprint normalization (board frame -> library frame), canonical form, diff and hash.
//!
//! Frame conventions (verified against KiCad 9/10 boards vs stock libraries):
//! * Graphics (`fp_*`) and pad/text *positions* are stored footprint-local and unrotated.
//! * Pad and text *angles* (third value of `at`) are absolute: they include the footprint rotation.
//! * B.Cu footprints: local Y is mirrored (y -> -y), local angles negated, F./B. layers swapped,
//!   texts get `(justify mirror)` toggled.
//! * Zones inside footprints are stored in absolute board coordinates.

use super::canon::{CVal, EPS, canon, multiset_diff, norm_angle, nums, parse_num, sha256_hex};
use super::symbol::canon_path;
use super::{FootprintDiff, PadDiff};
use crate::sexpr::Node;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------------------------
// Frame transformation
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Placement {
    x: f64,
    y: f64,
    rot: f64,
    back: bool,
}

fn placement(fp: &Node) -> Placement {
    let at = fp.find("at").map(nums).unwrap_or_default();
    Placement {
        x: at.first().copied().unwrap_or(0.0),
        y: at.get(1).copied().unwrap_or(0.0),
        rot: at.get(2).copied().unwrap_or(0.0),
        back: fp.value("layer") == Some("B.Cu"),
    }
}

pub fn flip_layer(l: &str) -> String {
    if let Some(r) = l.strip_prefix("F.") {
        format!("B.{r}")
    } else if let Some(r) = l.strip_prefix("B.") {
        format!("F.{r}")
    } else {
        l.to_string()
    }
}

fn fmt(x: f64) -> Node {
    Node::sym(super::canon::fmt_num_full(x))
}

fn set_num(n: &mut Node, idx: usize, v: f64) {
    if let Some(items) = n.items_mut() {
        while items.len() <= idx {
            items.push(Node::sym("0"));
        }
        items[idx] = fmt(v);
    }
}

fn get_num(n: &Node, idx: usize) -> Option<f64> {
    n.items().get(idx).and_then(Node::atom).and_then(parse_num)
}

/// Mirror Y of every point-like child in the subtree (not descending into `model`).
fn mirror_points(n: &mut Node) {
    let Some(items) = n.items_mut() else { return };
    for c in items.iter_mut().skip(1) {
        let h = c.head().unwrap_or("").to_string();
        match h.as_str() {
            "xy" | "start" | "end" | "mid" | "center" | "at" | "offset" => {
                if let Some(y) = get_num(c, 2) {
                    set_num(c, 2, -y);
                }
            }
            "model" | "effects" | "property" => {}
            _ => mirror_points(c),
        }
    }
}

/// Swap F./B. layer names in `layer`/`layers`/`private_layers` lists of the subtree.
fn flip_layers(n: &mut Node) {
    let is_layer_list = matches!(n.head(), Some("layer" | "layers" | "private_layers"));
    let Some(items) = n.items_mut() else { return };
    for c in items.iter_mut().skip(1) {
        match c {
            Node::Str(s) | Node::Sym(s) if is_layer_list => *s = flip_layer(s),
            Node::List(_) if !c.is("model") => flip_layers(c),
            _ => {}
        }
    }
}

/// Toggle `mirror` in `(effects (justify ...))`.
fn toggle_mirror(n: &mut Node) {
    let Some(eff) = n.find_mut("effects") else {
        return;
    };
    if eff.find("justify").is_none()
        && let Some(items) = eff.items_mut()
    {
        items.push(Node::list("justify", vec![]));
    }
    let j = eff.find_mut("justify").unwrap();
    let items = j.items_mut().unwrap();
    if let Some(pos) = items
        .iter()
        .skip(1)
        .position(|c| c.atom() == Some("mirror"))
    {
        items.remove(pos + 1);
    } else {
        items.push(Node::sym("mirror"));
    }
    if items.len() == 1 {
        eff.remove_children(&["justify"]);
    }
}

/// Board-absolute angle -> library-local angle.
fn local_angle(abs: f64, p: Placement) -> f64 {
    let a = abs - p.rot;
    norm_angle(if p.back { -a } else { a })
}

fn fix_at_angle(n: &mut Node, p: Placement) {
    if let Some(at) = n.find_mut("at") {
        let abs = get_num(at, 3).unwrap_or(0.0);
        let a = local_angle(abs, p);
        let items = at.items_mut().unwrap();
        items.truncate(3);
        if a != 0.0 {
            items.push(fmt(a));
        }
    }
}

/// Point in absolute board coordinates -> library frame.
fn abs_to_local(x: f64, y: f64, p: Placement) -> (f64, f64) {
    let (dx, dy) = (x - p.x, y - p.y);
    // KiCad RotatePoint(angle): x' = x cos + y sin ; y' = -x sin + y cos (y axis down).
    // board = pos + Rot(rot) * local  =>  local = Rot(-rot) * (board - pos)
    let (s, c) = sincos(-p.rot);
    let lx = dx * c + dy * s;
    let ly = -dx * s + dy * c;
    if p.back { (lx, -ly) } else { (lx, ly) }
}

/// KiCad 5 arc `(start <center>) (end <arc start>) (angle <deg>)` -> (start, mid, end), using
/// KiCad's RotatePoint convention (verified against `kicad-cli fp upgrade`).
fn legacy_arc(
    center: (f64, f64),
    start: (f64, f64),
    angle: f64,
) -> ((f64, f64), (f64, f64), (f64, f64)) {
    let rot = |deg: f64| {
        let (s, c) = sincos(deg);
        let (dx, dy) = (start.0 - center.0, start.1 - center.1);
        (center.0 + dx * c + dy * s, center.1 - dx * s + dy * c)
    };
    (start, rot(angle / 2.0), rot(angle))
}

fn sincos(deg: f64) -> (f64, f64) {
    let a = norm_angle(deg);
    for (q, s, c) in [
        (0.0, 0.0, 1.0),
        (90.0, 1.0, 0.0),
        (180.0, 0.0, -1.0),
        (270.0, -1.0, 0.0),
    ] {
        if (a - q).abs() < 1e-9 {
            return (s, c);
        }
    }
    a.to_radians().sin_cos()
}

fn abs_points_to_local(n: &mut Node, p: Placement) {
    let Some(items) = n.items_mut() else { return };
    for c in items.iter_mut().skip(1) {
        if c.is("xy") {
            if let (Some(x), Some(y)) = (get_num(c, 1), get_num(c, 2)) {
                let (lx, ly) = abs_to_local(x, y, p);
                set_num(c, 1, lx);
                set_num(c, 2, ly);
            }
        } else if matches!(c, Node::List(_)) {
            abs_points_to_local(c, p);
        }
    }
}

fn swap_chamfer(n: &mut Node) {
    if let Some(ch) = n.find_mut("chamfer")
        && let Some(items) = ch.items_mut()
    {
        for c in items.iter_mut().skip(1) {
            if let Node::Sym(s) = c {
                *s = match s.as_str() {
                    "top_left" => "bottom_left",
                    "bottom_left" => "top_left",
                    "top_right" => "bottom_right",
                    "bottom_right" => "top_right",
                    o => o,
                }
                .to_string();
            }
        }
    }
}

/// Bring a board footprint into library frame (unplaced, unrotated, unflipped). Content is
/// otherwise unchanged (uuids etc. are kept; stripping is done separately).
pub fn to_lib_frame(fp: &Node) -> Node {
    let p = placement(fp);
    let mut out = fp.clone();
    let Some(items) = out.items_mut() else {
        return out;
    };
    for c in items.iter_mut().skip(1) {
        let h = c.head().unwrap_or("").to_string();
        match h.as_str() {
            "at" => {}
            "layer" if p.back => flip_layers(c),
            "pad" => {
                if p.back {
                    mirror_points(c);
                    if let Some(rd) = c.find_mut("rect_delta")
                        && let Some(y) = get_num(rd, 2)
                    {
                        set_num(rd, 2, -y);
                    }
                    swap_chamfer(c);
                    flip_layers(c);
                }
                fix_at_angle(c, p);
            }
            "property" | "fp_text" => {
                if p.back {
                    if let Some(at) = c.find_mut("at")
                        && let Some(y) = get_num(at, 2)
                    {
                        set_num(at, 2, -y);
                    }
                    toggle_mirror(c);
                    flip_layers(c);
                }
                fix_at_angle(c, p);
            }
            "fp_text_box" => {
                if p.back {
                    mirror_points(c);
                    toggle_mirror(c);
                    flip_layers(c);
                }
                if let Some(a) = c.find_mut("angle")
                    && let Some(v) = get_num(a, 1)
                {
                    set_num(a, 1, local_angle(v, p));
                }
            }
            "zone" => {
                abs_points_to_local(c, p);
                if p.back {
                    flip_layers(c);
                }
            }
            "point" => {
                // KiCad 10 PCB_POINT: stored in absolute board coordinates, like zones.
                if let Some(at) = c.find_mut("at")
                    && let (Some(x), Some(y)) = (get_num(at, 1), get_num(at, 2))
                {
                    let (lx, ly) = abs_to_local(x, y, p);
                    set_num(at, 1, lx);
                    set_num(at, 2, ly);
                }
                if p.back {
                    flip_layers(c);
                }
            }
            "model" => {}
            _ => {
                if p.back {
                    mirror_points(c);
                    flip_layers(c);
                    if h == "dimension"
                        && let Some(hn) = c.find_mut("height")
                        && let Some(v) = get_num(hn, 1)
                    {
                        set_num(hn, 1, -v);
                    }
                }
            }
        }
    }
    if p.back
        && let Some(l) = out.find_mut("layer")
        && let Some(items) = l.items_mut()
    {
        items.truncate(1);
        items.push(Node::str("F.Cu"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Canonical form
// ---------------------------------------------------------------------------------------------

/// Heads ignored anywhere inside footprint content.
const SKIP_DEEP: &[&str] = &[
    "uuid",
    "tstamp",
    "net",
    "pinfunction",
    "pintype",
    "locked",
    "teardrops",
    "unlocked",
];

/// Footprint top-level heads that never count.
const IGNORED_TOP: &[&str] = &[
    "at",
    "layer",
    "uuid",
    "tstamp",
    "tedit",
    "locked",
    "placed",
    "property",
    "path",
    "sheetname",
    "sheetfile",
    "model",
    "embedded_files",
    "embedded_fonts",
    "teardrops",
    "generator",
    "generator_version",
    "version",
    "descr",
    "tags",
    "units",
    "group",
    "component_classes",
    "autoplace_cost90",
    "autoplace_cost180",
];

#[derive(Clone, Debug, Default)]
pub struct FpCanon {
    pub pads: Vec<(String, BTreeMap<String, CVal>)>,
    /// (layer, primitive)
    pub graphics: Vec<(String, CVal)>,
    pub other: BTreeMap<String, CVal>,
    pub zones: Vec<CVal>,
}

fn pad_field_name(h: &str) -> &str {
    match h {
        "solder_paste_margin" => "paste_margin",
        "solder_paste_margin_ratio" | "solder_paste_ratio" => "paste_margin_ratio",
        "thermal_width" => "thermal_bridge_width",
        o => o,
    }
}

fn layer_set(n: &Node) -> CVal {
    let mut v: Vec<String> = n
        .args()
        .iter()
        .filter_map(Node::atom)
        .map(String::from)
        .collect();
    v.sort();
    v.dedup();
    CVal::List(v.iter().map(|s| CVal::atom(s)).collect())
}

fn canon_pad(n: &Node) -> (String, BTreeMap<String, CVal>) {
    let number = n.str_at(1).unwrap_or("").to_string();
    let mut f: BTreeMap<String, CVal> = BTreeMap::new();
    let ty = n.str_at(2).unwrap_or("").to_string();
    let shape = n.str_at(3).unwrap_or("").to_string();
    f.insert("type".into(), CVal::atom(&ty));
    f.insert("shape".into(), CVal::atom(&shape));
    let at = n.find("at").map(nums).unwrap_or_default();
    f.insert(
        "position".into(),
        CVal::List(vec![
            CVal::Num(at.first().copied().unwrap_or(0.0)),
            CVal::Num(at.get(1).copied().unwrap_or(0.0)),
        ]),
    );
    f.insert(
        "orientation".into(),
        CVal::Num(norm_angle(at.get(2).copied().unwrap_or(0.0))),
    );
    for c in n.children() {
        let h = c.head().unwrap_or("");
        if SKIP_DEEP.contains(&h) || h == "at" {
            continue;
        }
        let v = match h {
            "layers" => layer_set(c),
            "drill" => canon_drill(c),
            "primitives" => canon_primitives(c),
            _ => canon(c, SKIP_DEEP),
        };
        // (x no) defaults
        if let CVal::List(items) = &v
            && items.len() == 2
            && matches!(&items[1], CVal::Atom(a) if a == "no")
        {
            continue;
        }
        f.insert(pad_field_name(h).to_string(), v);
    }
    // roundrect ratio only meaningful for roundrect pads (or chamfered rects)
    if shape != "roundrect" && !f.contains_key("chamfer") {
        f.remove("roundrect_rratio");
    }
    if !f.contains_key("chamfer") {
        f.remove("chamfer_ratio");
    }
    // thru-hole drill of 0 / smd without drill
    if let Some(CVal::List(d)) = f.get("drill")
        && d.iter()
            .all(|x| matches!(x, CVal::Num(v) if v.abs() <= EPS) || matches!(x, CVal::Atom(_)))
        && ty == "smd"
    {
        f.remove("drill");
    }
    (number, f)
}

fn canon_drill(n: &Node) -> CVal {
    let oval = n.args().iter().any(|a| a.atom() == Some("oval"));
    let sizes: Vec<f64> = n
        .args()
        .iter()
        .filter_map(|a| a.atom().and_then(parse_num))
        .collect();
    let w = sizes.first().copied().unwrap_or(0.0);
    let h = sizes.get(1).copied().unwrap_or(w);
    let mut v = vec![
        CVal::atom(if oval { "oval" } else { "round" }),
        CVal::Num(w),
        CVal::Num(if oval { h } else { w }),
    ];
    let off = n.find("offset").map(nums).unwrap_or_default();
    let (ox, oy) = (
        off.first().copied().unwrap_or(0.0),
        off.get(1).copied().unwrap_or(0.0),
    );
    v.push(CVal::List(vec![CVal::Num(ox), CVal::Num(oy)]));
    CVal::List(v)
}

fn canon_primitives(n: &Node) -> CVal {
    let mut v: Vec<CVal> = n.children().map(canon_graphic_body).collect();
    v.sort_by_cached_key(CVal::key);
    CVal::List(v)
}

fn pt(n: Option<&Node>) -> (f64, f64) {
    let v = n.map(nums).unwrap_or_default();
    (
        v.first().copied().unwrap_or(0.0),
        v.get(1).copied().unwrap_or(0.0),
    )
}

fn ptv((x, y): (f64, f64)) -> CVal {
    CVal::List(vec![CVal::Num(x), CVal::Num(y)])
}

fn pts_of(n: &Node) -> Vec<(f64, f64)> {
    n.find("pts")
        .map(|p| p.find_all("xy").map(|xy| pt(Some(xy))).collect())
        .unwrap_or_default()
}

fn pt_key(p: &(f64, f64)) -> (i64, i64) {
    (
        (p.0 * 10000.0).round() as i64,
        (p.1 * 10000.0).round() as i64,
    )
}

/// Closed polygon: start point and direction do not matter.
fn canon_ring(p: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut p = p;
    if p.len() > 1 && pt_key(&p[0]) == pt_key(&p[p.len() - 1]) {
        p.pop();
    }
    if p.is_empty() {
        return p;
    }
    let keys: Vec<(i64, i64)> = p.iter().map(pt_key).collect();
    let n = p.len();
    let min = *keys.iter().min().unwrap();
    let mut best_key: Option<Vec<(i64, i64)>> = None;
    let mut best_idx: Vec<usize> = Vec::new();
    for rev in [false, true] {
        for s in (0..n).filter(|&s| keys[s] == min) {
            let idx: Vec<usize> = (0..n)
                .map(|i| if rev { (s + n - i) % n } else { (s + i) % n })
                .collect();
            let k: Vec<(i64, i64)> = idx.iter().map(|&i| keys[i]).collect();
            if best_key.as_ref().is_none_or(|bk| k < *bk) {
                best_key = Some(k);
                best_idx = idx;
            }
        }
    }
    best_idx.into_iter().map(|i| p[i]).collect()
}

fn stroke(n: &Node) -> CVal {
    let s = n.find("stroke");
    let width = s
        .and_then(|s| s.find("width"))
        .or_else(|| n.find("width"))
        .map(nums)
        .and_then(|v| v.first().copied())
        .unwrap_or(0.0);
    let ty = match s.and_then(|s| s.value("type")).unwrap_or("solid") {
        "default" => "solid",
        o => o,
    };
    CVal::list("stroke", vec![CVal::Num(width), CVal::atom(ty)])
}

fn fill(n: &Node) -> CVal {
    let f = match n.find("fill") {
        None => false,
        Some(f) => match f.first_arg() {
            Some("yes" | "solid" | "true") => true,
            Some("no" | "none" | "false") | None => f
                .find("type")
                .is_some_and(|t| !matches!(t.first_arg(), Some("none"))),
            _ => true,
        },
    };
    CVal::list("fill", vec![CVal::atom(if f { "yes" } else { "no" })])
}

/// Canonical geometry of fp_*/gr_* primitives, without layer.
fn canon_graphic_body(n: &Node) -> CVal {
    let h = n.head().unwrap_or("");
    let kind = h
        .strip_prefix("fp_")
        .or_else(|| h.strip_prefix("gr_"))
        .unwrap_or(h);
    let mut v = vec![CVal::atom(kind)];
    match kind {
        "line" => {
            let (a, b) = (pt(n.find("start")), pt(n.find("end")));
            let (a, b) = if pt_key(&b) < pt_key(&a) {
                (b, a)
            } else {
                (a, b)
            };
            v.extend([ptv(a), ptv(b), stroke(n)]);
        }
        "rect" => {
            let (a, b) = (pt(n.find("start")), pt(n.find("end")));
            let (x0, y0, x1, y1) = (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));
            match n
                .find("radius")
                .map(nums)
                .and_then(|r| r.first().copied())
                .filter(|r| *r > EPS)
            {
                Some(r) => {
                    v.extend([
                        ptv((x0, y0)),
                        ptv((x1, y1)),
                        stroke(n),
                        fill(n),
                        CVal::list("radius", vec![CVal::Num(r)]),
                    ]);
                }
                None => {
                    // A rectangle is the same as a 4-point polygon (KiCad converts rects of
                    // footprints placed at non-orthogonal angles into polygons).
                    v[0] = CVal::atom("poly");
                    let ring = canon_ring(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
                    v.push(CVal::List(ring.into_iter().map(ptv).collect()));
                    v.extend([stroke(n), fill(n)]);
                }
            }
        }
        "circle" => {
            let c = pt(n.find("center"));
            let e = pt(n.find("end"));
            let r = ((e.0 - c.0).powi(2) + (e.1 - c.1).powi(2)).sqrt();
            v.extend([ptv(c), CVal::Num(r), stroke(n), fill(n)]);
        }
        "arc" => {
            let (a, m, b) = match (n.find("mid"), n.find("angle")) {
                (None, Some(ang)) => legacy_arc(
                    pt(n.find("start")),
                    pt(n.find("end")),
                    nums(ang).first().copied().unwrap_or(0.0),
                ),
                _ => (pt(n.find("start")), pt(n.find("mid")), pt(n.find("end"))),
            };
            let (a, b) = if pt_key(&b) < pt_key(&a) {
                (b, a)
            } else {
                (a, b)
            };
            v.extend([ptv(a), ptv(m), ptv(b), stroke(n)]);
        }
        "poly" => {
            let p = canon_ring(pts_of(n));
            v.push(CVal::List(p.into_iter().map(ptv).collect()));
            v.extend([stroke(n), fill(n)]);
        }
        "curve" | "bezier" => {
            let p = canon_path(pts_of(n));
            v.push(CVal::List(p.into_iter().map(ptv).collect()));
            v.push(stroke(n));
        }
        _ => {
            let c = canon(n, &[SKIP_DEEP, &["layer", "layers"]].concat());
            if let CVal::List(items) = c {
                v.extend(items.into_iter().skip(1));
            }
        }
    }
    CVal::List(v)
}

/// Text consisting only of `${VAR}` field references (and whitespace), e.g. `${REFERENCE}`.
fn is_field_placeholder(t: &str) -> bool {
    let mut rest = t.trim();
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        // KiCad 5 libraries use `%R` / `%V` for reference / value.
        if let Some(r) = rest.strip_prefix("%R").or_else(|| rest.strip_prefix("%V")) {
            rest = r.trim_start();
            continue;
        }
        let Some(r) = rest.strip_prefix("${") else {
            return false;
        };
        let Some(end) = r.find('}') else { return false };
        rest = r[end + 1..].trim_start();
    }
    true
}

fn canon_text(n: &Node) -> CVal {
    let at = n.find("at").map(nums).unwrap_or_default();
    let g = |i: usize| at.get(i).copied().unwrap_or(0.0);
    let eff = n
        .find("effects")
        .map(|e| canon(e, &["href"]))
        .unwrap_or(CVal::atom("-"));
    let mut v = vec![
        CVal::atom("text"),
        CVal::atom(n.str_at(2).unwrap_or("")),
        CVal::Num(g(0)),
        CVal::Num(g(1)),
        // Footprint texts are kept upright by KiCad: the stored angle is only meaningful mod 180.
        CVal::Num(norm_angle(norm_angle(g(2)) % 180.0)),
        eff,
    ];
    if n.flag("hide") == Some(true) {
        v.push(CVal::atom("hidden"));
    }
    if n.flag("knockout") == Some(true) {
        v.push(CVal::atom("knockout"));
    }
    CVal::List(v)
}

fn graphic_layer(n: &Node) -> String {
    if let Some(l) = n.value("layer") {
        return l.to_string();
    }
    if let Some(ls) = n.find("layers") {
        let mut v: Vec<&str> = ls.args().iter().filter_map(Node::atom).collect();
        v.sort();
        return v.join(",");
    }
    String::new()
}

pub fn canonical_lib_frame(fp: &Node) -> FpCanon {
    let mut c = FpCanon::default();
    for n in fp.children() {
        let h = n.head().unwrap_or("");
        if IGNORED_TOP.contains(&h) {
            continue;
        }
        match h {
            "pad" => c.pads.push(canon_pad(n)),
            "fp_text" => {
                if matches!(n.str_at(1), Some("reference" | "value")) {
                    continue;
                }
                // Field placeholders like `${REFERENCE}` on F.Fab are property displays: ignored.
                if is_field_placeholder(n.str_at(2).unwrap_or("")) {
                    continue;
                }
                c.graphics.push((graphic_layer(n), canon_text(n)));
            }
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" | "fp_curve" => {
                c.graphics.push((graphic_layer(n), canon_graphic_body(n)));
            }
            "fp_text_box" | "dimension" | "image" | "barcode" | "point" | "fp_point"
            | "fp_table" | "table" => {
                let mut g = canon(n, &[SKIP_DEEP, &["layer", "render_cache"]].concat());
                if let CVal::List(items) = &mut g
                    && h == "dimension"
                {
                    items.retain(|i| i.head() != Some("gr_text"));
                }
                c.graphics.push((graphic_layer(n), g));
            }
            "zone" => c.zones.push(canon_zone(n)),
            "attr" => {
                let mut flags: Vec<String> = n
                    .args()
                    .iter()
                    .filter_map(Node::atom)
                    .filter(|a| {
                        !matches!(*a, "exclude_from_pos_files" | "exclude_from_bom" | "dnp")
                    })
                    .map(String::from)
                    .collect();
                let ty = if flags
                    .first()
                    .is_some_and(|f| f == "smd" || f == "through_hole")
                {
                    flags.remove(0)
                } else {
                    "none".into()
                };
                c.other.insert("attr".into(), CVal::atom(&ty));
                flags.sort();
                for f in flags {
                    c.other.insert(f, CVal::atom("yes"));
                }
            }
            "net_tie_pad_groups" => {
                let mut groups: Vec<String> = n
                    .args()
                    .iter()
                    .filter_map(Node::atom)
                    .map(|g| {
                        let mut p: Vec<&str> = g
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .collect();
                        p.sort();
                        p.join(",")
                    })
                    .collect();
                groups.sort();
                c.other.insert(
                    h.into(),
                    CVal::List(groups.iter().map(|s| CVal::atom(s)).collect()),
                );
            }
            "private_layers" => {
                c.other.insert(h.into(), layer_set(n));
            }
            _ => {
                let v = canon(n, SKIP_DEEP);
                if let CVal::List(items) = &v
                    && items.len() == 2
                    && matches!(&items[1], CVal::Atom(a) if a == "no")
                {
                    continue;
                }
                c.other.insert(pad_field_name(h).to_string(), v);
            }
        }
    }
    c
}

fn canon_zone(n: &Node) -> CVal {
    let mut v = vec![CVal::atom("zone")];
    let layers = n
        .find("layers")
        .map(layer_set)
        .or_else(|| n.find("layer").map(layer_set))
        .unwrap_or(CVal::List(vec![]));
    v.push(CVal::list("layers", vec![layers]));
    let mut polys: Vec<CVal> = n
        .find_all("polygon")
        .map(|p| CVal::List(canon_ring(pts_of(p)).into_iter().map(ptv).collect()))
        .collect();
    polys.sort_by_cached_key(CVal::key);
    v.push(CVal::list("polygons", polys));
    for c in n.children() {
        let h = c.head().unwrap_or("");
        if matches!(
            h,
            "layer"
                | "layers"
                | "polygon"
                | "filled_polygon"
                | "fill_segments"
                | "uuid"
                | "tstamp"
                | "net"
                | "net_name"
                | "name"
                | "attr"
        ) {
            continue;
        }
        v.push(canon(c, SKIP_DEEP));
    }
    v[3..].sort_by_cached_key(CVal::key);
    CVal::List(v)
}

pub fn canonical(board_fp: &Node) -> FpCanon {
    canonical_lib_frame(&to_lib_frame(board_fp))
}

// ---------------------------------------------------------------------------------------------
// Diff / hash
// ---------------------------------------------------------------------------------------------

fn pad_json(f: &BTreeMap<String, CVal>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in f {
        let j = match (k.as_str(), v) {
            (_, CVal::List(items))
                if items.len() == 2
                    && matches!(items[0], CVal::Atom(_))
                    && !matches!(items[1], CVal::List(_))
                    && k != "position" =>
            {
                items[1].to_json()
            }
            _ => v.to_json(),
        };
        m.insert(k.clone(), j);
    }
    serde_json::Value::Object(m)
}

fn pad_cval(f: &BTreeMap<String, CVal>) -> CVal {
    CVal::List(
        f.iter()
            .map(|(k, v)| CVal::List(vec![CVal::atom(k), v.clone()]))
            .collect(),
    )
}

fn diff_pads(
    local: &[(String, BTreeMap<String, CVal>)],
    library: &[(String, BTreeMap<String, CVal>)],
    out: &mut Vec<PadDiff>,
) {
    type PadRef<'a> = &'a BTreeMap<String, CVal>;
    let mut lk: BTreeMap<&str, Vec<PadRef>> = BTreeMap::new();
    let mut rk: BTreeMap<&str, Vec<PadRef>> = BTreeMap::new();
    for (n, f) in local {
        lk.entry(n).or_default().push(f);
    }
    for (n, f) in library {
        rk.entry(n).or_default().push(f);
    }
    let keys: BTreeSet<&str> = lk.keys().chain(rk.keys()).copied().collect();
    for k in keys {
        let a: Vec<PadRef> = lk.get(k).cloned().unwrap_or_default();
        let b: Vec<PadRef> = rk.get(k).cloned().unwrap_or_default();
        let ac: Vec<CVal> = a.iter().map(|f| pad_cval(f)).collect();
        let bc: Vec<CVal> = b.iter().map(|f| pad_cval(f)).collect();
        let (only_a, only_b) = multiset_diff(&ac, &bc);
        let mut only_b = only_b;
        for i in only_a {
            let pa = a[i];
            // pair with the most similar remaining library pad
            let best = only_b
                .iter()
                .enumerate()
                .min_by_key(|(_, j)| changed_fields(pa, b[**j]).len())
                .map(|(pos, j)| (pos, *j));
            match best {
                Some((pos, j)) => {
                    only_b.remove(pos);
                    out.push(PadDiff {
                        number: k.to_string(),
                        change: "changed".into(),
                        fields: changed_fields(pa, b[j]),
                        library: Some(pad_json(b[j])),
                        local: Some(pad_json(pa)),
                    });
                }
                None => out.push(PadDiff {
                    number: k.to_string(),
                    change: "added".into(),
                    fields: vec![],
                    library: None,
                    local: Some(pad_json(pa)),
                }),
            }
        }
        for j in only_b {
            out.push(PadDiff {
                number: k.to_string(),
                change: "removed".into(),
                fields: vec![],
                library: Some(pad_json(b[j])),
                local: None,
            });
        }
    }
}

fn changed_fields(a: &BTreeMap<String, CVal>, b: &BTreeMap<String, CVal>) -> Vec<String> {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| match (a.get(*k), b.get(*k)) {
            (Some(x), Some(y)) => !x.approx_eq(y),
            _ => true,
        })
        .cloned()
        .collect()
}

pub fn diff(board_fp: &Node, library_fp: &Node) -> FootprintDiff {
    let l = canonical(board_fp);
    // library footprints are already in library frame (no placement)
    let r = canonical_lib_frame(library_fp);
    let mut d = FootprintDiff::default();
    diff_pads(&l.pads, &r.pads, &mut d.pads);

    // graphics per layer
    let mut layers: BTreeSet<&str> = BTreeSet::new();
    layers.extend(l.graphics.iter().map(|g| g.0.as_str()));
    layers.extend(r.graphics.iter().map(|g| g.0.as_str()));
    for layer in layers {
        let a: Vec<CVal> = l
            .graphics
            .iter()
            .filter(|g| g.0 == layer)
            .map(|g| g.1.clone())
            .collect();
        let b: Vec<CVal> = r
            .graphics
            .iter()
            .filter(|g| g.0 == layer)
            .map(|g| g.1.clone())
            .collect();
        let (only_a, only_b) = multiset_diff(&a, &b);
        let mut kinds: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for i in &only_b {
            kinds
                .entry(b[*i].head().unwrap_or("?").to_string())
                .or_default()
                .0 += 1;
        }
        for i in &only_a {
            kinds
                .entry(a[*i].head().unwrap_or("?").to_string())
                .or_default()
                .1 += 1;
        }
        let ln = if layer.is_empty() {
            "(no layer)"
        } else {
            layer
        };
        for (k, (rem, add)) in kinds {
            let ch = rem.min(add);
            if ch > 0 {
                d.graphics.push(format!("{ln}: {ch} {k} changed"));
            }
            if rem > ch {
                d.graphics.push(format!("{ln}: {} {k} removed", rem - ch));
            }
            if add > ch {
                d.graphics.push(format!("{ln}: {} {k} added", add - ch));
            }
        }
    }

    // other
    let keys: BTreeSet<&String> = l.other.keys().chain(r.other.keys()).collect();
    for k in keys {
        let (a, b) = (l.other.get(k), r.other.get(k));
        let same = match (a, b) {
            (Some(x), Some(y)) => x.approx_eq(y),
            (None, None) => true,
            (Some(x), None) | (None, Some(x)) => {
                k == "attr" && matches!(x, CVal::Atom(s) if s == "none")
            }
        };
        if !same {
            let s = |v: Option<&CVal>| v.map(short).unwrap_or_else(|| "absent".into());
            d.other
                .push(format!("{k}: library {}, local {}", s(b), s(a)));
        }
    }
    let (za, zb) = multiset_diff(&l.zones, &r.zones);
    if !za.is_empty() || !zb.is_empty() {
        let ch = za.len().min(zb.len());
        if ch > 0 {
            d.other.push(format!("zones: {ch} changed"));
        }
        if zb.len() > ch {
            d.other.push(format!("zones: {} removed", zb.len() - ch));
        }
        if za.len() > ch {
            d.other.push(format!("zones: {} added", za.len() - ch));
        }
    }
    d
}

fn short(v: &CVal) -> String {
    let s = match v {
        CVal::Atom(a) => a.clone(),
        CVal::Num(x) => super::canon::fmt_num(*x),
        CVal::List(items) if items.len() == 2 && matches!(items[0], CVal::Atom(_)) => {
            match &items[1] {
                CVal::Atom(a) => a.clone(),
                CVal::Num(x) => super::canon::fmt_num(*x),
                o => o.key(),
            }
        }
        _ => v.key(),
    };
    if s.len() > 120 {
        format!("{}...", &s[..117])
    } else {
        s
    }
}

pub fn hash(board_fp: &Node) -> String {
    let c = canonical(board_fp);
    let mut lines: Vec<String> = Vec::new();
    for (n, f) in &c.pads {
        lines.push(format!("pad {} {}", CVal::atom(n).key(), pad_cval(f).key()));
    }
    for (l, g) in &c.graphics {
        lines.push(format!("gr {} {}", CVal::atom(l).key(), g.key()));
    }
    for z in &c.zones {
        lines.push(format!("zone {}", z.key()));
    }
    lines.sort();
    let mut s = String::new();
    for (k, v) in &c.other {
        if k == "attr" && matches!(v, CVal::Atom(a) if a == "none") {
            continue;
        }
        s.push_str(&format!("other {k} {}\n", v.key()));
    }
    for l in lines {
        s.push_str(&l);
        s.push('\n');
    }
    sha256_hex(&s)
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Model {
    path: String,
    offset: [f64; 3],
    scale: [f64; 3],
    rotate: [f64; 3],
}

pub fn normalize_model_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    // ${KICAD9_3DMODEL_DIR} / ${KICAD10_3DMODEL_DIR} / ... -> ${KICAD_3DMODEL_DIR}
    if let Some(rest) = p.strip_prefix("${KICAD")
        && let Some(end) = rest.find("_3DMODEL_DIR}")
        && rest[..end].chars().all(|c| c.is_ascii_digit())
    {
        return format!(
            "${{KICAD_3DMODEL_DIR}}{}",
            &rest[end + "_3DMODEL_DIR}".len()..]
        );
    }
    p
}

fn xyz(n: Option<&Node>, def: f64, scale: f64) -> [f64; 3] {
    let v = n.and_then(|n| n.find("xyz")).map(nums).unwrap_or_default();
    let g = |i: usize| v.get(i).map(|x| x * scale).unwrap_or(def);
    [g(0), g(1), g(2)]
}

fn models(fp: &Node) -> Vec<Model> {
    fp.find_all("model")
        .map(|m| {
            let offset = match (m.find("offset"), m.find("at")) {
                (Some(o), _) => xyz(Some(o), 0.0, 1.0),
                (None, Some(a)) => xyz(Some(a), 0.0, 25.4),
                _ => [0.0; 3],
            };
            Model {
                path: normalize_model_path(m.first_arg().unwrap_or("")),
                offset,
                scale: xyz(m.find("scale"), 1.0, 1.0),
                rotate: xyz(m.find("rotate"), 0.0, 1.0),
            }
        })
        .collect()
}

pub fn models_diff(board_fp: &Node, library_fp: &Node) -> Vec<String> {
    let a = models(board_fp);
    let b = models(library_fp);
    let mut out = Vec::new();
    let mut used = vec![false; b.len()];
    let close3 = |x: &[f64; 3], y: &[f64; 3], ang: bool| {
        x.iter().zip(y).all(|(p, q)| {
            if ang {
                norm_angle(p - q).min(360.0 - norm_angle(p - q)) <= EPS
            } else {
                (p - q).abs() <= EPS
            }
        })
    };
    let f3 = |v: &[f64; 3]| {
        format!(
            "[{}, {}, {}]",
            super::canon::fmt_num(v[0]),
            super::canon::fmt_num(v[1]),
            super::canon::fmt_num(v[2])
        )
    };
    for m in &a {
        let Some(j) = (0..b.len()).find(|&j| !used[j] && b[j].path == m.path) else {
            out.push(format!("model added: {}", m.path));
            continue;
        };
        used[j] = true;
        let l = &b[j];
        if !close3(&m.offset, &l.offset, false) {
            out.push(format!(
                "model {}: offset library {}, local {}",
                m.path,
                f3(&l.offset),
                f3(&m.offset)
            ));
        }
        if !close3(&m.scale, &l.scale, false) {
            out.push(format!(
                "model {}: scale library {}, local {}",
                m.path,
                f3(&l.scale),
                f3(&m.scale)
            ));
        }
        if !close3(&m.rotate, &l.rotate, true) {
            out.push(format!(
                "model {}: rotate library {}, local {}",
                m.path,
                f3(&l.rotate),
                f3(&m.rotate)
            ));
        }
        // Visibility (hide) is intentionally ignored (user decision).
    }
    for (j, l) in b.iter().enumerate() {
        if !used[j] {
            out.push(format!("model removed: {}", l.path));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Conversion to library form
// ---------------------------------------------------------------------------------------------

const STRIP_DEEP: &[&str] = &[
    "uuid",
    "tstamp",
    "net",
    "pinfunction",
    "pintype",
    "locked",
    "filled_polygon",
    "fill_segments",
];
const STRIP_TOP: &[&str] = &[
    "at",
    "path",
    "sheetname",
    "sheetfile",
    "units",
    "group",
    "placed",
    "tedit",
    "component_classes",
];

fn strip_deep(n: &mut Node) {
    n.remove_children(STRIP_DEEP);
    if let Some(items) = n.items_mut() {
        // bare `locked` atoms
        let mut first = true;
        items.retain(|c| {
            let keep = first || !matches!(c, Node::Sym(s) if s == "locked");
            first = false;
            keep
        });
        for c in items.iter_mut().skip(1) {
            if matches!(c, Node::List(_)) && !c.is("model") {
                strip_deep(c);
            }
        }
    }
}

pub fn to_library(board_fp: &Node, name: &str, model_path_map: &[(String, String)]) -> Node {
    let mut n = to_lib_frame(board_fp);
    n.remove_children(STRIP_TOP);
    strip_deep(&mut n);
    // zones in a library footprint: drop net binding
    if let Some(items) = n.items_mut() {
        if items.len() > 1 {
            items[1] = Node::str(name);
        }
        for c in items.iter_mut().skip(1) {
            if c.is("attr")
                && let Some(a) = c.items_mut()
            {
                a.retain(|x| x.atom() != Some("dnp"));
            }
            if c.is("model")
                && let Some(m) = c.items_mut()
                && let Some(p) = m.get(1).and_then(Node::atom).map(String::from)
                && let Some((_, new)) = model_path_map.iter().find(|(old, _)| *old == p)
            {
                m[1] = Node::str(new.clone());
            }
            if c.is("zone") {
                c.remove_children(&["net_name"]);
            }
        }
        // `(locked)` at footprint level as a bare atom
        let mut first = true;
        items.retain(|c| {
            let keep = first || !matches!(c, Node::Sym(s) if s == "locked" || s == "placed");
            first = false;
            keep
        });
    }
    n
}

pub fn file(fp: &Node, version: crate::types::KicadVersion) -> String {
    use crate::types::KicadVersion;
    let (ver, gv) = match version {
        KicadVersion::V9 => ("20241229", "9.0"),
        KicadVersion::V10 => ("20260206", "10.0"),
    };
    let mut fp = fp.clone();
    fp.remove_children(&["version", "generator", "generator_version"]);
    let items = fp.items_mut().expect("footprint list");
    let header = [
        Node::list("version", vec![Node::sym(ver)]),
        Node::list("generator", vec![Node::str("pcbnew")]),
        Node::list("generator_version", vec![Node::str(gv)]),
    ];
    let at = 2.min(items.len());
    for (i, h) in header.into_iter().enumerate() {
        items.insert(at + i, h);
    }
    fp.to_kicad_string()
}

pub fn debug_dump(fp: &Node) -> String {
    let c = canonical(fp);
    let mut s = String::new();
    for (n, f) in &c.pads {
        s.push_str(&format!("pad {n} {}\n", pad_cval(f).key()));
    }
    for (l, g) in &c.graphics {
        s.push_str(&format!("gr {l} {}\n", g.key()));
    }
    for (k, v) in &c.other {
        s.push_str(&format!("other {k} {}\n", v.key()));
    }
    for z in &c.zones {
        s.push_str(&format!("zone {}\n", z.key()));
    }
    s
}

#[cfg(test)]
mod legacy_arc_tests {
    use super::*;
    #[test]
    fn legacy_arc_matches_kicad_upgrade() {
        // `kicad-cli fp upgrade` turns (start 16 8.59) (end 16 8.49) (angle -180) into
        // (start 16 8.49) (mid 16.1 8.59) (end 16 8.69).
        let (s, m, e) = legacy_arc((16.0, 8.59), (16.0, 8.49), -180.0);
        let close =
            |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9;
        assert!(
            close(s, (16.0, 8.49)) && close(m, (16.1, 8.59)) && close(e, (16.0, 8.69)),
            "{s:?} {m:?} {e:?}"
        );
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::is_field_placeholder;
    #[test]
    fn placeholders() {
        assert!(is_field_placeholder("${REFERENCE}"));
        assert!(is_field_placeholder(" ${VALUE} ${REFERENCE} "));
        assert!(!is_field_placeholder("+"));
        assert!(is_field_placeholder("%R"));
        assert!(!is_field_placeholder("Pin ${REFERENCE}"));
        assert!(!is_field_placeholder(""));
    }
}
