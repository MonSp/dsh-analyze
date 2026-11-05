/**
 * Model-facing analysis tools (hand-written ToolDefinition; no defineTool dep).
 * @module dsh-analyze/tools
 */

import type { AnalysisLimits } from './limits.js'
import { DEFAULT_ANALYSIS_LIMITS } from './limits.js'
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

function invalidArgs(message: string): Error & { code?: string } {
  const err = new Error(`analysis/invalid-args: ${message}`) as Error & { code?: string }
  err.code = 'analysis/invalid-args'
  return err
}

function limitError(message: string): Error & { code?: string } {
  const err = new Error(`analysis/limit: ${message}`) as Error & { code?: string }
  err.code = 'analysis/limit'
  return err
}

/** Unified `warnings[]` rendering appended to every tool result (R-20). */
export function renderWarningsBlock(value: unknown): string {
  const w = (value as { warnings?: unknown }).warnings
  if (!Array.isArray(w) || w.length === 0) return ''
  const lines = w.map((entry) => {
    const e = entry as {
      code?: unknown
      message?: unknown
      column?: unknown
      count?: unknown
      field?: unknown
      requested?: unknown
      actual?: unknown
    }
    let line = `- ${String(e.code ?? 'warning')}`
    if (typeof e.column === 'string' && e.column) line += `: ${e.column}`
    if (typeof e.count === 'number') line += ` (${e.count})`
    if (typeof e.field === 'string' && e.field) {
      line += ` ${e.field}: requested=${String(e.requested)} actual=${String(e.actual)}`
    }
    if (typeof e.message === 'string' && e.message) line += ` — ${e.message}`
    return line
  })
  return `\nwarnings:\n${lines.join('\n')}`
}

