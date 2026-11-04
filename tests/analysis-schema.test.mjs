// v0.1.5 defect regressions (dsh-analyze-v015-defects.md):
//  - BUG-1: output schema must not use JSON Schema type arrays (DSH host rejects)
//  - BUG-2: nullable outputs (outDatasetId / stratifyBy / label) must pass output
//    validation when the CLI returns null
//  - BUG-3: non-array `filter` and unknown top-level spec keys must be rejected
import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  createAnalysisToolDefinitions,
  validateAggSpec,
  validateMutateExpr,
} from '../packages/dsh-analyze/dist/tools.js'
import { DEFAULT_ANALYSIS_LIMITS } from '../packages/dsh-analyze/dist/limits.js'

const here = dirname(fileURLToPath(import.meta.url))
const mockCli = join(here, 'fixtures', 'mock-analyze-cli.mjs')

function makeWorkspace() {
  return mkdtempSync(join(tmpdir(), 'dsh-analyze-schema-'))
}

/** Resolve a spawnable node binary (MiMo-embedded node cannot be spawned standalone). */
function nodeBin() {
  const isNode = (p) => /(^|[\\/])node(\.exe)?$/i.test(p)
  const candidates = [
    process.env.INV_TEST_NODE,
    process.env.npm_node_execpath,
    process.env.ProgramFiles && join(process.env.ProgramFiles, 'nodejs', 'node.exe'),
    process.env['LOCALAPPDATA'] && join(process.env['LOCALAPPDATA'], 'Programs', 'nodejs', 'node.exe'),
    process.env['LOCALAPPDATA'] && join(process.env['LOCALAPPDATA'], 'node-portable', 'node-v22.14.0-win-x64', 'node.exe'),
    '/usr/local/bin/node',
    '/usr/bin/node',
    process.execPath,
  ].filter(Boolean)
  for (const c of candidates) {
    try {
      if (existsSync(c) && isNode(c)) return c
    } catch { /* ignore */ }
  }
  throw new Error('no spawnable node binary for mock CLI tests (set INV_TEST_NODE)')
}

function makeDefs(ws) {
  return createAnalysisToolDefinitions({
    cliPath: nodeBin(),
    cliPrefixArgs: [mockCli],
    workspaceRoot: ws,
  })
}

/**
 * Minimal JSON-schema subset (type / oneOf / properties / items / required)
 * mirroring what the DSH harness checks, so nullable CLI outputs can be
 * verified without a DSH host.
 */
function validateSchema(value, schema, path = 'value') {
  if (schema.oneOf) {
    const failures = []
    for (const branch of schema.oneOf) {
      try {
        validateSchema(value, branch, path)
        return
      } catch (e) {
        failures.push(e.message)
      }
    }
    throw new Error(`${path} matched no oneOf branch: ${failures.join('; ')}`)
  }
  if (schema.type) {
    const t = schema.type
    const ok =
      (t === 'string' && typeof value === 'string') ||
      (t === 'number' && typeof value === 'number') ||
      (t === 'boolean' && typeof value === 'boolean') ||
      (t === 'object' && value !== null && typeof value === 'object' && !Array.isArray(value)) ||
      (t === 'array' && Array.isArray(value)) ||
      (t === 'null' && value === null)
    if (!ok) {
      throw new Error(`${path} must be a ${t}, got ${value === null ? 'null' : typeof value}`)
    }
  }
  if (schema.type === 'object' && schema.properties) {
    for (const key of schema.required ?? []) {
      if (!(key in value)) throw new Error(`${path}.${key} is required`)
    }
    for (const [key, sub] of Object.entries(schema.properties)) {
      if (key in value) validateSchema(value[key], sub, `${path}.${key}`)
    }
  }
  if (schema.type === 'array' && schema.items) {
    value.forEach((item, i) => validateSchema(item, schema.items, `${path}[${i}]`))
  }
}

const NULLABLE_STRING = { oneOf: [{ type: 'string' }, { type: 'null' }] }

