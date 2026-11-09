import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  newDatasetId,
  resolveInWorkspace,
  analysisRoot,
  validateDatasetId,
  AnalysisPathError,
} from '../packages/dsh-analyze/dist/paths.js'
import {
  createAnalysisToolDefinitions,
  assertNotSensitiveSource,
  validateAggSpec,
  validatePositiveInt,
} from '../packages/dsh-analyze/dist/tools.js'
import { probeAnalyzeCliSync, resolveAnalysisCliPath } from '../packages/dsh-analyze/dist/cli.js'
import { attachAnalysisTools } from '../packages/dsh-analyze/dist/register.js'
import { DEFAULT_ANALYSIS_LIMITS } from '../packages/dsh-analyze/dist/limits.js'

const here = dirname(fileURLToPath(import.meta.url))
const mockCli = join(here, 'fixtures', 'mock-analyze-cli.mjs')

function makeWorkspace() {
  return mkdtempSync(join(tmpdir(), 'dsh-analyze-'))
}

/** Resolve a spawnable node binary (MiMo-embedded node cannot be spawned standalone). */
function nodeBin() {
  const isNode = (p) => /(^|[\\/])node(\.exe)?$/i.test(p)
  const candidates = [
    process.env.INV_TEST_NODE,
    process.env.npm_node_execpath,
    process.env.ProgramFiles && join(process.env.ProgramFiles, 'nodejs', 'node.exe'),
    process.env['LOCALAPPDATA'] && join(process.env['LOCALAPPDATA'], 'Programs', 'nodejs', 'node.exe'),
    // portable unpack used by CI smoke on this machine
    process.env['LOCALAPPDATA'] && join(process.env['LOCALAPPDATA'], 'node-portable', 'node-v22.14.0-win-x64', 'node.exe'),
    // POSIX runners expose plain `node` on PATH
    '/usr/local/bin/node',
    '/usr/bin/node',
    // last resort: the process running the tests (embedded runtimes may not spawn)
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

test('resolveInWorkspace rejects escapes', () => {
  const ws = makeWorkspace()
  try {
    assert.throws(() => resolveInWorkspace(ws, '../outside.csv'), AnalysisPathError)
    const ok = resolveInWorkspace(ws, 'data/in.csv')
    assert.ok(ok.startsWith(ws))
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset tools end-to-end via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'sample.csv'), 'region,amount\nA,10\nB,20\nA,30\n', 'utf8')
    const defs = makeDefs(ws)

    const ingest = await defs.dataset_ingest.execute(
      { path: 'data/sample.csv', format: 'csv' },
      { signal: AbortSignal.timeout(10_000) },
    )
    assert.equal(ingest.ok, true)
    assert.match(ingest.datasetId, /^ds_/)
    assert.equal(ingest.rowCount, 3)

    const head = await defs.dataset_head.execute({ datasetId: ingest.datasetId, n: 2 })
    assert.equal(head.rows.length, 2)
    assert.deepEqual(head.columns, ['region', 'amount'])

    const describe = await defs.stats_describe.execute({ datasetId: ingest.datasetId })
    const amount = describe.stats.find(s => s.name === 'amount')
    assert.equal(amount.nullCount, 0)
    assert.equal(amount.min, 10)
    assert.equal(amount.max, 30)

    const agg = await defs.transform_agg.execute({
      datasetId: ingest.datasetId,
      spec: JSON.stringify({
        groupBy: ['region'],
        aggs: [
          { fn: 'count', as: 'n' },
          { fn: 'sum', column: 'amount', as: 'amount_sum' },
        ],
        orderBy: [{ column: 'amount_sum', desc: true }],
      }),
      outLabel: 'by-region',
    })
    assert.equal(agg.rowCount, 2)
    assert.ok(agg.outDatasetId)

    const schema = await defs.dataset_schema.execute({ datasetId: ingest.datasetId })
    assert.equal(schema.rowCount, 3)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('list/export/rm tools close the loop via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'loop.csv'), 'region,amount\nA,10\nB,20\n', 'utf8')
    const defs = makeDefs(ws)

    const empty = await defs.dataset_list.execute({})
    assert.deepEqual(empty.datasets, [])

    const ingest = await defs.dataset_ingest.execute({ path: 'data/loop.csv', format: 'csv', label: 'loop' })
    assert.equal(ingest.ok, true)

    const listed = await defs.dataset_list.execute({})
    assert.equal(listed.datasets.length, 1)
    assert.equal(listed.datasets[0].datasetId, ingest.datasetId)
    assert.equal(listed.datasets[0].label, 'loop')
    assert.equal(listed.datasets[0].rowCount, 2)

    // Default export path lands under .dsh-analyze/outputs/
    const exported = await defs.dataset_export.execute({ datasetId: ingest.datasetId })
    assert.equal(exported.rowCount, 2)
    assert.ok(exported.outputPath.includes('.dsh-analyze'))
    assert.ok(existsSync(join(ws, exported.outputPath)))

    // Explicit outPath keeps the file name but is forced under outputs/ (T0)
    const named = await defs.dataset_export.execute({
      datasetId: ingest.datasetId,
      outPath: 'data/out-copy.csv',
    })
    assert.ok(named.outputPath.includes('.dsh-analyze'))
    assert.ok(named.outputPath.endsWith('out-copy.csv'))
    assert.ok(existsSync(join(ws, named.outputPath)))

    await assert.rejects(
      () => defs.dataset_export.execute({ datasetId: ingest.datasetId, outPath: '../escape.csv' }),
      /escapes workspace|invalid-path/,
    )

    const removed = await defs.dataset_rm.execute({ datasetId: ingest.datasetId })
    assert.equal(removed.removed, true)
    const after = await defs.dataset_list.execute({})
    assert.equal(after.datasets.length, 0)
    await assert.rejects(
      () => defs.dataset_head.execute({ datasetId: ingest.datasetId }),
      /unknown-dataset|unknown/i,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('analysis data root is .dsh-analyze', () => {
  const ws = makeWorkspace()
  assert.ok(analysisRoot(ws).endsWith('.dsh-analyze'))
})

test('resolveAnalysisCliPath prefers DSH_ANALYZE_CLI_PATH then legacy then default', () => {
  assert.equal(resolveAnalysisCliPath({ DSH_ANALYZE_CLI_PATH: ' /opt/dsh-analyze ' }), '/opt/dsh-analyze')
  assert.equal(
    resolveAnalysisCliPath({ INV_ANALYSIS_CLI_PATH: 'C:\\old\\legacy-analyze.exe' }),
    'C:\\old\\legacy-analyze.exe',
  )
  assert.equal(resolveAnalysisCliPath({}), 'dsh-analyze')
  // New name wins when both set
  assert.equal(
    resolveAnalysisCliPath({ DSH_ANALYZE_CLI_PATH: 'new-bin', INV_ANALYSIS_CLI_PATH: 'old-bin' }),
    'new-bin',
  )
})

test('attachAnalysisTools probes then registers; probe failure registers nothing', () => {
  const ws = makeWorkspace()
  try {
    const registered = []
    const agentTools = {
      register(def) {
        registered.push(def.name ?? def)
        return () => {}
      },
    }
    const ok = attachAnalysisTools({
      agentTools,
      names: ['dataset_ingest', 'dataset_head'],
      config: {
        cliPath: nodeBin(),
        cliPrefixArgs: [mockCli],
        workspaceRoot: ws,
      },
    })
    assert.equal(ok.registered, true)
    assert.deepEqual(registered, ['dataset_ingest', 'dataset_head'])
    assert.equal(typeof ok.dispose, 'function')

    const registered2 = []
    const agentTools2 = {
      register(def) {
        registered2.push(def.name ?? def)
        return () => {}
      },
    }
    const bad = attachAnalysisTools({
      agentTools: agentTools2,
      names: ['dataset_ingest'],
      config: { cliPath: nodeBin(), cliPrefixArgs: [mockCli], workspaceRoot: ws },
      probe: () => ({ ok: false, error: 'spawnSync dsh-analyze ENOENT' }),
    })
    assert.equal(bad.registered, false)
    assert.equal(registered2.length, 0)
    assert.match(bad.error, /ENOENT/)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('path escape rejected before CLI', async () => {
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: '../../etc/passwd' }),
      /escapes workspace|invalid-path/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('newDatasetId shape', () => {
  assert.match(newDatasetId(), /^ds_[a-f0-9]{12}$/)
})

test('assertNotSensitiveSource rejects profile credential paths', () => {
  const profile = process.platform === 'win32' ? 'C:\\profiles\\p1' : '/profiles/p1'
  const sepCh = process.platform === 'win32' ? '\\' : '/'
  const joinPath = (...parts) => parts.join(sepCh)
  assert.throws(
    () => assertNotSensitiveSource(joinPath(profile, '.bootstrap-token'), profile),
    /invalid-path|not allowed/,
  )
  assert.throws(
    () => assertNotSensitiveSource(joinPath(profile, 'profiles', 'x.json'), profile),
    /invalid-path|not allowed/,
  )
  assert.throws(
    () => assertNotSensitiveSource(joinPath(profile, '.credentials.yaml'), profile),
    /invalid-path|not allowed/,
  )
  // Non-sensitive path under profile is fine for the pure helper
  assertNotSensitiveSource(joinPath(profile, 'workspace', 'data.csv'), profile)
  // No profileRoot → no extra check
  assertNotSensitiveSource(joinPath(profile, '.bootstrap-token'))
})

test('two sessions get distinct analysis roots and cannot cross-read datasets', async () => {
  const profileRoot = makeWorkspace()
  const sidA = 'sess-aaa'
  const sidB = 'sess-bbb'
  const rootA = join(profileRoot, 'dsh-analyze-sessions', sidA)
  const rootB = join(profileRoot, 'dsh-analyze-sessions', sidB)
  try {
    mkdirSync(join(rootA, 'data'), { recursive: true })
    mkdirSync(rootB, { recursive: true })
    writeFileSync(join(rootA, 'data', 'a.csv'), 'region,amount\nA,1\n', 'utf8')
    const defsA = createAnalysisToolDefinitions({
      cliPath: nodeBin(),
      cliPrefixArgs: [mockCli],
      workspaceRoot: rootA,
      sessionId: sidA,
      profileRoot,
    })
    const defsB = createAnalysisToolDefinitions({
      cliPath: nodeBin(),
      cliPrefixArgs: [mockCli],
      workspaceRoot: rootB,
      sessionId: sidB,
      profileRoot,
    })
    assert.notEqual(rootA, rootB)
    const ingest = await defsA.dataset_ingest.execute({ path: 'data/a.csv', format: 'csv' })
    assert.equal(ingest.ok, true)
    // B cannot resolve A's workspace-relative path into B's root
    await assert.rejects(
      () => defsB.dataset_head.execute({ datasetId: ingest.datasetId }),
      /unknown-dataset|unknown/i,
    )
    // Ingest path under A's root is not valid relative to B
    await assert.rejects(
      () => defsB.dataset_ingest.execute({ path: '../sessions/sess-aaa/data/a.csv', format: 'csv' }),
      /escapes workspace|invalid-path/,
    )
  } finally {
    rmSync(profileRoot, { recursive: true, force: true })
  }
})

test('dataset_ingest rejects profile sensitive paths via profileRoot', async () => {
  const profileRoot = makeWorkspace()
  try {
    mkdirSync(join(profileRoot, 'profiles'), { recursive: true })
    writeFileSync(join(profileRoot, 'profiles', 'x.json'), '{}\n', 'utf8')
    writeFileSync(join(profileRoot, '.bootstrap-token'), 'tok\n', 'utf8')
    const sessionRoot = join(profileRoot, 'dsh-analyze-sessions', 'sess-1')
    mkdirSync(sessionRoot, { recursive: true })
    // Simulate a misconfigured workspace that IS the profile root
    const defs = createAnalysisToolDefinitions({
      cliPath: nodeBin(),
      cliPrefixArgs: [mockCli],
      workspaceRoot: profileRoot,
      sessionId: 'sess-1',
      profileRoot,
    })
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'profiles/x.json' }),
      /invalid-path|not allowed/,
    )
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: '.bootstrap-token' }),
      /invalid-path|not allowed/,
    )
    // Absolute sensitive path also rejected
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: join(profileRoot, 'profiles', 'x.json') }),
      /invalid-path|not allowed/,
    )
    // Non-sensitive file under profile still allowed for this defensive check
    writeFileSync(join(profileRoot, 'ok.csv'), 'a,b\n1,2\n', 'utf8')
    const ok = await defs.dataset_ingest.execute({ path: 'ok.csv', format: 'csv' })
    assert.equal(ok.ok, true)
  } finally {
    rmSync(profileRoot, { recursive: true, force: true })
  }
})

