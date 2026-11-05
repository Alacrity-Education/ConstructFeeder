//! Deterministic Markdown report of the custom components (GitHub job summary + report.md).
//!
//! Built purely from `report.json` data with string formatting: for every component it states
//! *what* is customized (symbol / footprint / 3D model, with the concrete differences) and
//! *where* it is used (branch, project, reference, sheet, board side).

use crate::analysis::InstanceRef;
use crate::compare::{PadDiff, PinDiff};
use crate::models::ModelOrigin;
use crate::report::{Component, Report};
use crate::types::LibSource;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// GitHub rejects job summaries larger than 1 MiB; stay well below.
pub const SUMMARY_LIMIT: usize = 900 * 1024;
const MAX_INSTANCE_ROWS: usize = 25;

/// Inline code span that survives backticks in KiCad names.
fn code(s: &str) -> String {
    if s.contains('`') {
        format!("`` {s} ``")
    } else {
        format!("`{s}`")
    }
}

/// Text safe inside a Markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn source_phrase(s: LibSource) -> &'static str {
    match s {
        LibSource::Stock => "the stock KiCad library",
        LibSource::Project => "a project library (listed in the project's library table)",
        LibSource::ProjectUnregistered => {
            "a project library file that is not registered in any library table"
        }
        LibSource::Unresolved => {
            "a library that is not available anywhere (only the copy embedded in the design exists)"
        }
    }
}

fn nickname(id: &Option<String>) -> String {
    id.as_deref()
        .and_then(|s| s.split_once(':'))
        .map(|(n, _)| n.to_string())
        .unwrap_or_default()
}

