# dsh-analyze interface specification (v0.1.10)

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
| Version | `0.1.10` |

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
| 4 | format not enabled (xlsx ingest) |
| 5 | unknown dataset (`analysis/unknown-dataset`) |

### Response warnings

Success responses may carry an optional `warnings` array (omitted when empty):

```json
"warnings":[{"code":"clamp","field":"n","requested":500,"actual":100}]
```

- Entry base shape: `code` (required), `message`, `column`, `count` (optional);
  code-specific extra fields are allowed (`field`/`requested`/`actual` for
  `clamp`; `nunique`/`threshold`/`emitted`/`total` for `profile` codes).
- Codes in this version: `mutate-null` (arithmetic produced Null at runtime,
  e.g. divide by zero), `cast-null` (`to_int`/`to_str`/`to_date` conversion
  failed for some rows), `date-parse-fail` (time-period grouping failures,
  alongside `parseFailSample`), `clamp` (requested cap was clamped).
- The tool layer renders `warnings` in every tool result.

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
{"ok":true,"version":"0.1.10","capabilities":["ingest","head","schema","describe","agg","list","export","rm","values","sample","join","profile","dedup","outliers","corr","crosstab","series","union","diff","pivot"]}
```

Capability tokens: the v0.1.6 set plus `union`, `diff`, `pivot` (appended,
never inserted mid-list). Mutate, time-period grouping, extended `describe`
quantiles, `having`, `topKPerGroup`, select rename, join `anti`/`semi`,
`window` ops, parquet ingest/export, export xlsx, corr `pValue`, and crosstab
`third` ship inside existing commands and do not add separate capability tokens.

### `ingest`

Flags: `--input <path>` (required), `--format csv|json|jsonl|parquet` (case-insensitive), `--dataset-id <id>`, `--label <text>`

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columnCount":N,"columns":[{"name":"…","dtype":"…"}],"bytesOnDisk":N}
```

- `--input` must resolve inside `--workspace`.
- `json` accepts a top-level array of objects `[{…},…]` or a wrapper object
  `{"records":[{…},…]}`; non-object elements fail with
  `analysis/invalid-args: bad json record at index {i}`. Columns are the union
  of record keys in first-seen order; dtype inference matches `jsonl`.
- `parquet` is enabled (v0.1.8, R-22). Extension `.parquet` auto-detects when
  `--format` is omitted. Type map:

  | Parquet | dtype | Value |
  |---|---|---|
  | `BOOLEAN` | `bool` | Bool |
  | `INT32` / `INT64` (and unsigned) | `i64` | I64 |
  | `FLOAT` / `DOUBLE` | `f64` | F64 |
  | `BYTE_ARRAY` / `STRING` / `BINARY` | `utf8` | Str |
  | `TIMESTAMP` (s/ms/us/ns) | `utf8` | UTC ISO-8601 string |
  | `DATE` | `utf8` | `YYYY-MM-DD` |

  Dictionary encoding is decoded to the values above; nulls → `Value::Null`.
  Nested `list` / `struct` / `map` → `analysis/invalid-args: parquet nested
  type not supported for column …` (exit 2). Any other type outside the table
  (Decimal, Time, Duration, Interval, Float16, Dictionary value type, …) →
  `analysis/invalid-args: parquet type not supported for column …` (exit 2) —
  never silently read as null. Corrupt / non-parquet masquerade
  → `analysis/cli-failed: not a valid parquet file` (exit 1), no OS text.
  Dataset storage remains the CSV placeholder next to meta (parquet is a
  source format only).
- `xlsx` ingest remains exit 4:
  `analysis/limit: xlsx not supported (convert to csv/jsonl)`.

### `head`

Flags: `--dataset-id` (required), `--n` (1–100, default 5)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columns":["…"],"rows":[[…]],"requestedN":N,"n":N}
```

- `requestedN` echoes the requested `--n`; `n` is the number of rows returned.
- `--n` non-integer or < 1 → exit 2; values above 100 clamp to 100 and add a
  `clamp` warning (`field: "n"`). `rowCount` is the dataset size.

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
{"ok":true,"datasetId":"ds_…","column":"…","dtype":"…","rowCount":N,"nullCount":N,"nunique":N,"requestedLimit":20,"limit":20,"top":[{"value":…,"count":N,"ratio":0.0}]}
```