test('BUG-1: tool schemas never use JSON Schema type arrays', () => {
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    const seen = []
    const walk = (node, path) => {
      if (!node || typeof node !== 'object') return
      if (Array.isArray(node)) {
        node.forEach((n, i) => walk(n, `${path}[${i}]`))
        return
      }
      if (Array.isArray(node.type)) seen.push(path)
      for (const [k, v] of Object.entries(node)) walk(v, `${path}.${k}`)
    }
    for (const [name, def] of Object.entries(defs)) {
      if (def.parameters) walk(def.parameters, `${name}.parameters`)
      if (def.output?.schema) walk(def.output.schema, `${name}.output.schema`)
    }
    assert.deepEqual(seen, [], `type arrays found at: ${seen.join(', ')}`)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('BUG-1/2: nullable output fields are declared oneOf [string, null]', () => {
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    assert.deepEqual(
      defs.dataset_list.output.schema.properties.datasets.items.properties.label,
      NULLABLE_STRING,
    )
    assert.deepEqual(
      defs.dataset_list.output.schema.properties.datasets.items.properties.parentDatasetId,
      NULLABLE_STRING,
    )
    assert.deepEqual(
      defs.dataset_dedup.output.schema.properties.keyColumns,
      {
        oneOf: [{ type: 'array', items: { type: 'string' } }, { type: 'null' }],
      },
    )
    assert.deepEqual(
      defs.transform_agg.output.schema.properties.outDatasetId,
      NULLABLE_STRING,
    )
    assert.deepEqual(
      defs.dataset_sample.output.schema.properties.sampling.properties.stratifyBy,
      NULLABLE_STRING,
    )
    assert.deepEqual(
      defs.dataset_outliers.output.schema.properties.outDatasetId,
      NULLABLE_STRING,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('BUG-2: CLI null outputs pass output schema validation', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 's.csv'), 'region,amount\nA,10\nB,20\nA,30\n', 'utf8')
    const defs = makeDefs(ws)
    await defs.dataset_ingest.execute({ path: 'data/s.csv', format: 'csv' })
    await defs.dataset_ingest.execute({ path: 'data/s.csv', format: 'csv', label: 'tagged' })

    // dataset_list: label is null without --label, string with it
    const list = await defs.dataset_list.execute({})
    const labels = list.datasets.map((d) => d.label)
    assert.ok(labels.includes(null))
    assert.ok(labels.includes('tagged'))
    validateSchema(list, defs.dataset_list.output.schema)

    const first = list.datasets[0]

    // dataset_sample without stratifyBy -> sampling.stratifyBy === null
    const sample = await defs.dataset_sample.execute({ datasetId: first.datasetId, n: 2 })
    assert.equal(sample.sampling.stratifyBy, null)
    validateSchema(sample, defs.dataset_sample.output.schema)

    // transform_agg without outLabel -> outDatasetId === null
    const agg = await defs.transform_agg.execute({
      datasetId: first.datasetId,
      spec: JSON.stringify({ aggs: [{ fn: 'count', as: 'n' }] }),
    })
    assert.equal(agg.outDatasetId, null)
    validateSchema(agg, defs.transform_agg.output.schema)

    // dataset_outliers without outLabel -> outDatasetId === null
    const out = await defs.dataset_outliers.execute({
      datasetId: first.datasetId,
      columns: ['amount'],
      method: 'iqr',
    })
    assert.equal(out.outDatasetId, null)
    validateSchema(out, defs.dataset_outliers.output.schema)

    // dataset_dedup exact mode -> keyColumns === null
    const dedup = await defs.dataset_dedup.execute({ datasetId: first.datasetId })
    assert.equal(dedup.keyColumns, null)
    validateSchema(dedup, defs.dataset_dedup.output.schema)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('BUG-3: validateAggSpec rejects non-array filter', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  for (const spec of [
    { filter: { column: 'amount', op: 'gt', value: 3000 } },
    { filter: 'amount > 3000' },
    { filter: {} },
    { filter: 1 },
  ]) {
    assert.throws(
      () => validateAggSpec(JSON.stringify(spec), limits),
      /filter must be an array/,
    )
  }
  // the array form still passes
  validateAggSpec(
    JSON.stringify({ filter: [{ column: 'amount', op: 'gt', value: 3000 }] }),
    limits,
  )
})

test('BUG-3: validateAggSpec rejects other wrong-shape keys and unknown keys', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  assert.throws(() => validateAggSpec(JSON.stringify({ select: 'a,b' }), limits), /select must be an array/)
  assert.throws(() => validateAggSpec(JSON.stringify({ aggs: {} }), limits), /aggs must be an array/)
  assert.throws(() => validateAggSpec(JSON.stringify({ orderBy: 'a' }), limits), /orderBy must be an array/)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: '100' }), limits), /limit must be a number/)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: 0 }), limits), /1\.\./)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: 1.5 }), limits), /1\.\./)
  assert.throws(() => validateAggSpec(JSON.stringify({ select: [1] }), limits), /select entries must be strings/)
  assert.throws(() => validateAggSpec(JSON.stringify({ orderBy: [{}] }), limits), /orderBy entries need a column name/)
  assert.throws(
    () => validateAggSpec(JSON.stringify({ orderBy: [{ column: 'a', desc: 'yes' }] }), limits),
    /desc must be boolean/,
  )
  assert.throws(() => validateAggSpec(JSON.stringify({ filters: [] }), limits), /unknown spec key/)
  assert.throws(() => validateAggSpec(JSON.stringify({ select: ['a'], extra: 1 }), limits), /unknown spec key/)
})

