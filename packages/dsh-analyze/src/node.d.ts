/**
 * Minimal Node ambient types (no @types/node pull — keeps the package lean
 * for classic-script-adjacent hosts that seed only builtins).
 */

declare module 'node:crypto' {
  export function randomBytes(size: number): Buffer
}

declare module 'node:fs' {
  export function existsSync(path: string): boolean
  export function mkdirSync(path: string, options?: { recursive?: boolean }): void
}

declare module 'node:path' {
  export function join(...parts: string[]): string
  export function normalize(path: string): string
  export function resolve(...parts: string[]): string
  export function isAbsolute(path: string): boolean
  export const sep: string
}

declare module 'node:child_process' {
  export interface ChildProcess {
    pid?: number
    stdout: { setEncoding(encoding: string): void; on(event: string, listener: (chunk: string) => void): void }
    stderr: { setEncoding(encoding: string): void; on(event: string, listener: (chunk: string) => void): void }
    kill(signal?: number | string): boolean
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    on(event: string, listener: (...args: any[]) => void): this
  }
  export interface SpawnOptions {
    cwd?: string
    stdio?: unknown
    signal?: AbortSignal
    windowsHide?: boolean
    env?: Record<string, string | undefined>
  }
  export function spawn(command: string, args?: string[], options?: SpawnOptions): ChildProcess
  export interface SpawnSyncOptions {
    encoding?: string
    timeout?: number
    windowsHide?: boolean
    env?: Record<string, string | undefined>
  }
  export interface SpawnSyncReturns<T = string> {
    status: number | null
    stdout: T
    stderr: T
    error?: Error
  }
  export function spawnSync(command: string, args?: string[], options?: SpawnSyncOptions): SpawnSyncReturns<string>
}

declare const process: {
  pid: number
  platform: string
  cwd(): string
  env: Record<string, string | undefined>
  kill(pid: number, signal?: number): void
}

declare const console: {
  log(...args: unknown[]): void
  info(...args: unknown[]): void
  warn(...args: unknown[]): void
  error(...args: unknown[]): void
}

declare function setTimeout(handler: (...args: unknown[]) => void, timeout?: number): unknown
declare function clearTimeout(handle: unknown): void

interface AbortSignal {
  readonly aborted: boolean
  readonly reason?: unknown
  addEventListener(type: 'abort', listener: () => void): void
  removeEventListener(type: 'abort', listener: () => void): void
}

declare const AbortSignal: {
  abort(reason?: unknown): AbortSignal
  timeout(ms: number): AbortSignal
}

declare const Buffer: {
  new (data: string | Uint8Array | ArrayBuffer, encoding?: string): Buffer
  from(data: string | Uint8Array, encoding?: string): Buffer
  isBuffer(value: unknown): value is Buffer
}

interface Buffer extends Uint8Array {
  toString(encoding?: string): string
  length: number
}

declare module '@deepseek-ai/cordis' {
  export interface Context {
    root: Context
    get(name: string): unknown
    effect(dispose: () => () => void, key?: string): void
  }
}
