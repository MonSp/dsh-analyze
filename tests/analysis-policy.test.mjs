import test from 'node:test'
import assert from 'node:assert/strict'
import {
  ANALYSIS_TOOL_NAMES,
  analysisToolsForExpert,
  isAnalysisToolName,
} from '../packages/dsh-analyze/dist/policy.js'

test('analysisToolsForExpert: unconfigured → none', () => {
  assert.deepEqual(
    analysisToolsForExpert({ mode: 'unconfigured', toolAllowlist: [...ANALYSIS_TOOL_NAMES] }),
    [],
  )
})

test('analysisToolsForExpert: deny-all → none', () => {
  assert.deepEqual(
    analysisToolsForExpert({ mode: 'deny-all', toolAllowlist: ['dataset_ingest'] }),
    [],
  )
})

test('analysisToolsForExpert: allowlist filters intersection', () => {
  const names = analysisToolsForExpert({
    mode: 'allowlist',
    toolAllowlist: ['dataset_ingest', 'stats_describe', 'bash', 'unknown_tool'],
  })
  assert.deepEqual(names, ['dataset_ingest', 'stats_describe'])
})

test('isAnalysisToolName', () => {
  assert.equal(isAnalysisToolName('transform_agg'), true)
  assert.equal(isAnalysisToolName('dataset_list'), true)
  assert.equal(isAnalysisToolName('bash'), false)
})

test('analysisToolsForExpert: explicit analysisTools works under unconfigured', () => {
  const names = analysisToolsForExpert({
    mode: 'unconfigured',
    toolAllowlist: [],
    analysisTools: ['dataset_ingest', 'stats_describe', 'not-a-tool'],
  })
  assert.deepEqual(names, ['dataset_ingest', 'stats_describe'])
})

test('analysisToolsForExpert: deny-all ignores analysisTools', () => {
  assert.deepEqual(
    analysisToolsForExpert({
      mode: 'deny-all',
      toolAllowlist: [],
      analysisTools: ['dataset_ingest'],
    }),
    [],
  )
})

test('ANALYSIS_TOOL_NAMES is the frozen v0.1 surface', () => {
  assert.deepEqual([...ANALYSIS_TOOL_NAMES], [
    'dataset_ingest',
    'dataset_head',
    'dataset_schema',
    'dataset_list',
    'dataset_export',
    'dataset_rm',
    'stats_describe',
    'transform_agg',
  ])
})
