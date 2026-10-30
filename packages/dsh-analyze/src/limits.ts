/**
 * Shared limits for analysis tools / CLI bridge.
 * @module @investigator/backend/analysis/limits
 */

export interface AnalysisLimits {
  timeoutMs: number
  maxOutputBytes: number
  maxSampleRows: number
  maxHeadRows: number
  maxIngestBytes: number
  maxRows: number
  defaultRowLimit: number
  maxSpecBytes: number
  maxAggLimit: number
}

export const DEFAULT_ANALYSIS_LIMITS: AnalysisLimits = {
  timeoutMs: 60_000,
  maxOutputBytes: 262_144,
  maxSampleRows: 8,
  maxHeadRows: 20,
  maxIngestBytes: 268_435_456,
  maxRows: 5_000_000,
  defaultRowLimit: 1000,
  maxSpecBytes: 32_768,
  maxAggLimit: 5000,
}

export function clampHeadN(n: unknown, limits: AnalysisLimits): number {
  const raw = typeof n === 'number' && Number.isFinite(n) ? Math.floor(n) : 5
  return Math.min(Math.max(raw, 1), limits.maxHeadRows)
}
