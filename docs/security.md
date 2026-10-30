# dsh-analyze security notes

Companion to the root [SECURITY.md](../SECURITY.md). This file details the
enforced boundaries implementers and auditors should verify.

## 1. Path confinement (two layers)

**Tool layer (TypeScript)**

- `resolveInWorkspace(workspaceRoot, userPath)` — relative paths join the
  workspace; absolute paths must stay under it after `path.normalize`.
- Escape → `AnalysisPathError` code `analysis/invalid-path` **before** any
  subprocess spawn.

**CLI layer (Rust)**

- `ensure_within_workspace` canonicalizes the workspace and the candidate
  (parent-dir canonicalize when the file does not exist yet).
- Prefix check uses `Path::starts_with` on canonical paths.

Both layers must stay. Removing either re-opens traversal via a compromised
tool host or a direct CLI call.

## 2. Credential deny list

When the host supplies a profile root, ingest refuses any resolved path under
these first segments:

```text
***REMOVED***
.bootstrap-token/
profiles/
.credentials.yaml
.writer.lock
```

Codes: `analysis/invalid-path`.

## 3. Subprocess environment

The TS bridge spawns the CLI with a whitelist only:

```text
PATH
DSH_ANALYZE_MAX_ROWS
DSH_ANALYZE_MAX_INGEST_BYTES
DSH_ANALYZE_DEFAULT_ROW_LIMIT
NODE_ENV   (when already set — host runtime hint only)
```

No proxy vars, no cloud credentials, no `HOME`/`USERPROFILE` forwarding from
the parent process env object.

## 4. Resource limits

| Limit | Enforced at |
|---|---|
| ingest bytes | CLI (`DSH_ANALYZE_MAX_INGEST_BYTES`) |
| ingest rows | CLI (`DSH_ANALYZE_MAX_ROWS`) |
| head rows | tool (`clampHeadN` 1..20) + CLI clamp |
| agg limit | tool (1..5000) + CLI default |
| spec size | tool (`maxSpecBytes`) |
| CLI stdout | tool kills child past ~4× `maxOutputBytes` |
| CLI wall time | tool timeout + process-tree kill (`taskkill /T` on Windows) |

## 5. Network

The Rust crate depends on **std only**. CI should run `cargo tree` and fail
if a network stack crate appears.

## 6. Output hygiene

- Ingest returns schema + counts, never the full table.
- Head is capped at 20 rows.
- Agg inline rows are budgeted at 20; larger results must use `--out`.
- Export writes to disk and returns a relative path + byte count.

## 7. Multi-tenant expectation

Workspaces are isolated **per session directory**, not by OS user
namespaces. Hosts that co-locate untrusted tenants must add their own
boundary (container, VM, ACL). This project does not claim multi-tenant
isolation.