- `null` cells are counted in `nullCount` and **excluded** from `top`.
- `nunique` counts distinct non-null values.
- `ratio` = `count / max(rowCount - nullCount, 1)` (share of non-null rows).
- `top` sorted by `count` desc, then stable by JSON form of `value` asc.
- `requestedLimit` echoes the requested `--limit`; `limit` is the effective cap
  (clamped to 100, with a `clamp` warning). It clamps the length of `top` only;
  does not change `nunique`.

### `sample`

Deterministic row sample. Never returns more than 200 rows.

Flags: `--dataset-id` (required), `--n` (1–200, default 10), `--seed` (u64, default `DSH_ANALYZE_DEFAULT_SAMPLE_SEED` or 42), `--stratify-by <column>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columns":["…"],"rows":[[…]],"requestedN":N,"n":N,"sampling":{"method":"random"|"stratified","n":10,"seed":42,"stratifyBy":null|"…","population":N}}
```

- `requestedN` echoes the requested `--n`; `n` is the number of rows returned.
  Non-integer or < 1 → exit 2; values above 200 clamp to 200 with a `clamp`
  warning (`field: "n"`).

- `sampling.stratifyBy` is `string | null` — `null` when `--stratify-by` is not given (unstratified), else the strata column name.
- PRNG: SplitMix64 seeded by `seed`. Same `(dataset, n, seed, stratifyBy)` always yields the same rows.
- Without `--stratify-by`: sample without replacement over the full row set.
- With `--stratify-by`: proportional allocation across strata (largest-remainder), then independent SplitMix64 draws per stratum. Null / empty stratum key is the string `__null__` in the key space only (not written into rows).
- If `n >= population`, returns all rows in original order and `sampling.method` is still the requested method.

### `agg`

Flags: `--dataset-id` (required), `--spec <json-or-path>` (required), `--out <label>` (optional)

Spec JSON:

```json
{
  "select": ["col", {"column":"c","as":"r"}],
  "filter": [{"column":"col","op":"eq","value":1}],
  "window": [
    {"fn":"rank","by":["account"],"orderBy":{"column":"amount","desc":true},"as":"r_amount"},
    {"fn":"row_number","by":["account"],"orderBy":{"column":"ts"},"as":"rn"},
    {"fn":"cumsum","column":"amount","by":["account"],"orderBy":{"column":"ts"},"as":"cum_amt"},
    {"fn":"ewm","column":"amount","alpha":0.3,"orderBy":{"column":"ts"},"as":"amt_ewm"}
  ],
  "topKPerGroup": {"by":["region"],"k":3,"orderBy":{"column":"amount","desc":true}},
  "mutate": [{"as":"rate","expr":"hits / total"}],
  "groupBy": ["col", {"dateColumn":"created_at","period":"day"}],
  "aggs": [{"fn":"count","as":"n"},{"fn":"sum","column":"col","as":"s"}],
  "having": [{"column":"n","op":"gt","value":1}],
  "orderBy": [{"column":"s","desc":true}],
  "limit": 100
}
```

Filter ops: `eq ne gt gte lt lte in contains is_null not_null` (see filter
shapes for the boolean-group form).
Agg fns: `count sum mean min max median std count_distinct percentile first last count_if`
Limit range: `1..5000` (default `DSH_ANALYZE_DEFAULT_ROW_LIMIT`).
`--spec` must be a JSON object or a workspace-relative file path
(`analysis/invalid-args: --spec must be JSON object or a workspace path`
otherwise; file read errors never leak OS text).

Spec key shapes (enforced by the CLI and by tool-layer `validateAggSpec`):

