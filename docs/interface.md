# dsh-analyze interface specification (v0.1.5)

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
| Version | `0.1.5` |

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
{"ok":true,"version":"0.1.5","capabilities":["ingest","head","schema","describe","agg","list","export","rm","values","sample","join","profile","dedup","outliers","corr","crosstab","series"]}
```

Capability tokens: the v0.1.4 set plus `series` (appended, never inserted
mid-list). Mutate, time-period grouping, extended `describe` quantiles,
and join `anti`/`semi` ship inside existing commands and do not add separate
capability tokens.

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
{"ok":true,"datasetId":"ds_…","stats":[{"name":"…","dtype":"…","nullCount":0,"min":0,"max":0,"mean":0,"std":0,"q25":0,"q50":0,"q75":0,"q01":0,"q10":0,"q90":0,"q99":0}]}
```

- Numeric columns always include `q01`/`q10`/`q90`/`q99` (same nearest-rank
  estimator as `q25`/`q50`/`q75`). Non-numeric columns set all quantile fields
  to `null` (additive fields; existing hosts may ignore them).

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

Flags: `--left <id>` (required), `--right <id>` (required), `--on a,b` (required, non-empty list of shared column names), `--how inner|left|anti|semi` (default `inner`), `--out <label>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…"}],"leftDatasetId":"ds_…","rightDatasetId":"ds_…","how":"inner"}
```

- Does **not** return sample rows. Callers use `dataset_head` / `agg` / `export` on the new id.
- Column layout for `inner`/`left`: all left columns (including keys) first, then right columns **excluding** key columns. On name collision, right columns get suffix `_r` (repeat `_r` until unique).
- Column layout for `anti`/`semi`: **left columns only** (no right payload).
- Key equality: same `Value` tag (I64==I64, Str==Str, …). `Null` keys never match (SQL-style).
- `how=inner`: rows where every `on` key matches on both sides.
- `how=left`: all left rows; unmatched right cells are `null`.
- `how=semi`: left rows whose key matches **≥1** right row (each left row at most once).
- `how=anti`: left rows whose key matches **0** right rows (null keys count as no match).
- Multi-match is a cross product per matching key group for `inner`/`left` only.
- Result row count must be ≤ `DSH_ANALYZE_MAX_ROWS` else `analysis/limit` (exit 3). Nothing is written on limit failure.
- New meta: `producedBy: "join"`, `parentDatasetId` = left dataset id, `label` = `--out` when provided, `sourceKind: "join"`.
- Missing `--on` column on either side → `analysis/invalid-args`.
- Bad `--how` → `analysis/invalid-args: --how must be inner|left|anti|semi`.

### `corr` (v0.1.4)

Pairwise correlation among numeric-capable columns. Summary only — never a full dense matrix dump beyond the pair cap.

Flags: `--dataset-id` (required), `--columns a,b[,c…]` (required, 2..=16), `--method pearson|spearman` (default `pearson`), `--pairwise complete|omit` (default `complete`), `--min-numeric <n>` (integer ≥ 2, default 2)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"method":"pearson","pairwise":"complete","minNumeric":2,"columns":["a","b","c"],"pairCount":3,"pairs":[{"columns":["a","b"],"n":123,"r":0.87,"degenerate":false}],"truncated":false}
```

- Response uses a **pairs array** (upper triangle), sorted by `|r|` desc (nulls last), then `n` desc, then input column order. Cap **50** pairs; more → `truncated: true`.
- Each pair has `n` (rows contributing under `--pairwise`). `n < --min-numeric` → `r: null`, `degenerate: false`.
- Non-numeric-capable column (zero `as_f64` successes) → exit 2 `analysis/invalid-args: column {name} is not numeric-capable`.
- Degenerate (either variance 0 after enough samples) → `r: null`, `degenerate: true`.
- `--pairwise complete`: pairwise deletion (each pair uses its own complete rows).
- `--pairwise omit`: listwise deletion (only rows where **all** listed columns are non-null numeric).
- Spearman: mid-ranks (average ranks for ties) then Pearson on ranks.

### `crosstab` (v0.1.4)

Two-column contingency table. Summary + top cells only.

Flags: `--dataset-id` (required), `--rows <col>` (required), `--cols <col>` (required, ≠ rows), `--top <n>` (1..=50, default 20), `--min-count <n>` (integer ≥ 1, default 1), `--drop-null true|false` (default `true`)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"rowColumn":"a","colColumn":"b","rowCategories":["x","y"],"colCategories":["1","2"],"matrix":[[c00,c01],[c10,c11]],"rowTotals":[…],"colTotals":[…],"grandTotal":N,"cellCount":N,"truncated":true,"nullRowCount":0,"topCells":[{"row":"x","col":"1","count":10}]}
```

