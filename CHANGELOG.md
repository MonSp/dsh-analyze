# Changelog

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
