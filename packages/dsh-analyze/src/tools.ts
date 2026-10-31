/**
 * Model-facing analysis tools (hand-written ToolDefinition; no defineTool dep).
 * @module dsh-analyze/tools
 */

import type { AnalysisLimits } from './limits.js'
import { DEFAULT_ANALYSIS_LIMITS, clampHeadN, clampSampleN, clampValuesLimit } from './limits.js'
import { AnalyzeCliError, runAnalyzeCli } from './cli.js'
import {
  AnalysisPathError,
  newDatasetId,
  resolveInWorkspace,
  validateDatasetId,
} from './paths.js'
import type { AnalysisToolName } from './policy.js'
import { resolve, sep } from 'node:path'

export interface AnalysisRuntimeConfig {
  cliPath: string
  /** Extra argv before analyze args (e.g. path to JS CLI when cliPath is node). */
  cliPrefixArgs?: readonly string[]
  /** Absolute session data root. Must not be the profile root itself. */
  workspaceRoot: string
  /** Session that owns this tool scope (for audit / future checks). */
  sessionId?: string
  /** Optional profile root used to hard-reject credential paths on ingest. */
  profileRoot?: string
  limits?: Partial<AnalysisLimits>
}

const MAX_LABEL_LEN = 200
const MAX_PATH_LEN = 1024

/** Per-workspace FIFO chain so one session's CLI calls never overlap. */
const cliLocks = new Map<string, Promise<unknown>>()

async function withWorkspaceCliLock<T>(key: string, fn: () => Promise<T>): Promise<T> {
  const prev = cliLocks.get(key) ?? Promise.resolve()
  const run = prev.then(fn, fn)
  const guard = run.then(
    () => undefined,
    () => undefined,
  )
  cliLocks.set(key, guard)
  try {
    return await run
  } finally {
    if (cliLocks.get(key) === guard) cliLocks.delete(key)
  }
}

/** Profile-relative prefixes that must never be ingested as data sources. */
const SENSITIVE_PROFILE_PREFIXES = [
  '.bootstrap-token',
  'profiles',
  '.credentials.yaml',
  '.writer.lock',
] as const

function isUnder(root: string, abs: string): boolean {
  const r = resolve(root)
  const a = resolve(abs)
  return a === r || a.startsWith(r + sep)
}

/** Reject profile credential / config paths even if they resolve under workspaceRoot. */
export function assertNotSensitiveSource(absPath: string, profileRoot?: string): void {
  if (!profileRoot) return
  const profile = resolve(profileRoot)
  if (!isUnder(profile, absPath)) return
  const rel = absPath.slice(profile.length).replace(/^[\\/]+/, '')
  const parts = rel.split(/[\\/]/).filter(Boolean)
  if (parts.length === 0) {
    throw new AnalysisPathError('analysis/invalid-path', 'profile root is not a data source')
  }
  const first = parts[0]!
  if ((SENSITIVE_PROFILE_PREFIXES as readonly string[]).includes(first)) {
    throw new AnalysisPathError(
      'analysis/invalid-path',
      `profile path not allowed as data source: ${first}`,
    )
  }
}

function textRender(text: string): Array<{ type: 'text'; text: string }> {
  return [{ type: 'text', text }]
}

function failMessage(e: unknown): never {
  if (e instanceof AnalyzeCliError || e instanceof AnalysisPathError) {
    const err = new Error(e.message) as Error & { code?: string }
    err.code = e.code
    throw err
  }
  const msg = e instanceof Error ? e.message : String(e)
  const err = new Error(msg) as Error & { code?: string }
  err.code = 'analysis/cli-failed'
  throw err
}

