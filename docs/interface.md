# dsh-analyze interface specification (v0.1.2)

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
| Version | `0.1.2` |

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
| 4 | format not enabled (e.g. parquet) |
| 5 | unknown dataset (`analysis/unknown-dataset`) |

## 3. Environment

| Variable | Default | Notes |
|---|---|---|
| `DSH_ANALYZE_MAX_INGEST_BYTES` | 268435456 | ingest byte cap |
| `DSH_ANALYZE_MAX_ROWS` | 5000000 | ingest row cap |
| `DSH_ANALYZE_DEFAULT_ROW_LIMIT` | 1000 | agg default limit |
| `DSH_ANALYZE_DEFAULT_SAMPLE_SEED` | 42 | sample seed when `--seed` omitted |
| `INV_ANALYSIS_*` | — | legacy fallback (new name wins) |

TS bridge only:

| Variable | Default |
|---|---|
| `DSH_ANALYZE_CLI_PATH` | — (else PATH `dsh-analyze`) |
| `DSH_ANALYZE_WORKSPACE` | `process.cwd()` |

## 4. CLI subcommands

### `version`

```json
{"ok":true,"version":"0.1.2","capabilities":["ingest","head","schema","describe","agg","list","export","rm","values","sample","join"]}
```

Capability tokens for this version: the eight v0.1 tokens plus `values`,
`sample`, `join`. Mutate and time-period grouping ship inside `agg` and do
not add a separate capability token.

### `ingest`

