# Changelog

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
