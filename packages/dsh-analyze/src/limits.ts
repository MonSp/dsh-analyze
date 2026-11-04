/**
 * Shared limits for analysis tools / CLI bridge.
 * @module dsh-analyze/limits
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
  maxValuesLimit: number
  maxJoinOnColumns: number
  maxKeyColumns: number
  maxOutlierColumns: number
  maxCorrColumns: number
  maxCrosstabTop: number
  maxSeriesOps: number
  maxSeriesPeriods: number
  maxSeriesWindow: number
}

export const DEFAULT_ANALYSIS_LIMITS: AnalysisLimits = {
  timeoutMs: 60_000,
  maxOutputBytes: 262_144,
  maxSampleRows: 200,
  maxHeadRows: 100,
  maxIngestBytes: 268_435_456,
  maxRows: 5_000_000,
  defaultRowLimit: 1000,
  maxSpecBytes: 32_768,
  maxAggLimit: 5000,
  maxValuesLimit: 100,
  maxJoinOnColumns: 8,
  maxKeyColumns: 16,
  maxOutlierColumns: 32,
  maxCorrColumns: 16,
  maxCrosstabTop: 50,
  maxSeriesOps: 8,
  maxSeriesPeriods: 10_000,
  maxSeriesWindow: 10_000,
}
