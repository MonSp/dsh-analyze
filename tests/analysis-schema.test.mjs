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