test('BUG-3: transform_agg rejects object filter before CLI spawn', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 's.csv'), 'a,b\n1,2\n', 'utf8')
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/s.csv', format: 'csv' })
    await assert.rejects(
      () =>
        defs.transform_agg.execute({
          datasetId: ingest.datasetId,
          spec: JSON.stringify({ filter: { column: 'a', op: 'gt', value: 0 } }),
        }),
      /filter must be an array/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

// ---- v0.1.7 contract: filter/having trees (R-5/R-7) ----

test('R-5: validateAggSpec accepts nested filter groups (all/any/not)', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  const pred = { column: 'amount', op: 'gt', value: 3000 }
  // OR group
  validateAggSpec(JSON.stringify({ filter: { any: [pred, { column: 'region', op: 'eq', value: 'A' }] } }), limits)
  // mixed AND/OR with nesting
  validateAggSpec(
    JSON.stringify({
      filter: {
        all: [
          { any: [{ column: 'region', op: 'eq', value: 'A' }, { column: 'region', op: 'eq', value: 'B' }] },
          { not: { column: 'amount', op: 'lte', value: 0 } },
        ],
      },
    }),
    limits,
  )
  // NOT group
  validateAggSpec(JSON.stringify({ filter: { not: pred } }), limits)
  // empty all → true; empty any → false (both structurally valid)
  validateAggSpec(JSON.stringify({ filter: { all: [] } }), limits)
  validateAggSpec(JSON.stringify({ filter: { any: [] } }), limits)
  // groups may appear inside AND-arrays
  validateAggSpec(JSON.stringify({ filter: [pred, { any: [pred] }] }), limits)
})

test('R-5: filter groups reject bad shapes and over-deep nesting', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  const pred = { column: 'amount', op: 'gt', value: 1 }
  // bare predicate object at top level stays rejected (v0.1.6 strictness)
  assert.throws(() => validateAggSpec(JSON.stringify({ filter: pred }), limits), /filter must be an array/)
  // unknown group key
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: { or: [pred] } }), limits),
    /filter must be an array/,
  )
  // two group keys in one object
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: { all: [pred], any: [pred] } }), limits),
    /bad filter group/,
  )
  // all must be an array
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: { all: pred } }), limits),
    /filter all must be an array/,
  )
  // unknown nested shape
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: { all: [{ nope: 1 }] } }), limits),
    /bad filter op/,
  )
  // depth ≤8: 8 nested not-groups pass, 9th fails with the spec message
  let deep = pred
  for (let i = 0; i < 7; i++) deep = { not: deep }
  validateAggSpec(JSON.stringify({ filter: deep }), limits)
  for (let i = 0; i < 2; i++) deep = { not: deep }
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: deep }), limits),
    /filter nesting too deep/,
  )
})