- `filter` is either an **array** of predicate objects (AND across items) or a
  **boolean-group object**: `{"all":[item…]}` / `{"any":[item…]}` /
  `{"not":item}` where `item` is a predicate or another group (nesting depth
  ≤ 8; deeper → `analysis/invalid-args: filter nesting too deep`). Empty
  `all` is true, empty `any` is false. Positive array:
  `"filter": [{"column":"amount","op":"gt","value":3000}]`; positive group:
  `"filter": {"any":[{"column":"region","op":"eq","value":"East"},{"column":"region","op":"eq","value":"North"}]}`.
  A bare predicate object `{"filter": {"column":"amount","op":"gt","value":3000}}`
  (or a string) is still rejected (`analysis/invalid-args: filter must be an
  array` for non-object/array shapes; unknown group keys fail), never applied
  as "no filter".
- A filter item needs a `column` string
  (`analysis/invalid-args: filter column required`). Its `value` is a scalar
  (string / number / boolean), required for every op except `is_null` /
  `not_null`. `op: "in"` also accepts an
  **array of scalars** — every member matches (OR); a single scalar counts as
  a one-member set and `[]` matches nothing. Arrays for other ops,
  non-scalar members, and missing values fail with
  `analysis/invalid-args: filter value …` — never a silent first-element
  pick or null default.
- `select` / `mutate` / `groupBy` / `aggs` / `orderBy` must be arrays;
  `select` entries are column-name strings or `{"column":"…","as":"…"}`
  rename objects (`as` required, identifier ≤64 chars; duplicate output names
  → `analysis/invalid-args`); `orderBy` items need a string `column` and an
  optional boolean `desc` (`analysis/invalid-args: orderBy desc must be
  boolean` otherwise); `limit` must be an integer `1..5000`.
  `having` has the same shape as `filter` and is applied to result columns
  (group keys + agg `as` names) after aggregation. `topKPerGroup` is
  `{"by":[col…1..8],"k":int 1..1000,"orderBy":{"column":…,"desc":bool}?}` —
  a row stage (after `window`, before `mutate`) keeping ≤ `k` rows per `by`
  key combination (Null its own group), ordered by `orderBy` (input order
  default). Wrong shapes fail with `analysis/invalid-args`.
- Unknown top-level spec keys are rejected
  (`analysis/invalid-args: unknown spec key …`), never silently dropped.

#### `window` (v0.1.8, R-21)

Optional array `window` (0..=16 ops). Each op writes a **new column** and
**never changes row count**. Pipeline: `filter → window → topKPerGroup →
mutate → group/agg → having → select → orderBy → limit`.

| fn | required | semantics |
|---|---|---|
| `rank` | `by` 1..8, `orderBy`, `as` | Competition rank within partition (1,1,3). Null sort key last. |
| `row_number` | same | Strict 1..n within partition after orderBy (nulls last, stable input order on ties). |
| `cumsum` | `by`, `column`, `orderBy`, `as` | Running sum in orderBy order; **null source contributes 0**. Output `i64` if every non-null source is I64, else `f64` — decided once for the whole column, so value variants always match the declared dtype. First non-null row amount → first cell equals that amount. |
| `ewm` | `column`, `orderBy`, `as`, `alpha`; `by` optional (default whole table) | `y0 = x0` (first non-null); `y_t = (1-α)·y_{t-1} + α·x_t`. Null `x` leaves `y` unchanged; before first non-null → Null. Output `f64`. `alpha ∈ (0,1]`. |

Shared: `as` matches `[A-Za-z_][A-Za-z0-9_]*` ≤64, no collision with existing
or prior window columns. Unknown `fn` / missing required fields / `alpha` out
of range / unknown keys → `analysis/invalid-args` (exit 2). Window columns
are appended after source columns and declared in schema.

Pipeline order (v0.1.10):

1. `filter` (AND array, or `all`/`any`/`not` groups)
2. `window` — append rank/row_number/cumsum/ewm columns (row count unchanged)
3. `topKPerGroup` — per-`by`-group row cap
4. `mutate` — each `as` becomes a derived column before grouping
5. `groupBy` + `aggs`
6. `having` — result-column filter
7. `select` — projection / rename
8. `orderBy` / `limit`

Agg fn notes (v0.1.7):

