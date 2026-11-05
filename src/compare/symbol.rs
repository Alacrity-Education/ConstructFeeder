//! Canonical form, diff and hash of schematic symbols.

use super::canon::{CVal, canon, flag_or, json_num, multiset_diff, norm_angle, nums, sha256_hex};
use super::{PinDiff, SymbolDiff};
use crate::sexpr::Node;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Pin {
    pub number: String,
    pub name: String,
    pub etype: String,
    pub shape: String,
    pub x: f64,
    pub y: f64,
    pub angle: f64,
    pub length: f64,
    pub hidden: bool,
    pub alternates: Vec<(String, String, String)>,
}

impl Pin {
    fn parse(n: &Node) -> Pin {
        let at = n.find("at").map(nums).unwrap_or_default();
        let g = |i: usize| at.get(i).copied().unwrap_or(0.0);
        let mut alternates: Vec<(String, String, String)> = n
            .find_all("alternate")
            .map(|a| {
                (
                    a.str_at(1).unwrap_or("").to_string(),
                    a.str_at(2).unwrap_or("").to_string(),
                    a.str_at(3).unwrap_or("").to_string(),
                )
            })
            .collect();
        alternates.sort();
        let name = n.find("name").and_then(Node::first_arg).unwrap_or("");
        Pin {
            number: n
                .find("number")
                .and_then(Node::first_arg)
                .unwrap_or("")
                .to_string(),
            name: if name == "~" {
                String::new()
            } else {
                name.to_string()
            },
            etype: n.str_at(1).unwrap_or("").to_string(),
            shape: n.str_at(2).unwrap_or("line").to_string(),
            x: g(0),
            y: g(1),
            angle: norm_angle(g(2)),
            length: n
                .find("length")
                .map(nums)
                .and_then(|v| v.first().copied())
                .unwrap_or(2.54),
            hidden: flag_or(n, "hide", false),
            alternates,
        }
    }

    fn key_id(&self) -> &str {
        if self.number.is_empty() {
            &self.name
        } else {
            &self.number
        }
    }

    fn cval(&self) -> CVal {
        CVal::List(vec![
            CVal::atom("pin"),
            CVal::atom(&self.number),
            CVal::atom(&self.name),
            CVal::atom(&self.etype),
            CVal::atom(&self.shape),
            CVal::Num(self.x),
            CVal::Num(self.y),
            CVal::Num(self.angle),
            CVal::Num(self.length),
            CVal::atom(if self.hidden { "hidden" } else { "visible" }),
            CVal::List(
                self.alternates
                    .iter()
                    .map(|(a, b, c)| CVal::List(vec![CVal::atom(a), CVal::atom(b), CVal::atom(c)]))
                    .collect(),
            ),
        ])
    }

