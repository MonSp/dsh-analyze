# TODO

## Later: optional MCP adapter (not on the DSH mainline)

**Decision.** Keep DSH as the product mainline. Do **not** extract a multi-host
“core + adapters” monorepo now. The compute surface is intentionally small;
others can reimplement it if they want. If non-DSH hosts need the same
behavior later, add a **thin MCP layer** that talks to the existing CLI.

### Contract rules

- The frozen public contract stays [`docs/interface.md`](docs/interface.md)
  and the `dsh-analyze` binary (one JSON line on stdout, `analysis/*` errors).
- MCP tools map 1:1 to CLI subcommands / existing tool names
  (`dataset_ingest`, `dataset_head`, `dataset_schema`, `dataset_list`,
  `dataset_export`, `dataset_rm`, `stats_describe`, `transform_agg`,
  `dataset_values`, `dataset_sample`, `dataset_join`).
- MCP is a **transport/adapter only**. No second query language, no extra
  compute path, no Parquet/network stack via MCP.
- Session workspace = MCP server cwd or an explicit `workspace` argument
  passed through as `--workspace`. Same path confinement as the DSH plugin.
- Fail-soft on missing binary: log warning, register nothing (parity with
  DSH plugin probe behavior).

### Sketch (when picked up)

1. New package or binary path, e.g. `packages/dsh-analyze-mcp` (or a subcommand
   `dsh-analyze mcp` if preferred later) — decide only when starting.
2. Tool schema mirrors TS `tools.ts` parameters; spawn CLI with env whitelist
   (`DSH_ANALYZE_*` only, same as DSH bridge).
3. Tests: mock CLI + one real-binary smoke (ingest → values → sample → join).
4. Docs: short README section “Other agents (MCP)” pointing at interface.md;
   do **not** rebrand the project as a general analytics suite.

### Out of scope until then

- Multi-adapter refactor / renaming the repo
- Publishing a public MCP registry package without an explicit request
- Codex / Claude Code–specific glue beyond generic MCP
