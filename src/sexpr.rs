//! Minimal S-expression parser/serializer for KiCad files.
//!
//! KiCad files are a single top-level list. Atoms are either bare symbols
//! (`yes`, `1.27`, `passive`) or double-quoted strings with backslash escapes.
//! We keep the distinction so serialization round-trips the quoting.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    List(Vec<Node>),
    /// Bare (unquoted) atom.
    Sym(String),
    /// Quoted string (unescaped value).
    Str(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unexpected end of input")]
    Eof,
    #[error("unexpected ')' at byte {0}")]
    UnexpectedClose(usize),
    #[error("trailing content at byte {0}")]
    Trailing(usize),
}

pub fn parse(text: &str) -> Result<Node, ParseError> {
    let b = text.as_bytes();
    let mut pos = 0usize;
    skip_ws(b, &mut pos);
    let node = parse_node(text, b, &mut pos)?;
    skip_ws(b, &mut pos);
    if pos < b.len() {
        return Err(ParseError::Trailing(pos));
    }
    Ok(node)
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && b[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
}

fn parse_node(text: &str, b: &[u8], pos: &mut usize) -> Result<Node, ParseError> {
    // Iterative to survive very deep / very large boards.
    let mut stack: Vec<Vec<Node>> = Vec::new();
    loop {
        skip_ws(b, pos);
        if *pos >= b.len() {
            return Err(ParseError::Eof);
        }
        let node = match b[*pos] {
            b'(' => {
                *pos += 1;
                stack.push(Vec::new());
                continue;
            }
            b')' => {
                let Some(items) = stack.pop() else {
                    return Err(ParseError::UnexpectedClose(*pos));
                };
                *pos += 1;
                Node::List(items)
            }
            b'"' => {
                *pos += 1;
                let mut s = String::new();
                let start = *pos;
                let mut seg = start;
                loop {
                    if *pos >= b.len() {
                        return Err(ParseError::Eof);
                    }
                    match b[*pos] {
                        b'"' => {
                            s.push_str(&text[seg..*pos]);
                            *pos += 1;
                            break;
                        }
                        b'\\' => {
                            s.push_str(&text[seg..*pos]);
                            *pos += 1;
                            if *pos >= b.len() {
                                return Err(ParseError::Eof);
                            }
                            // escaped char may be multibyte; take whole char
                            let ch = text[*pos..].chars().next().unwrap();
                            match ch {
                                'n' => s.push('\n'),
                                't' => s.push('\t'),
                                'r' => s.push('\r'),
                                c => s.push(c),
                            }
                            *pos += ch.len_utf8();
                            seg = *pos;
                        }
                        _ => *pos += 1,
                    }
                }
                Node::Str(s)
            }
            _ => {
                let start = *pos;
                while *pos < b.len()
                    && !b[*pos].is_ascii_whitespace()
                    && b[*pos] != b'('
                    && b[*pos] != b')'
                {
                    *pos += 1;
                }
                Node::Sym(text[start..*pos].to_string())
            }
        };
        match stack.last_mut() {
            Some(top) => top.push(node),
            None => return Ok(node),
        }
    }
}

impl Node {
    pub fn list(head: &str, args: Vec<Node>) -> Node {
        let mut v = Vec::with_capacity(args.len() + 1);
        v.push(Node::Sym(head.to_string()));
        v.extend(args);
        Node::List(v)
    }
    pub fn sym(s: impl Into<String>) -> Node {
        Node::Sym(s.into())
    }
    pub fn str(s: impl Into<String>) -> Node {
        Node::Str(s.into())
    }

    /// Atom value (Sym or Str) or None for lists.
    pub fn atom(&self) -> Option<&str> {
        match self {
            Node::Sym(s) | Node::Str(s) => Some(s),
            Node::List(_) => None,
        }
    }

    /// Head symbol of a list, e.g. `symbol` for `(symbol "R" ...)`.
    pub fn head(&self) -> Option<&str> {
        match self {
            Node::List(v) => match v.first() {
                Some(Node::Sym(s)) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is(&self, head: &str) -> bool {
        self.head() == Some(head)
    }

    /// All items of a list (including head). Empty slice for atoms.
    pub fn items(&self) -> &[Node] {
        match self {
            Node::List(v) => v,
            _ => &[],
        }
    }

    pub fn items_mut(&mut self) -> Option<&mut Vec<Node>> {
        match self {
            Node::List(v) => Some(v),
            _ => None,
        }
    }

    /// Items after the head.
    pub fn args(&self) -> &[Node] {
        let it = self.items();
        if it.is_empty() { it } else { &it[1..] }
    }

    /// Atom at index `i` of the list (0 = head).
    pub fn str_at(&self, i: usize) -> Option<&str> {
        self.items().get(i).and_then(Node::atom)
    }

    /// First argument as atom: `(lib_id "Device:R")` -> "Device:R".
    pub fn first_arg(&self) -> Option<&str> {
        self.str_at(1)
    }

    /// First direct child list with the given head.
    pub fn find(&self, head: &str) -> Option<&Node> {
        self.args().iter().find(|n| n.is(head))
    }

    pub fn find_mut(&mut self, head: &str) -> Option<&mut Node> {
        self.items_mut()?.iter_mut().skip(1).find(|n| n.is(head))
    }

    /// All direct child lists with the given head.
    pub fn find_all<'a>(&'a self, head: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.args().iter().filter(move |n| n.is(head))
    }

    /// Child lists (skipping atoms).
    pub fn children(&self) -> impl Iterator<Item = &Node> {
        self.args().iter().filter(|n| matches!(n, Node::List(_)))
    }

    /// `(name value ...)` -> value.
    pub fn value(&self, head: &str) -> Option<&str> {
        self.find(head).and_then(Node::first_arg)
    }

    /// `(property "Key" "Value" ...)` lookup.
    pub fn property(&self, key: &str) -> Option<&str> {
        self.find_all("property")
            .find(|p| p.str_at(1) == Some(key))
            .and_then(|p| p.str_at(2))
    }

    /// Boolean flag in either modern `(hide yes)` or legacy bare-atom `hide` form.
    /// Returns None if absent.
    pub fn flag(&self, name: &str) -> Option<bool> {
        for n in self.args() {
            match n {
                Node::Sym(s) if s == name => return Some(true),
                Node::List(_) if n.is(name) => {
                    return Some(match n.str_at(1) {
                        None => true,
                        Some(v) => v == "yes" || v == "true",
                    });
                }
                _ => {}
            }
        }
        None
    }

    /// Depth-first visit of every list node (including self).
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Node)) {
        if let Node::List(v) = self {
            f(self);
            for c in v {
                c.walk(f);
            }
        }
    }

    /// Remove direct children with any of the given heads.
    pub fn remove_children(&mut self, heads: &[&str]) {
        if let Node::List(v) = self {
            let mut first = true;
            v.retain(|n| {
                if first {
                    first = false;
                    return true;
                }
                !heads.iter().any(|h| n.is(h))
            });
        }
    }

    /// Serialize in KiCad-like layout (tab indented).
    pub fn to_kicad_string(&self) -> String {
        let mut out = String::new();
        write_node(self, 0, &mut out);
        out.push('\n');
        out
    }

    /// Compact single-line serialization (useful for hashing/debug).
    pub fn to_compact_string(&self) -> String {
        let mut out = String::new();
        write_compact(self, &mut out);
        out
    }
}

fn write_atom(n: &Node, out: &mut String) {
    match n {
        Node::Sym(s) => out.push_str(s),
        Node::Str(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Node::List(_) => unreachable!(),
    }
}

fn write_compact(n: &Node, out: &mut String) {
    match n {
        Node::List(v) => {
            out.push('(');
            for (i, c) in v.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_compact(c, out);
            }
            out.push(')');
        }
        _ => write_atom(n, out),
    }
}

fn write_node(n: &Node, depth: usize, out: &mut String) {
    match n {
        Node::List(v) => {
            let has_list = v.iter().any(|c| matches!(c, Node::List(_)));
            // Short "leaf-ish" lists like (at 1 2 0) or (xy 1 2) stay on one line.
            let inline_ok = !has_list || is_inline_list(n);
            out.push('(');
            if inline_ok {
                for (i, c) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    write_compact(c, out);
                }
                out.push(')');
                return;
            }
            let mut first = true;
            for c in v {
                match c {
                    Node::List(_) => {
                        out.push('\n');
                        for _ in 0..=depth {
                            out.push('\t');
                        }
                        write_node(c, depth + 1, out);
                    }
                    _ => {
                        if !first {
                            out.push(' ');
                        }
                        write_atom(c, out);
                    }
                }
                first = false;
            }
            out.push('\n');
            for _ in 0..depth {
                out.push('\t');
            }
            out.push(')');
        }
        _ => write_atom(n, out),
    }
}

