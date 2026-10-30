/**
 * Workspace path confinement for analysis datasets.
 * @module dsh-analyze/paths
 */

import { randomBytes } from 'node:crypto'
import { existsSync, mkdirSync } from 'node:fs'
import { isAbsolute, join, normalize, resolve, sep } from 'node:path'

export class AnalysisPathError extends Error {
  readonly code: string
  constructor(code: string, message: string) {
    super(message)
    this.name = 'AnalysisPathError'
    this.code = code
  }
}

export function newDatasetId(): string {
  return `ds_${randomBytes(6).toString('hex')}`
}

export function analysisRoot(workspaceRoot: string): string {
  return join(workspaceRoot, '.dsh-analyze')
}

export function datasetsDir(workspaceRoot: string): string {
  return join(analysisRoot(workspaceRoot), 'datasets')
}

export function outputsDir(workspaceRoot: string): string {
  return join(analysisRoot(workspaceRoot), 'outputs')
}

export function tmpDir(workspaceRoot: string): string {
  return join(analysisRoot(workspaceRoot), 'tmp')
}

/**
 * Per-session analysis workspace under a profile dir.
 * Never the profile root itself — sessions cannot read each other's data
 * or profile credentials. Creates the directory once.
 */
export function ensureSessionWorkspaceRoot(profileDir: string, sessionId: string): string {
  const sid = sessionId.replace(/[^A-Za-z0-9_-]/g, '_').slice(0, 128) || 'anon'
  const root = join(profileDir, 'dsh-analyze-sessions', sid)
  if (!existsSync(root)) {
    mkdirSync(root, { recursive: true })
  }
  return root
}

/** Resolve user path and require it stays under workspaceRoot. */
export function resolveInWorkspace(workspaceRoot: string, userPath: string): string {
  if (!userPath || typeof userPath !== 'string') {
    throw new AnalysisPathError('analysis/invalid-path', 'path is required')
  }
  const ws = resolve(workspaceRoot)
  const abs = isAbsolute(userPath) ? normalize(userPath) : resolve(ws, userPath)
  if (abs !== ws && !abs.startsWith(ws + sep)) {
    throw new AnalysisPathError('analysis/invalid-path', 'path escapes workspace')
  }
  return abs
}

export function validateDatasetId(id: string): void {
  if (!/^ds_[a-f0-9]{8,32}$/.test(id)) {
    throw new AnalysisPathError('analysis/invalid-args', 'bad datasetId')
  }
}
