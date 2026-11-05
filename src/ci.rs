//! GitHub Actions integration: step summary, annotations, outputs.

use crate::report::Report;
use std::io::Write;

fn esc_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}
fn esc_prop(s: &str) -> String {
    esc_data(s).replace(':', "%3A").replace(',', "%2C")
}

/// Emit `::warning`/`::error` workflow commands (GitHub reads them from stdout).
pub fn annotations(report: &Report, out: &mut dyn Write) -> std::io::Result<()> {
    for p in &report.projects {
        if p.status != "ok" {
            writeln!(
                out,
                "::error file={},title={}::{}",
                esc_prop(&p.pro),
                esc_prop(&format!("ConstructFeeder: project {} {}", p.name, p.status)),
                esc_data(p.error.as_deref().unwrap_or(""))
            )?;
        }
    }
    for c in &report.components {
        let Some(i) = c.instances.first() else {
            continue;
        };
        let file = i
            .sheet_file
            .clone()
            .unwrap_or_else(|| i.project_path.clone());
        let flags: Vec<&str> = [
            (c.flags.symbol, "symbol"),
            (c.flags.symbol_local, "symbolLocal"),
            (c.flags.footprint, "footprint"),
            (c.flags.footprint_local, "footprintLocal"),
            (c.flags.model, "model"),
        ]
        .iter()
        .filter(|(b, _)| *b)
        .map(|(_, n)| *n)
        .collect();
        let refs: Vec<String> = c
            .instances
            .iter()
            .take(8)
            .map(|i| format!("{}:{}", i.project, i.reference))
            .collect();
        writeln!(
            out,
            "::warning file={},title={}::{}",
            esc_prop(&file),
            esc_prop(&format!(
                "Custom component {} [{}]",
                c.folder,
                flags.join(", ")
            )),
            esc_data(&format!(
                "{} ({} instance(s): {})",
                c.reasons.join("; "),
                c.instance_count,
                refs.join(" ")
            ))
        )?;
    }
    Ok(())
}

pub fn step_summary(report: &Report, zip_name: &str) -> String {
    let s = &report.summary;
    let mut md = String::new();
    md.push_str("## ConstructFeeder — custom components\n\n");
    md.push_str(&format!(
        "Repository **{}** · {} project(s) · **{}** custom component(s), {} instance(s) · report `{}`\n\n",
        report.repository, s.projects, s.components, s.instances, zip_name
    ));
    if s.projects_unsupported + s.projects_error > 0 {
        md.push_str(&format!(
            "> ⚠️ {} unsupported and {} failed project(s) — see report.json\n\n",
            s.projects_unsupported, s.projects_error
        ));
    }
    if report.components.is_empty() {
        md.push_str("No custom components found.\n");
        return md;
    }
    md.push_str(
        "| Component | symbol | symbolLocal | footprint | footprintLocal | model | Instances |\n",
    );
    md.push_str("|---|:-:|:-:|:-:|:-:|:-:|---|\n");
    let b = |v: bool| if v { "✔" } else { "" };
    for c in &report.components {
        let mut refs: Vec<String> = c
            .instances
            .iter()
            .map(|i| format!("{}:{}", i.project, i.reference))
            .collect();
        refs.dedup();
        let more = if refs.len() > 6 {
            format!(" +{}", refs.len() - 6)
        } else {
            String::new()
        };
        refs.truncate(6);
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {}{} |\n",
            c.folder,
            b(c.flags.symbol),
            b(c.flags.symbol_local),
            b(c.flags.footprint),
            b(c.flags.footprint_local),
            b(c.flags.model),
            refs.join(", "),
            more
        ));
    }
    md
}

pub fn append_env_file(var: &str, content: &str) {
    if let Ok(p) = std::env::var(var)
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(p)
    {
        let _ = f.write_all(content.as_bytes());
    }
}