- Categories: distinct `Value` JSON forms (same encoding as `values`); selected by marginal count desc, then first-seen. Length ≤ `--top`. With the default `--drop-null true`, only non-null categories appear. With `--drop-null false`, null participates as JSON string category `__null__` (a data string `"__null__"` is a separate category with the same label). Mixed numeric kinds that share a JSON form (e.g. integer `1` vs float `1.0`) may yield repeated labels in the category list.
- `matrix` is `len(rowCategories) × len(colCategories)`; margins only cover the kept grid.
- `truncated`: distinct categories exceeded `--top` or cells fell outside the selected grid.
- `topCells`: cells with `count >= --min-count`, count desc, cap 50.
- Nulls: `--drop-null true` (default) excludes rows with null on either side → counted in `nullRowCount`; `--drop-null false` keeps them as the `__null__` category and leaves `nullRowCount` at 0.
- Algorithm: one `HashMap` pass over cell pairs — never materializes full cartesian of raw distinct values.

### `profile`

Dataset quality profile (v0.1.3). Summary only — never full columns.

Flags: `--dataset-id` (required), `--high-cardinality-threshold <n>` (optional, default 200, integer ≥ 1)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…","nullCount":0,"nullRate":0.0,"nunique":0,"uniqueRate":0.0,"highCardinality":false,"typeStability":"int|string|float|bool|mixed|mixed_numeric|empty","numericCount":0,"numericShare":0.0,"boolCount":0,"sampleValues":[]}],"suspectKeys":[{"columns":["…"],"reason":"unique_non_null","nullCount":0,"nunique":0}],"warnings":[{"code":"high_cardinality","column":"…","nunique":0,"threshold":200}]}
```

- `nunique` counts distinct non-null cells (same rule as `values`).
- `typeStability` is runtime observation of non-null cell kinds; may disagree with stored schema `dtype`.
- `sampleValues`: up to 3 distinct non-null values, first-seen order.
- `suspectKeys`: single-column `unique_non_null` candidates (max 10); composite keys deferred.
- `warnings` codes: `empty_table`, `high_cardinality`, `all_null`, `mostly_null`, `mixed_types`, `warnings_truncated` (cap 50).

### `dedup`

Exact whole-row or keyed duplicate detection. Samples only — never the full duplicate set.

Flags: `--dataset-id` (required), `--key-columns a,b` (optional, 1–16 shared columns), `--sample-limit <n>` (1–20, default 5; invalid integer → exit 2)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"mode":"exact","keyColumns":null,"duplicateRowCount":0,"duplicateRowRate":0.0,"distinctRowCount":0,"duplicateRowCountGroups":0,"maxMultiplicity":1,"samples":[{"key":null,"count":2,"rows":[[…],[…]]}],"samplesTruncated":false}
```

Keyed mode uses `"mode":"keyed"`, `"keyColumns":["…"]`, and replaces `distinctRowCount` / `duplicateRowCountGroups` with `distinctKeyCount` / `duplicateKeyCount`.

- Exact mode: fingerprint is the whole row; `samples[].key` is `null`.
- Keyed mode: fingerprint is the key columns in `--key-columns` order; `samples[].key` is that key cell array.
- `samples` ordered by group `count` desc, then first-seen row index asc; each sample group keeps at most 3 earliest rows.
- Row cells in samples are full source rows (all columns).

### `outliers`

Numeric outlier detection. Samples only; optional `--out` writes a new dataset.

Flags: `--dataset-id` (required), `--columns a,b` (required, 1–32 numeric-capable), `--method iqr|zscore|mad` (default `iqr`), `--k <f>` (finite > 0; default 1.5 for iqr else 3.0), `--sample-limit <n>` (1–20, default 10; invalid integer → exit 2), `--out <label>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"method":"iqr","k":1.5,"outDatasetId":null,"columns":[{"name":"…","dtype":"…","numericCount":0,"nullCount":0,"lowerBound":0,"upperBound":0,"outlierCount":0,"outlierRate":0.0,"degenerateIqr":false,"degenerateStd":false,"degenerateMad":false,"samples":[{"rowIndex":0,"value":0,"row":[…]}]}]}
```

- Methods: `iqr` bounds `Q1-k·IQR` / `Q3+k·IQR`; `zscore` uses sample std; `mad` uses `1.4826·MAD` around Q50.
- Degenerate cases (`IQR==0`, `std==0`, `MAD==0`) report the matching `degenerate*` flag and no outliers.
- Only cells where `as_f64()` succeeds count as numeric; nulls/non-numeric are excluded from bounds.
- `--out`: persists rows that are outliers in **any** listed column (`producedBy: "outliers"`, `sourceKind: "outliers"`). Response never returns full outlier rows; use `head`/`agg` on `outDatasetId`. Limit → exit 3, nothing written.