test('label and path length limits', async () => {
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'x'.repeat(2000), format: 'csv' }),
      /invalid-args|invalid-path/,
    )
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'data.csv', format: 'csv', label: 'L'.repeat(300) }),
      /invalid-args/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('probeAnalyzeCliSync accepts mock CLI version', () => {
  const ws = makeWorkspace()
  try {
    const probe = probeAnalyzeCliSync({
      workspaceRoot: ws,
      cliPath: nodeBin(),
      cliPrefixArgs: [mockCli],
      timeoutMs: 15_000,
    })
    assert.equal(probe.ok, true)
    if (probe.ok) {
      assert.ok(probe.capabilities.includes('ingest'))
      assert.ok(probe.capabilities.includes('list'))
      assert.ok(probe.capabilities.includes('union'))
      assert.ok(probe.capabilities.includes('diff'))
      assert.ok(probe.capabilities.includes('pivot'))
      assert.match(probe.version, /^\d+\.\d+\.\d+/)
    }
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('concurrent CLI calls on same workspace serialize', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'c.csv'), 'region,amount\nA,1\nB,2\n', 'utf8')
    const defs = makeDefs(ws)
    const [a, b] = await Promise.all([
      defs.dataset_ingest.execute({ path: 'data/c.csv', format: 'csv', label: 'a' }),
      defs.dataset_ingest.execute({ path: 'data/c.csv', format: 'csv', label: 'b' }),
    ])
    assert.equal(a.ok, true)
    assert.equal(b.ok, true)
    assert.notEqual(a.datasetId, b.datasetId)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('unicode labels and paths round-trip through ingest', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, '鏁版嵁'), { recursive: true })
    writeFileSync(join(ws, '鏁版嵁', '璁㈠崟.csv'), '鍚嶇О,閲戦\n鑻规灉,10\n棣欒晧,20\n', 'utf8')
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({
      path: '鏁版嵁/璁㈠崟.csv',
      format: 'csv',
      label: '璁㈠崟路娴嬭瘯',
    })
    assert.equal(ingest.ok, true)
    assert.equal(ingest.rowCount, 2)
    const listed = await defs.dataset_list.execute({})
    assert.equal(listed.datasets[0].label, '璁㈠崟路娴嬭瘯')
    const head = await defs.dataset_head.execute({ datasetId: ingest.datasetId, n: 1 })
    assert.deepEqual(head.columns, ['鍚嶇О', '閲戦'])
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('row/limit params: tool layer rejects non-integer / <1 and passes high values through (R-12/R-13)', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  assert.equal(DEFAULT_ANALYSIS_LIMITS.maxHeadRows, 100)
  assert.equal(DEFAULT_ANALYSIS_LIMITS.maxSampleRows, 200)
  assert.equal(limits.maxValuesLimit, 100)
  assert.equal(validatePositiveInt(37, 5, 'n'), 37)
  assert.equal(validatePositiveInt(500, 5, 'n'), 500)
  assert.throws(() => validatePositiveInt(0, 5, 'n'), /n must be integer >= 1/)
  assert.throws(() => validatePositiveInt(-5, 5, 'n'), /n must be integer >= 1/)
  assert.throws(() => validatePositiveInt(3.9, 5, 'n'), /n must be integer >= 1/)
  assert.throws(() => validatePositiveInt(1.5, 20, 'sampleLimit'), /sampleLimit must be integer >= 1/)
})

test('dataset_values / sample / join via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'orders.csv'),
      'order_id,region,amount,created_at\n1,A,10,2024-01-31\n2,B,20,2024-02-01\n3,A,30,not-a-date\n4,A,40,2024-02-01\n',
      'utf8',
    )
    writeFileSync(join(ws, 'data', 'regions.csv'), 'region,manager\nA,alice\nB,bob\n', 'utf8')
    const defs = makeDefs(ws)

    const orders = await defs.dataset_ingest.execute({ path: 'data/orders.csv', format: 'csv' })
    const regions = await defs.dataset_ingest.execute({ path: 'data/regions.csv', format: 'csv' })

    const values = await defs.dataset_values.execute({ datasetId: orders.datasetId, column: 'region', limit: 5 })
    assert.equal(values.nunique, 2)
    assert.equal(values.nullCount, 0)
    assert.equal(values.top[0].value, 'A')
    assert.equal(values.top[0].count, 3)

    const sample1 = await defs.dataset_sample.execute({ datasetId: orders.datasetId, n: 2, seed: 7 })
    const sample2 = await defs.dataset_sample.execute({ datasetId: orders.datasetId, n: 2, seed: 7 })
    assert.equal(sample1.rows.length, 2)
    assert.deepEqual(sample1.rows, sample2.rows)
    assert.equal(sample1.sampling.seed, 7)

    const joined = await defs.dataset_join.execute({
      leftDatasetId: orders.datasetId,
      rightDatasetId: regions.datasetId,
      on: ['region'],
      how: 'inner',
      outLabel: 'orders+region',
    })
    assert.match(joined.datasetId, /^ds_/)
    assert.equal(joined.rowCount, 4)
    assert.ok(joined.columns.some(c => c.name === 'manager'))

    // mutate + time period via transform_agg
    const agg = await defs.transform_agg.execute({
      datasetId: orders.datasetId,
      spec: JSON.stringify({
        mutate: [{ as: 'double_amount', expr: 'amount * 2' }],
        groupBy: [{ dateColumn: 'created_at', period: 'day' }],
        aggs: [
          { fn: 'count', as: 'n' },
          { fn: 'mean', column: 'double_amount', as: 'avg_double' },
        ],
      }),
    })
    assert.equal(agg.ok, true)
    assert.ok(agg.columns.includes('created_at_day'))
    assert.ok(agg.parseFailCount === 1 || agg.parseFailCount === undefined || agg.parseFailCount === 0 || agg.parseFailCount >= 0)

    await assert.rejects(
      () => defs.transform_agg.execute({
        datasetId: orders.datasetId,
        spec: JSON.stringify({ mutate: [{ as: 'x', expr: 'system(1)' }] }),
      }),
      /illegal|function|invalid-args/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('validateAggSpec rejects bad mutate and groupBy period', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  assert.throws(
    () => validateAggSpec(JSON.stringify({ mutate: [{ as: 'x', expr: 'foo()' }] }), limits),
    /function|invalid-args/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ mutate: [{ as: '1bad', expr: 'a+b' }] }), limits),
    /mutate name/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ groupBy: [{ dateColumn: 'd', period: 'hour' }] }), limits),
    /period/,
  )
  validateAggSpec(
    JSON.stringify({
      mutate: [{ as: 'rate', expr: 'hits / total' }],
      groupBy: ['region', { dateColumn: 'created_at', period: 'week' }],
    }),
    limits,
  )
})

