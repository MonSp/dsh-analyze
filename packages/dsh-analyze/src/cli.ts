import { spawn, spawnSync } from 'node:child_process'
import { AnalysisPathError } from './paths.js'
import type { AnalysisLimits } from './limits.js'

export interface AnalyzeCliResult {
  ok: true
  data: Record<string, unknown>
}

export class AnalyzeCliError extends Error {
  readonly code: string
  readonly stderr: string
  constructor(code: string, message: string, stderr = '') {
    super(message)
    this.name = 'AnalyzeCliError'
    this.code = code
    this.stderr = stderr
  }
}

export interface RunCliOptions {
  workspaceRoot: string
  cliPath: string
  /** Extra argv inserted before analyze args (e.g. mock script path when cliPath is node). */
  cliPrefixArgs?: readonly string[]
  args: string[]
  limits: AnalysisLimits
  signal?: AbortSignal
}

/** Kill process tree on Windows; plain kill elsewhere. */
function killChildTree(child: { pid?: number; kill: () => boolean }): void {
  if (process.platform === 'win32' && typeof child.pid === 'number' && child.pid > 0) {
    try {
      const killer = spawn('taskkill', ['/pid', String(child.pid), '/T', '/F'], {
        stdio: 'ignore',
        windowsHide: true,
      })
      const maybeUnref = killer as unknown as { unref?: () => void }
      if (typeof maybeUnref.unref === 'function') maybeUnref.unref()
    } catch { /* fall through to kill */ }
  }
  try { child.kill() } catch { /* already dead */ }
}

function mapCliFailure(exitCode: number | null, stderr: string): AnalyzeCliError {
  const line = stderr.split(/\r?\n/).map(s => s.trim()).filter(Boolean).pop() ?? 'analysis failed'
  if (line.startsWith('analysis/')) {
    const code = line.split(':')[0] ?? 'analysis/cli-failed'
    return new AnalyzeCliError(code, line, stderr)
  }
  if (exitCode === 5) {
    return new AnalyzeCliError('analysis/unknown-dataset', line, stderr)
  }
  if (exitCode === 3) {
    return new AnalyzeCliError('analysis/limit', line, stderr)
  }
  if (exitCode === 2) {
    return new AnalyzeCliError('analysis/invalid-args', line, stderr)
  }
  return new AnalyzeCliError('analysis/cli-failed', line, stderr)
}

export async function runAnalyzeCli(options: RunCliOptions): Promise<AnalyzeCliResult> {
  const { workspaceRoot, cliPath, cliPrefixArgs, args, limits, signal } = options
  const argv = [...(cliPrefixArgs ?? []), '--workspace', workspaceRoot, ...args]

  return await new Promise<AnalyzeCliResult>((resolvePromise, rejectPromise) => {
    const child = spawn(cliPath, argv, {
      cwd: workspaceRoot,
      stdio: ['ignore', 'pipe', 'pipe'],
      signal,
      windowsHide: true,
      env: {
        PATH: process.env.PATH,
        DSH_ANALYZE_MAX_ROWS: String(limits.maxRows),
        DSH_ANALYZE_MAX_INGEST_BYTES: String(limits.maxIngestBytes),
        DSH_ANALYZE_DEFAULT_ROW_LIMIT: String(limits.defaultRowLimit),
        ...(process.env.NODE_ENV ? { NODE_ENV: process.env.NODE_ENV } : {}),
      },
    })

    let stdout = ''
    let stderr = ''
    let settled = false
    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      killChildTree(child)
      rejectPromise(new AnalyzeCliError('analysis/limit', `analysis timed out after ${limits.timeoutMs}ms`))
    }, limits.timeoutMs)
    const timerLike = timer as unknown as { unref?: () => void }
    if (typeof timerLike.unref === 'function') {
      timerLike.unref()
    }

    child.stdout.setEncoding('utf8')
    child.stderr.setEncoding('utf8')
    child.stdout.on('data', (chunk: string) => {
      stdout += chunk
      if (stdout.length > limits.maxOutputBytes * 4) {
        killChildTree(child)
      }
    })
    child.stderr.on('data', (chunk: string) => {
      stderr += chunk
      if (stderr.length > 64_000) stderr = stderr.slice(-64_000)
    })

    child.on('error', (err) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      rejectPromise(new AnalyzeCliError('analysis/cli-missing', err.message))
    })

    child.on('close', (code) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      if (code !== 0) {
        rejectPromise(mapCliFailure(code, stderr))
        return
      }
      try {
        const trimmed = stdout.trim()
        const data = JSON.parse(trimmed) as Record<string, unknown>
        if (data.ok !== true) {
          rejectPromise(new AnalyzeCliError('analysis/cli-failed', trimmed.slice(0, 500), stderr))
          return
        }
        resolvePromise({ ok: true, data })
      } catch {
        rejectPromise(new AnalyzeCliError('analysis/cli-failed', 'invalid CLI JSON', stderr + '\n' + stdout.slice(0, 200)))
      }
    })
  })
}