- `count_distinct`: distinct non-null values of `column`.
- `percentile`: requires `p` number in (0,1); nearest-rank estimate over
  non-null values (same estimator as `describe` quantiles); all-null → `null`.
  Missing / out-of-range `p` → `analysis/invalid-args: percentile requires p
  in (0,1)`.
- `first` / `last`: first / last **non-null** value of `column` in input order.
- `count_if`: counts rows where `where` (a filter predicate or boolean group)
  is true; `where` is required and `column` is not allowed. Predicate semantics
  match `filter`.
- With `aggs` but no `groupBy`, the result is a single row even when the
  filtered table is empty (SQL-style; e.g. `count` yields 0).

#### `mutate` (derived columns)

- Each item: `{"as": "<new-column>", "expr": "<expression>"}`.
- `as` must be a new non-empty identifier (`[A-Za-z_][A-Za-z0-9_]*`, ≤64 chars) and must not collide with an existing column name or a previous `as`.
- Expression grammar (whitelist only):

```text
expr    := term (('+' | '-') term)*
term    := unary (('*' | '/') unary)*
unary   := '-' unary | primary
primary := number | string | identifier | call | '(' expr ')'
call    := ('to_int' | 'to_str') '(' expr ')' | 'to_date' '(' expr ',' string ')'
number  := integer or decimal literal (optional leading '-')
string  := double-quoted text with \" and \\ escapes
identifier := [A-Za-z_][A-Za-z0-9_]*
```

- No other function calls, no indexing, no assignment, no commas outside
  `to_date`. `to_date`'s format string supports the directives `%Y %m %d %H %M %S`
  plus literal separators; unknown directives → `analysis/invalid-args`.
- Identifiers resolve to **original** column names (not other `mutate` outputs from the same list — mutate columns are computed in order and earlier derived names **are** visible to later exprs).
- **Type checking (v0.1.7)**: arithmetic (`+ - * /`) and unary minus require
  numeric operands (`i64`/`f64`); a `utf8` / `bool` / date operand is a plan-time
  error → `analysis/invalid-args` (exit 2), e.g. `mutate x: cannot add number to
  string`. Verbs: `add {rhs} to {lhs}`, `subtract {rhs} from {lhs}`,
  `multiply {lhs} by {rhs}`, `divide {lhs} by {rhs}`, `negate {type}` (types:
  `number` / `string` / `boolean` / `date`). `to_date` products are `date`
  (accepted by time-period `groupBy`, rejected by arithmetic); `to_int` →
  `number`, `to_str` → `string`.
- Null operand value → result Null (propagation, not an error). Division by
  zero → Null and counts into the `mutate-null` warning. Runtime conversion
  failures (`to_int("abc")`, unparseable `to_date`) → Null and count into the
  `cast-null` warning — never a silent all-null column from a type error.
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
| slash date (v0.1.7) | `2024/01/31` |
| slash datetime (v0.1.7) | `2024/01/31 15:04:05` |
| compact date (v0.1.7) | `20240131` |

Timezone rule: values without an explicit offset are treated as **UTC**. With
offset, the instant is converted to UTC before bucketing.

Buckets:

| period | Bucket key |
|---|---|
| `day` | `YYYY-MM-DD` (UTC calendar day) |
| `week` | `YYYY-MM-DD` of the **Monday** containing that UTC day (ISO week start) |
| `month` | `YYYY-MM` |

- Unparseable **non-null** values go to the JSON `null` bucket and increment
  `parseFailCount` in the response (field present only when > 0); the response
  also carries `parseFailSample` — up to 10 failing raw values (first-seen
  order, deduped) — and a `date-parse-fail` warning.
- Cell `null` / empty string go to the `null` bucket and do **not** count as parse failures.
- Silent drop of all rows is forbidden: every filtered row lands in exactly one bucket.

```json
{"ok":true,"datasetId":"ds_…","outDatasetId":null,"rowCount":N,"columns":["…"],"rows":[[…]],"truncated":false,"parseFailCount":0,"parseFailSample":["…"]}
```

`parseFailCount` is omitted or `0` when there is no time-period grouping or no
failures; `parseFailSample` follows the same rule (at most 10 entries).