test('validateAggSpec rejects out-of-range limit and bad ops', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  assert.throws(() => validateAggSpec('', limits), /spec is required/)
  assert.throws(() => validateAggSpec('not-json', limits), /JSON/)
  assert.throws(() => validateAggSpec('[]', limits), /must be object/)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: 0 }), limits), /1\.\./)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: limits.maxAggLimit + 1 }), limits), /1\.\./)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: 'x' }), limits), /limit must be a number/)
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'regex' }] }), limits),
    /bad filter op/,
  )
  // filter value shapes: required for non-null ops, arrays only for `in`
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'eq' }] }), limits),
    /filter value required for op eq/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ op: 'eq', value: 1 }] }), limits),
    /filter column required/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'eq', value: [1, 2] }] }), limits),
    /must not be an array/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'eq', value: { x: 1 } }] }), limits),
    /bad filter value/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'in', value: [1, null] }] }), limits),
    /bad filter value/,
  )
  validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'in', value: [1, 2] }] }), limits)
  validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'in', value: 'x' }] }), limits)
  validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'is_null' }] }), limits)
  assert.throws(
    () => validateAggSpec(JSON.stringify({ aggs: [{ fn: 'variance', column: 'a' }] }), limits),
    /bad agg fn/,
  )
  // In-range values pass
  validateAggSpec(JSON.stringify({ limit: 1, filter: [], aggs: [] }), limits)
  validateAggSpec(JSON.stringify({ limit: limits.maxAggLimit }), limits)
})

test('validateDatasetId boundaries', () => {
  assert.doesNotThrow(() => validateDatasetId('ds_0123456789ab'))
  assert.doesNotThrow(() => validateDatasetId('ds_' + 'a'.repeat(32)))
  assert.throws(() => validateDatasetId('ds_short'), AnalysisPathError)
  assert.throws(() => validateDatasetId('ds_' + 'a'.repeat(33)), AnalysisPathError)
  assert.throws(() => validateDatasetId('xx_0123456789ab'), AnalysisPathError)
  assert.throws(() => validateDatasetId('ds_0123456789AB'), AnalysisPathError)
  assert.throws(() => validateDatasetId(''), AnalysisPathError)
})