### `series`

Time-series transforms. Always writes a new dataset (ids + schema only; no sample rows).

Flags: `--dataset-id` (required), `--order-by <col>` (required), `--ops <json-or-path>` (required, 1–8 ops), `--out <label>` (optional label)

```text
--ops may be an inline JSON array or a workspace-relative file path (UTF-8 BOM stripped).
```

```json
[{"fn":"lag","column":"v","periods":1,"as":"v_lag"},
 {"fn":"lead","column":"v","periods":1,"as":"v_lead"},
 {"fn":"diff","column":"v","periods":1,"as":"v_diff"},
 {"fn":"pct_change","column":"v","periods":1,"as":"v_pct"},
 {"fn":"rolling","column":"v","window":7,"agg":"mean","as":"v_ma7"}]
```

| Field | Required | Applies to | Rules |
|---|---|---|---|
| `fn` | yes | all | `lag` \| `lead` \| `diff` \| `pct_change` \| `rolling` |
| `column` | yes | all | Existing source column |
| `as` | yes | all | New column: `[A-Za-z_][A-Za-z0-9_]*`, ≤64 chars; no collision with input columns or prior `as` |
| `periods` | no (default 1) | lag/lead/diff/pct_change | Integer `1..=10000` |
| `window` | yes | rolling | Integer `1..=10000` |
| `agg` | yes | rolling | `mean` \| `sum` \| `min` \| `max` |

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…"}],"sourceDatasetId":"ds_…","orderBy":"…","opCount":N}
```

- Sequence: stable sort by `--order-by` ascending; **null keys last** (rows kept); ties keep input order. Cross-type key order: Bool < number < Str.
- `lag`/`lead`: copy source value `periods` steps back/forward; out of range → Null. Output dtype = source dtype.
- `diff`: `src(i) - src(i-p)` when both are finite numbers (`as_f64`); else Null. I64−I64 with integer result → i64, else f64.
- `pct_change`: `(src(i)-src(i-p))/src(i-p)`; prev 0 or non-numeric → Null; output f64.
- `rolling`: trailing positions `i-window+1..=i`. If fewer than `window` non-null finite values → Null. Else `mean`/`sum`/`min`/`max` over those non-null values. mean/sum → f64. min/max: cell values and schema dtype are i64 only when the **whole source column** is all I64/null; any float (or non-numeric) in the source forces f64 — a window of ints alone does not promote.
- No rows dropped. `producedBy: "series"`, `sourceKind: "series"`, `source_path: series:<src>`.
- Invalid ops / unknown column / empty ops / >8 ops → exit 2. No sample rows in the success body.

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
`dataset_values` · `dataset_sample` · `dataset_join` ·
`dataset_profile` · `dataset_dedup` · `dataset_outliers` ·
`dataset_corr` · `dataset_crosstab` · `dataset_series`

| Tool | CLI | Notes |
|---|---|---|
| `dataset_values` | `values` | requires `column`; `limit` 1–100 default 20 |
| `dataset_sample` | `sample` | `n` 1–20 default 10; optional `seed`, `stratifyBy` |
| `dataset_join` | `join` | writes new dataset; returns ids + counts only; `how` inner\|left\|anti\|semi |
| `dataset_profile` | `profile` | optional `highCardinalityThreshold` (default 200) |
| `dataset_dedup` | `dedup` | optional `keyColumns` (1–16); `sampleLimit` integer 1–20 default 5 |
| `dataset_outliers` | `outliers` | `method` iqr\|zscore\|mad; optional `k`, `outLabel`; `sampleLimit` integer 1–20 default 10 |
| `dataset_corr` | `corr` | 2..16 numeric-capable columns; `method` pearson\|spearman; `pairwise` complete\|omit; `minNumeric` integer ≥2 default 2 |
| `dataset_crosstab` | `crosstab` | `rows`/`cols` required and distinct; `top` 1–50 default 20; `minCount` ≥1 default 1; `dropNull` boolean default true |
| `dataset_series` | `series` | `orderBy` required; `ops` array 1–8 of lag\|lead\|diff\|pct_change\|rolling; always writes new dataset |

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
- Window functions, UDF / JS / Python expressions, diff of two datasets
- Full correlation-matrix export, p-values / confidence intervals
- Crosstab with three or more dimensions
- Centered series windows, ewm / cumsum / expanding, multi-key series order

(Duplicate detection, outlier detection shipped in v0.1.3; correlation, crosstab,
and anti/semi join shipped in v0.1.4; series lag/lead/diff/pct_change/rolling
shipped in v0.1.5.)