    fn changed_fields(&self, o: &Pin) -> Vec<String> {
        let close = |a: f64, b: f64| (a - b).abs() <= super::canon::EPS * 1.0001;
        let mut f = Vec::new();
        if !close(self.x, o.x) || !close(self.y, o.y) {
            f.push("position");
        }
        if !close(norm_angle(self.angle - o.angle), 0.0) {
            f.push("orientation");
        }
        if !close(self.length, o.length) {
            f.push("length");
        }
        if self.etype != o.etype {
            f.push("type");
        }
        if self.shape != o.shape {
            f.push("shape");
        }
        if self.name != o.name {
            f.push("name");
        }
        if self.hidden != o.hidden {
            f.push("hidden");
        }
        if self.alternates != o.alternates {
            f.push("alternates");
        }
        f.into_iter().map(String::from).collect()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "number": self.number,
            "name": self.name,
            "at": [json_num(self.x), json_num(self.y), json_num(self.angle)],
            "length": json_num(self.length),
            "type": self.etype,
            "shape": self.shape,
            "hidden": self.hidden,
            "alternates": self.alternates.iter().map(|(a, b, c)| serde_json::json!([a, b, c])).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Unit {
    pub pins: Vec<Pin>,
    pub graphics: Vec<CVal>,
}

#[derive(Clone, Debug, Default)]
pub struct SymCanon {
    pub attrs: Vec<(String, CVal)>,
    /// (unit, style) -> content
    pub units: BTreeMap<(u32, u32), Unit>,
}

/// `Name_1_2` -> (1, 2).
pub fn unit_suffix(name: &str) -> Option<(u32, u32)> {
    let (rest, style) = name.rsplit_once('_')?;
    let (_, unit) = rest.rsplit_once('_')?;
    Some((unit.parse().ok()?, style.parse().ok()?))
}

fn stroke(n: &Node) -> CVal {
    let s = n.find("stroke");
    let width = s
        .and_then(|s| s.find("width"))
        .map(nums)
        .and_then(|v| v.first().copied())
        .unwrap_or(0.0);
    let ty = s.and_then(|s| s.value("type")).unwrap_or("default");
    let mut v = vec![CVal::atom("stroke"), CVal::Num(width), CVal::atom(ty)];
    if let Some(c) = s.and_then(|s| s.find("color")) {
        let cn = nums(c);
        if cn.iter().any(|x| *x != 0.0) {
            v.push(CVal::List(cn.into_iter().map(CVal::Num).collect()));
        }
    }
    CVal::List(v)
}

fn fill(n: &Node) -> CVal {
    let f = n.find("fill");
    let ty = f.and_then(|f| f.value("type")).unwrap_or("none");
    let mut v = vec![CVal::atom("fill"), CVal::atom(ty)];
    if ty == "color"
        && let Some(c) = f.and_then(|f| f.find("color"))
    {
        v.push(CVal::List(nums(c).into_iter().map(CVal::Num).collect()));
    }
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

fn pts(n: &Node) -> Vec<(f64, f64)> {
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

/// Open path: direction does not matter.
pub fn canon_path(mut p: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let fwd: Vec<_> = p.iter().map(pt_key).collect();
    let rev: Vec<_> = fwd.iter().rev().copied().collect();
    if rev < fwd {
        p.reverse();
    }
    p
}

fn graphic(n: &Node) -> Option<CVal> {
    let h = n.head()?;
    let common = || vec![stroke(n), fill(n)];
    let v = match h {
        "polyline" | "bezier" => {
            let p = canon_path(pts(n));
            let mut v = vec![CVal::atom(h), CVal::List(p.into_iter().map(ptv).collect())];
            v.extend(common());
            v
        }
        "rectangle" => {
            let (a, b) = (pt(n.find("start")), pt(n.find("end")));
            let mut v = vec![
                CVal::atom(h),
                ptv((a.0.min(b.0), a.1.min(b.1))),
                ptv((a.0.max(b.0), a.1.max(b.1))),
            ];
            if let Some(r) = n.find("radius") {
                v.push(CVal::list(
                    "radius",
                    nums(r).into_iter().map(CVal::Num).collect(),
                ));
            }
            v.extend(common());
            v
        }
        "circle" => {
            let r = n
                .find("radius")
                .map(nums)
                .and_then(|v| v.first().copied())
                .unwrap_or(0.0);
            let mut v = vec![CVal::atom(h), ptv(pt(n.find("center"))), CVal::Num(r)];
            v.extend(common());
            v
        }
        "arc" => {
            let (mut a, m, mut b) = (pt(n.find("start")), pt(n.find("mid")), pt(n.find("end")));
            if pt_key(&b) < pt_key(&a) {
                std::mem::swap(&mut a, &mut b);
            }
            let mut v = vec![CVal::atom(h), ptv(a), ptv(m), ptv(b)];
            v.extend(common());
            v
        }
        "text" => {
            let at = n.find("at").map(nums).unwrap_or_default();
            let g = |i: usize| at.get(i).copied().unwrap_or(0.0);
            let eff = n
                .find("effects")
                .map(|e| canon(e, &["href"]))
                .unwrap_or(CVal::atom("-"));
            vec![
                CVal::atom(h),
                CVal::atom(n.first_arg().unwrap_or("")),
                CVal::Num(g(0)),
                CVal::Num(g(1)),
                CVal::Num(norm_angle(g(2))),
                eff,
            ]
        }
        "pin" | "property" | "unit_name" => return None,
        _ => {
            let c = canon(n, &["uuid"]);
            match c {
                CVal::List(v) => v,
                _ => return None,
            }
        }
    };
    Some(CVal::List(v))
}

const IGNORED_TOP: &[&str] = &[
    "property",
    "symbol",
    "extends",
    "pin_names",
    "pin_numbers",
    "power",
    "exclude_from_sim",
    "in_bom",
    "on_board",
    "in_pos_files",
    "duplicate_pin_numbers_are_jumpers",
    "embedded_fonts",
    "embedded_files",
    "jumper_pin_groups",
    "body_styles",
    "unit_name",
    "uuid",
];

pub fn canonical(sym: &Node) -> SymCanon {
    let mut attrs: Vec<(String, CVal)> = Vec::new();
    let b = |x: bool| CVal::atom(if x { "yes" } else { "no" });
    let power = match sym.find("power") {
        None => "none",
        Some(p) => match p.first_arg() {
            Some("local") => "local",
            _ => "global",
        },
    };
    attrs.push(("power".into(), CVal::atom(power)));
    let pn = sym.find("pin_names");
    let offset = pn
        .and_then(|p| p.find("offset"))
        .map(nums)
        .and_then(|v| v.first().copied())
        .unwrap_or(0.508);
    attrs.push(("pin_names offset".into(), CVal::Num(offset)));
    attrs.push((
        "pin_names hide".into(),
        b(pn.is_some_and(|p| flag_or(p, "hide", false))),
    ));
    attrs.push((
        "pin_numbers hide".into(),
        b(sym
            .find("pin_numbers")
            .is_some_and(|p| flag_or(p, "hide", false))),
    ));
    for (name, def) in [
        ("exclude_from_sim", false),
        ("in_bom", true),
        ("on_board", true),
        ("in_pos_files", true),
        ("duplicate_pin_numbers_are_jumpers", false),
        ("embedded_fonts", false),
    ] {
        attrs.push((name.into(), b(flag_or(sym, name, def))));
    }
    let mut groups: BTreeSet<Vec<String>> = BTreeSet::new();
    if let Some(j) = sym.find("jumper_pin_groups") {
        for g in j.args() {
            let mut v: Vec<String> = match g {
                Node::List(_) => g
                    .items()
                    .iter()
                    .filter_map(Node::atom)
                    .map(String::from)
                    .collect(),
                _ => continue,
            };
            v.sort();
            groups.insert(v);
        }
    }
    attrs.push((
        "jumper_pin_groups".into(),
        CVal::List(
            groups
                .into_iter()
                .map(|g| CVal::List(g.iter().map(|s| CVal::atom(s)).collect()))
                .collect(),
        ),
    ));
    // Anything else at top level we don't know: compare generically.
    for c in sym.children() {
        let h = c.head().unwrap_or("");
        if !IGNORED_TOP.contains(&h) {
            attrs.push((h.to_string(), canon(c, &["uuid"])));
        }
    }

    let mut raw: BTreeMap<(u32, u32), Unit> = BTreeMap::new();
    let mut add_items = |key: (u32, u32), node: &Node| {
        let u = raw.entry(key).or_default();
        for c in node.children() {
            if c.is("pin") {
                u.pins.push(Pin::parse(c));
            } else if let Some(g) = graphic(c) {
                u.graphics.push(g);
            }
        }
    };
    for sub in sym.find_all("symbol") {
        let key = sub.first_arg().and_then(unit_suffix).unwrap_or((0, 0));
        add_items(key, sub);
    }
    // Pins/graphics directly at top level (unusual) count as common.
    let mut top = Unit::default();
    for c in sym.children() {
        if c.is("pin") {
            top.pins.push(Pin::parse(c));
        }
    }
    if !top.pins.is_empty() {
        raw.entry((0, 0)).or_default().pins.extend(top.pins);
    }

    // Normalize: single-unit symbols -> unit 0 merged into 1; no de Morgan -> style 0 merged into 1.
    let units: BTreeSet<u32> = raw.keys().map(|k| k.0).filter(|&u| u != 0).collect();
    let styles: BTreeSet<u32> = raw.keys().map(|k| k.1).filter(|&s| s != 0).collect();
    let merge_units = units.len() <= 1;
    let merge_styles = styles.len() <= 1;
    let mut out: BTreeMap<(u32, u32), Unit> = BTreeMap::new();
    for ((u, s), unit) in raw {
        let u = if merge_units && u == 0 {
            units.iter().next().copied().unwrap_or(1)
        } else {
            u
        };
        let s = if merge_styles && s == 0 {
            styles.iter().next().copied().unwrap_or(1)
        } else {
            s
        };
        let e = out.entry((u, s)).or_default();
        e.pins.extend(unit.pins);
        e.graphics.extend(unit.graphics);
    }
    out.retain(|_, u| !u.pins.is_empty() || !u.graphics.is_empty());
    SymCanon { attrs, units: out }
}

fn attr_str(c: &CVal) -> String {
    match c {
        CVal::Atom(a) => a.clone(),
        CVal::Num(x) => super::canon::fmt_num(*x),
        _ => c.key(),
    }
}

fn summarize(prefix: &str, removed: &[&CVal], added: &[&CVal]) -> Vec<String> {
    let mut kinds: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in removed {
        kinds
            .entry(r.head().unwrap_or("?").to_string())
            .or_default()
            .0 += 1;
    }
    for a in added {
        kinds
            .entry(a.head().unwrap_or("?").to_string())
            .or_default()
            .1 += 1;
    }
    let mut out = Vec::new();
    for (k, (r, a)) in kinds {
        let changed = r.min(a);
        if changed > 0 {
            out.push(format!("{prefix}{changed} {k} changed"));
        }
        if r > changed {
            out.push(format!("{prefix}{} {k} removed", r - changed));
        }
        if a > changed {
            out.push(format!("{prefix}{} {k} added", a - changed));
        }
    }
    out
}

pub fn diff(local: &Node, library: &Node) -> SymbolDiff {
    let l = canonical(local);
    let r = canonical(library);
    let mut d = SymbolDiff::default();
    // attributes
    let lm: BTreeMap<&str, &CVal> = l.attrs.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let rm: BTreeMap<&str, &CVal> = r.attrs.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let keys: BTreeSet<&str> = lm.keys().chain(rm.keys()).copied().collect();
    for k in keys {
        match (lm.get(k), rm.get(k)) {
            (Some(a), Some(b)) if a.approx_eq(b) => {}
            (a, b) => d.other.push(format!(
                "{k}: library {}, local {}",
                b.map(|v| attr_str(v)).unwrap_or_else(|| "absent".into()),
                a.map(|v| attr_str(v)).unwrap_or_else(|| "absent".into())
            )),
        }
    }
    // unit structure
    let lu: BTreeSet<u32> = l.units.keys().map(|k| k.0).collect();
    let ru: BTreeSet<u32> = r.units.keys().map(|k| k.0).collect();
    if lu != ru {
        d.other
            .push(format!("units: library {:?}, local {:?}", ru, lu));
    }
    let ls: BTreeSet<u32> = l.units.keys().map(|k| k.1).collect();
    let rs: BTreeSet<u32> = r.units.keys().map(|k| k.1).collect();
    if ls != rs {
        d.other
            .push(format!("body styles: library {:?}, local {:?}", rs, ls));
    }

    let empty = Unit::default();
    let all: BTreeSet<(u32, u32)> = l.units.keys().chain(r.units.keys()).copied().collect();
    for key in all {
        let lu = l.units.get(&key).unwrap_or(&empty);
        let ru = r.units.get(&key).unwrap_or(&empty);
        diff_pins(key.0, &lu.pins, &ru.pins, &mut d.pins);
        let (only_l, only_r) = multiset_diff(&lu.graphics, &ru.graphics);
        if !only_l.is_empty() || !only_r.is_empty() {
            let prefix = if key.1 > 1 {
                format!("unit {} style {}: ", key.0, key.1)
            } else {
                format!("unit {}: ", key.0)
            };
            let removed: Vec<&CVal> = only_r.iter().map(|&i| &ru.graphics[i]).collect();
            let added: Vec<&CVal> = only_l.iter().map(|&i| &lu.graphics[i]).collect();
            d.graphics.extend(summarize(&prefix, &removed, &added));
        }
    }
    d
}

fn diff_pins(unit: u32, local: &[Pin], library: &[Pin], out: &mut Vec<PinDiff>) {
    let mut lk: BTreeMap<&str, Vec<&Pin>> = BTreeMap::new();
    let mut rk: BTreeMap<&str, Vec<&Pin>> = BTreeMap::new();
    for p in local {
        lk.entry(p.key_id()).or_default().push(p);
    }
    for p in library {
        rk.entry(p.key_id()).or_default().push(p);
    }
    let keys: BTreeSet<&str> = lk.keys().chain(rk.keys()).copied().collect();
    for k in keys {
        let mut a: Vec<&Pin> = lk.get(k).cloned().unwrap_or_default();
        let mut b: Vec<&Pin> = rk.get(k).cloned().unwrap_or_default();
        // drop exact matches
        a.retain(|p| {
            if let Some(pos) = b.iter().position(|q| p.cval().approx_eq(&q.cval())) {
                b.remove(pos);
                false
            } else {
                true
            }
        });
        let n = a.len().max(b.len());
        for i in 0..n {
            let (pa, pb) = (a.get(i), b.get(i));
            let (change, fields) = match (pa, pb) {
                (Some(x), Some(y)) => ("changed", x.changed_fields(y)),
                (Some(_), None) => ("added", vec![]),
                (None, Some(_)) => ("removed", vec![]),
                _ => unreachable!(),
            };
            out.push(PinDiff {
                number: k.to_string(),
                unit,
                change: change.into(),
                fields,
                library: pb.map(|p| p.json()),
                local: pa.map(|p| p.json()),
            });
        }
    }
}

pub fn hash(sym: &Node) -> String {
    let c = canonical(sym);
    let mut s = String::new();
    for (k, v) in &c.attrs {
        s.push_str(k);
        s.push('=');
        s.push_str(&v.key());
        s.push('\n');
    }
    for ((u, st), unit) in &c.units {
        s.push_str(&format!("unit {u} {st}\n"));
        let mut pins: Vec<String> = unit.pins.iter().map(|p| p.cval().key()).collect();
        pins.sort();
        let mut gr: Vec<String> = unit.graphics.iter().map(CVal::key).collect();
        gr.sort();
        for p in pins.iter().chain(gr.iter()) {
            s.push_str(p);
            s.push('\n');
        }
    }
    sha256_hex(&s)
}

pub fn all_pins(sym: &Node) -> Vec<Pin> {
    let mut v = Vec::new();
    sym.walk(&mut |n| {
        if n.is("pin") && n.find("number").is_some() {
            v.push(Pin::parse(n));
        }
    });
    v
}

pub fn is_power(sym: &Node) -> bool {
    if sym.find("power").is_some() {
        return true;
    }
    let pins = all_pins(sym);
    !pins.is_empty()
        && sym
            .property("Reference")
            .is_some_and(|r| r.starts_with("#PWR"))
        && pins.iter().all(|p| p.etype == "power_in" && p.hidden)
}

pub fn is_graphic_only(sym: &Node) -> bool {
    all_pins(sym).is_empty()
}

pub fn to_library(embedded: &Node, name: &str) -> Node {
    let mut n = embedded.clone();
    if let Some(items) = n.items_mut() {
        if items.len() > 1 {
            items[1] = Node::str(name);
        }
        for c in items.iter_mut().skip(2) {
            if c.is("symbol")
                && let Some(sub) = c.items_mut()
                && let Some(old) = sub.get(1).and_then(Node::atom).map(String::from)
            {
                let new = match unit_suffix(&old) {
                    Some((u, s)) => format!("{name}_{u}_{s}"),
                    None => format!("{name}_0_1"),
                };
                sub[1] = Node::str(new);
            }
        }
    }
    n
}

pub fn library_file(symbols: &[Node], version: crate::types::KicadVersion) -> String {
    use crate::types::KicadVersion;
    let (ver, gv) = match version {
        KicadVersion::V9 => ("20241209", "9.0"),
        KicadVersion::V10 => ("20251024", "10.0"),
    };
    let mut items = vec![
        Node::sym("kicad_symbol_lib"),
        Node::list("version", vec![Node::sym(ver)]),
        Node::list("generator", vec![Node::str("kicad_symbol_editor")]),
        Node::list("generator_version", vec![Node::str(gv)]),
    ];
    items.extend(symbols.iter().cloned());
    Node::List(items).to_kicad_string()
}

pub fn debug_dump(sym: &Node) -> String {
    let c = canonical(sym);
    let mut s = String::new();
    for (k, v) in &c.attrs {
        s.push_str(&format!("attr {k} {}\n", v.key()));
    }
    for ((u, st), unit) in &c.units {
        for p in &unit.pins {
            s.push_str(&format!("{u}/{st} {}\n", p.cval().key()));
        }
        for g in &unit.graphics {
            s.push_str(&format!("{u}/{st} {}\n", g.key()));
        }
    }
    s
}