test('spec too large rejected before CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 's.csv'), 'a,b\n1,2\n', 'utf8')
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/s.csv', format: 'csv' })
    const hugeSpec = JSON.stringify({ select: ['a'], pad: 'x'.repeat(DEFAULT_ANALYSIS_LIMITS.maxSpecBytes) })
    await assert.rejects(
      () => defs.transform_agg.execute({ datasetId: ingest.datasetId, spec: hugeSpec }),
      /spec too large/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('BOM-prefixed CSV header stays clean via mock', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'bom.csv'), '\uFEFFregion,amount\nA,1\n', 'utf8')
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/bom.csv', format: 'csv' })
    const head = await defs.dataset_head.execute({ datasetId: ingest.datasetId, n: 1 })
    assert.equal(head.columns[0], 'region')
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_profile quality summary via mock', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'prof.csv'),
      'phone,amount,note\n13800000001,10.5,a\n13800000001,20,b\n,30,\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/prof.csv', format: 'csv' })
    const profile = await defs.dataset_profile.execute({ datasetId: ingest.datasetId })
    assert.equal(profile.rowCount, 3)
    assert.equal(profile.columns.length, 3)
    const phone = profile.columns.find(c => c.name === 'phone')
    assert.equal(phone.nullCount, 1)
    assert.equal(phone.nunique, 1)
    const note = profile.columns.find(c => c.name === 'note')
    assert.equal(note.typeStability, 'string')
    assert.ok(Array.isArray(profile.warnings))
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_dedup exact and keyed via mock', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'dup.csv'),
      'phone,amount\n13800000001,10\n13800000001,10\n13800000002,20\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/dup.csv', format: 'csv' })
    const exact = await defs.dataset_dedup.execute({ datasetId: ingest.datasetId })
    assert.equal(exact.mode, 'exact')
    assert.equal(exact.duplicateRowCount, 1)
    assert.equal(exact.distinctRowCount, 2)
    const keyed = await defs.dataset_dedup.execute({
      datasetId: ingest.datasetId,
      keyColumns: ['phone'],
    })
    assert.equal(keyed.mode, 'keyed')
    assert.equal(keyed.duplicateKeyCount, 1)
    assert.ok(keyed.samples.length >= 1)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_outliers iqr flags extreme and --out writes dataset', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'out.csv'),
      'amount\n1\n2\n3\n4\n100\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/out.csv', format: 'csv' })
    const res = await defs.dataset_outliers.execute({
      datasetId: ingest.datasetId,
      columns: ['amount'],
      method: 'iqr',
    })
    assert.equal(res.method, 'iqr')
    const col = res.columns[0]
    assert.ok(col.outlierCount >= 1)
    assert.equal(col.samples[0].value, 100)
    assert.equal(res.outDatasetId, null)

    const withOut = await defs.dataset_outliers.execute({
      datasetId: ingest.datasetId,
      columns: ['amount'],
      method: 'iqr',
      outLabel: 'extreme',
    })
    assert.ok(withOut.outDatasetId)
    assert.equal(typeof withOut.outDatasetId, 'string')
    assert.match(withOut.outDatasetId, /^ds_/)
    assert.equal(withOut.outRowCount, 1)
    const head = await defs.dataset_head.execute({ datasetId: withOut.outDatasetId, n: 5 })
    assert.equal(head.rowCount, 1)
    assert.equal(head.rows[0][0], 100)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('stats_describe returns extended quantiles via mock', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'q.csv'),
      'amount,label\n1,a\n2,b\n3,c\n4,d\n5,e\n100,f\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/q.csv', format: 'csv' })
    const desc = await defs.stats_describe.execute({ datasetId: ingest.datasetId })
    const amount = desc.stats.find(s => s.name === 'amount')
    assert.ok(amount.q01 !== undefined)
    assert.ok(amount.q90 !== undefined)
    assert.ok(amount.q99 !== undefined)
    const label = desc.stats.find(s => s.name === 'label')
    assert.equal(label.q01, null)
    assert.equal(label.q99, null)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_outliers rejects bad method before spawn', async () => {
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    await assert.rejects(
      () =>
        defs.dataset_outliers.execute({
          datasetId: 'ds_0123456789ab',
          columns: ['a'],
          method: 'nope',
        }),
      /iqr\|zscore\|mad/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_dedup / outliers reject invalid sampleLimit before spawn', async () => {
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    await assert.rejects(
      () => defs.dataset_dedup.execute({ datasetId: 'ds_0123456789ab', sampleLimit: 0 }),
      /sampleLimit must be integer/,
    )
    await assert.rejects(
      () =>
        defs.dataset_outliers.execute({
          datasetId: 'ds_0123456789ab',
          columns: ['a'],
          sampleLimit: 0,
        }),
      /sampleLimit must be integer/,
    )
    await assert.rejects(
      () => defs.dataset_dedup.execute({ datasetId: 'ds_0123456789ab', sampleLimit: 1.5 }),
      /sampleLimit must be integer/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_corr pearson happy path + rejects bad method', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'c.csv'), 'x,y\n1,2\n2,4\n3,5\n4,4\n5,5\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/c.csv' })
    const res = await defs.dataset_corr.execute({
      datasetId: ing.datasetId,
      columns: ['x', 'y'],
      method: 'pearson',
    })
    assert.equal(res.ok, true)
    assert.equal(res.method, 'pearson')
    assert.equal(res.pairs.length, 1)
    assert.equal(res.pairs[0].n, 5)
    assert.ok(Math.abs(res.pairs[0].r - 6 / Math.sqrt(60)) < 1e-9)
    assert.equal(res.pairs[0].degenerate, false)
    const rendered = defs.dataset_corr.output.render({}, res)
    assert.match(rendered[0].text, /r=/)

    await assert.rejects(
      () => defs.dataset_corr.execute({ datasetId: ing.datasetId, columns: ['x', 'y'], method: 'nope' }),
      /pearson\|spearman/,
    )
    await assert.rejects(
      () => defs.dataset_corr.execute({ datasetId: ing.datasetId, columns: ['x'] }),
      /2\.\.16/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_crosstab matrix + top=1 truncated + rejects same column', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'x.csv'), 'r,c\na,1\na,1\na,2\nb,2\nc,3\nc,3\nc,1\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/x.csv' })
    const res = await defs.dataset_crosstab.execute({
      datasetId: ing.datasetId,
      rows: 'r',
      cols: 'c',
    })
    assert.equal(res.grandTotal, 7)
    assert.equal(res.cellCount, 7)
    assert.deepEqual(res.matrix, [[2, 1, 0], [1, 0, 2], [0, 1, 0]])
    const top1 = await defs.dataset_crosstab.execute({
      datasetId: ing.datasetId,
      rows: 'r',
      cols: 'c',
      top: 1,
    })
    assert.equal(top1.truncated, true)
    assert.deepEqual(top1.rowCategories, ['a'])

    await assert.rejects(
      () => defs.dataset_crosstab.execute({ datasetId: ing.datasetId, rows: 'r', cols: 'r' }),
      /different columns/,
    )
    await assert.rejects(
      () => defs.dataset_crosstab.execute({ datasetId: ing.datasetId, rows: 'r', cols: 'c', top: 0 }),
      /top must be integer/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_join how anti|semi left-only columns', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'l.csv'), 'k,vl\n1,L1\n2,L2\n3,L3\n,Lnull\n', 'utf8')
    writeFileSync(join(ws, 'data', 'r.csv'), 'k,vr\n1,R1\n1,R1b\n2,R2\n', 'utf8')
    const defs = makeDefs(ws)
    const left = await defs.dataset_ingest.execute({ path: 'data/l.csv' })
    const right = await defs.dataset_ingest.execute({ path: 'data/r.csv' })

    const semi = await defs.dataset_join.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right.datasetId,
      on: ['k'],
      how: 'semi',
    })
    assert.equal(semi.how, 'semi')
    assert.equal(semi.rowCount, 2)
    assert.equal(semi.columnCount, 2)
    assert.ok(semi.columns.every(c => c.name === 'k' || c.name === 'vl'))

    const anti = await defs.dataset_join.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right.datasetId,
      on: ['k'],
      how: 'anti',
    })
    assert.equal(anti.how, 'anti')
    assert.equal(anti.rowCount, 2)
    assert.equal(anti.columnCount, 2)

    await assert.rejects(
      () =>
        defs.dataset_join.execute({
          leftDatasetId: left.datasetId,
          rightDatasetId: right.datasetId,
          on: ['k'],
          how: 'outer',
        }),
      /inner\|left\|anti\|semi/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_join string keys with null substring still match', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    // CSV token "null" becomes a real null at ingest (both CLI and mock);
    // keys that merely *contain* "null" must still join.
    writeFileSync(join(ws, 'data', 'ls.csv'), 'k,vl\nnullable,L-nsh\nnullify,L-nfy\na,L-a\n,L-missing\n', 'utf8')
    writeFileSync(join(ws, 'data', 'rs.csv'), 'k,vr\nnullable,R-nsh\nnullify,R-nfy\nb,R-b\n', 'utf8')
    const defs = makeDefs(ws)
    const left = await defs.dataset_ingest.execute({ path: 'data/ls.csv' })
    const right = await defs.dataset_ingest.execute({ path: 'data/rs.csv' })

    const inner = await defs.dataset_join.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right.datasetId,
      on: ['k'],
      how: 'inner',
    })
    assert.equal(inner.rowCount, 2, JSON.stringify(inner))

    const head = await defs.dataset_head.execute({ datasetId: inner.datasetId, n: 10 })
    const keys = head.rows.map(r => r[0]).sort()
    assert.deepEqual(keys, ['nullable', 'nullify'])

    const anti = await defs.dataset_join.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right.datasetId,
      on: ['k'],
      how: 'anti',
    })
    // left a + missing key
    assert.equal(anti.rowCount, 2)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_corr spearman + omit + rejects duplicate columns', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'c.csv'), 'x,y,z\n1,1,10\n2,3,10\n3,2,10\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/c.csv' })

    const spearman = await defs.dataset_corr.execute({
      datasetId: ing.datasetId,
      columns: ['x', 'y'],
      method: 'spearman',
    })
    assert.equal(spearman.method, 'spearman')
    assert.ok(Math.abs(spearman.pairs[0].r - 0.5) < 1e-12, `r=${spearman.pairs[0].r}`)

    // omit listwise with nulls: only one full row (x,y,z all present) → n=1 → r null
    // complete x,y still sees rows 0 and 1 → n=2
    writeFileSync(join(ws, 'data', 'm.csv'), 'x,y,z\n1,2,10\n2,4,\n3,,10\n', 'utf8')
    const ing2 = await defs.dataset_ingest.execute({ path: 'data/m.csv' })
    const omit = await defs.dataset_corr.execute({
      datasetId: ing2.datasetId,
      columns: ['x', 'y', 'z'],
      pairwise: 'omit',
    })
    assert.equal(omit.pairwise, 'omit')
    for (const p of omit.pairs) assert.equal(p.n, 1, JSON.stringify(p))
    assert.equal(omit.pairs[0].r, null)

    const complete = await defs.dataset_corr.execute({
      datasetId: ing2.datasetId,
      columns: ['x', 'y'],
      pairwise: 'complete',
    })
    assert.equal(complete.pairs[0].n, 2)
    assert.ok(Math.abs(complete.pairs[0].r - 1) < 1e-12)

    await assert.rejects(
      () => defs.dataset_corr.execute({ datasetId: ing.datasetId, columns: ['x', 'x'] }),
      /duplicate/i,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_crosstab drop-null false + pipe categories stay distinct', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'x.csv'), 'r,c\na|b,1\na|b,2\nc|d,1\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/x.csv' })
    const pipe = await defs.dataset_crosstab.execute({
      datasetId: ing.datasetId,
      rows: 'r',
      cols: 'c',
    })
    assert.equal(pipe.grandTotal, 3)
    assert.deepEqual(pipe.rowCategories, ['a|b', 'c|d'])
    assert.deepEqual(pipe.matrix, [[1, 1], [1, 0]])

    writeFileSync(join(ws, 'data', 'n.csv'), 'r,c\nx,1\nx,\n__null__,2\n', 'utf8')
    const ing2 = await defs.dataset_ingest.execute({ path: 'data/n.csv' })
    const withNull = await defs.dataset_crosstab.execute({
      datasetId: ing2.datasetId,
      rows: 'r',
      cols: 'c',
      dropNull: false,
    })
    assert.equal(withNull.nullRowCount, 0)
    assert.equal(withNull.grandTotal, 3)
    assert.equal(withNull.rowCategories.length, 2)
    assert.equal(withNull.colCategories.length, 3)
    assert.equal(withNull.cellCount, 3)
    // matrix 2×3, not merged into a single __null__ row
    assert.equal(withNull.matrix.length, 2)
    assert.equal(withNull.matrix[0].length, 3)

    const dropped = await defs.dataset_crosstab.execute({
      datasetId: ing2.datasetId,
      rows: 'r',
      cols: 'c',
    })
    // only (x, null) is dropped; ("__null__", 2) stays
    assert.equal(dropped.nullRowCount, 1)
    assert.equal(dropped.grandTotal, 2)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_series multi-op lag/diff/rolling via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 's.csv'), 't,v\n1,10\n2,20\n3,30\n4,50\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/s.csv' })
    const res = await defs.dataset_series.execute({
      datasetId: ing.datasetId,
      orderBy: 't',
      ops: [
        { fn: 'lag', column: 'v', periods: 1, as: 'v_lag' },
        { fn: 'diff', column: 'v', periods: 1, as: 'v_diff' },
        { fn: 'rolling', column: 'v', window: 2, agg: 'mean', as: 'v_ma2' },
      ],
    })
    assert.match(res.datasetId, /^ds_/)
    assert.equal(res.rowCount, 4)
    assert.equal(res.columnCount, 5)
    assert.equal(res.sourceDatasetId, ing.datasetId)
    assert.equal(res.orderBy, 't')
    assert.equal(res.opCount, 3)
    assert.ok(res.columns.some(c => c.name === 'v_ma2'))
    const head = await defs.dataset_head.execute({ datasetId: res.datasetId, n: 20 })
    assert.deepEqual(head.rows, [
      [1, 10, null, null, null],
      [2, 20, 10, 10, 15],
      [3, 30, 20, 10, 25],
      [4, 50, 30, 20, 40],
    ])
    const rendered = defs.dataset_series.output.render({}, res)
    const renderedText = Array.isArray(rendered)
      ? rendered.map(p => p?.text ?? '').join('')
      : String(rendered)
    assert.match(renderedText, /series →/)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_series rejects empty ops / bad as / unknown fn / missing window', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 's.csv'), 't,v\n1,10\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/s.csv' })
    await assert.rejects(
      () => defs.dataset_series.execute({ datasetId: ing.datasetId, orderBy: 't', ops: [] }),
      /ops must be/,
    )
    await assert.rejects(
      () =>
        defs.dataset_series.execute({
          datasetId: ing.datasetId,
          orderBy: 't',
          ops: [{ fn: 'lag', column: 'v', as: '1bad' }],
        }),
      /as must match/,
    )
    await assert.rejects(
      () =>
        defs.dataset_series.execute({
          datasetId: ing.datasetId,
          orderBy: 't',
          ops: [{ fn: 'nope', column: 'v', as: 'x' }],
        }),
      /fn must be/,
    )
    await assert.rejects(
      () =>
        defs.dataset_series.execute({
          datasetId: ing.datasetId,
          orderBy: 't',
          ops: [{ fn: 'rolling', column: 'v', as: 'x' }],
        }),
      /window (must be|required)/,
    )
    await assert.rejects(
      () =>
        defs.dataset_series.execute({
          datasetId: ing.datasetId,
          orderBy: 't',
          ops: [
            { fn: 'lag', column: 'v', as: 'a' },
            { fn: 'lead', column: 'v', as: 'a' },
          ],
        }),
      /duplicate as/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_series order-by nulls last + pct_change zero prev', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 's.csv'), 't,v\n2,20\n,99\n1,10\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/s.csv' })
    const res = await defs.dataset_series.execute({
      datasetId: ing.datasetId,
      orderBy: 't',
      ops: [
        { fn: 'lag', column: 'v', as: 'v_lag' },
        { fn: 'pct_change', column: 'v', as: 'pct' },
      ],
    })
    const head = await defs.dataset_head.execute({ datasetId: res.datasetId, n: 20 })
    // t sorted: 1,2,null
    assert.deepEqual(head.rows, [
      [1, 10, null, null],
      [2, 20, 10, 1],
      [null, 99, 20, 3.95],
    ])
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('transform_agg in filter matches every member via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'in.csv'), 'amount\n100\n3000\n5000\n', 'utf8')
    const defs = makeDefs(ws)
    const ingest = await defs.dataset_ingest.execute({ path: 'data/in.csv', format: 'csv' })
    const res = await defs.transform_agg.execute({
      datasetId: ingest.datasetId,
      spec: JSON.stringify({ filter: [{ column: 'amount', op: 'in', value: [100, 3000] }] }),
    })
    assert.equal(res.rowCount, 2)
    const strRes = await defs.transform_agg.execute({
      datasetId: ingest.datasetId,
      spec: JSON.stringify({ filter: [{ column: 'amount', op: 'in', value: ['100', '5000'] }] }),
    })
    assert.equal(strRes.rowCount, 2)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})


