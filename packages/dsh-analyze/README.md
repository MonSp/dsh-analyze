# dsh-analyze

[English](README.md) | [中文](README.zh.md)

**Local dataset analysis tools for [DSH](https://github.com/deepseek-ai/deepseek-harness).**  
Ingest a CSV/JSON/JSONL/Parquet once, keep large tables on disk, and let the
model work through small JSON tool calls instead of dumping entire files
into context.

```text
Without dsh-analyze          With dsh-analyze
─────────────────────        ─────────────────────────────
paste whole CSV → tokens     dataset_ingest  → { datasetId }
context blows up             dataset_head    → 5 sample rows
no audit trail               stats_describe  → column stats
                             transform_agg   → grouped result
                             (full table stays on disk)
```

## What it is (and is not)

| Is | Is not |
|---|---|
| Local **file** dataset handles (CSV / JSON / JSONL / Parquet) | A SQL database connector |
| Offline Rust CLI + DSH tool plugin | A Python/R scientific stack |
| Session-workspace sandbox with hard limits | A multi-tenant cloud service |

## Install

```bash
dsh plugin --profile web add dsh-analyze
```

Place the `dsh-analyze` binary on `PATH`, or set:

```bash
export DSH_ANALYZE_CLI_PATH=/path/to/dsh-analyze
```

### Prebuilt binaries

Download from GitHub Releases and verify against `SHA256SUMS`:

| Target | Artifact |
|---|---|
| `x86_64-pc-windows-msvc` | `dsh-analyze-x86_64-pc-windows-msvc.zip` |
| `aarch64-apple-darwin` | `dsh-analyze-aarch64-apple-darwin.tar.gz` |
| `x86_64-apple-darwin` | `dsh-analyze-x86_64-apple-darwin.tar.gz` |
| `x86_64-unknown-linux-gnu` | `dsh-analyze-x86_64-unknown-linux-gnu.tar.gz` |

CLI lookup order: `DSH_ANALYZE_CLI_PATH` → legacy `INV_ANALYSIS_CLI_PATH` →
`dsh-analyze` on `PATH` → plugin logs a warning and registers nothing.

### Build from source

```bash
cargo build --release -p dsh-analyze
```

Runtime is fully offline — no network calls, no database drivers.

## Tools

| Tool | Purpose |
|---|---|
| `dataset_ingest` | Load a workspace CSV/JSON/JSONL/Parquet → `datasetId` + schema summary |
| `dataset_head` | Up to 20 sample rows |
| `dataset_schema` | Column names, dtypes, row count |
| `dataset_list` | All datasets in this workspace store |
| `dataset_export` | Write full table to CSV/XLSX/Parquet under outputs/ |
| `dataset_rm` | Permanently remove a dataset |
| `stats_describe` | Null counts, min/max/mean/std/quartiles |
| `transform_agg` | Filter / window / mutate / project / group-by / aggregate (incl. time buckets) |
| `dataset_values` | Top value distribution for one column (`nunique`, `top`) |
| `dataset_sample` | Deterministic random or stratified sample (≤ 20 rows) |
| `dataset_join` | Equi-join two datasets → new on-disk dataset (ids + counts only) |
| `dataset_profile` | Quality profile: nulls, nunique, type stability, suspect keys |
| `dataset_dedup` | Exact or keyed duplicate detection (counts + sample groups) |
| `dataset_outliers` | IQR / z-score / MAD outliers; optional write of outlier dataset |
| `dataset_corr` | Pairwise Pearson/Spearman correlations (top pairs by \|r\|, with pValue) |
| `dataset_crosstab` | Contingency table (optional third dimension): top categories, matrix, margins, top cells |
| `dataset_series` | Time-series transforms (lag/lead/diff/pct_change/rolling) → new dataset |
| `dataset_union` | Append two datasets (strict or intersect columns) → new dataset |
| `dataset_diff` | Keyed diff of two datasets: added / removed / changed (+ samples) |
| `dataset_pivot` | Long → wide pivot (index × columns → values, with agg) → new dataset |

CLI mirrors the same names as subcommands (`values`, `sample`, `join`, `profile`, …).
See [docs/interface.md](docs/interface.md) for the frozen JSON contract.

### Quality tools (v0.1.3)

- **`dataset_profile`**: one-call “can I use this table?” summary.
- **`dataset_dedup`**: omit `keyColumns` for whole-row exact dupes; pass keys for export-style collisions.
- **`dataset_outliers`**: `method: iqr|zscore|mad`; `outLabel` writes a new dataset — never dumps full outlier lists.
- **`stats_describe`**: also returns `q01` / `q10` / `q90` / `q99` on numeric columns.

### Relation tools (v0.1.4)

- **`dataset_corr`**: 2–16 numeric-capable columns; `method: pearson|spearman`; pairs sorted by \|r\| (cap 50).
- **`dataset_crosstab`**: `rows` × `cols` frequency table with `top` categories and top cells only.
- **`dataset_join`**: `how` now includes `anti` \| `semi` (left rows only; null keys never match).

### Series tools (v0.1.5)

- **`dataset_series`**: one order-by column (nulls last); one-call `lag` / `lead` / `diff` / `pct_change` / `rolling` (mean|sum|min|max).
- Always writes a new dataset; response is ids + schema only — inspect with `dataset_head` / `stats_describe`.

### Set & pivot tools (v0.1.7)

- **`dataset_union`**: `columnsPolicy: strict|intersect` — append right rows under matching columns; `strict` requires identical headers.
- **`dataset_diff`**: optional `on` keys; one call reports added / removed / changed counts with sample key sets (cap 20).
- **`dataset_pivot`**: `index` / `columns` / `values` / `agg` — long → wide; writes a new dataset.

### Window operators & Parquet ingest (v0.1.8)

- **`transform_agg` `window`**: `rank` / `row_number` / `cumsum` / `ewm` — each op appends a column and never changes row count (0..=16 ops).
- **`dataset_ingest` `--format parquet`**: column types map to dtypes; types outside the map (Decimal, Time, …) are rejected with a stable message.

### Export formats (v0.1.9)

- **`dataset_export` `--format`**: `csv` (default) | `xlsx` (header + true cell types) | `parquet` (round-trips through parquet ingest).

### Corr pValue & crosstab third (v0.1.10)

- **`dataset_corr`**: each pair carries `pValue` (two-sided Student-t) alongside `r` / `df` / `n`.
- **`dataset_crosstab`**: optional `third` column returns one 2-D slice (`tables[]`) per third-category.

### Timezone grouping & CSV encodings (v0.1.11)

- **Time `groupBy` `tz`**: `{"dateColumn":…,"period":"day|week|month","tz":"+08:00"|"Asia/Shanghai"}`
  — `day` / `week` (Monday) / `month` boundaries computed on the target
  timezone's calendar. Default stays UTC; date-only values never shift.
  Invalid `tz` → `analysis/invalid-args`.
- **`dataset_ingest` `--encoding`**: `utf-8` (default) | `gbk` | `gb18030` |
  `auto` for CSV — Excel GBK exports load directly; `auto` falls back to
  GB18030 with an `encoding-fallback` warning. Decoding is never lossy.

### `transform_agg` extras (v0.1.2)

- **`mutate`**: derived columns with a whitelist expression — identifiers,
  numbers, `+ - * /`, parentheses, plus the casts `to_int(e)` / `to_str(e)` /
  `to_date(e, fmt)`. No arbitrary function calls. Invalid expr →
  `analysis/invalid-args`.
- **Time `groupBy`**: `{"dateColumn":"created_at","period":"day|week|month"}`
  → column `created_at_day` etc., optional `tz` (v0.1.11, see above).
  Unparseable non-null values go to the `null` bucket and increment
  `parseFailCount` (never silently dropped).
- **`having` / `topKPerGroup` / `select` rename** (v0.1.7): post-aggregate
  filters on result columns, ≤ k rows per group, and output renames.

## Limits & data root

```text
<workspace>/.dsh-analyze/{datasets,outputs,tmp}/
```

| Env | Default | Meaning |
|---|---|---|
| `DSH_ANALYZE_CLI_PATH` | — | CLI executable path |
| `DSH_ANALYZE_WORKSPACE` | `process.cwd()` | Session workspace root |
| `DSH_ANALYZE_MAX_ROWS` | 5,000,000 | Ingest row cap |
| `DSH_ANALYZE_MAX_INGEST_BYTES` | 256 MiB | Ingest byte cap |
| `DSH_ANALYZE_DEFAULT_ROW_LIMIT` | 1000 | Agg default row limit |
| `DSH_ANALYZE_DEFAULT_SAMPLE_SEED` | 42 | Sample seed when omitted |

Legacy `INV_ANALYSIS_*` names are still accepted (new name wins).

Results returned to the model are intentionally small (samples ≤ 20 rows).
Full tables go through `outLabel` / `dataset_export` and stay on disk.

## Comparison

| | dsh-analyze | SQL connectors | Notebook stacks (Python/R) |
|---|---|---|---|
| Input | Local CSV/JSON/JSONL/Parquet | Live database | Anything |
| Network | Never | Required | Often |
| Model token cost | Small JSON per step | Query results | Kernel output |
| Runtime | Single static binary | Drivers + server | Full interpreter |

## Security

Path confinement, credential-path deny lists, env whitelist, and hard
limits — see [SECURITY.md](SECURITY.md) and [docs/security.md](docs/security.md).

## License

MIT — see [LICENSE](LICENSE).
