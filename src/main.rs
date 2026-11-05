use anyhow::{Context, Result, bail};
use clap::Parser;
use construct_feeder::analysis::{self, Finding, ProjectReport};
use construct_feeder::ci;
use construct_feeder::libtable::StockLibs;
use construct_feeder::report::{self, Report, SourceInfo};
use construct_feeder::types::KicadVersion;
use construct_feeder::vfs::{self, FsVfs, GitVfs, PrefixVfs, Vfs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Find custom KiCad components (project-library or locally edited symbols, footprints and
/// 3D models) in a repository and export them as a zip.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Repository root to scan.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Output zip (default "<repo-dir-name>-construct-feeder.zip").
    #[arg(long)]
    out: Option<PathBuf>,
    /// Scan these git refs (read-only) instead of the working tree.
    #[arg(long, value_delimiter = ',')]
    refs: Vec<String>,
    /// Scan all local + remote-only branches.
    #[arg(long)]
    all_branches: bool,
    /// Also scan the working tree when --refs/--all-branches is used.
    #[arg(long)]
    worktree: bool,
    /// Extra exclude globs (defaults: backups, .history, .git, target, .claude).
    #[arg(long)]
    exclude: Vec<String>,
    /// Stock library dirs as VER=PATH (default: $CONSTRUCT_FEEDER_STOCK_<VER>, else /opt/kicad/<VER>).
    #[arg(long = "stock-dir")]
    stock_dir: Vec<String>,
    /// Exit 1 if any custom component is found.
    #[arg(long)]
    fail_on_custom: bool,
    /// Don't print per-component JSON to stdout.
    #[arg(long)]
    quiet: bool,
    /// Force GitHub annotations + step summary (auto when GITHUB_ACTIONS=true).
    #[arg(long)]
    github: bool,
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("construct-feeder: error: {e:#}");
            std::process::exit(2);
        }
    }
}

fn stock_dirs(cli: &Cli) -> Result<BTreeMap<KicadVersion, StockLibs>> {
    let mut map = BTreeMap::new();
    for v in KicadVersion::ALL {
        let env = std::env::var(format!("CONSTRUCT_FEEDER_STOCK_{}", v.major())).ok();
        let p = PathBuf::from(env.unwrap_or_else(|| format!("/opt/kicad/{}", v.major())));
        map.insert(v, p);
    }
    for s in &cli.stock_dir {
        let (ver, path) = s
            .split_once('=')
            .with_context(|| format!("--stock-dir expects VER=PATH, got '{s}'"))?;
        let v = ver
            .trim()
            .parse::<u32>()
            .ok()
            .and_then(KicadVersion::from_major)
            .with_context(|| format!("--stock-dir: unsupported KiCad version '{ver}' (9 or 10)"))?;
        map.insert(v, PathBuf::from(path));
    }
    let mut out = BTreeMap::new();
    for (v, p) in map {
        let has_libs = |d: &str, exts: &[&str]| {
            std::fs::read_dir(p.join(d)).is_ok_and(|rd| {
                rd.filter_map(Result::ok).any(|e| {
                    exts.iter()
                        .any(|x| e.file_name().to_string_lossy().ends_with(x))
                })
            })
        };
        if has_libs("symbols", &[".kicad_sym", ".kicad_symdir"])
            && has_libs("footprints", &[".pretty"])
        {
            out.insert(
                v,
                StockLibs {
                    version: v,
                    root: p,
                },
            );
        } else {
            eprintln!(
                "construct-feeder: warning: no KiCad {} stock libraries at {} (projects of that version will fail)",
                v.major(),
                p.display()
            );
        }
    }
    Ok(out)
}