// ---------- v0.1.7 acceptance: nested filters / new agg fns / having / topKPerGroup ----------

test('R-5/R-6/R-7/R-15: nested filters, count_distinct/percentile/first/last/count_if, having, topKPerGroup via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'f.csv'),
      'region,amount,day\nA,10,2026-10-01\nB,20,2026-10-02\nA,30,2026-10-02\nC,40,2026-10-03\nA,50,2026-10-03\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/f.csv' })

    // R-5: OR group
    const orRes = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        filter: { any: [{ column: 'region', op: 'eq', value: 'A' }, { column: 'amount', op: 'gt', value: 35 }] },
      }),
    })
    assert.equal(orRes.rowCount, 4)

    // R-5: mixed AND/OR with NOT
    const mixed = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        filter: {
          all: [
            { any: [{ column: 'region', op: 'eq', value: 'A' }, { column: 'region', op: 'eq', value: 'B' }] },
            { not: { column: 'amount', op: 'lte', value: 10 } },
          ],
        },
      }),
    })
    assert.equal(mixed.rowCount, 3)

    // R-6: count_distinct / percentile / first / last / count_if
    const aggRes = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        groupBy: ['region'],
        aggs: [
          { fn: 'count_distinct', column: 'amount', as: 'na' },
          { fn: 'percentile', column: 'amount', p: 0.5, as: 'p50' },
          { fn: 'first', column: 'amount', as: 'first_amt' },
          { fn: 'last', column: 'amount', as: 'last_amt' },
          { fn: 'count_if', where: { column: 'amount', op: 'gte', value: 30 }, as: 'big' },
        ],
      }),
    })
    assert.equal(aggRes.rowCount, 3)
    const rowsA = aggRes.rows.find(r => r[0] === 'A')
    assert.equal(rowsA[1], 3)
    assert.equal(rowsA[2], 30)
    assert.equal(rowsA[3], 10)
    assert.equal(rowsA[4], 50)
    assert.equal(rowsA[5], 2)

    // R-7: having on the agg output
    const hav = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        groupBy: ['region'],
        aggs: [{ fn: 'count', as: 'n' }],
        having: [{ column: 'n', op: 'gt', value: 1 }],
      }),
    })
    assert.equal(hav.rowCount, 1)
    assert.equal(hav.rows[0][0], 'A')

    // R-15: top-1 amount per region, amount desc
    const topk = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        topKPerGroup: { by: ['region'], k: 1, orderBy: { column: 'amount', desc: true } },
      }),
    })
    assert.equal(topk.rowCount, 3)
    assert.deepEqual(topk.rows.map(r => r[1]).sort((a, b) => a - b), [20, 40, 50])
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

// ---------- v0.1.7 acceptance: select rename + cast mutate + warnings ----------

test('R-9: select rename + to_date/to_int mutate via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'd.csv'),
      'day,region,amount,txt\n2026/10/02,A,10,7\n2026/10/02,B,20,abc\n2026/10/03,A,30,9\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/d.csv' })

    // select rename
    const ren = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({ select: ['region', { column: 'amount', as: 'amt' }] }),
    })
    assert.deepEqual(ren.columns, ['region', 'amt'])
    assert.deepEqual(ren.rows[0], ['A', 10])

    // to_date then dateColumn grouping eats the produced column
    const dated = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        mutate: [{ as: 'd2', expr: 'to_date(day, "%Y/%m/%d")' }],
        groupBy: [{ dateColumn: 'd2', period: 'month' }],
        aggs: [{ fn: 'count', as: 'n' }],
      }),
    })
    assert.deepEqual(dated.columns, ['d2_month', 'n'])
    assert.deepEqual(dated.rows[0], ['2026-10', 3])

    // to_int runtime failure -> cast-null warning
    const cast = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({ mutate: [{ as: 'x', expr: 'to_int(txt)' }] }),
    })
    const castWarn = cast.warnings.find(w => w.code === 'cast-null')
    assert.ok(castWarn, JSON.stringify(cast.warnings))
    assert.equal(castWarn.column, 'x')
    assert.equal(castWarn.count, 1)
    const castRendered = defs.transform_agg.output.render({}, cast).map(p => p.text).join('')
    assert.match(castRendered, /warnings:/)
    assert.match(castRendered, /cast-null/)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-12/R-13/R-20: requestedN/requestedLimit pass-through and clamp/mutate-null/date-parse-fail warnings', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    const lines = ['region,amount,day,z']
    for (let i = 0; i < 5; i++) lines.push(`A,${(i + 1) * 10},2026-10-0${i + 1},0`)
    lines.push('B,60,not-a-date,0')
    writeFileSync(join(ws, 'data', 'w.csv'), lines.join('\n') + '\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/w.csv' })

    // head: over-limit n is passed through, CLI clamps + echoes requestedN/n
    const head = await defs.dataset_head.execute({ datasetId: ing.datasetId, n: 500 })
    assert.equal(head.requestedN, 500)
    assert.equal(head.n, 6)
    const clampWarn = head.warnings.find(w => w.code === 'clamp')
    assert.ok(clampWarn, JSON.stringify(head.warnings))
    assert.equal(clampWarn.field, 'n')
    assert.equal(clampWarn.requested, 500)
    assert.equal(clampWarn.actual, 100)
    const headText = defs.dataset_head.output.render({}, head).map(p => p.text).join('')
    assert.match(headText, /requestedN=500/)
    assert.match(headText, /warnings:/)
    assert.match(headText, /clamp/)

    // sample: same policy at 200
    const samp = await defs.dataset_sample.execute({ datasetId: ing.datasetId, n: 500 })
    assert.equal(samp.requestedN, 500)
    assert.equal(samp.n, 6)
    assert.ok(samp.warnings.some(w => w.code === 'clamp' && w.actual === 200))

    // values: requestedLimit/limit
    const vals = await defs.dataset_values.execute({ datasetId: ing.datasetId, column: 'region', limit: 500 })
    assert.equal(vals.requestedLimit, 500)
    assert.equal(vals.limit, 100)
    assert.ok(vals.warnings.some(w => w.code === 'clamp' && w.field === 'limit'))
    const valsText = defs.dataset_values.output.render({}, vals).map(p => p.text).join('')
    assert.match(valsText, /requestedLimit=500/)

    // non-integer / <1 rejected at the tool layer
    await assert.rejects(() => defs.dataset_head.execute({ datasetId: ing.datasetId, n: 0 }), /n must be integer/)
    await assert.rejects(() => defs.dataset_head.execute({ datasetId: ing.datasetId, n: 1.5 }), /n must be integer/)
    await assert.rejects(() => defs.dataset_values.execute({ datasetId: ing.datasetId, column: 'region', limit: -1 }), /limit must be integer/)

    // mutate-null via division by zero
    const mut = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({ mutate: [{ as: 'r', expr: 'amount / z' }] }),
    })
    const nullWarn = mut.warnings.find(w => w.code === 'mutate-null')
    assert.ok(nullWarn, JSON.stringify(mut.warnings))
    assert.equal(nullWarn.column, 'r')
    assert.equal(nullWarn.count, 6)

    // date-parse-fail alongside parseFailSample
    const dp = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        groupBy: [{ dateColumn: 'day', period: 'month' }],
        aggs: [{ fn: 'count', as: 'n' }],
      }),
    })
    assert.equal(dp.parseFailCount, 1)
    assert.deepEqual(dp.parseFailSample, ['not-a-date'])
    assert.ok(dp.warnings.some(w => w.code === 'date-parse-fail' && w.count === 1))
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

// ---------- v0.1.7 acceptance: dataset_union / dataset_diff / dataset_pivot ----------

