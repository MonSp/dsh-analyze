# dsh-analyze

[English](README.md) | [中文](README.zh.md)

**Local dataset analysis tools for [DSH](https://github.com/deepseek-ai/deepseek-harness).**  
Ingest a CSV/JSONL once, keep large tables on disk, and let the model work
through small JSON tool calls instead of dumping entire files into context.

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
| Local **file** dataset handles (CSV / JSONL) | A SQL database connector |
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

The crate is pure-std and builds offline.

## Tools

| Tool | Purpose |
|---|---|
| `dataset_ingest` | Load a workspace CSV/JSONL → `datasetId` + schema summary |
| `dataset_head` | Up to 20 sample rows |
| `dataset_schema` | Column names, dtypes, row count |
| `dataset_list` | All datasets in this workspace store |
| `dataset_export` | Write full table to CSV under outputs/ |
| `dataset_rm` | Permanently remove a dataset |
| `stats_describe` | Null counts, min/max/mean/std/quartiles |
| `transform_agg` | Filter / mutate / project / group-by / aggregate (incl. time buckets) |
| `dataset_values` | Top value distribution for one column (`nunique`, `top`) |
| `dataset_sample` | Deterministic random or stratified sample (≤ 20 rows) |
| `dataset_join` | Equi-join two datasets → new on-disk dataset (ids + counts only) |

CLI mirrors the same names as subcommands (`values`, `sample`, `join`, …).
See [docs/interface.md](docs/interface.md) for the frozen JSON contract.

### `transform_agg` extras (v0.1.2)

- **`mutate`**: derived columns with a whitelist expression
  (`[A-Za-z_][A-Za-z0-9_]*`, numbers, `+ - * /`, parentheses).
  No function calls. Invalid expr → `analysis/invalid-args`.
- **Time `groupBy`**: `{"dateColumn":"created_at","period":"day|week|month"}`
  → column `created_at_day` etc. Unparseable non-null values go to the
  `null` bucket and increment `parseFailCount` (never silently dropped).

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
| Input | Local CSV/JSONL | Live database | Anything |
| Network | Never | Required | Often |
| Model token cost | Small JSON per step | Query results | Kernel output |
| Runtime | Single static binary | Drivers + server | Full interpreter |

## Security

Path confinement, credential-path deny lists, env whitelist, and hard
limits — see [SECURITY.md](SECURITY.md) and [docs/security.md](docs/security.md).

## License

MIT — see [LICENSE](LICENSE).