/** Probe CLI version/capabilities (T0 §4.2). Does not require a real workspace file. */
export async function probeAnalyzeCli(options: {
  workspaceRoot: string
  cliPath: string
  cliPrefixArgs?: readonly string[]
  limits: AnalysisLimits
}): Promise<{ version: string; capabilities: string[] }> {
  const result = await runAnalyzeCli({
    workspaceRoot: options.workspaceRoot,
    cliPath: options.cliPath,
    cliPrefixArgs: options.cliPrefixArgs,
    args: ['version'],
    limits: options.limits,
  })
  const data = result.data as { version?: unknown; capabilities?: unknown }
  const version = typeof data.version === 'string' ? data.version : ''
  const capabilities = Array.isArray(data.capabilities)
    ? data.capabilities.filter((c): c is string => typeof c === 'string')
    : []
  if (!version || capabilities.length === 0) {
    throw new AnalyzeCliError('analysis/cli-missing', 'CLI version probe returned incomplete capabilities')
  }
  return { version, capabilities }
}

/** Sync probe for register-time gates (apply path is sync). */
export type AnalyzeCliProbeResult =
  | { ok: true; version: string; capabilities: string[] }
  | { ok: false; error: string }

/** Process-local probe cache: success is sticky; failure retries after TTL. */
const probeCache = new Map<string, { result: AnalyzeCliProbeResult; at: number }>()
const PROBE_FAIL_TTL_MS = 30_000

/** Clear probe cache (tests / after DSH_ANALYZE_CLI_PATH change). */
export function clearAnalyzeCliProbeCache(): void {
  probeCache.clear()
}

/**
 * Resolve analysis CLI executable: `DSH_ANALYZE_CLI_PATH` → legacy
 * `INV_ANALYSIS_CLI_PATH` (compat window) → bare `dsh-analyze` on PATH.
 */
export function resolveAnalysisCliPath(env: Record<string, string | undefined> = process.env): string {
  const preferred = env.DSH_ANALYZE_CLI_PATH
  if (preferred && preferred.trim()) return preferred.trim()
  const legacy = env.INV_ANALYSIS_CLI_PATH
  if (legacy && legacy.trim()) return legacy.trim()
  return 'dsh-analyze'
}

function probeCacheKey(workspaceRoot: string, cliPath: string, prefix?: readonly string[]): string {
  return JSON.stringify([cliPath, workspaceRoot, ...(prefix ?? [])])
}

export function probeAnalyzeCliSync(options: {
  workspaceRoot: string
  cliPath: string
  cliPrefixArgs?: readonly string[]
  timeoutMs?: number
  /** Bypass sticky cache (tests / forced re-probe). */
  force?: boolean
}): AnalyzeCliProbeResult {
  const key = probeCacheKey(options.workspaceRoot, options.cliPath, options.cliPrefixArgs)
  if (!options.force) {
    const hit = probeCache.get(key)
    if (hit) {
      if (hit.result.ok) return hit.result
      if (Date.now() - hit.at < PROBE_FAIL_TTL_MS) return hit.result
    }
  }
  const result = probeAnalyzeCliUncached(options)
  probeCache.set(key, { result, at: Date.now() })
  return result
}

function probeAnalyzeCliUncached(options: {
  workspaceRoot: string
  cliPath: string
  cliPrefixArgs?: readonly string[]
  timeoutMs?: number
}): AnalyzeCliProbeResult {
  const argv = [...(options.cliPrefixArgs ?? []), '--workspace', options.workspaceRoot, 'version']
  try {
    const r = spawnSync(options.cliPath, argv, {
      encoding: 'utf8',
      timeout: options.timeoutMs ?? 10_000,
      windowsHide: true,
      env: {
        PATH: process.env.PATH,
        ...(process.env.NODE_ENV ? { NODE_ENV: process.env.NODE_ENV } : {}),
      },
    })
    if (r.error) {
      return { ok: false, error: r.error.message }
    }
    const stdout = r.stdout ?? ''
    const stderr = r.stderr ?? ''
    if (r.status !== 0) {
      return { ok: false, error: stderr.slice(0, 200) || `exit ${String(r.status)}` }
    }
    const data = JSON.parse(stdout.trim()) as { ok?: boolean; version?: unknown; capabilities?: unknown }
    if (data.ok !== true || typeof data.version !== 'string' || !Array.isArray(data.capabilities)) {
      return { ok: false, error: 'invalid CLI version JSON' }
    }
    const capabilities = data.capabilities.filter((c): c is string => typeof c === 'string')
    if (!data.version || capabilities.length === 0) {
      return { ok: false, error: 'CLI version probe returned incomplete capabilities' }
    }
    return { ok: true, version: data.version, capabilities }
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) }
  }
}

export function rethrowAnalysis(e: unknown): never {
  if (e instanceof AnalysisPathError || e instanceof AnalyzeCliError) {
    throw e
  }
  const msg = e instanceof Error ? e.message : String(e)
  throw new AnalyzeCliError('analysis/cli-failed', msg)
}
