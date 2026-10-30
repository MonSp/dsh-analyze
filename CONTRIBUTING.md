# Contributing to dsh-analyze

Thanks for your interest in contributing.

## Development layout

```text
crates/dsh-analyze/     # Rust CLI (pure std; builds offline)
packages/dsh-analyze/   # DSH plugin (TypeScript)
tests/                  # node:test + mock CLI fixture
docs/                   # interface + security specs
```

## Prerequisites

- Rust toolchain (stable) for the CLI
- Node.js ≥ 20 for the plugin tests
- pnpm or npm (either works; lockfiles optional for the plugin)

## Local workflow

```bash
# CLI
cd crates/dsh-analyze
cargo test
cargo build --release

# Plugin
cd packages/dsh-analyze
npm install
npm run build

# Tests (from repo root; mock CLI needs no Rust)
node --test tests/analysis-policy.test.mjs tests/analysis-tools.test.mjs
```

When running TS tests that spawn the mock CLI, point `INV_TEST_NODE` at a
standalone `node` binary if your default Node cannot be spawned as a child
process.

## Pull requests

1. One logical change per PR.
2. Keep the tool contract stable: stdout is one JSON line; failures are
   non-zero exit + stderr `analysis/...` codes.
3. Never add network dependencies to the Rust crate.
4. Path confinement and limits must stay enforced on both TS and CLI sides.
5. Update `docs/interface.md` when the public CLI or tool surface changes.

## Code style

- TypeScript: `strict`, ES modules, no default exports for function plugins.
- Rust: stable edition 2021, no `unsafe`, no external crates in T0.
- Commit messages: Conventional Commits (`feat:`, `fix:`, `docs:`, …).