test('R-7: having is a top-level key with the filter shape', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  validateAggSpec(
    JSON.stringify({
      groupBy: ['region'],
      aggs: [{ fn: 'count', as: 'n' }],
      having: [{ column: 'n', op: 'gt', value: 1 }],
    }),
    limits,
  )
  validateAggSpec(
    JSON.stringify({ aggs: [{ fn: 'count', as: 'n' }], having: { any: [{ column: 'n', op: 'gt', value: 1 }] } }),
    limits,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ having: 'n > 1' }), limits),
    /having must be an array/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ having: [{ op: 'gt', value: 1 }] }), limits),
    /having column required/,
  )
})

// ---- v0.1.7 contract: agg fn whitelist (R-6) ----

test('R-6: validateAggSpec accepts count_distinct/percentile/first/last/count_if', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  validateAggSpec(JSON.stringify({ aggs: [{ fn: 'count_distinct', column: 'region', as: 'nr' }] }), limits)
  validateAggSpec(JSON.stringify({ aggs: [{ fn: 'percentile', column: 'amount', p: 0.95, as: 'p95' }] }), limits)
  validateAggSpec(JSON.stringify({ aggs: [{ fn: 'first', column: 'status', as: 's0' }] }), limits)
  validateAggSpec(JSON.stringify({ aggs: [{ fn: 'last', column: 'status', as: 's1' }] }), limits)
  validateAggSpec(
    JSON.stringify({ aggs: [{ fn: 'count_if', where: { column: 'amount', op: 'gt', value: 1 }, as: 'k' }] }),
    limits,
  )
  validateAggSpec(
    JSON.stringify({
      aggs: [{ fn: 'count_if', where: { any: [{ column: 'a', op: 'eq', value: 1 }] }, as: 'k' }],
    }),
    limits,
  )
})

test('R-6: bad agg params rejected (percentile p, count_if where/column, missing column)', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  const cases = [
    [{ fn: 'percentile', column: 'amount', as: 'p95' }, /percentile requires p in \(0,1\)/],
    [{ fn: 'percentile', column: 'amount', p: 0, as: 'p' }, /percentile requires p in \(0,1\)/],
    [{ fn: 'percentile', column: 'amount', p: 1, as: 'p' }, /percentile requires p in \(0,1\)/],
    [{ fn: 'percentile', column: 'amount', p: 1.5, as: 'p' }, /percentile requires p in \(0,1\)/],
    [{ fn: 'percentile', column: 'amount', p: '0.9', as: 'p' }, /percentile requires p in \(0,1\)/],
    [{ fn: 'percentile', p: 0.5, as: 'p' }, /percentile requires column/],
    [{ fn: 'count_distinct', as: 'nr' }, /count_distinct requires column/],
    [{ fn: 'first', as: 'f' }, /first requires column/],
    [{ fn: 'last', as: 'l' }, /last requires column/],
    [{ fn: 'count_if', as: 'k' }, /count_if requires where/],
    [{ fn: 'count_if', column: 'a', where: { column: 'a', op: 'eq', value: 1 }, as: 'k' }, /count_if must not specify column/],
    [{ fn: 'count_if', where: 'a = 1', as: 'k' }, /bad where/],
  ]
  for (const [agg, re] of cases) {
    assert.throws(() => validateAggSpec(JSON.stringify({ aggs: [agg] }), limits), re)
  }
  // unknown fn still rejected
  assert.throws(() => validateAggSpec(JSON.stringify({ aggs: [{ fn: 'mode', column: 'a' }] }), limits), /bad agg fn/)
})

// ---- v0.1.7 contract: topKPerGroup (R-15) ----