/** Strict integer 1..=max for quality-tool sample limits (invalid → invalid-args). */
export function validateSampleLimit(n: unknown, def: number, max: number): number {
  if (n === undefined || n === null) return def
  const raw = typeof n === 'number' ? n : Number(n)
  if (!Number.isInteger(raw) || raw < 1 || raw > max) {
    const err = new Error(
      `analysis/invalid-args: sampleLimit must be integer 1..${max}`,
    ) as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  return raw
}

/** Whitelist arithmetic expression: identifiers, numbers, + - * /, parens, unary minus. */
export function validateMutateExpr(expr: unknown): string {
  if (typeof expr !== 'string' || !expr.trim() || expr.length > 512) {
    const err = new Error('analysis/invalid-args: mutate expr must be non-empty string ≤512') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  // Reject anything outside the grammar before spawn (defense in depth; CLI re-checks).
  if (!/^[A-Za-z0-9_\s+\-*/().]+$/.test(expr)) {
    const err = new Error('analysis/invalid-args: mutate expr has illegal characters') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  if (/[A-Za-z_]\w*\s*\(/.test(expr)) {
    const err = new Error('analysis/invalid-args: function calls are not allowed in mutate expr') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  if (!/[A-Za-z_]/.test(expr) && !/\d/.test(expr)) {
    const err = new Error('analysis/invalid-args: mutate expr is empty of operands') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  return expr
}

/** Validate agg spec string before CLI. */
export function validateAggSpec(spec: string, limits: AnalysisLimits): string {
  if (typeof spec !== 'string' || !spec.trim()) {
    const err = new Error('analysis/invalid-args: spec is required') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  if (spec.length > limits.maxSpecBytes) {
    const err = new Error('analysis/limit: spec too large') as Error & { code?: string }
    err.code = 'analysis/limit'
    throw err
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(spec)
  } catch {
    const err = new Error('analysis/invalid-args: spec must be JSON object string') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    const err = new Error('analysis/invalid-args: spec must be object') as Error & { code?: string }
    err.code = 'analysis/invalid-args'
    throw err
  }
  const obj = parsed as Record<string, unknown>
  if (obj.limit !== undefined) {
    const n = Number(obj.limit)
    if (!Number.isFinite(n) || n < 1 || n > limits.maxAggLimit) {
      const err = new Error(`analysis/invalid-args: limit must be 1..${limits.maxAggLimit}`) as Error & { code?: string }
      err.code = 'analysis/invalid-args'
      throw err
    }
  }
  if (Array.isArray(obj.filter)) {
    const ops = new Set(['eq', 'ne', 'gt', 'gte', 'lt', 'lte', 'in', 'contains', 'is_null', 'not_null'])
    for (const f of obj.filter as Array<Record<string, unknown>>) {
      if (!f || typeof f !== 'object' || typeof f.op !== 'string' || !ops.has(f.op)) {
        const err = new Error('analysis/invalid-args: bad filter op') as Error & { code?: string }
        err.code = 'analysis/invalid-args'
        throw err
      }
    }
  }
  if (Array.isArray(obj.mutate)) {
    const asNames = new Set<string>()
    for (const m of obj.mutate as Array<Record<string, unknown>>) {
      if (!m || typeof m !== 'object' || typeof m.as !== 'string' || !/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(m.as)) {
        const err = new Error('analysis/invalid-args: invalid mutate name') as Error & { code?: string }
        err.code = 'analysis/invalid-args'
        throw err
      }
      if (asNames.has(m.as)) {
        const err = new Error(`analysis/invalid-args: duplicate mutate name ${m.as}`) as Error & { code?: string }
        err.code = 'analysis/invalid-args'
        throw err
      }
      asNames.add(m.as)
      validateMutateExpr(m.expr)
    }
  }
  if (Array.isArray(obj.groupBy)) {
    for (const g of obj.groupBy as unknown[]) {
      if (typeof g === 'string') continue
      if (g && typeof g === 'object' && !Array.isArray(g)) {
        const go = g as Record<string, unknown>
        if (typeof go.dateColumn !== 'string' || typeof go.period !== 'string') {
          const err = new Error('analysis/invalid-args: groupBy object needs dateColumn and period') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (!['day', 'week', 'month'].includes(go.period)) {
          const err = new Error('analysis/invalid-args: period must be day|week|month') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        continue
      }
      const err = new Error('analysis/invalid-args: groupBy entry must be string or object') as Error & { code?: string }
      err.code = 'analysis/invalid-args'
      throw err
    }
  }
  if (Array.isArray(obj.aggs)) {
    const fns = new Set(['count', 'sum', 'mean', 'min', 'max', 'median', 'std'])
    for (const a of obj.aggs as Array<Record<string, unknown>>) {
      if (!a || typeof a !== 'object' || typeof a.fn !== 'string' || !fns.has(a.fn)) {
        const err = new Error('analysis/invalid-args: bad agg fn') as Error & { code?: string }
        err.code = 'analysis/invalid-args'
        throw err
      }
    }
  }
  return spec
}

export function createAnalysisToolDefinitions(config: AnalysisRuntimeConfig) {
  const limits: AnalysisLimits = { ...DEFAULT_ANALYSIS_LIMITS, ...config.limits }
  const ws = config.workspaceRoot
  const cliPath = config.cliPath

  const run = async (args: string[], signal?: AbortSignal) => {
    try {
      return await withWorkspaceCliLock(ws, () => runAnalyzeCli({
        workspaceRoot: ws,
        cliPath,
        cliPrefixArgs: config.cliPrefixArgs,
        args,
        limits,
        signal,
      }))
    } catch (e) {
      failMessage(e)
    }
  }

  const datasetIngstDescription =
    'Ingest a local workspace file (csv/jsonl) into the analysis dataset store. ' +
    'Source path must be under this session analysis root (relative preferred). ' +
    'If the file was produced elsewhere (bash, connector, export), first copy/write it ' +
    'into the session analysis root, then ingest with a relative path. ' +
    'Returns a datasetId and schema summary only — never the full table. ' +
    'Use this before other dataset_* tools. Large data stays on disk.'

  const defs: Record<string, unknown> = {
    dataset_ingest: {
      name: 'dataset_ingest',
      description: datasetIngstDescription,
      parameters: {
        type: 'object',
        properties: {
          path: {
            type: 'string',
            description:
              'File path under the session analysis root (relative preferred). ' +
              'Copy external outputs into the analysis root first, then pass a relative path.',
          },
          format: {
            type: 'string',
            enum: ['csv', 'jsonl', 'parquet'],
            description: 'Source format. Omit to infer from extension.',
          },
          label: { type: 'string', description: 'Optional short label for humans.' },
        },
        required: ['path'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            columnCount: { type: 'number' },
            columns: {
              type: 'array',
              items: {
                type: 'object',
                properties: {
                  name: { type: 'string' },
                  dtype: { type: 'string' },
                },
              },
            },
            bytesOnDisk: { type: 'number' },
          },
          required: ['datasetId', 'rowCount', 'columnCount', 'columns'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columnCount: number
            columns: Array<{ name: string; dtype: string }>
          }
          const cols = (v.columns ?? []).map(c => `${c.name}:${c.dtype}`).join(', ')
          return textRender(
            `dataset ${v.datasetId} ready (${v.rowCount} rows × ${v.columnCount} cols).\n` +
              `columns: ${cols}\n` +
              'Next: dataset_head / stats_describe / transform_agg. Do not dump full rows into the conversation.',
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { path: string; format?: string; label?: string }
        if (typeof a.path !== 'string' || a.path.length === 0 || a.path.length > MAX_PATH_LEN) {
          const err = new Error('analysis/invalid-args: path length invalid') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (a.label !== undefined && a.label !== null) {
          if (typeof a.label !== 'string' || a.label.length > MAX_LABEL_LEN) {
            const err = new Error(`analysis/invalid-args: label must be string ≤ ${MAX_LABEL_LEN}`) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
        }
        const abs = resolveInWorkspace(ws, a.path)
        assertNotSensitiveSource(abs, config.profileRoot)
        if (a.format === 'parquet') {
          const err = new Error('analysis/limit: parquet not enabled in T0 (use csv/jsonl)') as Error & { code?: string }
          err.code = 'analysis/limit'
          throw err
        }
        const cliArgs = ['ingest', '--input', a.path, '--dataset-id', newDatasetId()]
        if (a.format) cliArgs.push('--format', a.format)
        if (a.label) cliArgs.push('--label', a.label)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_head: {
      name: 'dataset_head',
      description:
        'Return up to 20 sample rows and column names for a datasetId. Use for orientation only; not for bulk analysis.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          n: { type: 'number', description: '1–20, default 5' },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            columns: { type: 'array', items: { type: 'string' } },
            rows: { type: 'array', items: { type: 'array' } },
            rowCount: { type: 'number' },
          },
          required: ['datasetId', 'columns', 'rows'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columns: string[]
            rows: unknown[][]
          }
          return textRender(
            `head of ${v.datasetId} (${v.rowCount} total rows)\n` +
              JSON.stringify({ columns: v.columns, rows: v.rows }, null, 2),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; n?: number }
        validateDatasetId(a.datasetId)
        const n = clampHeadN(a.n, limits)
        const result = await run(['head', '--dataset-id', a.datasetId, '--n', String(n)], exec?.signal)
        return result.data
      },
    },

    dataset_schema: {
      name: 'dataset_schema',
      description: 'Column names, dtypes, and row count for a datasetId.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            columns: {
              type: 'array',
              items: {
                type: 'object',
                properties: {
                  name: { type: 'string' },
                  dtype: { type: 'string' },
                },
              },
            },
          },
          required: ['datasetId', 'rowCount', 'columns'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columns: Array<{ name: string; dtype: string }>
          }
          const lines = (v.columns ?? []).map(c => `- ${c.name}: ${c.dtype}`)
          return textRender(`${v.datasetId}: ${v.rowCount} rows\n${lines.join('\n')}`)
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string }
        validateDatasetId(a.datasetId)
        const result = await run(['schema', '--dataset-id', a.datasetId], exec?.signal)
        return result.data
      },
    },

    dataset_list: {
      name: 'dataset_list',
      description:
        'List datasets in this session analysis store: datasetId, label, rowCount, columnCount, producedBy. ' +
        'Use to discover prior ingests and transform outputs before re-running work.',
      parameters: {
        type: 'object',
        properties: {},
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasets: {
              type: 'array',
              items: {
                type: 'object',
                properties: {
                  datasetId: { type: 'string' },
                  rowCount: { type: 'number' },
                  columnCount: { type: 'number' },
                  label: { type: ['string', 'null'] },
                  producedBy: { type: 'string' },
                },
                additionalProperties: true,
              },
            },
          },
          required: ['datasets'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasets?: Array<{
              datasetId: string
              rowCount: number
              columnCount: number
              label?: string | null
              producedBy: string
            }>
          }
          const rows = v.datasets ?? []
          if (rows.length === 0) {
            return textRender('no datasets in this session store. Use dataset_ingest first.')
          }
          const lines = rows.map((d) =>
            `- ${d.datasetId}: ${d.rowCount} rows × ${d.columnCount} cols` +
            (d.label ? ` · ${d.label}` : '') +
            ` (${d.producedBy})`,
          )
          return textRender(`datasets (${rows.length}):\n${lines.join('\n')}`)
        },
      },
      async execute(_args: unknown, exec?: { signal?: AbortSignal }) {
        const result = await run(['list'], exec?.signal)
        return result.data
      },
    },

    dataset_export: {
      name: 'dataset_export',
      description:
        'Export a dataset to CSV under the session analysis root outputs/ (.dsh-analyze/outputs/). ' +
        'Returns outputPath relative to the workspace — does not print rows. ' +
        'Use when another tool or human needs the full table on disk.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          outPath: {
            type: 'string',
            description:
              'Optional file name (or workspace-relative path; the file name is kept). ' +
              'Always written under .dsh-analyze/outputs/. Defaults to <datasetId>.csv.',
          },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            outputPath: { type: 'string' },
            rowCount: { type: 'number' },
            bytes: { type: 'number' },
          },
          required: ['outputPath', 'rowCount'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as { outputPath: string; rowCount: number; bytes?: number }
          return textRender(
            `exported ${v.rowCount} rows → ${v.outputPath}` +
              (typeof v.bytes === 'number' ? ` (${v.bytes} bytes)` : ''),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; outPath?: string }
        validateDatasetId(a.datasetId)
        const outPath =
          typeof a.outPath === 'string' && a.outPath.trim()
            ? a.outPath.trim()
            : `.dsh-analyze/outputs/${a.datasetId}.csv`
        if (outPath.length > MAX_PATH_LEN) {
          const err = new Error('analysis/invalid-args: outPath too long') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        // Double-check destination stays in workspace before spawning CLI.
        resolveInWorkspace(ws, outPath)
        const result = await run(
          ['export', '--dataset-id', a.datasetId, '--output', outPath],
          exec?.signal,
        )
        return result.data
      },
    },

    dataset_rm: {
      name: 'dataset_rm',
      description:
        'Permanently remove a dataset from this session store (meta + data files). Irreversible.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            removed: { type: 'boolean' },
          },
          required: ['datasetId', 'removed'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as { datasetId: string; removed?: boolean }
          return textRender(`removed ${v.datasetId}`)
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string }
        validateDatasetId(a.datasetId)
        const result = await run(['rm', '--dataset-id', a.datasetId], exec?.signal)
        return result.data
      },
    },

    stats_describe: {
      name: 'stats_describe',
      description:
        'Column statistics (null count, min/max/mean/std/quartiles for numeric columns). ' +
        'Prefer this over exporting raw rows when summarizing data.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          columns: {
            type: 'array',
            items: { type: 'string' },
            description: 'Optional subset of column names.',
          },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            stats: {
              type: 'array',
              items: {
                type: 'object',
                properties: {
                  name: { type: 'string' },
                  dtype: { type: 'string' },
                  nullCount: { type: 'number' },
                },
                additionalProperties: true,
              },
            },
          },
          required: ['datasetId', 'stats'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as { datasetId: string; stats: unknown }
          return textRender(`describe ${v.datasetId}\n${JSON.stringify(v.stats, null, 2)}`)
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; columns?: string[] }
        validateDatasetId(a.datasetId)
        const cliArgs = ['describe', '--dataset-id', a.datasetId]
        if (Array.isArray(a.columns) && a.columns.length > 0) {
          cliArgs.push('--columns', a.columns.join(','))
        }
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    transform_agg: {
      name: 'transform_agg',
      description:
        'Filter / project / group-by / aggregate a dataset. ' +
        'spec may include optional mutate derived columns and time-period groupBy objects. ' +
        'Returns at most `limit` rows (default 1000). ' +
        'Set outLabel to persist a new datasetId for further steps.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          spec: {
            type: 'string',
            description:
              'JSON string: { select?, filter?, mutate?, groupBy?, aggs?, orderBy?, limit? }. ' +
              'filter ops: eq/ne/gt/gte/lt/lte/in/contains/is_null/not_null. ' +
              'agg fns: count/sum/mean/min/max/median/std. ' +
              'mutate: [{as,expr}] whitelist arithmetic only (no functions). ' +
              'groupBy entries: column name or {dateColumn,period:day|week|month}.',
          },
          outLabel: {
            type: 'string',
            description: 'If set, persist result as a new dataset and return its datasetId.',
          },
        },
        required: ['datasetId', 'spec'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            outDatasetId: { type: 'string' },
            rowCount: { type: 'number' },
            columns: { type: 'array', items: { type: 'string' } },
            rows: { type: 'array', items: { type: 'array' } },
            truncated: { type: 'boolean' },
          },
          required: ['datasetId'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            outDatasetId?: string | null
            rowCount: number
            columns: string[]
            truncated?: boolean
          }
          if (v.truncated || v.outDatasetId) {
            return textRender(
              `agg → ${v.outDatasetId ?? v.datasetId}: ${v.rowCount} rows ` +
                `(full result stored; use dataset_head or further agg). columns=${(v.columns ?? []).join(',')}`,
            )
          }
          return textRender(JSON.stringify(value, null, 2))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; spec: string; outLabel?: string }
        validateDatasetId(a.datasetId)
        const spec = validateAggSpec(a.spec, limits)
        const cliArgs = ['agg', '--dataset-id', a.datasetId, '--spec', spec]
        if (a.outLabel) cliArgs.push('--out', a.outLabel)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_values: {
      name: 'dataset_values',
      description:
        'Top value distribution for one column: nunique, nullCount, top values with counts/ratios. ' +
        'Use to understand categorical / low-cardinality columns without dumping rows.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          column: { type: 'string', description: 'Column name (required).' },
          limit: { type: 'number', description: '1–100, default 20. Length of top list only.' },
        },
        required: ['datasetId', 'column'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            column: { type: 'string' },
            nunique: { type: 'number' },
            nullCount: { type: 'number' },
            top: {
              type: 'array',
              items: {
                type: 'object',
                properties: {
                  value: {},
                  count: { type: 'number' },
                  ratio: { type: 'number' },
                },
              },
            },
          },
          required: ['datasetId', 'column', 'nunique', 'nullCount', 'top'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            column: string
            nunique: number
            nullCount: number
            top: Array<{ value: unknown; count: number; ratio: number }>
          }
          const lines = (v.top ?? []).map(
            t => `  ${JSON.stringify(t.value)} × ${t.count} (${(t.ratio * 100).toFixed(1)}%)`,
          )
          return textRender(
            `values ${v.column} in ${v.datasetId}\n` +
              `nunique=${v.nunique} nullCount=${v.nullCount}\n` +
              (lines.join('\n') || '  (no non-null values)'),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; column: string; limit?: number }
        validateDatasetId(a.datasetId)
        if (typeof a.column !== 'string' || !a.column.trim()) {
          const err = new Error('analysis/invalid-args: column required') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const limit = clampValuesLimit(a.limit, limits)
        const result = await run(
          ['values', '--dataset-id', a.datasetId, '--column', a.column, '--limit', String(limit)],
          exec?.signal,
        )
        return result.data
      },
    },

    dataset_sample: {
      name: 'dataset_sample',
      description:
        'Deterministic random (or stratified) sample of up to 20 rows. ' +
        'Same datasetId + n + seed always yields the same rows. Prefer over dataset_head when you need a spread, not the first rows.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          n: { type: 'number', description: '1–20, default 10' },
          seed: { type: 'number', description: 'Unsigned integer seed; default 42 for reproducibility.' },
          stratifyBy: { type: 'string', description: 'Optional column for proportional strata sampling.' },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            columns: { type: 'array', items: { type: 'string' } },
            rows: { type: 'array', items: { type: 'array' } },
            sampling: {
              type: 'object',
              properties: {
                method: { type: 'string' },
                n: { type: 'number' },
                seed: { type: 'number' },
                stratifyBy: { type: ['string', 'null'] },
                population: { type: 'number' },
              },
            },
          },
          required: ['datasetId', 'columns', 'rows', 'sampling'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            columns: string[]
            rows: unknown[][]
            sampling?: { method?: string; n?: number; seed?: number; population?: number }
          }
          return textRender(
            `sample of ${v.datasetId} (${v.rows?.length ?? 0} rows, ` +
              `${v.sampling?.method ?? 'random'} seed=${v.sampling?.seed ?? '?'} pop=${v.sampling?.population ?? '?'} )\n` +
              JSON.stringify({ columns: v.columns, rows: v.rows }, null, 2),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; n?: number; seed?: number; stratifyBy?: string }
        validateDatasetId(a.datasetId)
        const n = clampSampleN(a.n, limits)
        const cliArgs = ['sample', '--dataset-id', a.datasetId, '--n', String(n)]
        if (a.seed !== undefined && a.seed !== null) {
          if (!Number.isFinite(a.seed) || a.seed < 0 || !Number.isInteger(a.seed)) {
            const err = new Error('analysis/invalid-args: seed must be a non-negative integer') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          cliArgs.push('--seed', String(a.seed))
        }
        if (a.stratifyBy) cliArgs.push('--stratify-by', a.stratifyBy)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_join: {
      name: 'dataset_join',
      description:
        'Equi-join two datasets on shared key column(s). Always writes a new datasetId ' +
        '(producedBy=join). Returns only the new id + row/column counts — never full rows. ' +
        'how: inner (default) or left. Null keys never match.',
      parameters: {
        type: 'object',
        properties: {
          leftDatasetId: { type: 'string' },
          rightDatasetId: { type: 'string' },
          on: {
            type: 'array',
            items: { type: 'string' },
            description: 'Non-empty list of column names present on both sides.',
          },
          how: { type: 'string', enum: ['inner', 'left'], description: 'Default inner.' },
          outLabel: { type: 'string', description: 'Optional label stored on the new dataset.' },
        },
        required: ['leftDatasetId', 'rightDatasetId', 'on'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            columnCount: { type: 'number' },
            leftDatasetId: { type: 'string' },
            rightDatasetId: { type: 'string' },
            how: { type: 'string' },
          },
          required: ['datasetId', 'rowCount', 'columnCount'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columnCount: number
            how?: string
          }
          return textRender(
            `join → ${v.datasetId}: ${v.rowCount} rows × ${v.columnCount} cols (how=${v.how ?? 'inner'}). ` +
              'Inspect with dataset_head / stats_describe / transform_agg.',
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          leftDatasetId: string
          rightDatasetId: string
          on: string[]
          how?: string
          outLabel?: string
        }
        validateDatasetId(a.leftDatasetId)
        validateDatasetId(a.rightDatasetId)
        if (!Array.isArray(a.on) || a.on.length === 0 || a.on.length > limits.maxJoinOnColumns) {
          const err = new Error(`analysis/invalid-args: on must be 1..${limits.maxJoinOnColumns} column names`) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (a.on.some(c => typeof c !== 'string' || !c.trim())) {
          const err = new Error('analysis/invalid-args: on entries must be non-empty strings') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const how = a.how ?? 'inner'
        if (how !== 'inner' && how !== 'left') {
          const err = new Error('analysis/invalid-args: how must be inner|left') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const cliArgs = [
          'join',
          '--left', a.leftDatasetId,
          '--right', a.rightDatasetId,
          '--on', a.on.join(','),
          '--how', how,
        ]
        if (a.outLabel) cliArgs.push('--out', a.outLabel)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_profile: {
      name: 'dataset_profile',
      description:
        'Quality profile of a dataset: per-column null rate, nunique, type stability, ' +
        'high-cardinality flags, suspect primary keys, and coarse warnings. ' +
        'Summary only — never full columns. Use at the start of analysis to judge whether a table is usable.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          highCardinalityThreshold: {
            type: 'number',
            description: 'nunique above this marks highCardinality (default 200).',
          },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            columnCount: { type: 'number' },
            columns: { type: 'array', items: { type: 'object', additionalProperties: true } },
            suspectKeys: { type: 'array', items: { type: 'object', additionalProperties: true } },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
          },
          required: ['datasetId', 'rowCount', 'columns'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columns?: Array<{ name: string; nullRate?: number; typeStability?: string }>
            warnings?: Array<{ code: string; column?: string }>
            suspectKeys?: Array<{ columns: string[] }>
          }
          const cols = (v.columns ?? [])
            .map(
              c =>
                `- ${c.name}: ${c.typeStability ?? '?'}` +
                (typeof c.nullRate === 'number' ? ` null=${(c.nullRate * 100).toFixed(2)}%` : ''),
            )
            .join('\n')
          const warn = (v.warnings ?? []).map(w => `- ${w.code}${w.column ? `: ${w.column}` : ''}`).join('\n')
          const keys = (v.suspectKeys ?? []).map(k => k.columns.join('+')).join(', ')
          return textRender(
            `profile ${v.datasetId}: ${v.rowCount} rows\n${cols}` +
              (warn ? `\nwarnings:\n${warn}` : '') +
              (keys ? `\nsuspect keys: ${keys}` : ''),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; highCardinalityThreshold?: number }
        validateDatasetId(a.datasetId)
        const cliArgs = ['profile', '--dataset-id', a.datasetId]
        if (a.highCardinalityThreshold !== undefined && a.highCardinalityThreshold !== null) {
          const t = Number(a.highCardinalityThreshold)
          if (!Number.isInteger(t) || t < 1) {
            const err = new Error(
              'analysis/invalid-args: highCardinalityThreshold must be integer >= 1',
            ) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          cliArgs.push('--high-cardinality-threshold', String(t))
        }
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_dedup: {
      name: 'dataset_dedup',
      description:
        'Detect exact full-row duplicates or keyed duplicate groups. Returns counts + a few sample groups — never the full duplicate set. ' +
        'Omit keyColumns for exact whole-row mode; pass keyColumns for keyed export-duplicate checks.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          keyColumns: {
            type: 'array',
            items: { type: 'string' },
            description: 'Optional 1..16 key column names. Omit for exact whole-row dedup.',
          },
          sampleLimit: {
            type: 'number',
            description: '1–20 duplicate sample groups (default 5).',
          },
        },
        required: ['datasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            mode: { type: 'string' },
            duplicateRowCount: { type: 'number' },
            duplicateRowRate: { type: 'number' },
            samples: { type: 'array', items: { type: 'object', additionalProperties: true } },
            samplesTruncated: { type: 'boolean' },
          },
          required: ['datasetId', 'rowCount', 'mode', 'duplicateRowCount'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            mode: string
            duplicateRowCount: number
            duplicateRowRate?: number
            samplesTruncated?: boolean
          }
          const pct =
            typeof v.duplicateRowRate === 'number' ? ` (${(v.duplicateRowRate * 100).toFixed(2)}%)` : ''
          return textRender(
            `dedup ${v.datasetId} mode=${v.mode}: ${v.duplicateRowCount}/${v.rowCount} duplicate rows${pct}` +
              (v.samplesTruncated ? ' [samples truncated]' : ''),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; keyColumns?: string[]; sampleLimit?: number }
        validateDatasetId(a.datasetId)
        const cliArgs = ['dedup', '--dataset-id', a.datasetId]
        if (Array.isArray(a.keyColumns) && a.keyColumns.length > 0) {
          if (a.keyColumns.length > limits.maxKeyColumns) {
            const err = new Error(
              `analysis/invalid-args: keyColumns must be 1..${limits.maxKeyColumns} names`,
            ) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          if (a.keyColumns.some(c => typeof c !== 'string' || !c.trim())) {
            const err = new Error(
              'analysis/invalid-args: keyColumns entries must be non-empty strings',
            ) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          cliArgs.push('--key-columns', a.keyColumns.join(','))
        }
        const sampleLimit = validateSampleLimit(a.sampleLimit, 5, limits.maxSampleRows)
        cliArgs.push('--sample-limit', String(sampleLimit))
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_outliers: {
      name: 'dataset_outliers',
      description:
        'Flag numeric outliers on one or more columns using iqr | zscore | mad. ' +
        'Returns bounds, counts, and ≤20 sample rows per column — never full outlier lists. ' +
        'Optional outLabel writes a new dataset of outlier rows for further head/agg.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          columns: {
            type: 'array',
            items: { type: 'string' },
            description: 'Numeric-capable column names (1..32).',
          },
          method: { type: 'string', enum: ['iqr', 'zscore', 'mad'], description: 'Default iqr.' },
          k: { type: 'number', description: 'Threshold; default 1.5 (iqr) or 3.0 (zscore/mad).' },
          sampleLimit: {
            type: 'number',
            description: '1–20 sample rows per column (default 10).',
          },
          outLabel: {
            type: 'string',
            description: 'If set, write a new dataset of rows that are outliers in any column.',
          },
        },
        required: ['datasetId', 'columns'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            method: { type: 'string' },
            k: { type: 'number' },
            outDatasetId: { type: ['string', 'null'] },
            columns: { type: 'array', items: { type: 'object', additionalProperties: true } },
          },
          required: ['datasetId', 'method', 'columns'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            method: string
            k?: number
            outDatasetId?: string | null
            columns?: Array<{ name: string; outlierCount: number; lowerBound?: number | null; upperBound?: number | null }>
          }
          const lines = (v.columns ?? [])
            .map(
              c =>
                `- ${c.name}: ${c.outlierCount} outliers` +
                (c.lowerBound != null && c.upperBound != null
                  ? ` bounds=[${c.lowerBound}, ${c.upperBound}]`
                  : ''),
            )
            .join('\n')
          return textRender(
            `outliers ${v.datasetId} method=${v.method} k=${v.k ?? '?'}\n${lines}` +
              (v.outDatasetId ? `\n→ ${v.outDatasetId}` : ''),
          )
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          datasetId: string
          columns: string[]
          method?: string
          k?: number
          sampleLimit?: number
          outLabel?: string
        }
        validateDatasetId(a.datasetId)
        if (!Array.isArray(a.columns) || a.columns.length === 0 || a.columns.length > limits.maxOutlierColumns) {
          const err = new Error(
            `analysis/invalid-args: columns must be 1..${limits.maxOutlierColumns} names`,
          ) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (a.columns.some(c => typeof c !== 'string' || !c.trim())) {
          const err = new Error(
            'analysis/invalid-args: columns entries must be non-empty strings',
          ) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const method = a.method ?? 'iqr'
        if (method !== 'iqr' && method !== 'zscore' && method !== 'mad') {
          const err = new Error('analysis/invalid-args: method must be iqr|zscore|mad') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const cliArgs = [
          'outliers',
          '--dataset-id',
          a.datasetId,
          '--columns',
          a.columns.join(','),
          '--method',
          method,
        ]
        if (a.k !== undefined && a.k !== null) {
          if (!Number.isFinite(a.k) || a.k <= 0) {
            const err = new Error('analysis/invalid-args: k must be a finite number > 0') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          cliArgs.push('--k', String(a.k))
        }
        const sampleLimit = validateSampleLimit(a.sampleLimit, 10, limits.maxSampleRows)
        cliArgs.push('--sample-limit', String(sampleLimit))
        if (a.outLabel) cliArgs.push('--out', a.outLabel)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },
  }

  return defs as Record<AnalysisToolName, unknown>
}

export function createAnalysisDefinitionsForNames(
  names: readonly AnalysisToolName[],
  config: AnalysisRuntimeConfig,
): unknown[] {
  const all = createAnalysisToolDefinitions(config)
  return names.map(n => all[n])
}
