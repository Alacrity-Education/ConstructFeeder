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
