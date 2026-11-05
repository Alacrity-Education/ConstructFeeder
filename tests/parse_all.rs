//! Parses every KiCad file under $CF_TESTDATA (skipped if unset) and checks round-trip.
use construct_feeder::sexpr;
use walkdir::WalkDir;

#[test]
fn parse_all_testdata() {
    let Ok(root) = std::env::var("CF_TESTDATA") else {
        eprintln!("CF_TESTDATA unset, skipping");
        return;
    };
    let mut n = 0;
    let dirs = std::env::var("CF_TESTDATA_DIRS").unwrap_or_else(|_| "DemoBoards,AirBoxV2".into());
    for dir in dirs.split(',') {
        for e in WalkDir::new(format!("{root}/{dir}"))
            .into_iter()
            .filter_map(Result::ok)
        {
            let p = e.path();
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !matches!(ext, "kicad_sch" | "kicad_pcb" | "kicad_sym" | "kicad_mod")
                && !p.ends_with("sym-lib-table")
                && !p.ends_with("fp-lib-table")
            {
                continue;
            }
            let text = std::fs::read_to_string(p).unwrap();
            let node = sexpr::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            let back = sexpr::parse(&node.to_kicad_string()).unwrap();
            assert_eq!(node, back, "{}", p.display());
            n += 1;
        }
    }
    eprintln!("parsed {n} files");
    assert!(n > 0);
}
