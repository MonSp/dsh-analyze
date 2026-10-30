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
} from '../packages/dsh-analyze/dist/tools.js'
import { probeAnalyzeCliSync, resolveAnalysisCliPath } from '../packages/dsh-analyze/dist/cli.js'
import { attachAnalysisTools } from '../packages/dsh-analyze/dist/register.js'
import { DEFAULT_ANALYSIS_LIMITS, clampHeadN } from '../packages/dsh-analyze/dist/limits.js'

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
    resolveAnalysisCliPath({ INV_ANALYSIS_CLI_PATH: 'C:\\old\\investigator-analyze.exe' }),
    'C:\\old\\investigator-analyze.exe',
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
    () => assertNotSensitiveSource(joinPath(profile, 'investigator', 'account-store.json'), profile),
    /invalid-path|not allowed/,
  )
  assert.throws(
    () => assertNotSensitiveSource(joinPath(profile, '.bootstrap-token'), profile),
    /invalid-path|not allowed/,
  )
  assert.throws(
    () => assertNotSensitiveSource(joinPath(profile, 'profiles', 'x.json'), profile),
    /invalid-path|not allowed/,
  )
  // Non-sensitive path under profile is fine for the pure helper
  assertNotSensitiveSource(joinPath(profile, 'workspace', 'data.csv'), profile)
  // No profileRoot 鈫?no extra check
  assertNotSensitiveSource(joinPath(profile, 'investigator', 'account-store.json'))
})

test('two sessions get distinct analysis roots and cannot cross-read datasets', async () => {
  const profileRoot = makeWorkspace()
  const sidA = 'sess-aaa'
  const sidB = 'sess-bbb'
  const rootA = join(profileRoot, 'inv-analysis', 'sessions', sidA)
  const rootB = join(profileRoot, 'inv-analysis', 'sessions', sidB)
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
    mkdirSync(join(profileRoot, 'investigator'), { recursive: true })
    writeFileSync(join(profileRoot, 'investigator', 'account-store.json'), '{}\n', 'utf8')
    writeFileSync(join(profileRoot, '.bootstrap-token'), 'tok\n', 'utf8')
    const sessionRoot = join(profileRoot, 'inv-analysis', 'sessions', 'sess-1')
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
      () => defs.dataset_ingest.execute({ path: 'investigator/account-store.json' }),
      /invalid-path|not allowed/,
    )
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: '.bootstrap-token' }),
      /invalid-path|not allowed/,
    )
    // Absolute sensitive path also rejected
    await assert.rejects(
      () => defs.dataset_ingest.execute({ path: join(profileRoot, 'investigator', 'account-store.json') }),
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
      assert.match(probe.version, /0\.1\.0/)
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

test('clampHeadN boundaries', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  assert.equal(clampHeadN(0, limits), 1)
  assert.equal(clampHeadN(-5, limits), 1)
  assert.equal(clampHeadN(20, limits), 20)
  assert.equal(clampHeadN(999, limits), limits.maxHeadRows)
  assert.equal(clampHeadN(NaN, limits), 5)
  assert.equal(clampHeadN(undefined, limits), 5)
  assert.equal(clampHeadN(3.9, limits), 3)
})

test('validateAggSpec rejects out-of-range limit and bad ops', () => {
  const limits = DEFAULT_ANALYSIS_LIMITS
  assert.throws(() => validateAggSpec('', limits), /spec is required/)
  assert.throws(() => validateAggSpec('not-json', limits), /JSON/)
  assert.throws(() => validateAggSpec('[]', limits), /must be object/)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: 0 }), limits), /1\.\./)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: limits.maxAggLimit + 1 }), limits), /1\.\./)
  assert.throws(() => validateAggSpec(JSON.stringify({ limit: 'x' }), limits), /1\.\./)
  assert.throws(
    () => validateAggSpec(JSON.stringify({ filter: [{ column: 'a', op: 'regex' }] }), limits),
    /bad filter op/,
  )
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
