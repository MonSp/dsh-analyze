import test from 'node:test'
import assert from 'node:assert/strict'
import {
  ANALYSIS_TOOL_NAMES,
  analysisToolsForScopedAllow,
  isAnalysisToolName,
} from '../packages/dsh-analyze/dist/policy.js'

test('analysisToolsForScopedAllow: unconfigured → none', () => {
  assert.deepEqual(
    analysisToolsForScopedAllow({ mode: 'unconfigured', toolAllowlist: [...ANALYSIS_TOOL_NAMES] }),
    [],
  )
})

test('analysisToolsForScopedAllow: deny-all → none', () => {
  assert.deepEqual(
    analysisToolsForScopedAllow({ mode: 'deny-all', toolAllowlist: ['dataset_ingest'] }),
    [],
  )
})

test('analysisToolsForScopedAllow: allowlist filters intersection', () => {
  const names = analysisToolsForScopedAllow({
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

test('analysisToolsForScopedAllow: explicit analysisTools works under unconfigured', () => {
  const names = analysisToolsForScopedAllow({
    mode: 'unconfigured',
    toolAllowlist: [],
    analysisTools: ['dataset_ingest', 'stats_describe', 'not-a-tool'],
  })
  assert.deepEqual(names, ['dataset_ingest', 'stats_describe'])
})

test('analysisToolsForScopedAllow: deny-all ignores analysisTools', () => {
  assert.deepEqual(
    analysisToolsForScopedAllow({
      mode: 'deny-all',
      toolAllowlist: [],
      analysisTools: ['dataset_ingest'],
    }),
    [],
  )
})

test('ANALYSIS_TOOL_NAMES is the frozen v0.1.5 surface', () => {
  assert.deepEqual([...ANALYSIS_TOOL_NAMES], [
    'dataset_ingest',
    'dataset_head',
    'dataset_schema',
    'dataset_list',
    'dataset_export',
    'dataset_rm',
    'stats_describe',
    'transform_agg',
    'dataset_values',
    'dataset_sample',
    'dataset_join',
    'dataset_profile',
    'dataset_dedup',
    'dataset_outliers',
    'dataset_corr',
    'dataset_crosstab',
    'dataset_series',
  ])
})

test('v0.1.3 quality tools are recognized by isAnalysisToolName', () => {
  assert.equal(isAnalysisToolName('dataset_values'), true)
  assert.equal(isAnalysisToolName('dataset_sample'), true)
  assert.equal(isAnalysisToolName('dataset_join'), true)
  assert.equal(isAnalysisToolName('dataset_profile'), true)
  assert.equal(isAnalysisToolName('dataset_dedup'), true)
  assert.equal(isAnalysisToolName('dataset_outliers'), true)
})

test('v0.1.4 relation tools are recognized by isAnalysisToolName', () => {
  assert.equal(isAnalysisToolName('dataset_corr'), true)
  assert.equal(isAnalysisToolName('dataset_crosstab'), true)
})

test('v0.1.5 series tool is recognized by isAnalysisToolName', () => {
  assert.equal(isAnalysisToolName('dataset_series'), true)
})

test('analysisToolsForScopedAllow can enable quality tools only', () => {
  const names = analysisToolsForScopedAllow({
    mode: 'unconfigured',
    toolAllowlist: [],
    analysisTools: ['dataset_profile', 'dataset_dedup', 'dataset_outliers'],
  })
  assert.deepEqual(names, ['dataset_profile', 'dataset_dedup', 'dataset_outliers'])
})

test('analysisToolsForScopedAllow can enable relation tools only', () => {
  const names = analysisToolsForScopedAllow({
    mode: 'unconfigured',
    toolAllowlist: [],
    analysisTools: ['dataset_corr', 'dataset_crosstab'],
  })
  assert.deepEqual(names, ['dataset_corr', 'dataset_crosstab'])
})