`outDatasetId` is `string | null` — `null` when `--out` is not given (inline
result only), else the new dataset id. When `--out` is set, a new dataset is
persisted and `outDatasetId` is set; inline `rows` may be empty with
`truncated: true` (budget 20 rows).

### `join`

Equi-join on one or more key columns. Result is **always** a new dataset.

Flags: `--left <id>` (required), `--right <id>` (required), `--on pair[,pair…]` (required, 1–8 pairs; `pair := name | leftName:rightName`), `--how inner|left|anti|semi` (default `inner`), `--out <label>` (optional)

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
- Missing `--on` column → `analysis/invalid-args: unknown column {name} on
  left|right` (a `leftName:rightName` pair checks the left name on the left
  table and the right name on the right table; a bare `name` must exist on
  both). Duplicate key names → `analysis/invalid-args: duplicate join key {name}`.
- Bad `--how` → `analysis/invalid-args: --how must be inner|left|anti|semi`.

### `corr` (v0.1.4)

Pairwise correlation among numeric-capable columns. Summary only — never a full dense matrix dump beyond the pair cap.

Flags: `--dataset-id` (required), `--columns a,b[,c…]` (required, 2..=16), `--method pearson|spearman` (default `pearson`), `--pairwise complete|omit` (default `complete`), `--min-numeric <n>` (integer ≥ 2, default 2)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"method":"pearson","pairwise":"complete","minNumeric":2,"columns":["a","b","c"],"pairCount":3,"pairs":[{"columns":["a","b"],"n":123,"df":121,"r":0.87,"pValue":0.0,"degenerate":false}],"truncated":false}
```

- Response uses a **pairs array** (upper triangle), sorted by `|r|` desc (nulls last), then `n` desc, then input column order. Cap **50** pairs; more → `truncated: true`.
- Each pair has `n` (rows contributing under `--pairwise`), `df` (= `n-2`,
  saturating at 0), `r`, and `pValue` (v0.1.10, R-24).
- `pValue`: two-sided Student-t approximation on `t = r·√(df/(1−r²))` with
  `df = n−2`, via regularized incomplete beta `I_{df/(df+t²)}(df/2, 1/2)`.
  `|r| = 1` and `n ≥ 3` → `0`; `r = null` or `n < 3` → `null`. Same formula
  for Spearman (t approx on rank correlation). Schema: nullable number.
- `n < --min-numeric` → `r: null`, `pValue: null`, `degenerate: false`.
- Non-numeric-capable column (zero `as_f64` successes) → exit 2 `analysis/invalid-args: column {name} is not numeric-capable`.
- Degenerate (either variance 0 after enough samples) → `r: null`,
  `pValue: null`, `degenerate: true`.
- `--pairwise complete`: pairwise deletion (each pair uses its own complete rows).
- `--pairwise omit`: listwise deletion (only rows where **all** listed columns are non-null numeric).
- Spearman: mid-ranks (average ranks for ties) then Pearson on ranks.

### `crosstab` (v0.1.4; `third` added v0.1.10)

Two-column contingency table. Optional third dimension via `--third`.

Flags: `--dataset-id` (required), `--rows <col>` (required), `--cols <col>` (required, ≠ rows), `--third <col>` (optional, ≠ rows/cols), `--top <n>` (1..=50, default 20), `--min-count <n>` (integer ≥ 1, default 1), `--drop-null true|false` (default `true`)

**Without `--third`** (legacy v0.1.7 shape, unchanged):

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"rowColumn":"a","colColumn":"b","rowCategories":["x","y"],"colCategories":["1","2"],"matrix":[[c00,c01],[c10,c11]],"rowTotals":[…],"colTotals":[…],"grandTotal":N,"cellCount":N,"truncated":true,"nullRowCount":0,"topCells":[{"row":"x","col":"1","count":10}]}
```

