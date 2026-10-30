---
feature: main-rs-module-split
status: delivered
updated: 2026-10-30
source: refactor/main-rs-modules
---

# Split main.rs into CLI modules

## Report

**What was built** — Pure structural refactor of `dsh-analyze`: 917-line
`main.rs` reduced to process entry + dispatch (~72 lines). Shared helpers
(`fail`/`flag_value`/`env_u64`) live in `cli.rs`. All `cmd_*` handlers moved
into `commands/` (`ingest`/`inspect`/`sample`/`join`/`agg`/`manage`). Engine
modules (`agg`/`dataset`/`expr`/`time`) untouched. No CLI protocol, JSON,
exit-code, or env-var change.

**Verification** — `cargo test -p dsh-analyze --release`: 9 passed.
`node --test` analysis-tools + analysis-policy (after `tsc` build): 30 passed.
CLI smoke: version → ingest → head → values → sample → join → agg →
describe → schema → list → export → rm all `ok:true`. Independent review of
`a0ad8af..dc7b227`: no CRITICAL findings; moved bodies token-equivalent to
base (rustfmt rewraps + `crate::` path qualify only).

## [S1] Problem

`crates/dsh-analyze/src/main.rs` is ~917 lines and mixes process entry,
argument parsing, and every command implementation (`ingest`/`head`/`schema`/
`describe`/`values`/`sample`/`join`/`agg`/`list`/`export`/`rm`). Sibling
engines (`agg.rs`, `dataset.rs`, `expr.rs`, `time.rs`) are already modular;
the CLI file is now the largest source and is hard to navigate.

## [S2] Design

Pure structural refactor. No CLI protocol, JSON shapes, exit codes, env vars,
or behavior change. Existing unit tests and node policy/tool tests remain the
acceptance surface.

### S2.1 Target layout

```
crates/dsh-analyze/src/
  main.rs          # entry + run() dispatch only (~100 lines)
  cli.rs           # fail, flag_value, env_u64, CAPABILITIES/VERSION as needed
  commands/
    mod.rs         # mod declarations + shared cmd helpers if any
    ingest.rs      # cmd_ingest
    inspect.rs     # cmd_head, cmd_schema, cmd_describe, cmd_values
    sample.rs      # Rng, random_indices, stratified_indices, cmd_sample
    join.rs        # cmd_join
    agg.rs         # cmd_agg (wraps crate::agg engine)
    manage.rs      # cmd_list, cmd_export, cmd_rm
  agg.rs           # unchanged engine
  dataset.rs       # unchanged
  expr.rs          # unchanged
  time.rs          # unchanged
```

`agg` name collision: command wrapper stays `commands::agg`; engine remains
`crate::agg`. Callers use explicit paths.

### S2.2 Visibility and dispatch

- `main.rs` declares `mod agg; mod cli; mod commands; mod dataset; mod expr; mod time;`.
- Command handlers are `pub(crate)` in their modules; `run()` imports them from `commands`.
- Shared helpers (`fail`, `flag_value`, `env_u64`) live in `cli` and are used by command modules.
- `Rng` and sampling helpers stay private to `commands::sample` (only `cmd_sample` uses them).

### S2.3 Behavior freeze

- Same flags, defaults, clamps, error strings, exit codes.
- Same env keys (`DSH_ANALYZE_*` with `INV_ANALYSIS_*` fallback).
- `version`/`--version` still short-circuits without `--workspace`.
- No new abstractions beyond extracting the existing functions; avoid drive-by cleanups that change behavior.

## [S3] Out of Scope

- Protocol/JSON/exit-code changes
- New commands or flags
- Parquet/Polars work
- TypeScript package changes
- Perf rewrites of join/sample/describe

## Tasks

- [x] T1: Create `cli` + `commands` skeleton and move helpers — acceptance: project compiles; `fail`/`flag_value`/`env_u64` live in `cli` (covers: S2.1–S2.2)
- [x] T2: Move all `cmd_*` implementations into `commands/*` — acceptance: `main.rs` is entry+dispatch only; no duplicate command bodies (covers: S2.1–S2.3)
- [x] T3: Verify behavior — acceptance: `cargo test -p dsh-analyze --release` passes; node analysis tests pass or are PRE-EXISTING fail (covers: S2.3)

## Journey log
