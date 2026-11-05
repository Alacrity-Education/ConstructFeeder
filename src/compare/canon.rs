//! Canonical value trees shared by symbol and footprint comparison.
//!
//! A [`CVal`] is a normalized S-expression: numbers are parsed as `f64`, booleans unified to
//! `yes`/`no`, quoting is dropped. Equality is tolerant (1e-4) and hashing uses a deterministic
//! serialization with numbers rounded to 4 decimals.

use crate::sexpr::Node;
use sha2::{Digest, Sha256};

pub const EPS: f64 = 1e-4;

#[derive(Clone, Debug)]
pub enum CVal {
    Num(f64),
    Atom(String),
    List(Vec<CVal>),
}

impl CVal {
    pub fn atom(s: &str) -> CVal {
        CVal::Atom(s.to_string())
    }

    pub fn list(head: &str, mut rest: Vec<CVal>) -> CVal {
        rest.insert(0, CVal::atom(head));
        CVal::List(rest)
    }

    pub fn head(&self) -> Option<&str> {
        match self {
            CVal::List(v) => match v.first() {
                Some(CVal::Atom(s)) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn approx_eq(&self, o: &CVal) -> bool {
        match (self, o) {
            (CVal::Num(a), CVal::Num(b)) => (a - b).abs() <= EPS * 1.0001,
            (CVal::Atom(a), CVal::Atom(b)) => a == b,
            (CVal::List(a), CVal::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.approx_eq(y))
            }
            _ => false,
        }
    }

    /// Deterministic serialization (numbers rounded to 4 decimals).
    pub fn key(&self) -> String {
        let mut s = String::new();
        self.write_key(&mut s);
        s
    }

    fn write_key(&self, out: &mut String) {
        match self {
            CVal::Num(x) => out.push_str(&fmt_num(*x)),
            CVal::Atom(a) => {
                out.push('"');
                for c in a.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
            CVal::List(v) => {
                out.push('(');
                for (i, c) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    c.write_key(out);
                }
                out.push(')');
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            CVal::Num(x) => json_num(*x),
            CVal::Atom(a) => serde_json::Value::String(a.clone()),
            CVal::List(v) => serde_json::Value::Array(v.iter().map(CVal::to_json).collect()),
        }
    }
}

pub fn json_num(x: f64) -> serde_json::Value {
    let r = round4(x);
    serde_json::Number::from_f64(r)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

pub fn round4(x: f64) -> f64 {
    let r = (x * 10000.0).round() / 10000.0;
    if r == 0.0 { 0.0 } else { r }
}

pub fn fmt_num(x: f64) -> String {
    let r = round4(x);
    let mut s = format!("{r:.4}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// Number formatting for writing back into KiCad files (up to 6 decimals, trimmed).
pub fn fmt_num_full(x: f64) -> String {
    let mut s = format!("{x:.6}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// Normalize an angle in degrees to [0, 360), snapping values within EPS of 360 to 0.
pub fn norm_angle(a: f64) -> f64 {
    let mut r = a % 360.0;
    if r < 0.0 {
        r += 360.0;
    }
    if (360.0 - r).abs() <= EPS || r.abs() <= EPS {
        r = 0.0;
    }
    r
}

pub fn parse_num(s: &str) -> Option<f64> {
    let first = s.as_bytes().first()?;
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.')) {
        return None;
    }
    s.parse::<f64>().ok().filter(|x| x.is_finite())
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Bare flags that KiCad 9 wrote as atoms and KiCad 10 writes as `(flag yes)`.
const BARE_FLAGS: &[&str] = &[
    "hide",
    "bold",
    "italic",
    "locked",
    "knockout",
    "mirror_text",
];

/// Generic canonicalization of a node.
/// * `skip`: child list heads to drop at any depth.
/// * Booleans unified; bare flags become `(flag yes)`; `(x no)` children dropped (absent == no).
/// * Child lists sorted by key except inside order-sensitive heads (`pts`).
pub fn canon(n: &Node, skip: &[&str]) -> CVal {
    match n {
        Node::Sym(s) => canon_atom(s),
        Node::Str(s) => CVal::Atom(s.clone()),
        Node::List(items) => {
            let head = n.head().unwrap_or("");
            let mut atoms = Vec::new();
            let mut lists = Vec::new();
            for (i, c) in items.iter().enumerate() {
                if i == 0 {
                    atoms.push(match c {
                        Node::Sym(s) | Node::Str(s) => CVal::Atom(s.clone()),
                        _ => canon(c, skip),
                    });
                    continue;
                }
                match c {
                    Node::List(_) => {
                        if c.head().is_some_and(|h| skip.contains(&h)) {
                            continue;
                        }
                        let cv = canon(c, skip);
                        if is_no_flag(&cv) {
                            continue;
                        }
                        lists.push(cv);
                    }
                    Node::Sym(s) if head != "justify" && BARE_FLAGS.contains(&s.as_str()) => {
                        if !skip.contains(&s.as_str()) {
                            lists.push(CVal::list(s, vec![CVal::atom("yes")]));
                        }
                    }
                    _ => atoms.push(canon(c, skip)),
                }
            }
            if head != "pts" {
                lists.sort_by_cached_key(CVal::key);
            }
            if head == "justify" && atoms.len() > 1 {
                // justify atoms are a set
                atoms[1..].sort_by_cached_key(CVal::key);
            }
            atoms.extend(lists);
            CVal::List(atoms)
        }
    }
}

fn canon_atom(s: &str) -> CVal {
    if let Some(x) = parse_num(s) {
        return CVal::Num(x);
    }
    match s {
        "yes" | "true" => CVal::atom("yes"),
        "no" | "false" => CVal::atom("no"),
        _ => CVal::Atom(s.to_string()),
    }
}

/// `(x no)` — a flag explicitly set to its (false) default.
fn is_no_flag(c: &CVal) -> bool {
    match c {
        CVal::List(v) => v.len() == 2 && matches!(&v[1], CVal::Atom(a) if a == "no"),
        _ => false,
    }
}

/// Numeric args of a node, e.g. `(at 1 2 90)` -> [1, 2, 90].
pub fn nums(n: &Node) -> Vec<f64> {
    n.args()
        .iter()
        .filter_map(|a| a.atom().and_then(parse_num))
        .collect()
}

/// Boolean flag value with default.
pub fn flag_or(n: &Node, name: &str, default: bool) -> bool {
    n.flag(name).unwrap_or(default)
}

/// Multiset difference with tolerance. Returns (only_in_a, only_in_b) indices.
/// Exact (rounded-key) matches first, then tolerant pairing of the leftovers.
pub fn multiset_diff(a: &[CVal], b: &[CVal]) -> (Vec<usize>, Vec<usize>) {
    use std::collections::HashMap;
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, x) in b.iter().enumerate() {
        by_key.entry(x.key()).or_default().push(i);
    }
    let mut used_b = vec![false; b.len()];
    let mut left_a = Vec::new();
    for (i, x) in a.iter().enumerate() {
        match by_key.get_mut(&x.key()).and_then(Vec::pop) {
            Some(j) => used_b[j] = true,
            None => left_a.push(i),
        }
    }
    let mut left_b: Vec<usize> = (0..b.len()).filter(|&j| !used_b[j]).collect();
    let mut only_a = Vec::new();
    for i in left_a {
        if let Some(pos) = left_b.iter().position(|&j| a[i].approx_eq(&b[j])) {
            left_b.remove(pos);
        } else {
            only_a.push(i);
        }
    }
    (only_a, left_b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexpr::parse;

    #[test]
    fn numbers_and_flags() {
        let a = canon(
            &parse("(text \"x\" (at 0 0.0 90) (effects (font (size 1.27 1.27) bold) hide))")
                .unwrap(),
            &[],
        );
        let b = canon(&parse("(text \"x\" (at 0.00001 0 90.0) (effects (hide yes) (font (bold yes) (size 1.27 1.270))))").unwrap(), &[]);
        assert!(a.approx_eq(&b), "{} vs {}", a.key(), b.key());
        assert_eq!(fmt_num(-0.00001), "0");
        assert_eq!(fmt_num(1.5), "1.5");
        assert_eq!(norm_angle(-90.0), 270.0);
        assert_eq!(norm_angle(359.99999), 0.0);
    }
}