fn run(cli: Cli) -> Result<i32> {
    let root = cli
        .root
        .canonicalize()
        .with_context(|| format!("--root {}", cli.root.display()))?;
    let repo_name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let stock = stock_dirs(&cli)?;
    let github = cli.github || std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");

    let mut sources: Vec<Box<dyn Vfs>> = Vec::new();
    let use_refs = cli.all_branches || !cli.refs.is_empty();
    if use_refs {
        let info =
            vfs::repo_info(&root).context("--refs/--all-branches require a git repository")?;
        let prefix = root
            .strip_prefix(
                info.toplevel
                    .canonicalize()
                    .unwrap_or(info.toplevel.clone()),
            )
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .replace('\\', "/");
        let mut refs = cli.refs.clone();
        if cli.all_branches {
            for b in vfs::list_branches(&info.toplevel)? {
                if !refs.contains(&b) {
                    refs.push(b);
                }
            }
        }
        if refs.is_empty() {
            bail!("no git refs to scan");
        }
        for r in &refs {
            let g = GitVfs::new(&info.toplevel, r)?;
            sources.push(if prefix.is_empty() {
                Box::new(g)
            } else {
                Box::new(PrefixVfs::new(g, &prefix))
            });
        }
        if cli.worktree {
            sources.push(Box::new(FsVfs::new(&root)?));
        }
    } else {
        sources.push(Box::new(FsVfs::new(&root)?));
    }

    let mut projects: Vec<ProjectReport> = Vec::new();
    let mut findings: Vec<Finding> = Vec::new();
    let mut source_info = Vec::new();
    let mut warnings = Vec::new();
    for v in &sources {
        source_info.push(SourceInfo {
            label: v.label().to_string(),
            commit: v.commit().map(str::to_string),
            dirty: v.dirty(),
        });
        match analysis::analyse_source(v.as_ref(), &cli.exclude, &stock) {
            Ok((p, f)) => {
                projects.extend(p);
                findings.extend(f);
            }
            Err(e) => warnings.push(format!("{}: {e:#}", v.label())),
        }
    }

    let mut groups = report::group(findings);
    let mut files = Vec::new();
    for g in &mut groups {
        files.extend(report::component_files(g));
    }
    let components: Vec<_> = groups.into_iter().map(|g| g.component).collect();
    let summary = report::summarize(&projects, &components);
    let report = Report {
        tool: BTreeMap::from([
            ("name", env!("CARGO_PKG_NAME")),
            ("version", env!("CARGO_PKG_VERSION")),
        ]),
        generated_at: report::now_rfc3339(),
        repository: repo_name.clone(),
        sources: source_info,
        stock_libraries: stock
            .iter()
            .map(|(v, s)| (v.major().to_string(), s.root.display().to_string()))
            .collect(),
        summary,
        projects,
        components,
        warnings,
    };

    let out = cli
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{repo_name}-construct-feeder.zip")));
    report::write_zip(&out, &report, &files)?;

    if !cli.quiet {
        let states: Vec<serde_json::Value> = report
            .components
            .iter()
            .map(|c| {
                serde_json::json!({
                    "component": c.folder,
                    "symbol": c.flags.symbol,
                    "symbolLocal": c.flags.symbol_local,
                    "footprint": c.flags.footprint,
                    "footprintLocal": c.flags.footprint_local,
                    "model": c.flags.model,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&states)?);
    }
    if github {
        let mut stdout = std::io::stdout();
        ci::annotations(&report, &mut stdout)?;
        ci::append_env_file(
            "GITHUB_STEP_SUMMARY",
            &ci::step_summary(&report, &out.display().to_string()),
        );
        ci::append_env_file(
            "GITHUB_OUTPUT",
            &format!(
                "zip={}\ncustom-count={}\n",
                out.display(),
                report.summary.components
            ),
        );
    }
    let s = &report.summary;
    eprintln!(
        "construct-feeder: {} project(s), {} custom component(s) ({} instances) -> {}",
        s.projects,
        s.components,
        s.instances,
        out.display()
    );
    for p in report.projects.iter().filter(|p| p.status != "ok") {
        eprintln!(
            "construct-feeder: {} [{}] {}: {}",
            p.pro,
            p.source,
            p.status,
            p.error.as_deref().unwrap_or("")
        );
    }
    if s.projects_unsupported + s.projects_error > 0 || !report.warnings.is_empty() {
        return Ok(2);
    }
    if cli.fail_on_custom && s.components > 0 {
        return Ok(1);
    }
    Ok(0)
}