/// Lists that KiCad itself writes on one line even though they contain lists.
fn is_inline_list(n: &Node) -> bool {
    matches!(n.head(), Some("font" | "xyz" | "size" | "stroke" | "lib"))
        && n.to_compact_string().len() < 120
        && n.args()
            .iter()
            .all(|c| c.args().iter().all(|cc| !matches!(cc, Node::List(_))))
}

/// Parse a KiCad number atom (tolerant).
pub fn num(n: Option<&str>) -> f64 {
    n.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
}

/// Debug helper: write a short description of a node.
pub fn describe(n: &Node) -> String {
    let mut s = String::new();
    let c = n.to_compact_string();
    let _ = write!(s, "{}", if c.len() > 200 { &c[..200] } else { &c });
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let n = parse(r#"(a (b "x \"q\" y") c 1.5 (hide yes))"#).unwrap();
        assert_eq!(n.head(), Some("a"));
        assert_eq!(n.value("b"), Some(r#"x "q" y"#));
        assert_eq!(n.flag("hide"), Some(true));
        assert_eq!(n.flag("c"), Some(true));
        let back = parse(&n.to_kicad_string()).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn property_lookup() {
        let n =
            parse(r#"(symbol (property "Reference" "R1" (at 0 0 0)) (property "Value" "10k"))"#)
                .unwrap();
        assert_eq!(n.property("Value"), Some("10k"));
        assert_eq!(n.property("Nope"), None);
    }

    #[test]
    fn errors() {
        assert!(parse("(a (b)").is_err());
        assert!(parse("(a))").is_err());
    }
}