**With `--third`**: top-level flat keys (`rowCategories`, `matrix`, …) are
**omitted**; response carries:

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"rowColumn":"a","colColumn":"b","thirdColumn":"m","thirdCategories":["2026-01"],"tables":[{"third":"2026-01","rowCategories":[…],"colCategories":[…],"matrix":[[…]],"rowTotals":[…],"colTotals":[…],"grandTotal":N,"cellCount":N,"truncated":false,"nullRowCount":0,"topCells":[…]}],"thirdTruncated":false,"nullThirdCount":0}
```

- Categories: distinct `Value` JSON forms (same encoding as `values`); selected by marginal count desc, then first-seen. Length ≤ `--top`. With the default `--drop-null true`, only non-null categories appear. With `--drop-null false`, null participates as JSON string category `__null__` (a data string `"__null__"` is a separate category with the same label). Mixed numeric kinds that share a JSON form (e.g. integer `1` vs float `1.0`) may yield repeated labels in the category list.
- `matrix` is `len(rowCategories) × len(colCategories)`; margins only cover the kept grid.
- `truncated`: distinct categories exceeded `--top` or cells fell outside the selected grid.
- `topCells`: cells with `count >= --min-count`, count desc, cap 50.
- Nulls: `--drop-null true` (default) excludes rows with null on **any** of rows/cols/third → counted in `nullRowCount` (per-table) / `nullThirdCount`; `--drop-null false` keeps them as the `__null__` category.
- Unknown third column → `analysis/invalid-args: unknown column {name}` (exit 2).
- `--third` equal to `--rows` or `--cols` → `analysis/invalid-args: --third must differ from --rows and --cols`.
- Algorithm: one pass over cells — never materializes full cartesian of raw distinct values.

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
- `outDatasetId` is `string | null` — `null` when `--out` is not given, else the new dataset id.
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
- Numeric math uses the crate `as_f64` model: integers beyond 2^53 lose precision in `diff`/`pct_change`/`rolling` (same as other numeric ops in this version).
- `rolling`: trailing positions `i-window+1..=i`. If fewer than `window` non-null finite values → Null. Else `mean`/`sum`/`min`/`max` over those non-null values. mean/sum → f64. min/max: cell values and schema dtype are i64 only when the **whole source column** is all I64/null; any float (or non-numeric) in the source forces f64 — a window of ints alone does not promote.
- No rows dropped. `producedBy: "series"`, `sourceKind: "series"`, `source_path: series:<src>`.
- Invalid ops / unknown column / empty ops / >8 ops → exit 2. No sample rows in the success body.

### `union` (v0.1.7)

Vertical concatenation of two datasets with column alignment. Writes a new
dataset.

Flags: `--left <id>` (required), `--right <id>` (required), `--columns-policy strict|intersect` (default `strict`), `--out <label>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"leftRows":N,"rightRows":N,"columns":[{"name":"…","dtype":"…"}]}
```

- `strict`: both sides must present the same column-name **set** (output order
  = left's); otherwise `analysis/invalid-args: union column mismatch: …`.
- `intersect`: keep only shared columns (left's order).
- Same-name dtype conflicts promote via the mixed-dtype rules (`utf8` wins).
- Result rows ≤ `DSH_ANALYZE_MAX_ROWS` else `analysis/limit`. Meta:
  `producedBy: "union"`, `parentDatasetId` = left id.

### `diff` (v0.1.7)

Row-level difference report between two datasets. Writes a report dataset.

Flags: `--left <id>` (required), `--right <id>` (required), `--on pair[,pair…]` (optional; same `leftKey:rightKey` grammar as `join`), `--sample-limit <n>` (1–20, default 5; non-integer or < 1 → exit 2; > 20 clamps to 20 with a `clamp` warning)

```json
{"ok":true,"datasetId":"ds_…","onlyLeft":N,"onlyRight":N,"changed":N,"samples":{"onlyLeft":[…],"onlyRight":[…],"changed":[{"key":{"…":…},"diffColumns":["…"],"left":{"…":…},"right":{"…":…}}]}}
```

- Without `--on`: multiset comparison over shared columns (`changed` is 0).
- With `--on`: keyed match (Null keys never match); `changed` counts keys
  present on both sides with at least one differing non-key shared column.
- Each `samples` list holds at most `--sample-limit` entries.
- Meta: `producedBy: "diff"`, `parentDatasetId` = left id; `datasetId` is the
  report dataset of differing rows (additive field).

### `pivot` (v0.1.7)

Wide-format pivot of one dimension into columns. Writes a new dataset.

Flags: `--dataset-id` (required), `--index <col,…>` (required, 1+ columns), `--columns <col>` (required), `--values <col>` (required), `--agg sum|mean|min|max|count` (required), `--out <label>` (optional)

```json
{"ok":true,"datasetId":"ds_…","rowCount":N,"columns":[{"name":"…","dtype":"…"}],"indexColumns":["…"],"pivotColumn":"…"}
```

- Rows = distinct `--index` combinations (input order); output columns = index
  columns followed by the distinct values of `--columns` (first-seen order,
  ≤ 50; more → `analysis/limit: pivot columns exceed 50`); cells = the `--agg`
  aggregate of `--values` (missing → `null`).
- Meta: `producedBy: "pivot"`, `parentDatasetId` = source id.

### `list`

Flags: `--label-like <substr>` (optional), `--produced-by <name>` (optional), `--since <YYYY-MM-DD[THH:MM:SS]>` (optional, UTC)

```json
{"ok":true,"datasets":[{"datasetId":"ds_…","rowCount":N,"columnCount":N,"label":null,"producedBy":"ingest","parentDatasetId":null,"sourceKind":"csv","createdAt":"2026-11-04T03:23:02Z"}]}
```

- `datasets[].label` is `string | null` — `null` when the dataset carries no
  label (created without `--label` / `--out`).
- `datasets[].parentDatasetId` is `string | null` — `null` for datasets with
  no parent (plain ingests); `sourceKind` is always a string.
- `datasets[].createdAt` is `string | null` — ISO-8601 UTC creation timestamp
  recorded since v0.1.7; `null` for older data.
- Filters combine with AND: `--label-like` is a case-insensitive substring of
  the label, `--produced-by` an exact `producedBy` match, `--since` keeps
  `createdAt >= since` (datasets without `createdAt` are excluded when
  `--since` is given).

### `export`

Flags: `--dataset-id` (required), `--output <path>` (required), `--format csv|xlsx|parquet` (optional, case-insensitive; default inferred from extension, else `csv`)

- Output is forced under `<workspace>/.dsh-analyze/outputs/` (file name kept).
- Missing parent directories are created before writing (v0.1.7); escapes stay
  `analysis/invalid-path`.
- Formats (v0.1.9, R-23):
  - `csv` — same as before (default).
  - `xlsx` — single sheet `data`, header row, true cell types (numbers as
    numbers, booleans as bool; timestamps stored as utf8 stay text). No styles /
    multi-sheet.
  - `parquet` — type map inverse of ingest (i64→INT64, f64→DOUBLE, bool→BOOLEAN,
    utf8→STRING/BYTE_ARRAY). Round-trips through `ingest --format parquet`.
    Empty tables write each column with its declared dataset dtype (never
    all-Boolean).
- Unknown `--format` → `analysis/invalid-args: bad format … (csv|xlsx|parquet)` (exit 2).
- Explicit `--format` wins over extension mismatch.

```json
{"ok":true,"outputPath":".dsh-analyze/outputs/….csv","rowCount":N,"bytes":N,"format":"csv"}
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
`dataset_corr` · `dataset_crosstab` · `dataset_series` ·
`dataset_union` · `dataset_diff` · `dataset_pivot`

