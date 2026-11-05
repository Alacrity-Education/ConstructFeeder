# Golden reports

`<Repo>.expected.json` files are manually curated expectations for real KiCad repositories,
built by inspecting the designs independently of this tool's code. `tests/golden_realdata.rs`
runs the analysis on `$CF_TESTDATA/<Repo>` and requires the same five flags for every
instance and the same grouping of instances into components.

Run with `CF_TESTDATA=<dir containing the repos> CF_STOCK_9=<kicad 9 libs> CF_STOCK_10=<kicad 10 libs> cargo test`.
Golden files for private repositories can be kept in `private/golden/` (gitignored); they are
picked up automatically.

## Review decisions encoded in the expectations
- Footprint text that only displays fields (`${REFERENCE}`, `${VALUE}`, any `${VAR}`-only text,
  KiCad 5 `%R`/`%V`) is ignored; real text labels (e.g. silkscreen `+`/`-`, pin-1 `*`) count
  toward `footprintLocal`.
- 3D model visibility (`hide`) does not count toward `model`.
- Library drift: a stock part that differs from its file version's stock library but equals the
  other supported stock version (KiCad 9.0.9 / 10.0.8) is not flagged.
- Libraries that cannot be found anywhere make `symbol`/`footprint` true but cannot be compared,
  so `symbolLocal`/`footprintLocal` stay false.