Flags: `--input <path>` (required), `--format csv|jsonl|parquet`, `--dataset-id <id>`, `--label <text>`

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…"}],"bytesOnDisk":N}
```

- `--input` must resolve inside `--workspace`.
- parquet is rejected with exit 4 (not enabled).

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

### `values`

Column value distribution (top values + uniqueness).

Flags: `--dataset-id` (required), `--column` (required), `--limit` (1–100, default 20)

```json
{"ok":true,"datasetId":"ds_…","column":"…","dtype":"…","rowCount":N,"nullCount":N,"nunique":N,"limit":20,"top":[{"value":…,"count":N,"ratio":0.0}]}
```

- `null` cells are counted in `nullCount` and **excluded** from `top`.
- `nunique` counts distinct non-null values.
- `ratio` = `count / max(rowCount - nullCount, 1)` (share of non-null rows).
- `top` sorted by `count` desc, then stable by JSON form of `value` asc.
- `limit` clamps the length of `top` only; does not change `nunique`.

### `sample`

Deterministic row sample. Never returns more than 20 rows.

Flags: `--dataset-id` (required), `--n` (1–20, default 10), `--seed` (u64, default `DSH_ANALYZE_DEFAULT_SAMPLE_SEED` or 42), `--stratify-by <column>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columns":["…"],"rows":[[…]],"sampling":{"method":"random"|"stratified","n":10,"seed":42,"stratifyBy":null|"…","population":N}}
```

- PRNG: SplitMix64 seeded by `seed`. Same `(dataset, n, seed, stratifyBy)` always yields the same rows.
- Without `--stratify-by`: sample without replacement over the full row set.
- With `--stratify-by`: proportional allocation across strata (largest-remainder), then independent SplitMix64 draws per stratum. Null / empty stratum key is the string `__null__` in the key space only (not written into rows).
- If `n >= population`, returns all rows in original order and `sampling.method` is still the requested method.

### `agg`

Flags: `--dataset-id` (required), `--spec <json-or-path>` (required), `--out <label>` (optional)

Spec JSON:

```json
{
  "select": ["col"],
  "filter": [{"column":"col","op":"eq","value":1}],
  "mutate": [{"as":"rate","expr":"hits / total"}],
  "groupBy": ["col", {"dateColumn":"created_at","period":"day"}],
  "aggs": [{"fn":"count","as":"n"},{"fn":"sum","column":"col","as":"s"}],
  "orderBy": [{"column":"s","desc":true}],
  "limit": 100
}
```

Filter ops: `eq ne gt gte lt lte in contains is_null not_null`  
Agg fns: `count sum mean min max median std`  
Limit range: `1..5000` (default `DSH_ANALYZE_DEFAULT_ROW_LIMIT`).  
`--spec` may be inline JSON or a workspace-relative file path.

Pipeline order (v0.1.2):

1. `filter` (AND across predicates)
2. `mutate` — each `as` becomes a derived column before grouping
3. `groupBy` — plain column names and time-period objects
4. `aggs`
5. `orderBy` / `limit` / `select`

#### `mutate` (derived columns)

- Each item: `{"as": "<new-column>", "expr": "<expression>"}`.
- `as` must be a new non-empty identifier (`[A-Za-z_][A-Za-z0-9_]*`, ≤64 chars) and must not collide with an existing column name or a previous `as`.
- Expression grammar (whitelist only):

```text
expr    := term (('+' | '-') term)*
term    := unary (('*' | '/') unary)*
unary   := '-' unary | primary
primary := number | identifier | '(' expr ')'
number  := integer or decimal literal (optional leading '-')
identifier := [A-Za-z_][A-Za-z0-9_]*
```

- No function calls, no string literals, no indexing, no assignment, no commas.
- Identifiers resolve to **original** column names (not other `mutate` outputs from the same list — mutate columns are computed in order and earlier derived names **are** visible to later exprs).
- Null / non-numeric operand → result Null for that cell. Division by zero → Null.
- Invalid expression (parse error, unknown identifier at eval time, illegal char) → `analysis/invalid-args`. No partial apply: the CLI validates and evaluates against the filtered working table; any error aborts the whole `agg` before `--out` is written.

#### Time-period `groupBy`

A `groupBy` array element is either:

- a column name string, or
- an object `{"dateColumn": "<col>", "period": "day"|"week"|"month"}`.

Output column name is `"<dateColumn>_<period>"` (e.g. `created_at_day`).

Accepted timestamp text (after trim):

| Form | Example |
|---|---|
| date | `2024-01-31` |
| datetime | `2024-01-31T15:04:05` |
| datetime + Z | `2024-01-31T15:04:05Z` |
| datetime + offset | `2024-01-31T23:04:05+08:00` |

Timezone rule: values without an explicit offset are treated as **UTC**. With
offset, the instant is converted to UTC before bucketing.

Buckets:

| period | Bucket key |
|---|---|
| `day` | `YYYY-MM-DD` (UTC calendar day) |
| `week` | `YYYY-MM-DD` of the **Monday** containing that UTC day (ISO week start) |
| `month` | `YYYY-MM` |

- Unparseable **non-null** values go to the JSON `null` bucket and increment
  `parseFailCount` in the response (field present only when > 0).
- Cell `null` / empty string go to the `null` bucket and do **not** count as parse failures.
- Silent drop of all rows is forbidden: every filtered row lands in exactly one bucket.

```json
{"ok":true,"datasetId":"ds_…","outDatasetId":null,"rowCount":N,"columns":["…"],"rows":[[…]],"truncated":false,"parseFailCount":0}
```

`parseFailCount` is omitted or `0` when there is no time-period grouping or no failures.

When `--out` is set, a new dataset is persisted and `outDatasetId` is set;
inline `rows` may be empty with `truncated: true` (budget 20 rows).

### `join`

Equi-join on one or more key columns. Result is **always** a new dataset.

Flags: `--left <id>` (required), `--right <id>` (required), `--on a,b` (required, non-empty list of shared column names), `--how inner|left` (default `inner`), `--out <label>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…"}],"leftDatasetId":"ds_…","rightDatasetId":"ds_…","how":"inner"}
```

- Does **not** return sample rows. Callers use `dataset_head` / `agg` / `export` on the new id.
- Column layout: all left columns (including keys) first, then right columns **excluding** key columns. On name collision, right columns get suffix `_r` (repeat `_r` until unique).
- Key equality: same `Value` tag (I64==I64, Str==Str, …). `Null` keys never match (SQL-style).
- `how=inner`: rows where every `on` key matches on both sides.
- `how=left`: all left rows; unmatched right cells are `null`.
- Multi-match is a cross product per matching key group (standard relational join).
- Result row count must be ≤ `DSH_ANALYZE_MAX_ROWS` else `analysis/limit` (exit 3). Nothing is written on limit failure.
- New meta: `producedBy: "join"`, `parentDatasetId` = left dataset id, `label` = `--out` when provided, `sourceKind: "join"`.
- Missing `--on` column on either side → `analysis/invalid-args`.

### `list`

```json
{"ok":true,"datasets":[{"datasetId":"ds_…","rowCount":N,"columnCount":N,"label":null,"producedBy":"ingest","parentDatasetId":null,"sourceKind":"csv"}]}
```

### `export`

Flags: `--dataset-id` (required), `--output <path>` (required)

- Output is forced under `<workspace>/.dsh-analyze/outputs/` (file name kept).
- CSV only.

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
`dataset_export` · `dataset_rm` · `stats_describe` · `transform_agg` ·
`dataset_values` · `dataset_sample` · `dataset_join`

| Tool | CLI | Notes |
|---|---|---|
| `dataset_values` | `values` | requires `column`; `limit` 1–100 default 20 |
| `dataset_sample` | `sample` | `n` 1–20 default 10; optional `seed`, `stratifyBy` |
| `dataset_join` | `join` | writes new dataset; returns ids + counts only |

`transform_agg` `spec` string gains optional `mutate` and time-period
`groupBy` objects (see §4 `agg`). Tool-layer `validateAggSpec` must reject
illegal mutate expressions with `analysis/invalid-args` before spawn.

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

Ingest additionally rejects profile-sensitive first path segments when a
profile root is known. The deny list is a defensive path guard only; see
`docs/security.md`.

## 7. Non-goals (until later versions)

- Online database connectors
- Arbitrary code execution
- Write-side SQL or mutating external systems
- Parquet (planned behind a feature flag; still exit 4 in this version)
- Related matrices, outlier detection, diff, duplicate detection
- UDF / JS / Python expressions
