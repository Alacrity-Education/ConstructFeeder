# ConstructFeeder — reference

A CLI (and GitHub Action) that scans a repository of KiCad 9/10 projects, finds the
**custom** components, and exports them as one zip per repository.

## What counts as custom

A component counts only if it is present on the schematic or board and has any of:

| Flag | Meaning |
|---|---|
| `symbol` | symbol comes from a project-level library (not a stock library) |
| `symbolLocal` | symbol was edited on the schematic (`lib_name` variant, or the embedded copy differs from the library in pins/graphics) |
| `footprint` | footprint comes from a project-level library |
| `footprintLocal` | footprint on the board differs from its library (pads/graphics/attributes) |
| `model` | non-stock 3D model, or the model list/transform differs from the library footprint |

Property/field edits are ignored. Power symbols and graphic-only (pinless) symbols are
excluded. Only KiCad 9 and 10 projects are supported; older projects are reported as
unsupported. Excluded directories: backups (`*backup*`, `*-backups`), `.history`, `.git`, `target`.

## CLI usage

```
construct-feeder [OPTIONS]
  --root <DIR>                 repo root to scan (default ".")
  --out <FILE>                 output zip (default "<repo-dir-name>-construct-feeder.zip")
  --refs <REF,...>             scan these git refs (read-only) instead of the working tree
  --all-branches               scan all local + remote-only branches
  --worktree                   also scan the working tree when --refs/--all-branches is used
  --exclude <GLOB>...          extra exclude globs (defaults: backups, .history, .git, target)
  --stock-dir <VER=PATH>...    stock lib dirs (default from CONSTRUCT_FEEDER_STOCK_9/_10, else /opt/kicad/<ver>)
  --fail-on-custom             exit 1 if any custom component is found
  --quiet                      don't print per-component JSON to stdout
  --github                     force GitHub annotations + step summary (auto when GITHUB_ACTIONS=true)
```

Stock libraries are expected as `<dir>/symbols/<Nick>.kicad_sym` and
`<dir>/footprints/<Nick>.pretty/`. When `$GITHUB_OUTPUT` is set, `custom-count` and `zip`
are written to it.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | OK |
| 1 | custom component found and `--fail-on-custom` given |
| 2 | error, or unsupported project (KiCad < 9) |

### Build from source

```sh
cargo build --release
./target/release/construct-feeder --root . --stock-dir 9=/path/to/stock/9 --stock-dir 10=/path/to/stock/10
```

## Output zip layout

```
<repo>-construct-feeder.zip
  report.json          full machine-readable findings
  report.md            human-readable report (same text as the job summary)
  components/<Name>[_<board>]/
    <Name>.kicad_sym
    <Name>.pretty/<footprint>.kicad_mod
    3d/<model>
```

## Job summary

In GitHub Actions the tool writes a Markdown report to the run's job summary
(`$GITHUB_STEP_SUMMARY`). It is generated deterministically from `report.json`, with no AI. For
every custom component it states:

- **what** is customized: a symbol or footprint from a local/unregistered/unavailable library, a
  symbol edited on the schematic (e.g. "pins 3, 4 changed (position, orientation)"), a footprint
  edited on the board (pads and per-layer graphics), and custom 3D models (repository path, where
  the file is in the zip, and any differences from the library footprint's models);
- **where** it is used: one row per instance with branch (if several were scanned), project,
  reference, value, schematic sheet and file, and board side.

GitHub caps job summaries at 1 MiB, so very large reports are truncated there. `report.md` in the
zip is always complete.

## Docker usage

The image bundles the binary (static musl), `git`, and the stock KiCad libraries
(symbols + footprints only) for KiCad 9.0.9 and 10.0.8 under `/opt/kicad/{9,10}`.

```sh
docker build -t construct-feeder .
docker run --rm -v "$PWD:/github/workspace" construct-feeder --root .
```

Build arguments:

| Arg | Default | Purpose |
|---|---|---|
| `RUST_VERSION` | `1.99` | Rust toolchain for the build stage |
| `KICAD_9_TAG` / `KICAD_10_TAG` | `9.0.9` / `10.0.8` | stock library tags |
| `WITH_STOCK_3D` | `0` | `1` also bundles `kicad-packages3D` under `/opt/kicad/<ver>/3dmodels`. **Several GB**, slow build; off by default |

## GitHub Action usage

```yaml
steps:
  - uses: actions/checkout@v4
    with:
      fetch-depth: 0          # needed for all-branches / refs
  - id: feeder
    uses: Alacrity-Education/ConstructFeeder@v1
    with:
      all-branches: "true"
      worktree: "true"
  - uses: actions/upload-artifact@v4
    with:
      name: construct-feeder
      path: ${{ steps.feeder.outputs.zip }}
```

A full example is in [`examples/workflows/construct-feeder.yml`](../examples/workflows/construct-feeder.yml).

Inputs (all optional): `root` (`.`), `out`, `refs`, `all-branches`, `worktree`, `exclude`
(whitespace/newline separated globs), `stock-dir` (whitespace/newline separated `VER=PATH`),
`fail-on-custom`, `quiet`. Boolean inputs take `"true"`/`"false"`.
Outputs: `zip` (path of the zip), `custom-count`.

The action runs the prebuilt image `ghcr.io/alacrity-education/construct-feeder` (KiCad 9 and 10
stock libraries baked in), so a run only pulls the image: nothing is compiled and no libraries are
downloaded from the KiCad mirrors.

## Releases

1. Set the image tag in `action.yml` to the new version (e.g. `1.1.0`) and commit.
2. Push the tag `v1.1.0`. The release workflow builds and pushes
   `ghcr.io/alacrity-education/construct-feeder:{1.1.0,1.1,1,latest}` and attaches a static
   `x86_64-unknown-linux-musl` binary to the GitHub release.
3. After the release workflow is green, move the major tag: `git tag -f v1 v1.1.0 && git push -f origin v1`.