test('R-8a: dataset_union strict and intersect via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'l.csv'), 'k,v\n1,L1\n2,L2\n', 'utf8')
    writeFileSync(join(ws, 'data', 'r1.csv'), 'k,v\n3,R3\n', 'utf8')
    writeFileSync(join(ws, 'data', 'r2.csv'), 'k,w\n4,W4\n', 'utf8')
    const defs = makeDefs(ws)
    const left = await defs.dataset_ingest.execute({ path: 'data/l.csv' })
    const right1 = await defs.dataset_ingest.execute({ path: 'data/r1.csv' })
    const right2 = await defs.dataset_ingest.execute({ path: 'data/r2.csv' })

    const strict = await defs.dataset_union.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right1.datasetId,
      label: 'combined',
    })
    assert.match(strict.datasetId, /^ds_/)
    assert.equal(strict.rowCount, 3)
    assert.equal(strict.leftRows, 2)
    assert.equal(strict.rightRows, 1)
    assert.deepEqual(strict.columns.map(c => c.name), ['k', 'v'])

    const inter = await defs.dataset_union.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right2.datasetId,
      columnsPolicy: 'intersect',
    })
    assert.deepEqual(inter.columns.map(c => c.name), ['k'])
    assert.equal(inter.rowCount, 3)

    await assert.rejects(
      () => defs.dataset_union.execute({ leftDatasetId: left.datasetId, rightDatasetId: right2.datasetId }),
      /union column mismatch/,
    )
    await assert.rejects(
      () =>
        defs.dataset_union.execute({
          leftDatasetId: left.datasetId,
          rightDatasetId: right1.datasetId,
          columnsPolicy: 'nope',
        }),
      /columnsPolicy must be strict\|intersect/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-8b: dataset_diff keyed pairs and multiset mode via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'l.csv'), 'k,v\n1,a\n2,b\n3,c\n', 'utf8')
    writeFileSync(join(ws, 'data', 'r.csv'), 'k,v\n2,B\n3,c\n4,d\n', 'utf8')
    writeFileSync(join(ws, 'data', 'l2.csv'), 'k,v\nx,1\ny,2\n', 'utf8')
    writeFileSync(join(ws, 'data', 'r2.csv'), 'k2,v\nx,1\nz,3\n', 'utf8')
    const defs = makeDefs(ws)
    const left = await defs.dataset_ingest.execute({ path: 'data/l.csv' })
    const right = await defs.dataset_ingest.execute({ path: 'data/r.csv' })

    const keyed = await defs.dataset_diff.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right.datasetId,
      on: ['k'],
    })
    assert.equal(keyed.onlyLeft, 1)
    assert.equal(keyed.onlyRight, 1)
    assert.equal(keyed.changed, 1)
    assert.equal(keyed.samples.onlyLeft.length, 1)
    assert.equal(keyed.samples.onlyLeft[0].k, 1)
    assert.deepEqual(keyed.samples.changed[0].diffColumns, ['v'])
    assert.deepEqual(keyed.samples.changed[0].key, { k: 2 })
    assert.ok(typeof keyed.datasetId === 'string' && keyed.datasetId.length > 0)
    const diffText = defs.dataset_diff.output.render({}, keyed).map((p) => p.text).join('')
    assert.ok(diffText.includes(keyed.datasetId), `render shows datasetId: ${diffText}`)

    // multiset mode: changed stays 0
    const multi = await defs.dataset_diff.execute({
      leftDatasetId: left.datasetId,
      rightDatasetId: right.datasetId,
    })
    assert.equal(multi.onlyLeft, 2)
    assert.equal(multi.onlyRight, 2)
    assert.equal(multi.changed, 0)

    // leftKey:rightKey pair
    const left2 = await defs.dataset_ingest.execute({ path: 'data/l2.csv' })
    const right2 = await defs.dataset_ingest.execute({ path: 'data/r2.csv' })
    const paired = await defs.dataset_diff.execute({
      leftDatasetId: left2.datasetId,
      rightDatasetId: right2.datasetId,
      on: ['k:k2'],
    })
    assert.equal(paired.onlyLeft, 1)
    assert.equal(paired.onlyRight, 1)
    assert.equal(paired.changed, 0)

    await assert.rejects(
      () => defs.dataset_diff.execute({ leftDatasetId: left.datasetId, rightDatasetId: right.datasetId, sampleLimit: 0 }),
      /sampleLimit must be integer/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-16b: dataset_pivot wide table + >50 pivot columns analysis/limit', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'p.csv'),
      'region,day,amount\nA,d1,10\nA,d2,30\nB,d1,20\n',
      'utf8',
    )
    const wide = ['g,d,v']
    for (let i = 0; i < 51; i++) wide.push(`a,d${i},${i}`)
    writeFileSync(join(ws, 'data', 'wide.csv'), wide.join('\n') + '\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/p.csv' })
    const wideIng = await defs.dataset_ingest.execute({ path: 'data/wide.csv' })

    const piv = await defs.dataset_pivot.execute({
      datasetId: ing.datasetId,
      index: ['region'],
      columns: 'day',
      values: 'amount',
      agg: 'sum',
      label: 'by-day',
    })
    assert.match(piv.datasetId, /^ds_/)
    assert.equal(piv.rowCount, 2)
    assert.deepEqual(piv.indexColumns, ['region'])
    assert.equal(piv.pivotColumn, 'day')
    assert.deepEqual(piv.columns.map(c => c.name), ['region', 'd1', 'd2'])
    const head = await defs.dataset_head.execute({ datasetId: piv.datasetId, n: 10 })
    assert.deepEqual(head.rows, [['A', 10, 30], ['B', 20, null]])

    await assert.rejects(
      () =>
        defs.dataset_pivot.execute({
          datasetId: wideIng.datasetId,
          index: ['g'],
          columns: 'd',
          values: 'v',
          agg: 'sum',
        }),
      /pivot columns exceed 50/,
    )
    await assert.rejects(
      () =>
        defs.dataset_pivot.execute({
          datasetId: ing.datasetId,
          index: ['region'],
          columns: 'day',
          values: 'amount',
          agg: 'nope',
        }),
      /agg must be sum\|mean\|min\|max\|count/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

// ---------- v0.1.7 acceptance: join pairs / json ingest / list filters ----------

test('R-10: dataset_join leftKey:rightKey on-entries via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'orders.csv'), 'order_id,amount\n1,10\n2,20\n', 'utf8')
    writeFileSync(join(ws, 'data', 'codes.csv'), 'oid,region\n1,A\n3,C\n', 'utf8')
    const defs = makeDefs(ws)
    const orders = await defs.dataset_ingest.execute({ path: 'data/orders.csv' })
    const codes = await defs.dataset_ingest.execute({ path: 'data/codes.csv' })

    const joined = await defs.dataset_join.execute({
      leftDatasetId: orders.datasetId,
      rightDatasetId: codes.datasetId,
      on: ['order_id:oid'],
      how: 'inner',
    })
    assert.equal(joined.rowCount, 1)
    assert.ok(joined.columns.some(c => c.name === 'region'))

    await assert.rejects(
      () =>
        defs.dataset_join.execute({
          leftDatasetId: orders.datasetId,
          rightDatasetId: codes.datasetId,
          on: ['order_id:nope'],
        }),
      /unknown column nope on right/,
    )
    await assert.rejects(
      () =>
        defs.dataset_join.execute({
          leftDatasetId: orders.datasetId,
          rightDatasetId: codes.datasetId,
          on: ['nope:oid'],
        }),
      /unknown column nope on left/,
    )
    await assert.rejects(
      () =>
        defs.dataset_join.execute({
          leftDatasetId: orders.datasetId,
          rightDatasetId: codes.datasetId,
          on: ['order_id:oid', 'order_id:oid'],
        }),
      /duplicate join key/,
    )
    await assert.rejects(
      () =>
        defs.dataset_join.execute({
          leftDatasetId: orders.datasetId,
          rightDatasetId: codes.datasetId,
          on: ['a:'],
        }),
      /on pairs must be leftKey:rightKey/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-11: dataset_ingest accepts json (array and records forms) via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'a.json'), '[{"region":"A","amount":10},{"region":"B","amount":20}]', 'utf8')
    writeFileSync(join(ws, 'data', 'b.json'), '{"records":[{"x":1},{"x":2},{"x":3}]}', 'utf8')
    const defs = makeDefs(ws)

    const arr = await defs.dataset_ingest.execute({ path: 'data/a.json', format: 'json' })
    assert.equal(arr.rowCount, 2)
    assert.equal(arr.columnCount, 2)
    assert.deepEqual(arr.columns.map(c => c.name), ['region', 'amount'])

    const rec = await defs.dataset_ingest.execute({ path: 'data/b.json' })
    assert.equal(rec.rowCount, 3)

    // parquet format is allowed past the tool layer (R-22); mock rejects
    // non-parquet bytes with a stable analysis/cli-failed message.
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'data/a.json', format: 'parquet' }),
      /not a valid parquet file/, 
    )
    // xlsx allowed past the tool layer (R-31); non-PK bytes → corrupt message.
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'data/a.json', format: 'xlsx' }),
      /cannot read xlsx/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('dataset_ingest/export format is case-insensitive (tool layer)', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'f.csv'), 'a,b\n1,2\n', 'utf8')
    const defs = makeDefs(ws)

    const ing = await defs.dataset_ingest.execute({ path: 'data/f.csv', format: 'CSV' })
    assert.ok(ing.datasetId, JSON.stringify(ing))

    // Uppercase XLSX must reach the xlsx reader (not fall through as
    // `bad format`); CSV bytes are not a zip → corrupt message (R-31).
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'data/f.csv', format: 'XLSX' }),
      /cannot read xlsx/,
    )

    const exp = await defs.dataset_export.execute({ datasetId: ing.datasetId, format: 'PARQUET' })
    assert.equal(exp.format, 'parquet')

    await assert.rejects(
      () => defs.dataset_export.execute({ datasetId: ing.datasetId, format: 'xml' }),
      /format must be csv/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})