| Tool | CLI | Notes |
|---|---|---|
| `dataset_values` | `values` | requires `column`; `limit` 1–100 default 20; echoes `requestedLimit` |
| `dataset_sample` | `sample` | `n` 1–200 default 10; optional `seed`, `stratifyBy`; echoes `requestedN` |
| `dataset_head` | `head` | `n` 1–100 default 5; echoes `requestedN` |
| `dataset_join` | `join` | writes new dataset; returns ids + counts only; `how` inner\|left\|anti\|semi; `on` entries may be `leftKey:rightKey` |
| `dataset_profile` | `profile` | optional `highCardinalityThreshold` (default 200) |
| `dataset_dedup` | `dedup` | optional `keyColumns` (1–16); `sampleLimit` integer 1–20 default 5 |
| `dataset_outliers` | `outliers` | `method` iqr\|zscore\|mad; optional `k`, `outLabel`; `sampleLimit` integer 1–20 default 10 |
| `dataset_corr` | `corr` | 2..16 numeric-capable columns; `method` pearson\|spearman; `pairwise` complete\|omit; `minNumeric` integer ≥2 default 2; pairs carry `pValue` |
| `dataset_crosstab` | `crosstab` | `rows`/`cols` required and distinct; optional `third`; `top` 1–50 default 20; `minCount` ≥1 default 1; `dropNull` boolean default true |
| `dataset_series` | `series` | `orderBy` required; `ops` array 1–8 of lag\|lead\|diff\|pct_change\|rolling; always writes new dataset |
| `dataset_list` | `list` | optional `labelLike`, `producedBy`, `since` filters |
| `dataset_union` | `union` | `leftDatasetId`/`rightDatasetId` required; `columnsPolicy` strict\|intersect; writes new dataset |
| `dataset_diff` | `diff` | `leftDatasetId`/`rightDatasetId` required; optional `on`, `sampleLimit`; writes report dataset |
| `dataset_pivot` | `pivot` | `index` array, `columns`, `values`, `agg` required; writes new dataset |
| `dataset_export` | `export` | optional `format` csv\|xlsx\|parquet (default csv) |
| `dataset_ingest` | `ingest` | `format` may be `parquet`; `xlsx` still rejected pre-spawn |