/// "3, 4" style list of numbers grouped by (change, fields).
fn group_pins<'a, I>(items: I) -> Vec<(usize, String)>
where
    I: Iterator<Item = (&'a str, &'a str, &'a [String])>,
{
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (num, change, fields) in items {
        let f = if change == "changed" && !fields.is_empty() {
            fields.join(", ")
        } else {
            String::new()
        };
        groups
            .entry((change.to_string(), f))
            .or_default()
            .push(num.to_string());
    }
    groups
        .into_iter()
        .map(|((change, fields), nums)| {
            let mut s = nums.join(", ");
            if s.len() > 120 {
                s = format!(
                    "{} … ({} total)",
                    nums[..8.min(nums.len())].join(", "),
                    nums.len()
                );
            }
            let t = if fields.is_empty() {
                format!("{s} {change}")
            } else {
                format!("{s} {change} ({fields})")
            };
            (nums.len(), t)
        })
        .collect()
}

fn pin_lines(pins: &[PinDiff]) -> Vec<String> {
    group_pins(
        pins.iter()
            .map(|p| (p.number.as_str(), p.change.as_str(), p.fields.as_slice())),
    )
    .into_iter()
    .map(|(n, g)| format!("{} {g}", if n == 1 { "pin" } else { "pins" }))
    .collect()
}

fn pad_lines(pads: &[PadDiff]) -> Vec<String> {
    group_pins(
        pads.iter()
            .map(|p| (p.number.as_str(), p.change.as_str(), p.fields.as_slice())),
    )
    .into_iter()
    .map(|(n, g)| format!("{} {g}", if n == 1 { "pad" } else { "pads" }))
    .collect()
}

fn bullets(md: &mut String, indent: &str, items: &[String]) {
    for i in items {
        let _ = writeln!(md, "{indent}- {i}");
    }
}

/// The "what is customized" part for one component.
fn what(c: &Component, md: &mut String) {
    let f = &c.flags;
    let sym = c.lib_id.as_deref().unwrap_or("?");
    let fp = c.footprint_id.as_deref().unwrap_or("?");

    if f.symbol {
        let _ = writeln!(
            md,
            "- **Symbol from a local library** — {} comes from {}: {}.",
            code(sym),
            c.symbol_source
                .map(source_phrase)
                .unwrap_or("a non-stock library"),
            code(&nickname(&c.lib_id))
        );
    }
    if f.symbol_local {
        let variant = c
            .lib_name
            .as_deref()
            .map(|n| format!(" (schematic variant {})", code(n)))
            .unwrap_or_default();
        let _ = writeln!(
            md,
            "- **Symbol edited on the schematic**{variant} — differs from its library symbol {}:",
            code(sym)
        );
        if let Some(d) = &c.symbol_diff {
            let mut items = pin_lines(&d.pins);
            items.extend(d.graphics.iter().map(|g| format!("graphics: {g}")));
            items.extend(d.other.iter().cloned());
            bullets(md, "  ", &items);
        }
    }
    if f.footprint {
        let _ = writeln!(
            md,
            "- **Footprint from a local library** — {} comes from {}: {}.",
            code(fp),
            c.footprint_source
                .map(source_phrase)
                .unwrap_or("a non-stock library"),
            code(&nickname(&c.footprint_id))
        );
    }
    if f.footprint_local {
        let _ = writeln!(
            md,
            "- **Footprint edited on the board** — differs from its library footprint {}:",
            code(fp)
        );
        if let Some(d) = &c.footprint_diff {
            let mut items = pad_lines(&d.pads);
            items.extend(d.graphics.iter().map(|g| format!("graphics: {g}")));
            items.extend(d.other.iter().cloned());
            bullets(md, "  ", &items);
        }
    }
    if f.model {
        let _ = writeln!(md, "- **Custom 3D model**:");
        let mut items: Vec<String> = c
            .models
            .iter()
            .filter(|m| m.origin != ModelOrigin::Stock)
            .map(|m| {
                let state = match (m.origin, m.found) {
                    (ModelOrigin::Project, true) => {
                        format!(
                            "repository file {}",
                            code(m.repo_path.as_deref().unwrap_or(&m.path))
                        )
                    }
                    (ModelOrigin::Project, false) => "repository file — **missing**".to_string(),
                    (ModelOrigin::Embedded, true) => "embedded in the design".to_string(),
                    (ModelOrigin::Embedded, false) => "embedded — **data missing**".to_string(),
                    (ModelOrigin::External, _) => {
                        "outside the repository — **not portable**".to_string()
                    }
                    (ModelOrigin::Stock, _) => unreachable!(),
                };
                let exported = m
                    .exported_as
                    .as_deref()
                    .map(|e| format!(", exported as {}", code(e)))
                    .unwrap_or_default();
                format!("{} — {state}{exported}", code(&m.path))
            })
            .collect();
        items.extend(
            c.models_diff
                .iter()
                .map(|d| format!("differs from the library footprint: {d}")),
        );
        bullets(md, "  ", &items);
    }
    if !c.notes.is_empty() {
        let _ = writeln!(md, "- _Notes:_ {}", c.notes.join("; "));
    }
}

/// The "where" part: one table row per instance.
fn where_(c: &Component, md: &mut String) {
    // Collapse instances that only differ by branch into one row listing the branches.
    let mut rows: Vec<(&InstanceRef, Vec<&str>)> = Vec::new();
    for i in &c.instances {
        let same = |a: &InstanceRef| {
            (
                &a.project_path,
                &a.reference,
                &a.value,
                &a.sheet,
                &a.sheet_file,
                &a.layer,
                a.dnp,
            ) == (
                &i.project_path,
                &i.reference,
                &i.value,
                &i.sheet,
                &i.sheet_file,
                &i.layer,
                i.dnp,
            )
        };
        match rows.iter_mut().find(|(r, _)| same(r)) {
            Some((_, srcs)) => {
                if !srcs.contains(&i.source.as_str()) {
                    srcs.push(&i.source);
                }
            }
            None => rows.push((i, vec![i.source.as_str()])),
        }
    }
    let multi_source = c
        .instances
        .iter()
        .any(|i| i.source != c.instances[0].source);
    let _ = writeln!(
        md,
        "\n| {}Project | Ref | Value | Schematic | Board |",
        if multi_source { "Branch | " } else { "" }
    );
    let _ = writeln!(
        md,
        "|{}---|---|---|---|---|",
        if multi_source { "---|" } else { "" }
    );
    for (i, srcs) in rows.iter().take(MAX_INSTANCE_ROWS) {
        let branch = if multi_source {
            let list: Vec<String> = srcs.iter().map(|s| code(&cell(s))).collect();
            format!("{} | ", list.join(", "))
        } else {
            String::new()
        };
        let _ = writeln!(md, "{}", instance_row(i, &branch));
    }
    if rows.len() > MAX_INSTANCE_ROWS {
        let _ = writeln!(
            md,
            "\n_…and {} more instance(s); see report.json._",
            rows.len() - MAX_INSTANCE_ROWS
        );
    }
}

fn instance_row(i: &InstanceRef, branch: &str) -> String {
    let sch = match (&i.sheet, &i.sheet_file) {
        (Some(s), Some(f)) => format!("{} ({})", cell(s), code(&cell(f))),
        (None, Some(f)) => code(&cell(f)),
        _ => "—".into(),
    };
    let side = match i.layer.as_deref() {
        Some("F.Cu") => "top".to_string(),
        Some("B.Cu") => "bottom".to_string(),
        Some(l) => cell(l),
        None if i.on_schematic => "not on board".into(),
        None => "—".into(),
    };
    let dnp = if i.dnp { " (DNP)" } else { "" };
    format!(
        "| {branch}{} | **{}**{dnp} | {} | {sch} | {side} |",
        code(&cell(&i.project_path)),
        cell(&i.reference),
        cell(&i.value),
    )
}

fn flag_marks(c: &Component) -> String {
    let f = &c.flags;
    let names: Vec<&str> = [
        (f.symbol, "symbol"),
        (f.symbol_local, "symbolLocal"),
        (f.footprint, "footprint"),
        (f.footprint_local, "footprintLocal"),
        (f.model, "model"),
    ]
    .iter()
    .filter(|(b, _)| *b)
    .map(|(_, n)| *n)
    .collect();
    names.iter().map(|n| code(n)).collect::<Vec<_>>().join(" ")
}

/// Full report. `zip_name` is shown as the artifact name; `limit` truncates (job summary).
pub fn render(report: &Report, zip_name: &str, limit: Option<usize>) -> String {
    let s = &report.summary;
    let mut md = String::new();
    let _ = writeln!(
        md,
        "## ConstructFeeder — custom components in {}\n",
        code(&report.repository)
    );
    let sources: Vec<String> = report
        .sources
        .iter()
        .map(|x| match &x.commit {
            Some(c) => format!("{} @ {}", code(&x.label), code(&c[..c.len().min(8)])),
            None => code(&x.label),
        })
        .collect();
    let _ = writeln!(
        md,
        "Scanned {} project(s) on {} · **{} custom component(s)** in {} instance(s) · report {}\n",
        s.projects,
        sources.join(", "),
        s.components,
        s.instances,
        code(zip_name)
    );
    let _ = writeln!(
        md,
        "| symbol | symbolLocal | footprint | footprintLocal | model |\n|:-:|:-:|:-:|:-:|:-:|\n| {} | {} | {} | {} | {} |\n",
        s.symbol, s.symbol_local, s.footprint, s.footprint_local, s.model
    );
    for p in report.projects.iter().filter(|p| p.status != "ok") {
        let _ = writeln!(
            md,
            "> [!WARNING]\n> {} on {} is **{}**: {}\n",
            code(&p.pro),
            code(&p.source),
            p.status,
            p.error.as_deref().unwrap_or("")
        );
    }
    if report.components.is_empty() {
        md.push_str("No custom components found. ✅\n");
        return md;
    }

    let mut body = String::new();
    for c in &report.components {
        let mut sec = String::new();
        let _ = writeln!(sec, "### {} \u{2014} {}\n", code(&c.folder), flag_marks(c));
        what(c, &mut sec);
        where_(c, &mut sec);
        sec.push('\n');
        if limit.is_some_and(|l| md.len() + body.len() + sec.len() > l) {
            let _ = writeln!(
                body,
                "\n_Report truncated (GitHub summary size limit); the full report is in report.json / report.md in the artifact._"
            );
            break;
        }
        body.push_str(&sec);
    }
    md.push_str(&body);
    md
}
