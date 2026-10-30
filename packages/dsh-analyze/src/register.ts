/**
 * Host-agnostic attach path: probe CLI + register selected analysis tools
 * on any agent-scope tools runtime. No product-specific routing dependency —
 * a bare plugin host can call this with an empty profile.
 * @module dsh-analyze/register
 */

import { probeAnalyzeCliSync } from './cli.js'
import type { AnalysisToolName } from './policy.js'
import { createAnalysisDefinitionsForNames, type AnalysisRuntimeConfig } from './tools.js'

export interface AgentToolsRegisterLike {
  register?(definition: unknown): () => void
}

export interface AttachAnalysisToolsOptions {
  agentTools: AgentToolsRegisterLike
  names: readonly AnalysisToolName[]
  config: AnalysisRuntimeConfig
  /**
   * Injected probe (tests / custom CLI resolution). Defaults to probeAnalyzeCliSync.
   * Return shape is looser than AnalyzeCliProbeResult so host stubs can opt in.
   */
  probe?: (options: {
    workspaceRoot: string
    cliPath: string
    cliPrefixArgs?: readonly string[]
    timeoutMs?: number
  }) => {
    ok: boolean
    version?: string
    capabilities?: string[]
    error?: string
  }
  timeoutMs?: number
}

export interface AttachAnalysisToolsResult {
  registered: boolean
  version?: string
  capabilities?: string[]
  error?: string
  dispose?: () => void
}

/**
 * Probe the CLI, then register the named tools. On probe failure nothing is
 * registered (fail-soft: caller logs and continues without analysis tools).
 */
export function attachAnalysisTools(options: AttachAnalysisToolsOptions): AttachAnalysisToolsResult {
  if (options.names.length === 0) {
    return { registered: false, error: 'no analysis tools selected' }
  }
  if (typeof options.agentTools.register !== 'function') {
    return { registered: false, error: 'agent tools register unavailable' }
  }
  const register = options.agentTools.register.bind(options.agentTools)
  const probe = options.probe ?? probeAnalyzeCliSync
  const probed = probe({
    workspaceRoot: options.config.workspaceRoot,
    cliPath: options.config.cliPath,
    cliPrefixArgs: options.config.cliPrefixArgs,
    timeoutMs: options.timeoutMs ?? 10_000,
  })
  if (!probed.ok) {
    return { registered: false, error: probed.error ?? 'probe failed' }
  }
  const defs = createAnalysisDefinitionsForNames(options.names, options.config)
  const offs = defs.map((def) => register(def))
  return {
    registered: true,
    ...(probed.version !== undefined ? { version: probed.version } : {}),
    ...(probed.capabilities !== undefined ? { capabilities: probed.capabilities } : {}),
    dispose: () => {
      for (const off of offs) {
        try { off() } catch { /* ignore */ }
      }
    },
  }
}
