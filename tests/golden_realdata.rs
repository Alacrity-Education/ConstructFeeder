//! Compares the tool's analysis against the manually curated golden reports in tests/golden/.
//! Skipped unless CF_TESTDATA, CF_STOCK_9 and CF_STOCK_10 are set.
use construct_feeder::analysis::{self, Finding};
use construct_feeder::libtable::StockLibs;
use construct_feeder::report;
use construct_feeder::types::KicadVersion;
use construct_feeder::vfs::{FsVfs, GitVfs, Vfs};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const FLAGS: [&str; 5] = [
    "symbol",
    "symbolLocal",
    "footprint",
    "footprintLocal",
    "model",
];

fn stock() -> Option<BTreeMap<KicadVersion, StockLibs>> {
    let mut m = BTreeMap::new();
    for (v, var) in [
        (KicadVersion::V9, "CF_STOCK_9"),
        (KicadVersion::V10, "CF_STOCK_10"),
    ] {
        m.insert(
            v,
            StockLibs {
                version: v,
                root: PathBuf::from(std::env::var(var).ok()?),
            },
        );
    }
    Some(m)
}

/// Golden instance project is the project name, or `dir/name` when names are ambiguous.
fn same_project(golden: &str, f: &Finding) -> bool {
    match golden.rsplit_once('/') {
        Some((_, name)) => {
            name == f.instance.project
                && f.instance
                    .project_path
                    .ends_with(&format!("{golden}.kicad_pro"))
        }
        None => golden == f.instance.project,
    }
}

fn flags(f: &Finding) -> [bool; 5] {
    let x = f.flags;
    [
        x.symbol,
        x.symbol_local,
        x.footprint,
        x.footprint_local,
        x.model,
    ]
}

fn check(file: &std::path::Path, repo: &str) {
    let (Ok(data), Some(stock)) = (std::env::var("CF_TESTDATA"), stock()) else {
        return;
    };
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let root = PathBuf::from(data).join(repo);
    let sources: Vec<String> = golden["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    let mut findings = Vec::new();
    for s in &sources {
        let v: Box<dyn Vfs> = if s == "worktree" {
            Box::new(FsVfs::new(&root).unwrap())
        } else {
            Box::new(GitVfs::new(&root, s).unwrap())
        };
        let (projects, f) = analysis::analyse_source(v.as_ref(), &[], &stock).unwrap();
        for p in &projects {
            assert_eq!(p.status, "ok", "{repo} {}: {:?}", p.pro, p.error);
        }
        findings.extend(f);
    }

    let mut errors = Vec::new();
    let mut matched = BTreeSet::new();
    for c in golden["components"].as_array().unwrap() {
        let want: [bool; 5] = FLAGS.map(|k| c[k].as_bool().unwrap());
        for i in c["instances"].as_array().unwrap() {
            let (src, proj, refr) = (
                i["source"].as_str().unwrap(),
                i["project"].as_str().unwrap(),
                i["reference"].as_str().unwrap(),
            );
            match findings.iter().position(|f| {
                f.instance.source == src && f.instance.reference == refr && same_project(proj, f)
            }) {
                None => errors.push(format!("missing {src}:{proj}:{refr} (expected {want:?})")),
                Some(ix) => {
                    matched.insert(ix);
                    let got = flags(&findings[ix]);
                    if got != want {
                        errors.push(format!(
                            "{src}:{proj}:{refr}: flags {got:?} != expected {want:?}; reasons {:?}",
                            findings[ix].reasons
                        ));
                    }
                }
            }
        }
    }
    for (ix, f) in findings.iter().enumerate() {
        if !matched.contains(&ix) {
            errors.push(format!(
                "unexpected {}:{}:{} {:?} {:?}",
                f.instance.source,
                f.instance.project_path,
                f.instance.reference,
                f.flags,
                f.reasons
            ));
        }
    }

    // Grouping: same partition of instances into components.
    let key = |src: &str, r: &str, p: &str| format!("{src}|{p}|{r}");
    let mut want_groups: Vec<Vec<String>> = golden["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let mut v: Vec<String> = c["instances"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    let p = i["project"].as_str().unwrap();
                    key(
                        i["source"].as_str().unwrap(),
                        i["reference"].as_str().unwrap(),
                        p.rsplit('/').next().unwrap(),
                    )
                })
                .collect();
            v.sort();
            v
        })
        .collect();
    let mut got_groups: Vec<Vec<String>> = report::group(findings)
        .iter()
        .map(|g| {
            let mut v: Vec<String> = g
                .component
                .instances
                .iter()
                .map(|i| key(&i.source, &i.reference, &i.project))
                .collect();
            v.sort();
            v
        })
        .collect();
    want_groups.sort();
    got_groups.sort();
    if want_groups != got_groups {
        errors.push(format!(
            "grouping differs:\n  expected-only {:?}\n  got-only {:?}",
            want_groups
                .iter()
                .filter(|g| !got_groups.contains(g))
                .collect::<Vec<_>>(),
            got_groups
                .iter()
                .filter(|g| !want_groups.contains(g))
                .collect::<Vec<_>>()
        ));
    }
    assert!(
        errors.is_empty(),
        "{repo}: {} mismatch(es):\n{}",
        errors.len(),
        errors.join("\n")
    );
}

/// Every `<Repo>.expected.json` in tests/golden (public repos) and private/golden (local only).
#[test]
fn golden_reports() {
    if std::env::var("CF_TESTDATA").is_err() || stock().is_none() {
        return;
    }
    let mut n = 0;
    for dir in ["tests/golden", "private/golden"] {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            if let Some(repo) = name.strip_suffix(".expected.json") {
                check(&f, repo);
                n += 1;
            }
        }
    }
    eprintln!("checked {n} golden report(s)");
    assert!(n > 0);
}

/// The job-summary Markdown names every component, what is customized and where.
#[test]
fn markdown_summary_demoboards() {
    let (Ok(data), Some(stock)) = (std::env::var("CF_TESTDATA"), stock()) else {
        return;
    };
    let vfs = FsVfs::new(&PathBuf::from(data).join("DemoBoards")).unwrap();
    let (projects, findings) = analysis::analyse_source(&vfs, &[], &stock).unwrap();
    let mut groups = report::group(findings);
    for g in &mut groups {
        report::component_files(g);
    }
    let components: Vec<_> = groups.into_iter().map(|g| g.component).collect();
    let rep = report::Report {
        tool: Default::default(),
        generated_at: String::new(),
        repository: "DemoBoards".into(),
        sources: vec![],
        stock_libraries: Default::default(),
        summary: report::summarize(&projects, &components),
        projects,
        components,
        warnings: vec![],
    };
    let md = construct_feeder::markdown::render(
        &rep,
        "x.zip",
        Some(construct_feeder::markdown::SUMMARY_LIMIT),
    );
    for needle in [
        "### `PIR_Sensor`",
        "**Symbol from a local library** — `New_Library:PIR_Sensor`",
        "not available anywhere",
        "### `SWITCH_BASCULANT`",
        "**Custom 3D model**",
        "**SW1**",
        "**SW2**",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
    }
}