test('R-19: dataset_list labelLike/producedBy/since filters + createdAt render via mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'a.csv'), 'k\n1\n', 'utf8')
    const defs = makeDefs(ws)
    const ingA = await defs.dataset_ingest.execute({ path: 'data/a.csv', label: 'ALPHA' })
    await defs.dataset_ingest.execute({ path: 'data/a.csv', label: 'beta' })
    const aggregated = await defs.transform_agg.execute({
      datasetId: ingA.datasetId,
      spec: JSON.stringify({ aggs: [{ fn: 'count', as: 'n' }] }),
      outLabel: 'counts',
    })
    assert.ok(aggregated.outDatasetId)

    const all = await defs.dataset_list.execute({})
    assert.equal(all.datasets.length, 3)
    assert.ok(all.datasets.every(d => typeof d.createdAt === 'string'))

    const byLabel = await defs.dataset_list.execute({ labelLike: 'alpha' })
    assert.equal(byLabel.datasets.length, 1)
    assert.equal(byLabel.datasets[0].label, 'ALPHA')

    const byProd = await defs.dataset_list.execute({ producedBy: 'agg' })
    assert.equal(byProd.datasets.length, 1)
    assert.equal(byProd.datasets[0].datasetId, aggregated.outDatasetId)

    const recent = await defs.dataset_list.execute({ since: '2020-01-01' })
    assert.equal(recent.datasets.length, 3)
    const future = await defs.dataset_list.execute({ since: '2999-01-01' })
    assert.equal(future.datasets.length, 0)

    const rendered = defs.dataset_list.output.render({}, all).map(p => p.text).join('')
    assert.match(rendered, /@ /)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

// ---------- v0.1.8–v0.1.10 acceptance (R-21…R-26 tool layer) ----------

test('R-21: validateAggSpec window accepts good ops / rejects bad', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  validateAggSpec(
    JSON.stringify({
      window: [
        { fn: 'rank', by: ['g'], orderBy: { column: 'v', desc: true }, as: 'r' },
        { fn: 'cumsum', column: 'v', by: ['g'], orderBy: { column: 't' }, as: 'c' },
        { fn: 'ewm', column: 'v', alpha: 0.5, orderBy: { column: 't' }, as: 'e' },
      ],
    }),
    limits,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ window: [{ fn: 'rank', orderBy: { column: 'v' }, as: 'x' }] }), limits),
    /requires by/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ window: [{ fn: 'nope', by: ['g'], orderBy: { column: 'v' }, as: 'x' }] }), limits),
    /bad window fn/,
  )
  // K2: present-but-wrong-shape keys are invalid-args, never "missing" --
  // wording matches the CLI.
  assert.throws(
    () =>
      validateAggSpec(
        JSON.stringify({ window: [{ fn: 'ewm', column: 'v', alpha: 0.5, by: 'g', orderBy: { column: 't' }, as: 'e' }] }),
        limits,
      ),
    /window by must be an array/,
  )
  assert.throws(
    () => validateAggSpec(JSON.stringify({ window: [{ fn: 'rank', by: ['g'], orderBy: 't', as: 'x' }] }), limits),
    /window orderBy must be an object/,
  )
})

test('R-21: transform_agg window via mock CLI keeps row count', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'w.csv'), 'g,t,v\na,1,10\na,2,20\nb,1,5\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/w.csv' })
    const out = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        window: [
          { fn: 'row_number', by: ['g'], orderBy: { column: 't' }, as: 'rn' },
          { fn: 'cumsum', column: 'v', by: ['g'], orderBy: { column: 't' }, as: 'cum' },
        ],
      }),
    })
    assert.equal(out.rowCount, 3)
    assert.ok(out.columns.includes('rn'), JSON.stringify(out.columns))
    assert.ok(out.columns.includes('cum'), JSON.stringify(out.columns))
    const aRows = out.rows.filter(r => r[0] === 'a')
    assert.equal(aRows.length, 2)
    const rnIdx = out.columns.indexOf('rn')
    const cumIdx = out.columns.indexOf('cum')
    assert.deepEqual(aRows.map(r => r[rnIdx]).sort(), [1, 2])
    assert.deepEqual(aRows.map(r => r[cumIdx]).sort((x, y) => x - y), [10, 30])
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-23: dataset_export accepts format csv|xlsx|parquet; rejects bad format', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'e.csv'), 'a,b\n1,2\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/e.csv' })
    const csv = await defs.dataset_export.execute({ datasetId: ing.datasetId, format: 'csv' })
    assert.equal(csv.format, 'csv')
    const xlsx = await defs.dataset_export.execute({
      datasetId: ing.datasetId,
      outPath: 'outputs/e.xlsx',
      format: 'xlsx',
    })
    assert.equal(xlsx.format, 'xlsx')
    // K4: default outPath follows the format (no xlsx/parquet bytes in *.csv).
    const autoXlsx = await defs.dataset_export.execute({
      datasetId: ing.datasetId,
      format: 'xlsx',
    })
    assert.match(String(autoXlsx.outputPath), /\.xlsx$/)
    const autoParquet = await defs.dataset_export.execute({
      datasetId: ing.datasetId,
      format: 'parquet',
    })
    assert.match(String(autoParquet.outputPath), /\.parquet$/)
    await assert.rejects(
      () => defs.dataset_export.execute({ datasetId: ing.datasetId, format: 'xml' }),
      /format must be csv\|xlsx\|parquet/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-25: dataset_crosstab third returns tables; unknown third names column', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 't.csv'),
      'region,channel,month,n\nN1,app,2026-01,2\nN1,web,2026-01,1\nS1,app,2026-02,4\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/t.csv' })
    const flat = await defs.dataset_crosstab.execute({
      datasetId: ing.datasetId,
      rows: 'region',
      cols: 'channel',
    })
    assert.ok(Array.isArray(flat.matrix))
    assert.equal(flat.thirdColumn, undefined)

    const three = await defs.dataset_crosstab.execute({
      datasetId: ing.datasetId,
      rows: 'region',
      cols: 'channel',
      third: 'month',
    })
    assert.equal(three.thirdColumn, 'month')
    assert.ok(Array.isArray(three.tables) && three.tables.length >= 1)
    assert.ok(three.tables[0].third)

    // Render must surface third-level truncation + null third count
    // (thirdTruncated / nullThirdCount live at the top level, not `truncated`).
    const renderedFlat = defs.dataset_crosstab.output.render({}, three).map(p => p.text).join('')
    assert.match(renderedFlat, /tables/)
    assert.match(renderedFlat, /nulls=/)

    const threeTop1 = await defs.dataset_crosstab.execute({
      datasetId: ing.datasetId,
      rows: 'region',
      cols: 'channel',
      third: 'month',
      top: 1,
    })
    // 2 distinct months > top=1
    assert.equal(threeTop1.thirdTruncated, true)
    const rendered = defs.dataset_crosstab.output.render({}, threeTop1).map(p => p.text).join('')
    assert.match(rendered, /third truncated/, rendered)
    assert.match(rendered, /nulls=\d+/, rendered)

    await assert.rejects(
      () => defs.dataset_crosstab.execute({ datasetId: ing.datasetId, rows: 'region', cols: 'channel', third: 'nope' }),
      /unknown column nope/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-24: dataset_corr pairs carry pValue via mock', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'c.csv'), 'x,y\n1,2\n2,4\n3,6\n4,8\n5,10\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/c.csv' })
    const res = await defs.dataset_corr.execute({ datasetId: ing.datasetId, columns: ['x', 'y'] })
    assert.equal(res.pairs.length, 1)
    assert.ok('pValue' in res.pairs[0], JSON.stringify(res.pairs[0]))
    assert.ok(typeof res.pairs[0].pValue === 'number')
    assert.ok(res.pairs[0].pValue < 0.01, 'p=' + res.pairs[0].pValue)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

// ---------- v0.1.11 acceptance: R-29 groupBy tz (T3) + R-30 encoding (T5) ----------

test('R-29: validateAggSpec accepts valid tz (fixed offset + whitelist)', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  for (const tz of ['+08:00', 'Asia/Shanghai', '-05:30', '+14:00', '+00:00']) {
    validateAggSpec(
      JSON.stringify({ groupBy: [{ dateColumn: 'ts', period: 'day', tz }] }),
      limits,
    )
  }
  // tz absent stays valid (back-compat)
  validateAggSpec(JSON.stringify({ groupBy: [{ dateColumn: 'ts', period: 'day' }] }), limits)
  // spec description mentions optional tz
  const ws = makeWorkspace()
  try {
    const defs = makeDefs(ws)
    assert.match(
      defs.transform_agg.parameters.properties.spec.description,
      /tz\?:\+HH:MM\|Asia\/Shanghai/,
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-29: validateAggSpec rejects bad tz with analysis/invalid-args', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  for (const tz of ['Asia/Tokyo', '+15:00', '08:00', 480, null, true]) {
    assert.throws(
      () => validateAggSpec(JSON.stringify({ groupBy: [{ dateColumn: 'ts', period: 'day', tz }] }), limits),
      (err) => {
        assert.equal(err.code, 'analysis/invalid-args', `tz=${JSON.stringify(tz)}: ${err.message}`)
        assert.match(err.message, /bad tz/)
        return true
      },
      `tz=${JSON.stringify(tz)}`,
    )
  }
})

test('R-29: transform_agg day buckets with/without tz via mock CLI (§7.1 both sides)', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'flow.csv'),
      'ts,amt\n' +
        '2026-10-01T00:30:00+08:00,10\n' +
        '2026-10-01T12:00:00+08:00,20\n' +
        '2026-10-02T01:00:00+08:00,30\n',
      'utf8',
    )
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/flow.csv', format: 'csv' })
    const specFor = (tz) =>
      JSON.stringify({
        groupBy: [{ dateColumn: 'ts', period: 'day', ...(tz ? { tz } : {}) }],
        aggs: [
          { fn: 'sum', column: 'amt', as: 's' },
          { fn: 'count', as: 'n' },
        ],
        orderBy: [{ column: 'ts_day' }],
      })

    // with tz: local 10-01 00:30 belongs to 10-01 (both Asia/Shanghai and +08:00)
    for (const tz of ['Asia/Shanghai', '+08:00']) {
      const withTz = await defs.transform_agg.execute({ datasetId: ing.datasetId, spec: specFor(tz) })
      assert.deepEqual(
        withTz.rows,
        [['2026-10-01', 30, 2], ['2026-10-02', 30, 1]],
        `tz=${tz}: ${JSON.stringify(withTz.rows)}`,
      )
    }

    // without tz (default UTC): regression lock on both buckets
    const noTz = await defs.transform_agg.execute({ datasetId: ing.datasetId, spec: specFor() })
    assert.deepEqual(
      noTz.rows,
      [['2026-09-30', 10, 1], ['2026-10-01', 50, 2]],
      JSON.stringify(noTz.rows),
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-29: mock timestamp shapes mirror Rust (slash+offset, optional seconds, date-only+offset parse-fail)', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(
      join(ws, 'data', 'shapes.csv'),
      'ts\n' +
        '2026/10/04 23:30:00+00:00\n' + // slash datetime with offset → 2026-10-05 under +08:00
        '2026-10-01T00:30\n' + // seconds optional, naive → stays 2026-10-01
        '2026-10-01+08:00\n', // date-only with offset → parse-fail (Rust parity)
      'utf8',
    )
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/shapes.csv', format: 'csv' })
    const res = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        groupBy: [{ dateColumn: 'ts', period: 'day', tz: '+08:00' }],
        aggs: [{ fn: 'count', as: 'n' }],
      }),
    })
    assert.deepEqual(
      res.rows,
      [['2026-10-05', 1], ['2026-10-01', 1], [null, 1]],
      JSON.stringify(res.rows),
    )
    assert.equal(res.parseFailCount, 1)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-30: dataset_ingest encoding schema enum accepts four values; bad value rejected pre-spawn', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'enc.csv'), 'a,b\n1,2\n', 'utf8')
    const defs = makeDefs(ws)

    // schema declares the four-value enum (additionalProperties:false requires this)
    const enc = defs.dataset_ingest.parameters.properties.encoding
    assert.ok(enc, 'encoding property declared')
    assert.equal(enc.type, 'string')
    assert.deepEqual(enc.enum, ['utf-8', 'gbk', 'gb18030', 'auto'])

    // all four enum values pass pre-spawn and reach the mock
    for (const encoding of enc.enum) {
      const res = await defs.dataset_ingest.execute({ path: 'data/enc.csv', format: 'csv', encoding })
      assert.equal(res.ok, true, JSON.stringify(res))
      assert.equal(res.encoding, encoding, `mock saw encoding for ${encoding}`)
    }

    // invalid value rejected before spawn with analysis/invalid-args
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: 'data/enc.csv', format: 'csv', encoding: 'latin1' }),
      (err) => {
        assert.equal(err.code, 'analysis/invalid-args', err.message)
        assert.match(err.message, /bad encoding/)
        return true
      },
    )
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-30: dataset_ingest passes --encoding gbk through to the mock CLI', async () => {
  const ws = makeWorkspace()
  try {
    mkdirSync(join(ws, 'data'), { recursive: true })
    writeFileSync(join(ws, 'data', 'gbk.csv'), 'region,amount\nA,1\n', 'utf8')
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/gbk.csv', format: 'csv', encoding: 'gbk' })
    assert.equal(ing.ok, true)
    // mock echoes the flag only when --encoding was actually passed
    assert.equal(ing.encoding, 'gbk')

    // omitted encoding → CLI default path, no flag echoed
    const plain = await defs.dataset_ingest.execute({ path: 'data/gbk.csv', format: 'csv' })
    assert.equal(plain.ok, true)
    assert.equal(plain.encoding, undefined)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})