**head / sample / values caps (R-26, freeze):**

| Tool | Field | Range | Default | Clamp | Echo |
|---|---|---|---|---|---|
| `head` | `n` | 1..=100 | 5 | >100 → 100 + `clamp` | `requestedN`, `n` |
| `sample` | `n` | 1..=200 | 10 | >200 → 200 + `clamp` | `requestedN`, `n` |
| `values` | `limit` | 1..=100 | 20 | >100 → 100 + `clamp` | `requestedLimit`, `limit` |

Non-integer or `< 1` → exit 2 for all three. Tool descriptions must match this table.

`transform_agg` `spec` string gains optional `mutate`, time-period
`groupBy` objects, `having`, `topKPerGroup`, `window`, select rename, and the
v0.1.7 agg fns (see §4 `agg`). `filter` is a predicate array or an `all`/`any`/`not`
group object (a bare predicate object / string filter is rejected). Tool-layer
`validateAggSpec` must reject illegal mutate expressions, bad `filter`
shapes, bad `window` ops, and unknown top-level spec keys with
`analysis/invalid-args` before spawn. All tool renders surface `warnings[]` and the `requestedN` /
`requestedLimit` echo fields.

The npm package root re-exports `createAnalysisToolDefinitions`,
`attachAnalysisTools`, and `validateAggSpec` for hosts that embed the tool
layer directly.

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
- **xlsx ingest** (still exit 4; convert to csv/jsonl). Parquet ingest/export
  shipped in v0.1.8/v0.1.9.
- UDF / JS / Python expressions
- Full correlation-matrix export / confidence intervals / regression (pair
  `pValue` only, v0.1.10)
- Nested parquet (list/struct/map) and non-map parquet types (decimal/time/
  duration/interval/…)
- Expanding windows, centered series windows, multi-key series order
  (`agg` `window` rank/row_number/cumsum/ewm is the v0.1.8 slice)
- MCP adapter (deferred; see TODO.md)

(Duplicate detection, outlier detection shipped in v0.1.3; correlation, crosstab,
and anti/semi join shipped in v0.1.4; series lag/lead/diff/pct_change/rolling
shipped in v0.1.5; filter groups, extended agg fns, having, topKPerGroup,
select rename, mutate casts, union/diff/pivot, join/diff key pairs, json
ingest, list filters, warnings, requested caps shipped in v0.1.7;
window ops + parquet ingest shipped in v0.1.8; export xlsx/parquet shipped in
v0.1.9; corr pValue, crosstab third, head/sample/values caps table shipped in
v0.1.10.)