test('R-15: topKPerGroup validates by/k/orderBy ranges', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  validateAggSpec(
    JSON.stringify({ topKPerGroup: { by: ['region'], k: 3 }, aggs: [] }),
    limits,
  )
  validateAggSpec(
    JSON.stringify({
      topKPerGroup: {
        by: ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'],
        k: 1000,
        orderBy: { column: 'amount', desc: true },
      },
    }),
    limits,
  )
  const cases = [
    [{ topKPerGroup: { by: [], k: 3 } }, /topKPerGroup\.by must be 1\.\.8/],
    [{ topKPerGroup: { by: ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i'], k: 3 } }, /topKPerGroup\.by must be 1\.\.8/],
    [{ topKPerGroup: { by: [1], k: 3 } }, /topKPerGroup\.by must be 1\.\.8/],
    [{ topKPerGroup: { by: ['a'], k: 0 } }, /topKPerGroup\.k must be integer 1\.\.1000/],
    [{ topKPerGroup: { by: ['a'], k: 1001 } }, /topKPerGroup\.k must be integer 1\.\.1000/],
    [{ topKPerGroup: { by: ['a'], k: 1.5 } }, /topKPerGroup\.k must be integer 1\.\.1000/],
    [{ topKPerGroup: { by: ['a'], k: 3, orderBy: {} } }, /topKPerGroup\.orderBy needs a column name/],
    [{ topKPerGroup: { by: ['a'], k: 3, orderBy: { column: 'x', desc: 'yes' } } }, /topKPerGroup\.orderBy desc must be boolean/],
    [{ topKPerGroup: { by: 'a', k: 3 } }, /topKPerGroup\.by must be 1\.\.8/],
  ]
  for (const [spec, re] of cases) {
    assert.throws(() => validateAggSpec(JSON.stringify(spec), limits), re)
  }
})

// ---- v0.1.7 contract: select rename + mutate casts (R-9) ----

test('R-9: select entries accept {column,as} renames and reject duplicates', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  validateAggSpec(JSON.stringify({ select: ['a', { column: 'b', as: 'bee' }] }), limits)
  assert.throws(
    () => validateAggSpec(JSON.stringify({ select: [{ column: 'b' }] }), limits),
    /select as must match/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ select: [{ column: 'b', as: '1bad' }] }), limits),
    /select as must match/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ select: [{ as: 'x' }] }), limits),
    /select entry column required/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ select: ['a', { column: 'b', as: 'a' }] }), limits),
    /duplicate select name/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ select: ['a', 'a'] }), limits),
    /duplicate select name/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ select: [1] }), limits),
    /select entries must be strings/,
  )
})

test('R-9: validateMutateExpr whitelist to_int/to_str/to_date + string literals', () => {
  validateMutateExpr('amount * 2')
  validateMutateExpr('to_int(amount)')
  validateMutateExpr('to_str(amount)')
  validateMutateExpr('to_date("2026/10/02","%Y/%m/%d")')
  validateMutateExpr('to_date(day, "%Y-%m-%d")')
  validateMutateExpr('to_date("2026-10-02T11:22:33", "%Y-%m-%dT%H:%M:%S")')
  validateMutateExpr('to_int(amount) + 1')
  assert.throws(() => validateMutateExpr('foo()'), /function calls are not allowed/)
  assert.throws(() => validateMutateExpr('system(1)'), /function calls are not allowed/)
  assert.throws(() => validateMutateExpr('to_date(a)'), /to_date\(e, fmt\) takes 2 arguments/)
  assert.throws(() => validateMutateExpr('to_int(a, b)'), /to_int\(e\) takes 1 argument/)
  assert.throws(() => validateMutateExpr('to_date(a, b)'), /to_date fmt must be a string literal/)
  assert.throws(
    () => validateMutateExpr('to_date(a, "%Q")'),
    /unsupported directive/,
  )
  assert.throws(
    () => validateMutateExpr('to_date(a, "%Y-%m-%d %q")'),
    /unsupported directive/,
  )
  assert.throws(() => validateMutateExpr('a; drop'), /illegal characters/)
})

// ---- v0.1.7 contract: R-18 package-root library surface ----

test('R-18: package root (dist/index.js) re-exports createAnalysisToolDefinitions, attachAnalysisTools, validateAggSpec', async () => {
  const api = await import('../packages/dsh-analyze/dist/index.js')
  assert.equal(typeof api.createAnalysisToolDefinitions, 'function')
  assert.equal(typeof api.attachAnalysisTools, 'function')
  assert.equal(typeof api.validateAggSpec, 'function')
  // apply stays exported for the plugin entry
  assert.equal(typeof api.apply, 'function')
  // the root createAnalysisToolDefinitions is the same function the tools module exposes
  assert.equal(api.createAnalysisToolDefinitions, createAnalysisToolDefinitions)
  assert.equal(api.validateAggSpec, validateAggSpec)
})