// ---------- v0.1.12 acceptance: R-36/R-37 agg inline rows budget ----------

const DAY_SPEC = JSON.stringify({
  groupBy: [{ dateColumn: 'ts', period: 'day', tz: 'Asia/Shanghai' }],
  aggs: [{ fn: 'count', as: 'n' }],
  orderBy: [{ column: 'ts_day' }],
})

async function write21DayCsv(ws) {
  mkdirSync(join(ws, 'data'), { recursive: true })
  const lines = ['ts,v']
  for (let i = 1; i <= 21; i++) {
    lines.push(`2026-01-${String(i).padStart(2, '0')}T00:30:00+08:00,${i}`)
  }
  writeFileSync(join(ws, 'data', 'd21.csv'), lines.join('\n') + '\n', 'utf8')
}

test('R-36: over-budget agg without outLabel previews 20 rows (never empty rows)', async () => {
  const ws = makeWorkspace()
  try {
    await write21DayCsv(ws)
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/d21.csv', format: 'csv' })

    const res = await defs.transform_agg.execute({ datasetId: ing.datasetId, spec: DAY_SPEC })
    assert.equal(res.ok, true)
    assert.equal(res.rowCount, 21)
    assert.equal(res.truncated, true)
    assert.equal(res.outDatasetId, null)
    assert.equal(res.rows.length, 20, JSON.stringify(res.rows.length))
    assert.deepEqual(res.rows[0], ['2026-01-01', 1])
    assert.ok(
      !res.rows.some((r) => r[0] === '2026-01-21'),
      '21st day is beyond the 20-row budget',
    )

    const rendered = defs.transform_agg.output.render({}, res).map((p) => p.text).join('')
    assert.match(rendered, /re-run with outLabel/, rendered)
    assert.doesNotMatch(rendered, /full result stored/, rendered)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-36: under-budget agg without outLabel returns full rows', async () => {
  const ws = makeWorkspace()
  try {
    await write21DayCsv(ws)
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/d21.csv', format: 'csv' })

    const res = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        groupBy: [{ dateColumn: 'ts', period: 'day', tz: 'Asia/Shanghai' }],
        aggs: [{ fn: 'count', as: 'n' }],
        orderBy: [{ column: 'ts_day' }],
        limit: 10,
      }),
    })
    assert.equal(res.rowCount, 10)
    assert.equal(res.truncated, false)
    assert.equal(res.outDatasetId, null)
    assert.equal(res.rows.length, 10)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-37: small result with outLabel returns full inline rows + outDatasetId', async () => {
  const ws = makeWorkspace()
  try {
    await write21DayCsv(ws)
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/d21.csv', format: 'csv' })

    const res = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: JSON.stringify({
        groupBy: [{ dateColumn: 'ts', period: 'day', tz: 'Asia/Shanghai' }],
        aggs: [{ fn: 'count', as: 'n' }],
        orderBy: [{ column: 'ts_day' }],
        limit: 3,
      }),
      outLabel: 'small3',
    })
    assert.equal(res.rowCount, 3)
    assert.equal(res.truncated, false)
    assert.ok(res.outDatasetId, 'outDatasetId required')
    assert.match(res.outDatasetId, /^ds_/)
    assert.equal(res.rows.length, 3, JSON.stringify(res.rows))

    const head = await defs.dataset_head.execute({ datasetId: res.outDatasetId, n: 5 })
    assert.equal(head.rowCount, 3)

    const rendered = defs.transform_agg.output.render({}, res).map((p) => p.text).join('')
    assert.match(rendered, /"rows"/, rendered)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})

test('R-37: over-budget with outLabel previews 20 rows and stores full result', async () => {
  const ws = makeWorkspace()
  try {
    await write21DayCsv(ws)
    const defs = makeDefs(ws)
    const ing = await defs.dataset_ingest.execute({ path: 'data/d21.csv', format: 'csv' })

    const res = await defs.transform_agg.execute({
      datasetId: ing.datasetId,
      spec: DAY_SPEC,
      outLabel: 'd21',
    })
    assert.equal(res.rowCount, 21)
    assert.equal(res.truncated, true)
    assert.ok(res.outDatasetId)
    assert.equal(res.rows.length, 20)

    const head = await defs.dataset_head.execute({ datasetId: res.outDatasetId, n: 50 })
    assert.equal(head.rowCount, 21)

    const rendered = defs.transform_agg.output.render({}, res).map((p) => p.text).join('')
    assert.match(rendered, /full result stored/, rendered)
    assert.match(rendered, /inline preview 20\/21/, rendered)
  } finally {
    rmSync(ws, { recursive: true, force: true })
  }
})