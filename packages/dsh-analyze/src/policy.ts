/**
 * Allowlist helpers for analysis tools.
 * @module dsh-analyze/policy
 */

export const ANALYSIS_TOOL_NAMES = [
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
] as const

export type AnalysisToolName = (typeof ANALYSIS_TOOL_NAMES)[number]

export function isAnalysisToolName(name: string): name is AnalysisToolName {
  return (ANALYSIS_TOOL_NAMES as readonly string[]).includes(name)
}

/**
 * Analysis tools to register on an agent scope.
 * - deny-all → none
 * - explicit `analysisTools` wins (any mode except deny-all)
 * - allowlist → intersection with toolAllowlist
 * - unconfigured without analysisTools → none (safe default)
 */
export function analysisToolsForScopedAllow(input: {
  mode: 'unconfigured' | 'deny-all' | 'allowlist'
  toolAllowlist: readonly string[]
  analysisTools?: readonly string[]
}): AnalysisToolName[] {
  if (input.mode === 'deny-all') return []
  if (input.analysisTools && input.analysisTools.length > 0) {
    const set = new Set(input.analysisTools)
    return ANALYSIS_TOOL_NAMES.filter(n => set.has(n))
  }
  if (input.mode !== 'allowlist') return []
  const allow = new Set(input.toolAllowlist)
  return ANALYSIS_TOOL_NAMES.filter(n => allow.has(n))
}

/** Back-compat alias for hosts that still import the old name. */
export const analysisToolsForExpert = analysisToolsForScopedAllow
