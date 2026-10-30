# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | ✅ |

## Reporting a vulnerability

Please report security issues privately via GitHub Security Advisories on this
repository (or open a confidential issue if advisories are unavailable). Do
**not** open a public issue for exploitable vulnerabilities.

We aim to acknowledge reports within 72 hours and ship a fix or mitigation
within 14 days for high-severity issues.

## Security model (summary)

`dsh-analyze` is a **local, offline** dataset tool:

- The CLI and plugin never open network sockets. `cargo tree` must stay free
  of network stacks.
- All file paths are confined to the session workspace (`--workspace`).
  Escape attempts fail with `analysis/invalid-path`.
- Ingest refuses profile credential paths (see `docs/security.md`).
- Subprocess env is a whitelist: `PATH` + `DSH_ANALYZE_*` (+ `NODE_ENV` for
  the TS bridge when present).
- Row / byte / spec limits are enforced on both the TS tool layer and the CLI.
- Prebuilt release binaries ship with `SHA256SUMS`. Verify before use.

## Threat notes

This tool is designed for **trusted local operators** driving an agent over
local files. It is not a multi-tenant sandbox. Do not expose the CLI or tools
to untrusted remote callers without an additional isolation boundary
(containers, VMs, filesystem ACLs).
