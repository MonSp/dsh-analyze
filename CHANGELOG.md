# Changelog

## 0.1.11

P0 correctness batch (R-29..R-30) from the post-v0.1.10 requirement list:
timezone-aware day boundaries and Chinese-locale CSV ingest.

### Added

- `agg` / `transform_agg` time-period `groupBy` accepts optional `tz`
  (R-29): fixed `±HH:MM` (HH 00..=14) or IANA whitelist v1 (`Asia/Shanghai`
  → +08:00, hand-rolled). `day` / `week` (Monday) / `month` bucket on the
  target timezone's calendar. Default remains UTC; date-only values never
  shift; naive date-times with `tz` set are treated as local wall-clock.
  Invalid `tz` → `analysis/invalid-args` (exit 2); `validateAggSpec` mirrors
  the grammar before spawn.
- `ingest --encoding utf-8|gbk|gb18030|auto` (R-30, csv only, default
  `utf-8`). `auto` = BOM sniff → strict UTF-8 → fatal GB18030 fallback with
  `warnings:[{"code":"encoding-fallback","encoding":"gb18030"}]`. Decoding is
  never lossy.

### Changed

- CSV decode failures are now `analysis/invalid-args` **exit 2** with an
  actionable message (`file is not valid UTF-8 (re-save as CSV UTF-8, or pass
  --encoding gbk)`); previously exit 1 `cli-failed: … read failed`. Real
  filesystem errors stay exit 1.
- `dataset_list --since` documented as UTC-interpreted (no behavior change).
- Interface contract updated in `docs/interface.md` (v0.1.11); USAGE lists
  `--encoding`.
- Version and `version` capabilities: `0.1.11` (token list unchanged).
- Package version `dsh-analyze@0.1.11`. New dependency: `encoding_rs` (GB18030
  decode).

## 0.1.10

P2 gap list (R-24..R-26) from the post-v0.1.7 requirement batch (targets
renumbered from v0.2.x to v0.1.8/v0.1.9/v0.1.10).

### Added

- `corr` / `dataset_corr` pairs report `pValue` (two-sided Student-t
  approximation via regularized incomplete beta; `r` null or `n < 3` → null).
- `crosstab` / `dataset_crosstab` optional `--third` / `third`: one call
  returns `thirdColumn`, `thirdCategories`, and `tables[]` of 2-D slices.
  Without `third`, response shape is unchanged from v0.1.7.

### Changed

- Interface documents a frozen head/sample/values caps table (R-26; no
  behavior change). Tool descriptions match `requestedN` / `requestedLimit`.
- Version and `version` capabilities: `0.1.10` (token list unchanged).
- Public contract updated in `docs/interface.md` (v0.1.10).
- Package version `dsh-analyze@0.1.10`.

### Fixed

- Parquet ingest: types outside the documented map (Decimal, Time, Duration,
  Interval, Float16, Dictionary value type, …) are rejected with
  `analysis/invalid-args: parquet type not supported for column …` (exit 2)
  instead of being silently read as null.
- `export --format parquet`: columns with no value evidence (empty tables,
  all-null columns) write each column with its declared dataset dtype instead
  of collapsing to Boolean.
- `agg` `window` cumsum: declared dtype is decided once for the whole column
  and shared by every partition (mixed i64/f64 data), and integral overflow
  promotes the whole column to `f64` — value variants always match the schema.
- `agg` `window`: a `by` / `orderBy` key present with the wrong JSON type
  (e.g. `"by":"acc"`) is `analysis/invalid-args` instead of being treated as
  missing — ewm no longer silently computes a global window.
- `corr` `pValue`: corrected the `ln_gamma` constant (`ln(2π)` → `½ln(2π)`)
  so p-values match the documented Student-t / incomplete-beta formula;
  closed-form reference tests added (df=1 exact identity, known table point).
- `dataset_export`: the default `outPath` follows `format`
  (`.xlsx` / `.parquet` / `.csv`) instead of always `.csv`.
- `dataset_crosstab` render: third responses now surface `thirdTruncated`,
  per-table `truncated`, and `nullThirdCount`.
- `--format` / `format` is case-insensitive on ingest and export (CLI and
  tool layer), matching the documented behavior; `XLSX` ingest reaches the
  exit-4 limit instead of `bad format`.
- `export --format xlsx`: integers beyond 2^53 are written as text cells
  instead of silently losing precision as numbers.

### Design notes

Review findings are fixed against a unified design (K1 ground-truth tests for
numeric helpers; K2 strict JSON shapes; K3 declared-dtype = value-variant
invariant; K4 defaults follow the primary flag).

## 0.1.9

Delivery-chain increment (R-23): multi-format export.

### Added

- `export` / `dataset_export`: `--format` / `format` `csv|xlsx|parquet`
  (default `csv`; extension may infer when `--format` omitted).
  - `xlsx`: single sheet `data`, header, true cell types (numbers/bools).
  - `parquet`: same type map as ingest; round-trips through parquet ingest.
- Response includes `"format"` alongside `outputPath` / `rowCount` / `bytes`.
- Unknown format → `analysis/invalid-args` (exit 2).