/** Append the unified warnings block to a rendered tool result. */
function withWarnings(text: string, value: unknown): string {
  return text + renderWarningsBlock(value)
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

/**
 * Positive-integer row-count / limit param: omitted → default; non-integer or
 * <1 → invalid-args. Values above the CLI cap pass through unclamped so the
 * CLI can clamp, echo `requestedN`/`requestedLimit`, and warn (`clamp`).
 */
export function validatePositiveInt(value: unknown, def: number, name: string): number {
  if (value === undefined || value === null) return def
  const raw = typeof value === 'number' ? value : Number(value)
  if (!Number.isInteger(raw) || raw < 1) {
    throw invalidArgs(`${name} must be integer >= 1`)
  }
  return raw
}

/** Back-compat alias: quality-tool sample limits share the pass-through policy. */
export function validateSampleLimit(n: unknown, def: number): number {
  return validatePositiveInt(n, def, 'sampleLimit')
}

/** Whitelist mutate call: only to_int(e) / to_str(e) / to_date(e, fmt). */
const MUTATE_CAST_FNS = new Set(['to_int', 'to_str', 'to_date'])
/** to_date fmt directives: %Y %m %d %H %M %S (literal separators allowed). */
const FMT_DIRECTIVES = new Set(['Y', 'm', 'd', 'H', 'M', 'S'])

function validateDateFormat(fmt: string): void {
  for (let i = 0; i < fmt.length; i++) {
    if (fmt[i] !== '%') continue
    const directive = fmt[i + 1]
    if (directive === undefined || !FMT_DIRECTIVES.has(directive)) {
      throw invalidArgs('to_date fmt has unsupported directive (only %Y %m %d %H %M %S)')
    }
    i++
  }
}

type MutToken =
  | { t: 'str'; v: string }
  | { t: 'id'; v: string }
  | { t: 'num'; v: string }
  | { t: 'op'; v: string }

function tokenizeMutateExpr(expr: string): MutToken[] {
  const tokens: MutToken[] = []
  let i = 0
  while (i < expr.length) {
    const ch = expr[i]!
    if (/\s/.test(ch)) {
      i++
      continue
    }
    if (ch === '"') {
      let j = i + 1
      while (j < expr.length && expr[j] !== '"') {
        if (expr[j] === '\\') j++
        j++
      }
      if (j >= expr.length) throw invalidArgs('mutate expr has illegal characters')
      tokens.push({ t: 'str', v: expr.slice(i + 1, j) })
      i = j + 1
      continue
    }
    if (/[A-Za-z_]/.test(ch)) {
      let j = i
      while (j < expr.length && /\w/.test(expr[j]!)) j++
      tokens.push({ t: 'id', v: expr.slice(i, j) })
      i = j
      continue
    }
    if (/[0-9]/.test(ch)) {
      let j = i
      while (j < expr.length && /[0-9.]/.test(expr[j]!)) j++
      tokens.push({ t: 'num', v: expr.slice(i, j) })
      i = j
      continue
    }
    if ('+-*/(),'.includes(ch)) {
      tokens.push({ t: 'op', v: ch })
      i++
      continue
    }
    throw invalidArgs('mutate expr has illegal characters')
  }
  return tokens
}

/**
 * Whitelist arithmetic expression: identifiers, numbers, string literals,
 * + - * /, parens, unary minus, and to_int/to_str/to_date(e, fmt) calls.
 */
export function validateMutateExpr(expr: unknown): string {
  if (typeof expr !== 'string' || !expr.trim() || expr.length > 512) {
    throw invalidArgs('mutate expr must be non-empty string ≤512')
  }
  if (!/[A-Za-z_]/.test(expr) && !/\d/.test(expr)) {
    throw invalidArgs('mutate expr is empty of operands')
  }
  const tokens = tokenizeMutateExpr(expr)
  let pos = 0
  const peek = (): MutToken | undefined => tokens[pos]
  const next = (): MutToken | undefined => tokens[pos++]

  const parsePrimary = (): void => {
    const tok = next()
    if (!tok) throw invalidArgs('mutate expr has illegal characters')
    if (tok.t === 'num' || tok.t === 'str') return
    if (tok.t === 'op' && tok.v === '(') {
      parseAdd()
      const close = next()
      if (!close || close.t !== 'op' || close.v !== ')') {
        throw invalidArgs('mutate expr has illegal characters')
      }
      return
    }
    if (tok.t === 'id') {
      const nxt = peek()
      if (nxt && nxt.t === 'op' && nxt.v === '(') {
        if (!MUTATE_CAST_FNS.has(tok.v)) {
          throw invalidArgs('function calls are not allowed in mutate expr')
        }
        next() // consume '('
        let argc = 0
        if (!(peek() && peek()!.t === 'op' && peek()!.v === ')')) {
          parseAdd() // value operand (any expression)
          argc = 1
          const sep = peek()
          if (sep && sep.t === 'op' && sep.v === ',') {
            if (tok.v !== 'to_date') {
              throw invalidArgs(`${tok.v}(e) takes 1 argument`)
            }
            next()
            const fmtTok = next()
            if (!fmtTok || fmtTok.t !== 'str') {
              throw invalidArgs('to_date fmt must be a string literal')
            }
            validateDateFormat(fmtTok.v)
            argc = 2
          }
        }
        const close = next()
        if (!close || close.t !== 'op' || close.v !== ')') {
          throw invalidArgs('mutate expr has illegal characters')
        }
        if (tok.v === 'to_date' && argc !== 2) {
          throw invalidArgs('to_date(e, fmt) takes 2 arguments')
        }
        if (tok.v !== 'to_date' && argc !== 1) {
          throw invalidArgs(`${tok.v}(e) takes 1 argument`)
        }
        return
      }
      return // plain identifier
    }
    if (tok.t === 'op' && tok.v === '-') {
      parseUnary()
      return
    }
    throw invalidArgs('mutate expr has illegal characters')
  }
  const parseUnary = (): void => {
    const tok = peek()
    if (tok && tok.t === 'op' && tok.v === '-') {
      next()
      parseUnary()
      return
    }
    parsePrimary()
  }
  const parseMul = (): void => {
    parseUnary()
    for (;;) {
      const tok = peek()
      if (tok && tok.t === 'op' && (tok.v === '*' || tok.v === '/')) {
        next()
        parseUnary()
      } else return
    }
  }
  const parseAdd = (): void => {
    parseMul()
    for (;;) {
      const tok = peek()
      if (tok && tok.t === 'op' && (tok.v === '+' || tok.v === '-')) {
        next()
        parseMul()
      } else return
    }
  }
  parseAdd()
  if (pos !== tokens.length) throw invalidArgs('mutate expr has illegal characters')
  return expr
}

const FILTER_OPS = new Set(['eq', 'ne', 'gt', 'gte', 'lt', 'lte', 'in', 'contains', 'is_null', 'not_null'])
const MAX_FILTER_DEPTH = 8

function validatePredicateEntry(f: Record<string, unknown>, label: string): void {
  if (!f || typeof f !== 'object' || typeof f.op !== 'string' || !FILTER_OPS.has(f.op)) {
    throw invalidArgs(`bad ${label} op`)
  }
  if (typeof f.column !== 'string' || !f.column) {
    throw invalidArgs(`${label} column required`)
  }
  if (f.op === 'is_null' || f.op === 'not_null') return // value ignored by the CLI
  if (!('value' in f)) {
    throw invalidArgs(`${label} value required for op ${f.op}`)
  }
  const v = f.value
  if (Array.isArray(v)) {
    if (f.op !== 'in') {
      throw invalidArgs(`${label} value must not be an array for op ${f.op}`)
    }
    if ((v as unknown[]).some((e) => typeof e !== 'string' && typeof e !== 'number' && typeof e !== 'boolean')) {
      throw invalidArgs(`bad ${label} value`)
    }
  } else if (typeof v !== 'string' && typeof v !== 'number' && typeof v !== 'boolean') {
    throw invalidArgs(`bad ${label} value`)
  }
}

/**
 * Predicate tree item: predicate | {"all":[item…]} | {"any":[item…]} | {"not":item}.
 * `depth` counts enclosing GROUP objects (wrapper arrays are not a level),
 * mirroring the CLI: 8 nested groups are allowed, a 9th errors. Empty all →
 * true, empty any → false.
 */
function validateFilterNode(node: unknown, label: string, depth: number): void {
  if (Array.isArray(node)) {
    throw invalidArgs(`${label} item must be a predicate or group`)
  }
  if (node && typeof node === 'object') {
    const o = node as Record<string, unknown>
    const keys = Object.keys(o)
    const groupKeys = keys.filter((k) => k === 'all' || k === 'any' || k === 'not')
    if ('op' in o || groupKeys.length === 0) {
      validatePredicateEntry(o, label)
      return
    }
    if (groupKeys.length !== 1 || keys.length !== 1) {
      throw invalidArgs(`bad ${label} group`)
    }
    if (depth + 1 > MAX_FILTER_DEPTH) throw invalidArgs(`${label} nesting too deep`)
    const kind = groupKeys[0]!
    if (kind === 'all' || kind === 'any') {
      const list = o[kind]
      if (!Array.isArray(list)) throw invalidArgs(`${label} ${kind} must be an array`)
      for (const item of list) validateFilterNode(item, label, depth + 1)
      return
    }
    validateFilterNode(o.not, label, depth + 1)
    return
  }
  throw invalidArgs(`bad ${label} op`)
}

/** Top-level filter/having shape: AND-array or a single {all|any|not} group. */
function validateFilterTree(root: unknown, label: string): void {
  if (Array.isArray(root)) {
    for (const item of root) validateFilterNode(item, label, 0)
    return
  }
  if (root && typeof root === 'object') {
    const o = root as Record<string, unknown>
    const keys = Object.keys(o)
    const groupKeys = keys.filter((k) => k === 'all' || k === 'any' || k === 'not')
    if (groupKeys.length === 1 && keys.length === 1 && !('op' in o)) {
      validateFilterNode(root, label, 0)
      return
    }
    if (groupKeys.length > 0) {
      throw invalidArgs(`bad ${label} group`)
    }
  }
  throw invalidArgs(`${label} must be an array or an all/any/not group`)
}

/** Validate agg spec string before CLI. */
export function validateAggSpec(spec: string, limits: AnalysisLimits): string {
  if (typeof spec !== 'string' || !spec.trim()) {
    throw invalidArgs('spec is required')
  }
  if (spec.length > limits.maxSpecBytes) {
    throw limitError('spec too large')
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(spec)
  } catch {
    throw invalidArgs('spec must be JSON object string')
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw invalidArgs('spec must be object')
  }
  const obj = parsed as Record<string, unknown>
  const knownSpecKeys = new Set([
    'select', 'filter', 'mutate', 'groupBy', 'aggs', 'orderBy', 'limit',
    'having', 'topKPerGroup', 'window',
  ])
  for (const key of Object.keys(obj)) {
    if (!knownSpecKeys.has(key)) {
      throw invalidArgs(`unknown spec key ${key}`)
    }
  }
  for (const key of ['select', 'mutate', 'groupBy', 'aggs', 'orderBy', 'window']) {
    if (obj[key] !== undefined && !Array.isArray(obj[key])) {
      throw invalidArgs(`${key} must be an array`)
    }
  }
  if (Array.isArray(obj.window)) {
    const wOps = obj.window as unknown[]
    if (wOps.length > 16) {
      throw invalidArgs('window must list 1..=16 ops')
    }
    const wNames = new Set<string>()
    for (const w of wOps) {
      if (!w || typeof w !== 'object' || Array.isArray(w)) {
        throw invalidArgs('window entry must be an object')
      }
      const wo = w as Record<string, unknown>
      for (const k of Object.keys(wo)) {
        if (!['fn', 'by', 'orderBy', 'column', 'alpha', 'as'].includes(k)) {
          throw invalidArgs(`unknown window key ${k}`)
        }
      }
      const fn = wo.fn
      if (typeof fn !== 'string' || !['rank', 'row_number', 'cumsum', 'ewm'].includes(fn)) {
        throw invalidArgs('bad window fn (rank|row_number|cumsum|ewm)')
      }
      const by = wo.by
      let byList: string[] = []
      if (by !== undefined) {
        if (!Array.isArray(by) || by.some((b) => typeof b !== 'string' || !b)) {
          throw invalidArgs('window by must be column names')
        }
        byList = by as string[]
        if (byList.length > 8) {
          throw invalidArgs('window by must list 0..=8 columns')
        }
      }
      const needsBy = fn === 'rank' || fn === 'row_number' || fn === 'cumsum'
      if (needsBy && byList.length === 0) {
        throw invalidArgs(`window ${fn} requires by`)
      }
      const ob = wo.orderBy
      if (!ob || typeof ob !== 'object' || Array.isArray(ob)) {
        throw invalidArgs(`window ${fn} requires orderBy`)
      }
      const obO = ob as Record<string, unknown>
      if (typeof obO.column !== 'string' || !obO.column) {
        throw invalidArgs('window orderBy needs a column name')
      }
      if (obO.desc !== undefined && typeof obO.desc !== 'boolean') {
        throw invalidArgs('window orderBy desc must be boolean')
      }
      const needsCol = fn === 'cumsum' || fn === 'ewm'
      if (needsCol && (typeof wo.column !== 'string' || !wo.column)) {
        throw invalidArgs(`window ${fn} requires column`)
      }
      if ((fn === 'rank' || fn === 'row_number') && wo.column !== undefined) {
        throw invalidArgs(`window ${fn} must not take column`)
      }
      if (fn === 'ewm') {
        const alpha = wo.alpha
        if (typeof alpha !== 'number' || !(alpha > 0 && alpha <= 1)) {
          throw invalidArgs('window alpha must be in (0,1]')
        }
      } else if (wo.alpha !== undefined) {
        throw invalidArgs(`window ${fn} must not take alpha`)
      }
      if (typeof wo.as !== 'string' || !/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(wo.as)) {
        throw invalidArgs('invalid window name')
      }
      if (wNames.has(wo.as)) {
        throw invalidArgs(`duplicate window name ${wo.as}`)
      }
      wNames.add(wo.as)
    }
  }
  if (obj.limit !== undefined) {
    const n = obj.limit
    if (typeof n !== 'number' || !Number.isFinite(n)) {
      throw invalidArgs('limit must be a number')
    }
    if (!Number.isInteger(n) || n < 1 || n > limits.maxAggLimit) {
      throw invalidArgs(`limit must be an integer 1..${limits.maxAggLimit}`)
    }
  }
  if (Array.isArray(obj.select)) {
    const outNames = new Set<string>()
    for (const s of obj.select as unknown[]) {
      let outName: string
      if (typeof s === 'string') {
        outName = s
      } else if (s && typeof s === 'object' && !Array.isArray(s)) {
        const so = s as Record<string, unknown>
        if (typeof so.column !== 'string' || !so.column) {
          throw invalidArgs('select entry column required')
        }
        if (typeof so.as !== 'string' || !/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(so.as)) {
          throw invalidArgs('select as must match [A-Za-z_][A-Za-z0-9_]* (<=64)')
        }
        outName = so.as
      } else {
        throw invalidArgs('select entries must be strings or {column,as}')
      }
      if (outNames.has(outName)) {
        throw invalidArgs(`duplicate select name ${outName}`)
      }
      outNames.add(outName)
    }
  }
  if (obj.filter !== undefined) {
    validateFilterTree(obj.filter, 'filter')
  }
  if (obj.having !== undefined) {
    validateFilterTree(obj.having, 'having')
  }
  if (Array.isArray(obj.mutate)) {
    const asNames = new Set<string>()
    for (const m of obj.mutate as Array<Record<string, unknown>>) {
      if (!m || typeof m !== 'object' || typeof m.as !== 'string' || !/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(m.as)) {
        throw invalidArgs('invalid mutate name')
      }
      if (asNames.has(m.as)) {
        throw invalidArgs(`duplicate mutate name ${m.as}`)
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
          throw invalidArgs('groupBy object needs dateColumn and period')
        }
        if (!['day', 'week', 'month'].includes(go.period)) {
          throw invalidArgs('period must be day|week|month')
        }
        continue
      }
      throw invalidArgs('groupBy entry must be string or object')
    }
  }
  if (Array.isArray(obj.aggs)) {
    const fns = new Set([
      'count', 'sum', 'mean', 'min', 'max', 'median', 'std',
      'count_distinct', 'percentile', 'first', 'last', 'count_if',
    ])
    for (const a of obj.aggs as Array<Record<string, unknown>>) {
      if (!a || typeof a !== 'object' || typeof a.fn !== 'string' || !fns.has(a.fn)) {
        throw invalidArgs('bad agg fn')
      }
      if (a.fn === 'percentile') {
        const p = a.p
        if (typeof p !== 'number' || !Number.isFinite(p) || p <= 0 || p >= 1) {
          throw invalidArgs('percentile requires p in (0,1)')
        }
        if (typeof a.column !== 'string' || !a.column) {
          throw invalidArgs('percentile requires column')
        }
      }
      if (a.fn === 'count_distinct' || a.fn === 'first' || a.fn === 'last') {
        if (typeof a.column !== 'string' || !a.column) {
          throw invalidArgs(`${a.fn} requires column`)
        }
      }
      if (a.fn === 'count_if') {
        if (a.column !== undefined && a.column !== null) {
          throw invalidArgs('count_if must not specify column')
        }
        if (a.where === undefined) {
          throw invalidArgs('count_if requires where')
        }
        validateFilterNode(a.where, 'where', 1)
      }
    }
  }
  if (obj.topKPerGroup !== undefined) {
    const t = obj.topKPerGroup
    if (!t || typeof t !== 'object' || Array.isArray(t)) {
      throw invalidArgs('topKPerGroup must be an object')
    }
    const tk = t as Record<string, unknown>
    const by = tk.by
    if (!Array.isArray(by) || by.length < 1 || by.length > 8) {
      throw invalidArgs('topKPerGroup.by must be 1..8 column names')
    }
    if ((by as unknown[]).some((c) => typeof c !== 'string' || !c.trim())) {
      throw invalidArgs('topKPerGroup.by must be 1..8 column names')
    }
    const k = tk.k
    if (typeof k !== 'number' || !Number.isInteger(k) || k < 1 || k > 1000) {
      throw invalidArgs('topKPerGroup.k must be integer 1..1000')
    }
    if (tk.orderBy !== undefined && tk.orderBy !== null) {
      const o = tk.orderBy
      if (!o || typeof o !== 'object' || Array.isArray(o) || typeof (o as Record<string, unknown>).column !== 'string') {
        throw invalidArgs('topKPerGroup.orderBy needs a column name')
      }
      const oo = o as Record<string, unknown>
      if (oo.desc !== undefined && typeof oo.desc !== 'boolean') {
        throw invalidArgs('topKPerGroup.orderBy desc must be boolean')
      }
    }
  }
  if (Array.isArray(obj.orderBy)) {
    for (const o of obj.orderBy as unknown[]) {
      const co = o as Record<string, unknown> | null
      if (!co || typeof co !== 'object' || Array.isArray(co) || typeof co.column !== 'string') {
        throw invalidArgs('orderBy entries need a column name')
      }
      if (co.desc !== undefined && typeof co.desc !== 'boolean') {
        throw invalidArgs('orderBy desc must be boolean')
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
    'Ingest a local workspace file (csv/json/jsonl) into the analysis dataset store. ' +
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
            enum: ['csv', 'json', 'jsonl', 'parquet'],
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
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
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
          return textRender(withWarnings(
            `dataset ${v.datasetId} ready (${v.rowCount} rows × ${v.columnCount} cols).\n` +
              `columns: ${cols}\n` +
              'Next: dataset_head / stats_describe / transform_agg. Do not dump full rows into the conversation.',
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { path: string; format?: string; label?: string }
        if (typeof a.path !== 'string' || a.path.length === 0 || a.path.length > MAX_PATH_LEN) {
          throw invalidArgs('path length invalid')
        }
        if (a.label !== undefined && a.label !== null) {
          if (typeof a.label !== 'string' || a.label.length > MAX_LABEL_LEN) {
            throw invalidArgs(`label must be string ≤ ${MAX_LABEL_LEN}`)
          }
        }
        const abs = resolveInWorkspace(ws, a.path)
        assertNotSensitiveSource(abs, config.profileRoot)
        // parquet is enabled (R-22); xlsx remains rejected pre-spawn.
        if (a.format === 'xlsx') {
          throw limitError('xlsx not supported (convert to csv/jsonl)')
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
        'Return up to 100 sample rows and column names for a datasetId. Use for orientation only; not for bulk analysis. ' +
        'Response always carries requestedN (as asked) and n (rows actually returned); over-limit requests are clamped by the CLI with a clamp warning.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          n: { type: 'number', description: '1–100, default 5. Values above 100 are clamped by the CLI.' },
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
            requestedN: { type: 'number' },
            n: { type: 'number' },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
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
            requestedN?: number
            n?: number
          }
          return textRender(withWarnings(
            `head of ${v.datasetId} (${v.rowCount} total rows` +
              (typeof v.requestedN === 'number' ? `, requestedN=${v.requestedN} n=${v.n ?? v.rows?.length}` : '') +
              ')\n' +
              JSON.stringify({ columns: v.columns, rows: v.rows }, null, 2),
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; n?: number }
        validateDatasetId(a.datasetId)
        const n = validatePositiveInt(a.n, 5, 'n')
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
          return textRender(withWarnings(`${v.datasetId}: ${v.rowCount} rows\n${lines.join('\n')}`, value))
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
        'List datasets in this session analysis store: datasetId, label, rowCount, columnCount, producedBy, createdAt. ' +
        'Optional filters (AND): labelLike (case-insensitive label substring), producedBy (exact), ' +
        'since (createdAt >= since, UTC; datasets without createdAt are excluded). ' +
        'Use to discover prior ingests and transform outputs before re-running work.',
      parameters: {
        type: 'object',
        properties: {
          labelLike: {
            type: 'string',
            description: 'Case-insensitive substring match on the label.',
          },
          producedBy: {
            type: 'string',
            description: 'Exact match on producedBy (e.g. ingest, agg, join).',
          },
          since: {
            type: 'string',
            description: 'Keep datasets with createdAt >= since (UTC), YYYY-MM-DD[THH:MM:SS].',
          },
        },
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
                  label: { oneOf: [{ type: 'string' }, { type: 'null' }] },
                  producedBy: { type: 'string' },
                  parentDatasetId: { oneOf: [{ type: 'string' }, { type: 'null' }] },
                  sourceKind: { type: 'string' },
                  createdAt: { oneOf: [{ type: 'string' }, { type: 'null' }] },
                },
                additionalProperties: true,
              },
            },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
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
              createdAt?: string | null
            }>
          }
          const rows = v.datasets ?? []
          if (rows.length === 0) {
            return textRender(withWarnings('no datasets in this session store. Use dataset_ingest first.', value))
          }
          const lines = rows.map((d) =>
            `- ${d.datasetId}: ${d.rowCount} rows × ${d.columnCount} cols` +
            (d.label ? ` · ${d.label}` : '') +
            ` (${d.producedBy})` +
            (d.createdAt ? ` @ ${d.createdAt}` : ''),
          )
          return textRender(withWarnings(`datasets (${rows.length}):\n${lines.join('\n')}`, value))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { labelLike?: string; producedBy?: string; since?: string }
        const cliArgs = ['list']
        for (const [flag, key] of [
          ['--label-like', 'labelLike'],
          ['--produced-by', 'producedBy'],
          ['--since', 'since'],
        ] as const) {
          const raw = a[key]
          if (raw === undefined || raw === null) continue
          if (typeof raw !== 'string' || !raw.trim()) {
            throw invalidArgs(`${key} must be a non-empty string`)
          }
          cliArgs.push(flag, raw.trim())
        }
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_export: {
      name: 'dataset_export',
      description:
        'Export a dataset to CSV / XLSX / Parquet under the session analysis root outputs/ (.dsh-analyze/outputs/). ' +
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
          format: {
            type: 'string',
            enum: ['csv', 'xlsx', 'parquet'],
            description: 'Export format; default csv (or inferred from outPath extension).',
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
            format: { type: 'string' },
          },
          required: ['outputPath', 'rowCount'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as { outputPath: string; rowCount: number; bytes?: number }
          return textRender(withWarnings(
            `exported ${v.rowCount} rows → ${v.outputPath}` +
              (typeof v.bytes === 'number' ? ` (${v.bytes} bytes)` : ''),
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; outPath?: string; format?: string }
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
        const cliArgs = ['export', '--dataset-id', a.datasetId, '--output', outPath]
        if (a.format !== undefined && a.format !== null) {
          if (typeof a.format !== 'string' || !['csv', 'xlsx', 'parquet'].includes(a.format)) {
            throw invalidArgs('format must be csv|xlsx|parquet')
          }
          cliArgs.push('--format', a.format)
        }
        const result = await run(cliArgs, exec?.signal)
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
          return textRender(withWarnings(`removed ${v.datasetId}`, value))
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
          return textRender(withWarnings(`describe ${v.datasetId}\n${JSON.stringify(v.stats, null, 2)}`, value))
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
        'spec may include window ops, mutate derived columns, time-period groupBy objects, having, and topKPerGroup. ' +
        'Pipeline order: filter → window → topKPerGroup → mutate → group/agg → having → select → orderBy → limit. ' +
        'Returns at most `limit` rows (default 1000). ' +
        'Set outLabel to persist a new datasetId for further steps.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          spec: {
            type: 'string',
            description:
              'JSON string: { select?, filter?, window?, topKPerGroup?, mutate?, groupBy?, aggs?, having?, orderBy?, limit? }. ' +
              'filter/having are predicate arrays (all-AND) or nested groups ' +
              '{"all":[…]} | {"any":[…]} | {"not":…} (depth ≤8; empty all=true, empty any=false). ' +
              'Predicate: {column, op, value?}; filter ops: eq/ne/gt/gte/lt/lte/in/contains/is_null/not_null. ' +
              'value: scalar (op in also accepts an array of scalars; every member matches). ' +
              'window: [{fn:rank|row_number|cumsum|ewm, by?, orderBy:{column,desc?}, column?, alpha?, as}] — writes new columns, row count unchanged. ' +
              'rank/row_number/cumsum need by; cumsum/ewm need column; ewm needs alpha in (0,1]; by optional for ewm (whole table). ' +
              'cumsum: null source contributes 0. ewm: y0=first non-null; null x leaves y unchanged. ' +
              'topKPerGroup: {by:[col…1..8], k:1..1000, orderBy?:{column,desc?}} — keep ≤k rows per group, after window. ' +
              'agg fns: count/sum/mean/min/max/median/std/count_distinct/percentile/first/last/count_if. ' +
              'percentile needs p in (0,1); count_if needs {where:<predicate|group>} and no column; ' +
              'count_distinct/first/last need column. ' +
              'mutate: [{as,expr}] whitelist arithmetic + to_int(e)/to_str(e)/to_date(e,fmt) + "string literals"; ' +
              'to_date fmt supports %Y %m %d %H %M %S directives only. ' +
              'groupBy entries: column name or {dateColumn,period:day|week|month}. ' +
              'having filters result rows after group/agg (same shape as filter). ' +
              'select entries: "col" or {column,as} renames. ' +
              'Unknown top-level spec keys are rejected.',
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
            outDatasetId: { oneOf: [{ type: 'string' }, { type: 'null' }] },
            rowCount: { type: 'number' },
            columns: { type: 'array', items: { type: 'string' } },
            rows: { type: 'array', items: { type: 'array' } },
            truncated: { type: 'boolean' },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
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
            return textRender(withWarnings(
              `agg → ${v.outDatasetId ?? v.datasetId}: ${v.rowCount} rows ` +
                `(full result stored; use dataset_head or further agg). columns=${(v.columns ?? []).join(',')}`,
              value,
            ))
          }
          return textRender(withWarnings(JSON.stringify(value, null, 2), value))
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
        'Use to understand categorical / low-cardinality columns without dumping rows. ' +
        'Response always carries requestedLimit and limit; over-limit requests are clamped by the CLI with a clamp warning.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          column: { type: 'string', description: 'Column name (required).' },
          limit: {
            type: 'number',
            description: '1–100, default 20. Length of top list only; values above 100 are clamped by the CLI.',
          },
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
            requestedLimit: { type: 'number' },
            limit: { type: 'number' },
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
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
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
            requestedLimit?: number
            limit?: number
            top: Array<{ value: unknown; count: number; ratio: number }>
          }
          const lines = (v.top ?? []).map(
            t => `  ${JSON.stringify(t.value)} × ${t.count} (${(t.ratio * 100).toFixed(1)}%)`,
          )
          return textRender(withWarnings(
            `values ${v.column} in ${v.datasetId}\n` +
              `nunique=${v.nunique} nullCount=${v.nullCount}` +
              (typeof v.requestedLimit === 'number'
                ? ` requestedLimit=${v.requestedLimit} limit=${v.limit ?? ''}`
                : '') +
              '\n' +
              (lines.join('\n') || '  (no non-null values)'),
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; column: string; limit?: number }
        validateDatasetId(a.datasetId)
        if (typeof a.column !== 'string' || !a.column.trim()) {
          throw invalidArgs('column required')
        }
        const limit = validatePositiveInt(a.limit, 20, 'limit')
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
        'Deterministic random (or stratified) sample of up to 200 rows. ' +
        'Same datasetId + n + seed always yields the same rows. Prefer over dataset_head when you need a spread, not the first rows. ' +
        'Response always carries requestedN (as asked) and n (rows actually returned); over-limit requests are clamped by the CLI with a clamp warning.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          n: { type: 'number', description: '1–200, default 10. Values above 200 are clamped by the CLI.' },
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
            requestedN: { type: 'number' },
            n: { type: 'number' },
            sampling: {
              type: 'object',
              properties: {
                method: { type: 'string' },
                n: { type: 'number' },
                seed: { type: 'number' },
                stratifyBy: { oneOf: [{ type: 'string' }, { type: 'null' }] },
                population: { type: 'number' },
              },
            },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
          },
          required: ['datasetId', 'columns', 'rows', 'sampling'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            columns: string[]
            rows: unknown[][]
            requestedN?: number
            n?: number
            sampling?: { method?: string; n?: number; seed?: number; population?: number }
          }
          return textRender(withWarnings(
            `sample of ${v.datasetId} (${v.rows?.length ?? 0} rows` +
              (typeof v.requestedN === 'number' ? `, requestedN=${v.requestedN} n=${v.n ?? v.rows?.length}` : '') +
              `, ${v.sampling?.method ?? 'random'} seed=${v.sampling?.seed ?? '?'} pop=${v.sampling?.population ?? '?'} )\n` +
              JSON.stringify({ columns: v.columns, rows: v.rows }, null, 2),
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as { datasetId: string; n?: number; seed?: number; stratifyBy?: string }
        validateDatasetId(a.datasetId)
        const n = validatePositiveInt(a.n, 10, 'n')
        const cliArgs = ['sample', '--dataset-id', a.datasetId, '--n', String(n)]
        if (a.seed !== undefined && a.seed !== null) {
          if (!Number.isFinite(a.seed) || a.seed < 0 || !Number.isInteger(a.seed)) {
            throw invalidArgs('seed must be a non-negative integer')
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
        'Equi-join two datasets on key column(s). Entries of `on` are a shared column name ' +
        'or a `leftKey:rightKey` pair for differently named keys. Always writes a new datasetId ' +
        '(producedBy=join). Returns only the new id + row/column counts — never full rows. ' +
        'how: inner (default), left, anti, semi. anti/semi keep left rows only. Null keys never match.',
      parameters: {
        type: 'object',
        properties: {
          leftDatasetId: { type: 'string' },
          rightDatasetId: { type: 'string' },
          on: {
            type: 'array',
            items: { type: 'string' },
            description:
              'Non-empty list of key entries: a shared column name, or "leftKey:rightKey" to join differently named columns. Max 8 entries.',
          },
          how: {
            type: 'string',
            enum: ['inner', 'left', 'anti', 'semi'],
            description:
              'Default inner. anti/semi keep left rows only (no right payload columns); null keys never match.',
          },
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
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
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
          return textRender(withWarnings(
            `join → ${v.datasetId}: ${v.rowCount} rows × ${v.columnCount} cols (how=${v.how ?? 'inner'}). ` +
              'Inspect with dataset_head / stats_describe / transform_agg.',
            value,
          ))
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
          throw invalidArgs(`on must be 1..${limits.maxJoinOnColumns} column names`)
        }
        for (const c of a.on) {
          if (typeof c !== 'string' || !c.trim()) {
            throw invalidArgs('on entries must be non-empty strings')
          }
          const parts = c.split(':')
          if (parts.length > 2 || (parts.length === 2 && (!parts[0] || !parts[1]))) {
            throw invalidArgs('on pairs must be leftKey:rightKey')
          }
        }
        const how = a.how ?? 'inner'
        if (how !== 'inner' && how !== 'left' && how !== 'anti' && how !== 'semi') {
          throw invalidArgs('how must be inner|left|anti|semi')
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
            suspectKeys?: Array<{ columns: string[] }>
          }
          const cols = (v.columns ?? [])
            .map(
              c =>
                `- ${c.name}: ${c.typeStability ?? '?'}` +
                (typeof c.nullRate === 'number' ? ` null=${(c.nullRate * 100).toFixed(2)}%` : ''),
            )
            .join('\n')
          const keys = (v.suspectKeys ?? []).map(k => k.columns.join('+')).join(', ')
          return textRender(withWarnings(
            `profile ${v.datasetId}: ${v.rowCount} rows\n${cols}` +
              (keys ? `\nsuspect keys: ${keys}` : ''),
            value,
          ))
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
            keyColumns: {
              oneOf: [
                { type: 'array', items: { type: 'string' } },
                { type: 'null' },
              ],
            },
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
          return textRender(withWarnings(
            `dedup ${v.datasetId} mode=${v.mode}: ${v.duplicateRowCount}/${v.rowCount} duplicate rows${pct}` +
              (v.samplesTruncated ? ' [samples truncated]' : ''),
            value,
          ))
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
        const sampleLimit = validateSampleLimit(a.sampleLimit, 5)
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
            outDatasetId: { oneOf: [{ type: 'string' }, { type: 'null' }] },
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
          return textRender(withWarnings(
            `outliers ${v.datasetId} method=${v.method} k=${v.k ?? '?'}\n${lines}` +
              (v.outDatasetId ? `\n→ ${v.outDatasetId}` : ''),
            value,
          ))
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
        const sampleLimit = validateSampleLimit(a.sampleLimit, 10)
        cliArgs.push('--sample-limit', String(sampleLimit))
        if (a.outLabel) cliArgs.push('--out', a.outLabel)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_corr: {
      name: 'dataset_corr',
      description:
        'Pairwise correlations among numeric-capable columns (Pearson or Spearman). ' +
        'Returns top pairs sorted by |r| with sample size n and two-sided pValue (Student-t approx) — never a full matrix dump beyond the pair cap. ' +
        'Use to find co-moving numeric signals before deeper agg; treat n<30 or large p as weak evidence.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          columns: {
            type: 'array',
            items: { type: 'string' },
            description: '2..16 numeric-capable column names.',
          },
          method: {
            type: 'string',
            enum: ['pearson', 'spearman'],
            description: 'Default pearson.',
          },
          pairwise: {
            type: 'string',
            enum: ['complete', 'omit'],
            description: 'complete = per-pair complete rows; omit = listwise (all columns non-null).',
          },
          minNumeric: {
            type: 'number',
            description: 'Pairs with n below this report r=null (default 2).',
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
            method: { type: 'string' },
            pairwise: { type: 'string' },
            columns: { type: 'array', items: { type: 'string' } },
            pairs: { type: 'array', items: { type: 'object', additionalProperties: true } },
            truncated: { type: 'boolean' },
          },
          required: ['datasetId', 'method', 'pairs'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            method?: string
            pairwise?: string
            pairs?: Array<{ columns: string[]; n: number; r: number | null; pValue?: number | null; degenerate?: boolean }>
            truncated?: boolean
          }
          const lines = (v.pairs ?? []).map((p) => {
            const pval = (p as { pValue?: number | null }).pValue
            const pStr = pval == null ? 'p=null' : `p=${pval}`
            return `- ${p.columns.join('×')}: r=${p.r == null ? 'null' : p.r} n=${p.n} ${pStr}` +
              (p.degenerate ? ' (degenerate)' : '')
          })
          return textRender(withWarnings(
            `corr ${v.datasetId} method=${v.method ?? 'pearson'} pairwise=${v.pairwise ?? 'complete'}` +
              (v.truncated ? ' [pairs truncated]' : '') +
              '\n' +
              (lines.join('\n') || '  (no pairs)'),
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          datasetId: string
          columns: string[]
          method?: string
          pairwise?: string
          minNumeric?: number
        }
        validateDatasetId(a.datasetId)
        if (!Array.isArray(a.columns) || a.columns.length < 2 || a.columns.length > limits.maxCorrColumns) {
          const err = new Error(
            `analysis/invalid-args: columns must be 2..${limits.maxCorrColumns} names`,
          ) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (a.columns.some(c => typeof c !== 'string' || !c.trim())) {
          const err = new Error('analysis/invalid-args: columns entries must be non-empty strings') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (new Set(a.columns).size !== a.columns.length) {
          const dup = a.columns.find((c, i) => a.columns.indexOf(c) !== i)
          const err = new Error(`analysis/invalid-args: duplicate column ${dup}`) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const method = a.method ?? 'pearson'
        if (method !== 'pearson' && method !== 'spearman') {
          const err = new Error('analysis/invalid-args: method must be pearson|spearman') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const pairwise = a.pairwise ?? 'complete'
        if (pairwise !== 'complete' && pairwise !== 'omit') {
          const err = new Error('analysis/invalid-args: pairwise must be complete|omit') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const cliArgs = [
          'corr',
          '--dataset-id',
          a.datasetId,
          '--columns',
          a.columns.join(','),
          '--method',
          method,
          '--pairwise',
          pairwise,
        ]
        if (a.minNumeric !== undefined && a.minNumeric !== null) {
          const n = Number(a.minNumeric)
          if (!Number.isInteger(n) || n < 2) {
            const err = new Error('analysis/invalid-args: minNumeric must be integer >= 2') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          cliArgs.push('--min-numeric', String(n))
        }
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_crosstab: {
      name: 'dataset_crosstab',
      description:
        'Two- or three-column contingency table (frequency cross-tab). Returns top categories per dimension, ' +
        'a compact matrix, margins, and top cells — never a full cartesian dump. ' +
        'Optional `third` splits into multiple 2-D tables in one call (region × channel × month). ' +
        'Use to compare two categorical / low-cardinality columns.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          rows: { type: 'string', description: 'Row category column name.' },
          cols: { type: 'string', description: 'Column category column name (must differ from rows).' },
          third: {
            type: 'string',
            description: 'Optional third dimension column (must differ from rows and cols). Response uses tables[].',
          },
          top: {
            type: 'number',
            description: '1–50 categories per dimension (default 20).',
          },
          minCount: {
            type: 'number',
            description: 'Cells with count below this are omitted from topCells (default 1).',
          },
          dropNull: {
            type: 'boolean',
            description: 'Default true: exclude rows with null on any dimension (counted in nullRowCount / nullThirdCount).',
          },
        },
        required: ['datasetId', 'rows', 'cols'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowColumn: { type: 'string' },
            colColumn: { type: 'string' },
            thirdColumn: { type: 'string' },
            rowCategories: { type: 'array' },
            colCategories: { type: 'array' },
            thirdCategories: { type: 'array' },
            matrix: { type: 'array', items: { type: 'array', items: { type: 'number' } } },
            tables: { type: 'array', items: { type: 'object', additionalProperties: true } },
            topCells: { type: 'array', items: { type: 'object', additionalProperties: true } },
            truncated: { type: 'boolean' },
            nullRowCount: { type: 'number' },
          },
          required: ['datasetId', 'rowColumn', 'colColumn'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowColumn: string
            colColumn: string
            thirdColumn?: string
            rowCategories?: unknown[]
            colCategories?: unknown[]
            matrix?: number[][]
            tables?: Array<{ third: unknown; rowCategories?: unknown[]; colCategories?: unknown[]; matrix?: number[][] }>
            grandTotal?: number
            truncated?: boolean
            nullRowCount?: number
          }
          if (Array.isArray(v.tables) && v.tables.length > 0) {
            const parts = v.tables.map((t) => {
              const rows = (t.rowCategories ?? []).map(String)
              const cols = (t.colCategories ?? []).map(String)
              const header = ['', ...cols].join('\t')
              const body = rows
                .map((r, i) => [r, ...(t.matrix?.[i] ?? [])].join('\t'))
                .join('\n')
              return `third=${String(t.third)}\n${header}\n${body}`
            })
            return textRender(withWarnings(
              `crosstab ${v.datasetId}: ${v.rowColumn} × ${v.colColumn} × ${v.thirdColumn ?? 'third'} ` +
                `(${v.tables.length} tables` + (v.truncated ? ', truncated' : '') + ')\n' +
                parts.join('\n\n'),
              value,
            ))
          }
          const rows = (v.rowCategories ?? []).map(String)
          const cols = (v.colCategories ?? []).map(String)
          const header = ['', ...cols].join('\t')
          const body = rows
            .map((r, i) => [r, ...(v.matrix?.[i] ?? [])].join('\t'))
            .join('\n')
          return textRender(withWarnings(
            `crosstab ${v.datasetId}: ${v.rowColumn} × ${v.colColumn} ` +
              `(grandTotal=${v.grandTotal ?? '?'} nulls=${v.nullRowCount ?? 0}` +
              (v.truncated ? ', truncated' : '') +
              ')\n' +
              header +
              '\n' +
              body,
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          datasetId: string
          rows: string
          cols: string
          third?: string
          top?: number
          minCount?: number
          dropNull?: boolean
        }
        validateDatasetId(a.datasetId)
        if (typeof a.rows !== 'string' || !a.rows.trim()) {
          const err = new Error('analysis/invalid-args: rows required') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (typeof a.cols !== 'string' || !a.cols.trim()) {
          const err = new Error('analysis/invalid-args: cols required') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (a.rows.trim() === a.cols.trim()) {
          const err = new Error('analysis/invalid-args: rows and cols must be different columns') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (a.third !== undefined && a.third !== null) {
          if (typeof a.third !== 'string' || !a.third.trim()) {
            const err = new Error('analysis/invalid-args: third must be a non-empty string') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          if (a.third.trim() === a.rows.trim() || a.third.trim() === a.cols.trim()) {
            const err = new Error('analysis/invalid-args: third must differ from rows and cols') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
        }
        const top = a.top === undefined || a.top === null ? 20 : Number(a.top)
        if (!Number.isInteger(top) || top < 1 || top > limits.maxCrosstabTop) {
          const err = new Error(
            `analysis/invalid-args: top must be integer 1..${limits.maxCrosstabTop}`,
          ) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const minCount = a.minCount === undefined || a.minCount === null ? 1 : Number(a.minCount)
        if (!Number.isInteger(minCount) || minCount < 1) {
          const err = new Error('analysis/invalid-args: minCount must be integer >= 1') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const dropNull = a.dropNull !== false
        const cliArgs = [
          'crosstab',
          '--dataset-id',
          a.datasetId,
          '--rows',
          a.rows.trim(),
          '--cols',
          a.cols.trim(),
        ]
        if (a.third !== undefined && a.third !== null) {
          cliArgs.push('--third', a.third.trim())
        }
        cliArgs.push(
          '--top',
          String(top),
          '--min-count',
          String(minCount),
          '--drop-null',
          dropNull ? 'true' : 'false',
        )
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_series: {
      name: 'dataset_series',
      description:
        'Time-series transforms on one order-by column. Always writes a new datasetId ' +
        '(producedBy=series). Ops in one call: lag | lead | diff | pct_change | rolling ' +
        '(mean|sum|min|max). Returns only the new id + schema — never full rows. ' +
        'Null order-by values sort last and stay in the output.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          orderBy: {
            type: 'string',
            description: 'Existing column used as sequence order (ascending; nulls last).',
          },
          ops: {
            type: 'array',
            minItems: 1,
            maxItems: 8,
            items: {
              type: 'object',
              properties: {
                fn: {
                  type: 'string',
                  enum: ['lag', 'lead', 'diff', 'pct_change', 'rolling'],
                },
                column: { type: 'string' },
                as: {
                  type: 'string',
                  description: 'New output column name [A-Za-z_][A-Za-z0-9_]* (<=64).',
                },
                periods: {
                  type: 'integer',
                  minimum: 1,
                  maximum: 10000,
                  description: 'lag/lead/diff/pct_change only; default 1.',
                },
                window: {
                  type: 'integer',
                  minimum: 1,
                  maximum: 10000,
                  description: 'rolling only; trailing window size.',
                },
                agg: {
                  type: 'string',
                  enum: ['mean', 'sum', 'min', 'max'],
                  description: 'rolling only.',
                },
              },
              required: ['fn', 'column', 'as'],
              additionalProperties: false,
            },
          },
          outLabel: { type: 'string', description: 'Optional label stored on the new dataset.' },
        },
        required: ['datasetId', 'orderBy', 'ops'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            columnCount: { type: 'number' },
            sourceDatasetId: { type: 'string' },
            orderBy: { type: 'string' },
            opCount: { type: 'number' },
          },
          required: ['datasetId', 'rowCount', 'columnCount', 'sourceDatasetId', 'orderBy', 'opCount'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columnCount: number
            orderBy?: string
            opCount?: number
          }
          return textRender(withWarnings(
            `series → ${v.datasetId}: ${v.rowCount} rows × ${v.columnCount} cols ` +
              `(orderBy=${v.orderBy ?? '?'}, ops=${v.opCount ?? '?'}). ` +
              'Inspect with dataset_head / stats_describe / transform_agg.',
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          datasetId: string
          orderBy: string
          ops: Array<{
            fn: string
            column: string
            as: string
            periods?: number | string | null
            window?: number | string | null
            agg?: string
          }>
          outLabel?: string
        }
        const requireInt = (value: number | string | null | undefined, name: string, max: number): number => {
          if (value === undefined || value === null) {
            const err = new Error(`analysis/invalid-args: ${name} required`) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          if (typeof value !== 'number' && !(typeof value === 'string' && /^\d+$/.test(value.trim()))) {
            const err = new Error(`analysis/invalid-args: ${name} must be a non-negative integer`) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          const n = Number(value)
          if (!Number.isInteger(n) || n < 1 || n > max) {
            const err = new Error(
              `analysis/invalid-args: ${name} must be integer 1..${max}`,
            ) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          return n
        }
        validateDatasetId(a.datasetId)
        if (typeof a.orderBy !== 'string' || !a.orderBy.trim()) {
          const err = new Error('analysis/invalid-args: orderBy required') as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        if (!Array.isArray(a.ops) || a.ops.length === 0 || a.ops.length > limits.maxSeriesOps) {
          const err = new Error(
            `analysis/invalid-args: ops must be 1..${limits.maxSeriesOps} transforms`,
          ) as Error & { code?: string }
          err.code = 'analysis/invalid-args'
          throw err
        }
        const seen = new Set<string>()
        const fns = new Set(['lag', 'lead', 'diff', 'pct_change', 'rolling'])
        const aggs = new Set(['mean', 'sum', 'min', 'max'])
        const asRe = /^[A-Za-z_][A-Za-z0-9_]*$/
        const normalized = a.ops.map(op => {
          if (!op || typeof op !== 'object') {
            const err = new Error('analysis/invalid-args: each op must be an object') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          const fn = op.fn
          if (typeof fn !== 'string' || !fns.has(fn)) {
            const err = new Error(
              'analysis/invalid-args: fn must be lag|lead|diff|pct_change|rolling',
            ) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          if (typeof op.column !== 'string' || !op.column.trim()) {
            const err = new Error('analysis/invalid-args: column required') as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          const as = op.as
          if (typeof as !== 'string' || !asRe.test(as) || as.length > 64) {
            const err = new Error(
              'analysis/invalid-args: as must match [A-Za-z_][A-Za-z0-9_]* (<=64)',
            ) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          if (seen.has(as)) {
            const err = new Error(`analysis/invalid-args: duplicate as column ${as}`) as Error & { code?: string }
            err.code = 'analysis/invalid-args'
            throw err
          }
          seen.add(as)
          const out: {
            fn: string
            column: string
            as: string
            periods?: number
            window?: number
            agg?: string
          } = { fn, column: op.column.trim(), as }
          if (fn === 'rolling') {
            if (op.periods !== undefined && op.periods !== null) {
              const err = new Error('analysis/invalid-args: rolling takes window, not periods') as Error & { code?: string }
              err.code = 'analysis/invalid-args'
              throw err
            }
            out.window = requireInt(op.window, 'window', limits.maxSeriesWindow)
            if (typeof op.agg !== 'string' || !aggs.has(op.agg)) {
              const err = new Error('analysis/invalid-args: rolling agg must be mean|sum|min|max') as Error & { code?: string }
              err.code = 'analysis/invalid-args'
              throw err
            }
            out.agg = op.agg
          } else {
            if (op.window !== undefined && op.window !== null) {
              const err = new Error(`analysis/invalid-args: fn ${fn} does not take window`) as Error & { code?: string }
              err.code = 'analysis/invalid-args'
              throw err
            }
            if (op.agg !== undefined && op.agg !== null) {
              const err = new Error(`analysis/invalid-args: fn ${fn} does not take agg`) as Error & { code?: string }
              err.code = 'analysis/invalid-args'
              throw err
            }
            out.periods =
              op.periods === undefined || op.periods === null
                ? 1
                : requireInt(op.periods, 'periods', limits.maxSeriesPeriods)
          }
          return out
        })
        const cliArgs = [
          'series',
          '--dataset-id',
          a.datasetId,
          '--order-by',
          a.orderBy.trim(),
          '--ops',
          JSON.stringify(normalized),
        ]
        if (a.outLabel) cliArgs.push('--out', a.outLabel)
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_union: {
      name: 'dataset_union',
      description:
        'Row-wise union (append) of two datasets into a new datasetId (producedBy=union). ' +
        'columnsPolicy strict (default) requires identical column-name sets; intersect keeps only shared columns. ' +
        'Output column order follows the left dataset. Returns the new id + row counts — never full rows.',
      parameters: {
        type: 'object',
        properties: {
          leftDatasetId: { type: 'string' },
          rightDatasetId: { type: 'string' },
          columnsPolicy: {
            type: 'string',
            enum: ['strict', 'intersect'],
            description: 'Default strict: both sides must have the same column-name set. intersect keeps shared columns only.',
          },
          label: { type: 'string', description: 'Optional label stored on the new dataset.' },
        },
        required: ['leftDatasetId', 'rightDatasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            rowCount: { type: 'number' },
            leftRows: { type: 'number' },
            rightRows: { type: 'number' },
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
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
          },
          required: ['datasetId', 'rowCount', 'leftRows', 'rightRows', 'columns'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            leftRows: number
            rightRows: number
            columns?: Array<{ name: string }>
          }
          return textRender(withWarnings(
            `union → ${v.datasetId}: ${v.rowCount} rows × ${(v.columns ?? []).length} cols ` +
              `(left=${v.leftRows}, right=${v.rightRows}). ` +
              'Inspect with dataset_head / stats_describe / transform_agg.',
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          leftDatasetId: string
          rightDatasetId: string
          columnsPolicy?: string
          label?: string
        }
        validateDatasetId(a.leftDatasetId)
        validateDatasetId(a.rightDatasetId)
        const cliArgs = ['union', '--left', a.leftDatasetId, '--right', a.rightDatasetId]
        if (a.columnsPolicy !== undefined && a.columnsPolicy !== null) {
          if (a.columnsPolicy !== 'strict' && a.columnsPolicy !== 'intersect') {
            throw invalidArgs('columnsPolicy must be strict|intersect')
          }
          cliArgs.push('--columns-policy', a.columnsPolicy)
        }
        if (a.label !== undefined && a.label !== null) {
          if (typeof a.label !== 'string' || a.label.length > MAX_LABEL_LEN) {
            throw invalidArgs(`label must be string ≤ ${MAX_LABEL_LEN}`)
          }
          cliArgs.push('--out', a.label)
        }
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_diff: {
      name: 'dataset_diff',
      description:
        'Compare two datasets: rows only in left, only in right, or changed under matching keys. ' +
        'on entries are shared column names or "leftKey:rightKey" pairs (max 8); omit on to compare ' +
        'rows as a multiset over shared columns (changed is then 0). Null keys never match. ' +
        'Returns counts + small samples — never full row sets.',
      parameters: {
        type: 'object',
        properties: {
          leftDatasetId: { type: 'string' },
          rightDatasetId: { type: 'string' },
          on: {
            type: 'array',
            items: { type: 'string' },
            description:
              'Optional key entries: shared column name or "leftKey:rightKey" (max 8). Omit for multiset comparison over shared columns.',
          },
          sampleLimit: {
            type: 'number',
            description: '1–20 sample rows per category (default 5). Values above 20 are clamped by the CLI.',
          },
        },
        required: ['leftDatasetId', 'rightDatasetId'],
        additionalProperties: false,
      },
      output: {
        schema: {
          type: 'object',
          properties: {
            datasetId: { type: 'string' },
            onlyLeft: { type: 'number' },
            onlyRight: { type: 'number' },
            changed: { type: 'number' },
            samples: {
              type: 'object',
              properties: {
                onlyLeft: { type: 'array', items: { type: 'object', additionalProperties: true } },
                onlyRight: { type: 'array', items: { type: 'object', additionalProperties: true } },
                changed: { type: 'array', items: { type: 'object', additionalProperties: true } },
              },
            },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
          },
          required: ['datasetId', 'onlyLeft', 'onlyRight', 'changed', 'samples'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            onlyLeft: number
            onlyRight: number
            changed: number
            samples?: { onlyLeft?: unknown[]; onlyRight?: unknown[]; changed?: unknown[] }
          }
          return textRender(withWarnings(
            `diff → ${v.datasetId}: onlyLeft=${v.onlyLeft} onlyRight=${v.onlyRight} ` +
              `changed=${v.changed}\n` +
              `samples: onlyLeft=${v.samples?.onlyLeft?.length ?? 0} ` +
              `onlyRight=${v.samples?.onlyRight?.length ?? 0} ` +
              `changed=${v.samples?.changed?.length ?? 0}`,
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          leftDatasetId: string
          rightDatasetId: string
          on?: string[]
          sampleLimit?: number
        }
        validateDatasetId(a.leftDatasetId)
        validateDatasetId(a.rightDatasetId)
        const cliArgs = ['diff', '--left', a.leftDatasetId, '--right', a.rightDatasetId]
        if (a.on !== undefined && a.on !== null) {
          if (!Array.isArray(a.on) || a.on.length === 0 || a.on.length > limits.maxJoinOnColumns) {
            throw invalidArgs(`on must be 1..${limits.maxJoinOnColumns} key entries`)
          }
          for (const c of a.on) {
            if (typeof c !== 'string' || !c.trim()) {
              throw invalidArgs('on entries must be non-empty strings')
            }
            const parts = c.split(':')
            if (parts.length > 2 || (parts.length === 2 && (!parts[0] || !parts[1]))) {
              throw invalidArgs('on pairs must be leftKey:rightKey')
            }
          }
          cliArgs.push('--on', a.on.join(','))
        }
        const sampleLimit = validateSampleLimit(a.sampleLimit, 5)
        cliArgs.push('--sample-limit', String(sampleLimit))
        const result = await run(cliArgs, exec?.signal)
        return result.data
      },
    },

    dataset_pivot: {
      name: 'dataset_pivot',
      description:
        'Pivot a dataset to a wide table: rows = index column combinations (input order), ' +
        'columns = distinct values of the `columns` field (first-seen order, ≤50), ' +
        'cells = the `values` column aggregated with `agg` over each (index, pivot-value) group ' +
        '(missing cells are null). Writes a new datasetId (producedBy=pivot) and returns only the summary.',
      parameters: {
        type: 'object',
        properties: {
          datasetId: { type: 'string' },
          index: {
            type: 'array',
            items: { type: 'string' },
            description: '1..16 index column names (row identity of the wide table).',
          },
          columns: { type: 'string', description: 'Column whose distinct values become output columns.' },
          values: { type: 'string', description: 'Column to aggregate into the cells.' },
          agg: {
            type: 'string',
            enum: ['sum', 'mean', 'min', 'max', 'count'],
            description: 'Cell aggregation function.',
          },
          label: { type: 'string', description: 'Optional label stored on the new dataset.' },
        },
        required: ['datasetId', 'index', 'columns', 'values', 'agg'],
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
            indexColumns: { type: 'array', items: { type: 'string' } },
            pivotColumn: { type: 'string' },
            warnings: { type: 'array', items: { type: 'object', additionalProperties: true } },
          },
          required: ['datasetId', 'rowCount', 'columns', 'indexColumns', 'pivotColumn'],
          additionalProperties: true,
        },
        render(_args: unknown, value: unknown) {
          const v = value as {
            datasetId: string
            rowCount: number
            columns?: Array<{ name: string }>
            indexColumns?: string[]
            pivotColumn?: string
          }
          return textRender(withWarnings(
            `pivot → ${v.datasetId}: ${v.rowCount} rows × ${(v.columns ?? []).length} cols ` +
              `(index=${(v.indexColumns ?? []).join('+')}, pivot=${v.pivotColumn ?? '?'}). ` +
              'Inspect with dataset_head / stats_describe / transform_agg.',
            value,
          ))
        },
      },
      async execute(args: unknown, exec?: { signal?: AbortSignal }) {
        const a = args as {
          datasetId: string
          index: string[]
          columns: string
          values: string
          agg: string
          label?: string
        }
        validateDatasetId(a.datasetId)
        if (!Array.isArray(a.index) || a.index.length === 0 || a.index.length > limits.maxKeyColumns) {
          throw invalidArgs(`index must be 1..${limits.maxKeyColumns} column names`)
        }
        if (a.index.some((c) => typeof c !== 'string' || !c.trim())) {
          throw invalidArgs('index entries must be non-empty strings')
        }
        for (const key of ['columns', 'values', 'agg'] as const) {
          if (typeof a[key] !== 'string' || !a[key].trim()) {
            throw invalidArgs(`${key} required`)
          }
        }
        if (a.agg !== 'sum' && a.agg !== 'mean' && a.agg !== 'min' && a.agg !== 'max' && a.agg !== 'count') {
          throw invalidArgs('agg must be sum|mean|min|max|count')
        }
        const cliArgs = [
          'pivot',
          '--dataset-id', a.datasetId,
          '--index', a.index.map((c) => c.trim()).join(','),
          '--columns', a.columns.trim(),
          '--values', a.values.trim(),
          '--agg', a.agg,
        ]
        if (a.label !== undefined && a.label !== null) {
          if (typeof a.label !== 'string' || a.label.length > MAX_LABEL_LEN) {
            throw invalidArgs(`label must be string ≤ ${MAX_LABEL_LEN}`)
          }
          cliArgs.push('--out', a.label)
        }
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
