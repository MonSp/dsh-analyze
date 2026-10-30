/**
 * Host half of the dsh-analyze DSH plugin.
 *
 * Registers session-dataset analysis tools (`dataset_*`, `stats_describe`,
 * `transform_agg`) on the global tool registry when the `dsh-analyze` CLI
 * is discoverable. Fail-soft: a missing CLI logs a warning and registers
 * nothing — the profile stays usable without analysis tools.
 *
 * Workspace: `DSH_ANALYZE_WORKSPACE` if set, else process cwd. Datasets live
 * under `<workspace>/.dsh-analyze/{datasets,outputs,tmp}` and never leave it.
 *
 * CLI path: `DSH_ANALYZE_CLI_PATH` → legacy `INV_ANALYSIS_CLI_PATH` → PATH
 * `dsh-analyze`.
 * @module dsh-analyze
 */

import type { Context } from '@deepseek-ai/cordis'
import { probeAnalyzeCliSync, resolveAnalysisCliPath } from './cli.js'
import { ANALYSIS_TOOL_NAMES } from './policy.js'
import { createAnalysisToolDefinitions } from './tools.js'

interface ToolsSlice {
  register?(definition: unknown): () => void
}

function getTools(ctx: Context): ToolsSlice | undefined {
  try {
    return ctx.root.get('tools') as ToolsSlice | undefined
  } catch {
    return undefined
  }
}

function workspaceRoot(): string {
  const env = process.env.DSH_ANALYZE_WORKSPACE
  if (env && env.trim()) return env.trim()
  return process.cwd()
}

export function apply(ctx: Context): void {
  const tools = getTools(ctx)
  if (!tools || typeof tools.register !== 'function') {
    console.warn('[dsh-analyze] tools runtime unavailable; analysis tools not registered')
    return
  }
  const cliPath = resolveAnalysisCliPath()
  const ws = workspaceRoot()
  const probe = probeAnalyzeCliSync({ workspaceRoot: ws, cliPath, timeoutMs: 10_000 })
  if (!probe.ok) {
    console.warn('[dsh-analyze] CLI not found or probe failed; tools not registered:', probe.error)
    return
  }
  const defs = createAnalysisToolDefinitions({ cliPath, workspaceRoot: ws })
  const offs = ANALYSIS_TOOL_NAMES.map((name) => tools.register!(defs[name]))
  console.info(
    `[dsh-analyze] registered ${offs.length} tools (cli ${probe.version}) at ${ws}`,
  )
  ctx.effect(
    () => () => {
      for (const off of offs) {
        try { off() } catch { /* already gone */ }
      }
    },
    'dsh-analyze-tools',
  )
}