### Changed

- Version and `version` capabilities: `0.1.9`.
- Public contract updated in `docs/interface.md` (v0.1.9 section).

## 0.1.8

P0 gap list (R-21..R-22): window operators + Parquet ingest.

### Added

- `agg` spec `window` array: `rank` (competition 1,1,3) / `row_number` /
  `cumsum` (null source contributes 0) / `ewm` (`alpha ∈ (0,1]`; null x
  leaves y unchanged). Writes new columns; row count unchanged. Pipeline:
  `filter → window → topKPerGroup → mutate → group/agg → having → select →
  orderBy → limit`.
- `ingest --format parquet` and `.parquet` extension auto-detect. Type map
  for bool/i64/f64/utf8/timestamp/date; nested list/struct/map rejected with
  `analysis/invalid-args`; corrupt files → stable `analysis/cli-failed`
  (no OS text). `xlsx` ingest remains exit 4.

### Changed

- Version and `version` capabilities: `0.1.8` (token list unchanged).
- Public contract updated in `docs/interface.md` (v0.1.8).
- Workspace / package version: intermediate `0.1.8` (final branch ships 0.1.10).

## 0.1.7

Full v0.1.6-联调 requirement list (R-1..R-20): correctness hardening, analysis
capability gaps, and tool-layer experience fixes.

### Added

- `filter` accepts nested boolean groups `{"all":[…]}` / `{"any":[…]}` /
  `{"not":…}` alongside the legacy AND array (nesting depth ≤ 8).
- `agg` functions: `count_distinct`, `percentile` (nearest-rank, `p` in (0,1)),
  `first` / `last` (first/last non-null in input order), `count_if` (`where`
  predicate or group).
- `agg` `having` (post-aggregation result filter) and `topKPerGroup`
  (`{by:[…],k,orderBy?}` row stage before mutate/grouping).
- `select` entries accept `{"column":"…","as":"…"}` rename; `mutate` whitelist
  gains `to_int` / `to_str` / `to_date(col, fmt)` plus string literals.
- CLI + tools: `union` / `dataset_union`, `diff` / `dataset_diff`, `pivot` /
  `dataset_pivot`.
- `join --on` (and `diff --on`) accept `leftKey:rightKey` pairs.
- `ingest --format json` (top-level array or `{"records":[…]}`).
- `list` / `dataset_list`: `--label-like` / `--produced-by` / `--since` filters;
  meta records `createdAt` (UTC).
- Response `warnings[]` (`mutate-null` / `cast-null` / `date-parse-fail` /
  `clamp`) with `requestedN` / `requestedLimit` echo fields; `parseFailSample`
  on time-period grouping.
- `corr` pairs report `df`; `dsh-analyze --help` usage text.
- npm package re-exports `createAnalysisToolDefinitions` /
  `attachAnalysisTools` / `validateAggSpec` from the package root.

### Fixed

- `mutate` type errors are rejected at plan time (`analysis/invalid-args`,
  exit 2) instead of silently producing null columns.
- `export` creates missing parent directories before writing.
- `--ops` / `--spec` / path errors no longer leak OS error text.
- Time-period grouping accepts `YYYY/MM/DD`, `YYYY/MM/DD HH:MM:SS`, `YYYYMMDD`
  in addition to the ISO forms.

### Changed

- `head --n` cap 100 (was 20); `sample --n` cap 200 (was 20); `values --limit`
  echoes `requestedLimit` and clamps visibly with a `clamp` warning.
- Version and `version` capabilities: `0.1.7` plus `union`, `diff`, `pivot`.
- Public contract updated in `docs/interface.md` (v0.1.7).
- Package version `dsh-analyze@0.1.7`.

## 0.1.6

Tool-layer contract hardening: DSH-host-compatible nullable output schemas
and strict `agg`-spec parsing, from the v0.1.4/v0.1.5 联调 defect list plus
two rounds of review follow-ups.

### Fixed

- Tool schemas: nullable outputs (`outDatasetId` / `stratifyBy` / `label`) use
  `oneOf: [string, null]` instead of JSON Schema type arrays / plain `string`,
  so tools register on DSH hosts and CLI `null` values pass output validation.
  `dataset_list` now declares `parentDatasetId` (`string | null`) and
  `sourceKind` alongside `label`.
- `agg` spec parsing: non-array `filter`, wrong-shape `select` / `mutate` /
  `groupBy` / `aggs` / `orderBy` / `limit`, and unknown top-level keys are
  rejected (`analysis/invalid-args`, CLI exit 2) instead of silently dropped;
  nested same-name keys no longer shadow top-level fields, so key order cannot
  change results.
- `agg` `limit` must be an integer `1..5000` at parse time (was silently
  clamped to the row count). Type errors now report `limit must be a number`
  and range errors `limit must be an integer 1..5000`, consistently on both
  the tool layer and the CLI. Tool-layer `validateAggSpec` also checks
  `select` entries and `orderBy` items before spawn.
