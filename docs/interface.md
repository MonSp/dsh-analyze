# dsh-analyze interface specification (v0.1)

This document is the **public contract** for the CLI binary and the DSH tool
plugin. Hosts and tests should depend only on this spec.

## 1. Identity

| Item | Value |
|---|---|
| Binary | `dsh-analyze` (`dsh-analyze.exe` on Windows) |
| npm / plugin | `dsh-analyze` |
| DatasetId | `ds_` + 8–32 lowercase hex |
| Data root | `<workspace>/.dsh-analyze/{datasets,outputs,tmp}` |
| License | MIT |

## 2. Invocation

```text
dsh-analyze --workspace <dir> [--pretty] <cmd> [flags...]
```

- `--workspace` is required for every command except `version` / `--version`.
- stdout: **one JSON line** on success (`{"ok":true,...}`).
- failure: non-zero exit code + last stderr line starting with `analysis/`.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | I/O or internal CLI failure (`analysis/cli-failed`) |
| 2 | invalid arguments / path (`analysis/invalid-args`, `analysis/invalid-path`) |
| 3 | limit exceeded (`analysis/limit`) |
| 4 | format not enabled (e.g. parquet in T0) |
| 5 | unknown dataset (`analysis/unknown-dataset`) |

## 3. Environment

| Variable | Default | Notes |
|---|---|---|
| `DSH_ANALYZE_MAX_INGEST_BYTES` | 268435456 | ingest byte cap |
| `DSH_ANALYZE_MAX_ROWS` | 5000000 | ingest row cap |
| `DSH_ANALYZE_DEFAULT_ROW_LIMIT` | 1000 | agg default limit |
| `INV_ANALYSIS_*` | — | legacy fallback (new name wins) |

TS bridge only:

| Variable | Default |
|---|---|
| `DSH_ANALYZE_CLI_PATH` | — (else PATH `dsh-analyze`) |
| `DSH_ANALYZE_WORKSPACE` | `process.cwd()` |

## 4. CLI subcommands

### `version`

```json
{"ok":true,"version":"0.1.0","capabilities":["ingest","head","schema","describe","agg","list","export","rm"]}
```

### `ingest`

Flags: `--input <path>` (required), `--format csv|jsonl|parquet`, `--dataset-id <id>`, `--label <text>`

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…"}],"bytesOnDisk":N}
```

- `--input` must resolve inside `--workspace`.
- parquet is rejected with exit 4 in T0.

### `head`

Flags: `--dataset-id` (required), `--n` (1–20, default 5)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columns":["…"],"rows":[[…]]}
```

### `schema`

Flags: `--dataset-id` (required)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columns":[{"name":"…","dtype":"…"}]}
```

### `describe`

Flags: `--dataset-id` (required), `--columns a,b,c` (optional subset)

```json
{"ok":true,"datasetId":"ds_…","stats":[{"name":"…","dtype":"…","nullCount":0,"min":0,"max":0,"mean":0,"std":0,"q25":0,"q50":0,"q75":0}]}
```

### `agg`

Flags: `--dataset-id` (required), `--spec <json-or-path>` (required), `--out <label>` (optional)

Spec JSON:

```json
{
  "select": ["col"],
  "filter": [{"column":"col","op":"eq","value":1}],
  "groupBy": ["col"],
  "aggs": [{"fn":"count","as":"n"},{"fn":"sum","column":"col","as":"s"}],
  "orderBy": [{"column":"s","desc":true}],
  "limit": 100
}
```

Filter ops: `eq ne gt gte lt lte in contains is_null not_null`  
Agg fns: `count sum mean min max median std`  
Limit range: `1..5000` (default `DSH_ANALYZE_DEFAULT_ROW_LIMIT`).  
`--spec` may be inline JSON or a workspace-relative file path.

```json
{"ok":true,"datasetId":"ds_…","outDatasetId":null,"rowCount":N,"columns":["…"],"rows":[[…]],"truncated":false}
```

When `--out` is set, a new dataset is persisted and `outDatasetId` is set;
inline `rows` may be empty with `truncated: true` (budget 20 rows).

### `list`

```json
{"ok":true,"datasets":[{"datasetId":"ds_…","rowCount":N,"columnCount":N,"label":null,"producedBy":"ingest","parentDatasetId":null,"sourceKind":"csv"}]}
```

### `export`

Flags: `--dataset-id` (required), `--output <path>` (required)

- Output is forced under `<workspace>/.dsh-analyze/outputs/` (file name kept).
- CSV only in T0.

```json
{"ok":true,"outputPath":".dsh-analyze/outputs/….csv","rowCount":N,"bytes":N}
```

### `rm`

Flags: `--dataset-id` (required)

```json
{"ok":true,"datasetId":"ds_…","removed":true}
```

## 5. Model-facing tools

The DSH plugin registers the same operations as tools:

`dataset_ingest` · `dataset_head` · `dataset_schema` · `dataset_list` ·
`dataset_export` · `dataset_rm` · `stats_describe` · `transform_agg`

Tool-layer extra guards:

- path / label length caps
- workspace double-check before spawn
- profile credential path deny list (see security.md)
- per-workspace FIFO CLI lock (one session never overlaps its own CLI calls)

## 6. Path confinement

1. **Tool layer**: `resolveInWorkspace` — relative paths resolve under the
   session workspace; absolute escapes throw `analysis/invalid-path`.
2. **CLI layer**: `ensure_within_workspace` — canonicalize + prefix check
   after the path leaves the tool process.

Ingest additionally rejects profile-sensitive prefixes when a profile root
is known: `.bootstrap-token`, `profiles/`,
`.credentials.yaml`, `.writer.lock`.

## 7. Non-goals (until v1)

- Online database connectors
- Arbitrary code execution (`run_rust_analysis`)
- Write-side SQL or mutating external systems
- Parquet (planned behind a feature flag after T0)