- `filter` value shapes: `value` is required for every op except `is_null` /
  `not_null` (was a silent null default); only `op: "in"` accepts an array of
  scalars — all members now match (previously only the first was kept, and
  numeric members matched nothing). Arrays for other ops and non-scalar
  values are rejected. `extract_array` bracket matching is string-aware, so
  `select` / `in` strings containing `[` or `]` parse intact.
- Number literals use one lexer across `limit`, filter `value`, and `in`
  members: exponents (`2e1`, `3e3`) parse as JSON numbers everywhere, and
  trailing junk (`2abc`) is rejected instead of silently reading a prefix.
- Item arrays (`filter` / `aggs` / `orderBy` / `mutate`) and `select` string
  arrays are escape-aware: `\"` inside a string no longer swallows later
  items or truncates names.
- `validateAggSpec` requires a filter `column` before spawn; CLI-side
  `orderBy.desc` must be a boolean (was silently `false`); `dataset_dedup`
  declares `keyColumns` (`string[] | null`) in its output schema.

### Changed

- Version and `version` capabilities: `0.1.6` (no new tools).
- Public contract updated in `docs/interface.md` (v0.1.6).
- Package version `dsh-analyze@0.1.6`.

## 0.1.5

Time-series increment (lag / lead / diff / pct_change / rolling).

### Added

- CLI + tools: `series` / `dataset_series` — one call sorts by `--order-by`
  (nulls last), applies 1–8 ops (`lag`, `lead`, `diff`, `pct_change`,
  `rolling` mean|sum|min|max), and **always** writes a new dataset. Response is
  ids + schema only (never full rows).

### Changed

- Version and `version` capabilities: `0.1.5` plus `series`.
- Public contract updated in `docs/interface.md` (v0.1.5).
- Package version `dsh-analyze@0.1.5`.

## 0.1.4

Relation & association increment (corr / crosstab / join anti·semi).

### Added

- CLI + tools: `corr` / `dataset_corr` — pairwise Pearson or Spearman among
  2–16 numeric-capable columns; pairs sorted by \|r\| (cap 50) with per-pair `n`.
- CLI + tools: `crosstab` / `dataset_crosstab` — two-column contingency table
  (top categories per dimension, matrix, margins, top cells).
- `join` / `dataset_join`: `--how` extends to `anti` \| `semi` (left rows only;
  no right payload columns; null keys never match).

### Changed

- Version and `version` capabilities: `0.1.4` plus `corr`, `crosstab`.
- Public contract updated in `docs/interface.md` (v0.1.4).
- Package version `dsh-analyze@0.1.4`.

## 0.1.3

Data-quality increment for agent sessions (profile / dedup / outliers + long-tail quantiles).

### Added

- CLI + tools: `profile` / `dataset_profile` — quality profile (null rate, nunique,
  type stability, high-cardinality flags, suspect primary keys, coarse warnings).
- CLI + tools: `dedup` / `dataset_dedup` — exact whole-row or keyed duplicate
  detection with bounded sample groups.
- CLI + tools: `outliers` / `dataset_outliers` — `iqr` | `zscore` | `mad` numeric
  outliers; optional `--out` writes a new dataset of outlier rows.
- `describe` / `stats_describe`: additive quantile fields `q01` / `q10` / `q90` / `q99`
  (same nearest-rank estimator as existing quartiles).

### Changed

- Version and `version` capabilities: `0.1.3` plus `profile`, `dedup`, `outliers`.
- Public contract updated in `docs/interface.md` (v0.1.3).
- Package version `dsh-analyze@0.1.3`.
- `dedup` / `outliers` `--sample-limit` is strict: non-integer or `< 1` → exit 2
  (was silently clamped to the default). Values `> 20` still clamp to 20.

## 0.1.2

Analysis capability increment for agent sessions (small JSON tool calls; large tables stay on disk).

### Added

- CLI + tools: `values` / `dataset_values` — column value distribution
  (`nunique`, `nullCount`, top values with `count` / `ratio`).
- CLI + tools: `sample` / `dataset_sample` — deterministic SplitMix64 sampling
  (default seed 42); optional `stratifyBy` for proportional strata.
- CLI + tools: `join` / `dataset_join` — equi-join (`inner` | `left`) writing a
  new dataset; response is ids + counts only (never full rows).
- `transform_agg` `mutate`: derived columns via whitelist arithmetic expressions
  (identifiers, numbers, `+ - * /`, parentheses). No function calls.
- `transform_agg` time-period `groupBy`:
  `{"dateColumn":"…","period":"day|week|month"}` with UTC bucketing;
  unparseable non-null values land in the null bucket and report
  `parseFailCount`.

### Changed

- Version and `version` capabilities: `0.1.2` plus `values`, `sample`, `join`.
- Public contract frozen in `docs/interface.md` (v0.1.2).
- Neutral wording in user-facing docs and module paths.

### Security

- Mutate expressions are validated in the TS tool layer and re-checked in the
  CLI; invalid expressions abort before any `--out` write.
- Profile credential path deny list remains enforced on ingest (see
  `docs/security.md`).
