# Changelog

Format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions track [Semantic Versioning](https://semver.org/spec/v2.0.0.html) on the
plugin's CLI + MCP-tool contract, NOT on the embedded engine version.

## [0.2.6] — 2026-06-15

Ingest-performance patch for the pinned heliosdb-nano 3.36.1.

### Changed

- **Ingest: wrap `code_index` in bulk-load mode.** The code-graph phase now
  runs under `SET bulk_load_mode = true` + `SET fk_validation = deferred`
  (`RESET` after). This sidesteps the heliosdb-nano FK-validation regression
  (v3.28.0+, which includes the pinned 3.36.1) where per-row FK checks fall
  back to a linear scan that dominated ingest wall time (~93 min / 700 files).
  Measured ~14× faster code-graph phase on the pilot portfolio. Both SETs are
  best-effort (`.is_ok()`-guarded) and no-op on engines that don't recognise
  them, so the change is safe across the supported 3.x range. The KB is
  regenerable from source, so deferring FK validation to COMMIT is acceptable.

### Fixed

- **`tests/ingest_contract.rs` fast-tier `body_vec` check.** The assertion
  inferred "embeddings ran" from a `WHERE body_vec IS NOT NULL` count, but
  heliosdb-nano 3.36.1 evaluates that predicate as always-true when the column
  is absent (returning every row) instead of erroring. The test now probes the
  `body_vec` column directly (`SELECT body_vec …`, which authoritatively errors
  when the column is missing), so a fast-tier ingest passes as intended.

## [0.2.5] — 2026-06-05

Patch release to publish the HTTP-first adoption docs and installer
cleanup after `0.2.4`.

### Changed

- README now leads with the practical CodeKB MCP benefits, the observed
  portfolio benchmark range, and honest caveats for narrow lookups.
- `install/install.sh` installs the latest crates.io release by default
  and uses `HELIOS_CODEKB_VERSION` only for explicit pinned installs.
- Release/adoption benchmark assets are included for warm HTTP MCP
  portfolio testing.

## [0.2.4] — 2026-06-03

Adoption-focused release candidate for Claude Code and Codex users.

### Added

- `doctor --source <PATH> --mcp-url <URL>` to diagnose config, KB path,
  RocksDB/Nano lock holders, background quality phase, and live HTTP
  reachability/cache state.
- `config set-serve` to persist `[serve]` defaults without hand-editing
  TOML.
- `helios_file_lookup` / `helios_doc_lookup` wrappers plus mega-tool
  actions `file_lookup` / `doc_lookup` for exact path and doc questions.
  These hit the compact ingest tables before falling back to broader
  GraphRAG, reducing the direct-file regression seen in earlier benches.
- Loopback-bind safety for HTTP mode. Non-loopback binds require the
  explicit `HELIOS_ALLOW_NON_LOOPBACK_HTTP=1` override.

### Changed

- Lockfile refreshed to published `heliosdb-nano 3.36.1`.
- Install docs, plugin metadata, slash-command setup, and portfolio
  templates now prefer a shared loopback HTTP daemon over per-session
  stdio.
- `helios_ask` now routes likely file/doc path questions to exact lookup
  wrappers before symbol-card or graph search.

## [0.2.3] — 2026-05-29

Release-readiness and portfolio-scale install patch.

### Added

- Ready-to-merge install templates under `install/` for Claude Code and
  Codex MCP registration against a HeliosDB portfolio KB.
- `--skip-linker` and `--skip-code-graph` ingest flags for fast first-pass
  portfolio KB generation when full code-reference materialisation is too
  expensive.
- Reserved `--skip-cross-file-resolve` and `--skip-code-refs` flags for the
  next HeliosDB-Nano code-index API. They are accepted for CLI compatibility
  and print a warning when running against published Nano.
- Copy-paste announcement/social launch pack prepared in the Marketing repo
  under `Social-Media/*/HeliosDB-CodeKB-MCP/`.

### Changed

- Lockfile refreshed to published `heliosdb-nano 3.33.0`.
- Plugin manifest version bumped to `0.2.3`.
- Fixed macOS release builds by moving native document-ingestion optional
  dependencies out of the Linux-only dependency section.

## [0.2.1] — 2026-05-27

Documentation-only patch on top of 0.2.0. No code changes.

### Changed

- `README.md` rewritten to lead with **install instructions for Claude
  Code + OpenAI CODEX** (the two MCP-aware coding agents this plugin
  primarily targets), each with a concrete `.mcp.json` /
  `~/.codex/config.toml` snippet plus a one-shot copy/paste invocation.
- Removed the broken `curl … releases/download/v0.2.0/…` instruction.
  The v0.2.0 release was crates.io-only — no GitHub release artifact
  existed for that tag, so the documented URL returned HTTP 404.
  Pre-built Linux x86_64 binary still ships at the v0.1.0 GH release;
  documented as a fallback with the honest caveat that it's stale vs
  the published crate.
- Install section reordered: `cargo install heliosdb-codekb-mcp` is
  now the only first-class recommendation. Pre-built binary is
  demoted to "alternative" with the version-staleness warning.

## [0.2.0] — 2026-05-27

First substantive feature release after 0.1.0. Headline: **−37.2% model
tokens vs no-MCP on the `/home/gpc/HDB/Full` corpus** with
`qwen3-coder:30b` (15 questions, MCP won 8/15, biggest wins −76k / −69k /
−47k tokens on broad-architecture queries). Full report:
[`MCP_ECOSYSTEM_BENCHMARK_REPORT_2026-05-27.md`](./MCP_ECOSYSTEM_BENCHMARK_REPORT_2026-05-27.md).

### Added

- **Compact `helios(action, args)` mega-tool is now the default `serve`
  mode.** `tools/list` returns one entry (~720 bytes) instead of 12
  (~6 KB) — an **88% smaller catalogue payload** that propagates into
  every subsequent agent turn's prompt-cache window. `--no-mega-tool`
  reverts to the per-tool surface when a client needs explicit schemas.
- **`helios_ask` question router** — single entry point that picks
  the smallest useful wrapper (repo summary / outline / symbol card /
  doc drill) and returns a compact answer card. Agents call
  `helios(action="ask", args={question})` and get back the result;
  no manual tool selection.
- **`helios.answer_card.v1` response shape** — standardised summaries +
  evidence + omission metadata + per-call token-budget tracking, so the
  agent sees a uniform "what I asked, what the server returned, what it
  dropped" envelope across every wrapper.
- **Six plugin-side wrapper tools** (`helios_repo_summary`,
  `helios_outline_first`, `helios_doc_drill`, `helios_semantic_filter`,
  `helios_git_summary`, `helios_symbol_card`) that compose multiple
  engine library calls into one distilled response.
- **Phase-2 LLM distillation at ingest** (`--with-llm-distill`) —
  opt-in pass that POSTs each symbol to an OpenAI-compatible chat
  endpoint (e.g. Tailscale-reachable Ollama / `qwen3-coder:30b`) for a
  one-sentence purpose summary, stored in
  `_hdb_plugin_symbol_cards.llm_summary`. **Batched** at 8 symbols per
  call (`--llm-distill-batch-size`), **PageRank-ordered** so
  `--llm-distill-max-symbols N` distills the most-cited code first.
- **Bulk-write distill** — `build_symbol_cards` /
  `build_repomap_cards` now use the linker's tempfile +
  `execute_batch` + `SET bulk_load_mode = true` pattern (smoke
  corpus: 8 min → 0.3 s, ~1600× speedup over the per-row execute_params
  path).
- **Per-process LRU wrapper cache** (`--wrapper-cache-size N`,
  default 0 = off). Repeat queries within a serve session short-circuit
  past the engine via FNV-1a hash of canonical arg-JSON.
- **Tool-surface gateway** (`src/mcp_trim.rs`,
  `src/main.rs::stdio_loop_with_gateway` + custom axum HTTP router):
  - `--profile {minimal|standard|full}` filters `tools/list`.
  - `--strip-tool-descriptions <int|all|none>` shortens advertised
    descriptions.
- **`fields=` projection** on `helios_symbol_card` and
  `helios_repo_summary` — agents request a subset of fields per call
  (smoke: 90% smaller per-call payload when only 1–2 fields are
  needed).
- **`--max-tool-result-bytes N`** flag — caps every string in a
  `tools/call` result at N bytes (UTF-8 char-boundary safe).
- **Self-hosted Ollama bench runner** (`bench/ollama_run.py` +
  `bench/ollama_compare.py`) — drives any OpenAI-compatible chat
  endpoint, removes the API-spend gate from comparison runs, supports
  `TRIALS`, `STEER`, `MCP_MEGA`, `MCP_PROFILE` env knobs.
- **`commands/codekb-tip.md` slash command** + expanded
  `skills/codekb-pro-features.md` with the compression-mode tool
  mapping.
- **Engine FR tracking doc** —
  [`ENGINE_FRS_FROM_CODEKB_2026-05-26.md`](./ENGINE_FRS_FROM_CODEKB_2026-05-26.md)
  — four FRs filed against `heliosdb-nano` (FK validation throughput,
  multi-row VALUES on user tables, SQL parser multibyte miscount,
  `tools/list verbose=false`). FR #1 unlocks the next bench iteration
  at scale.

### Changed

- **CLAUDE.md "No new tool surface here" rule relaxed** —
  composition wrappers (no primitive query tools) are now permitted
  in `src/wrappers.rs`.
- **`Phase::Distill` added** to the ingest pipeline checkpoint enum
  (`src/checkpoint.rs`). Resumes correctly after Walk / CodeIndex /
  GraphRag interruptions.
- **`[serve]` section in `~/.config/heliosdb-codekb-mcp/config.toml`**
  carries `profile`, `strip_tool_descriptions`, `mega_tool`,
  `wrapper_cache_size`. CLI flags still override.
- **First-run setup wizard** (`commands/codekb-setup.md`) defaults
  the new install to compact mode + asks about embeddings and LLM
  distill choices.

### Fixed

- `helios_doc_drill` now follows `DocSection → DocChunk PART_OF`
  edges directly instead of re-searching by title.
- RepoMap top-symbol `doc1l` lookup now uses qualified names (was
  occasionally pulling the wrong symbol's docstring on collisions).
- ASCII-sanitise text fields before SQL inserts to work around an
  engine SQL-parser multibyte byte/char counting bug (em-dashes,
  smart quotes, ellipses, arrows). Tracked as engine FR #3; the
  workaround is purely cosmetic — distilled cards lose typography.
- Pre-existing `_hdb_plugin_symbol_cards` row hash mismatches between
  ingests caused unnecessary re-writes; pair every INSERT with a
  DELETE-by-key in the bulk-upsert path so re-runs don't fail on
  PK conflicts when the existing-row map loaded as empty.

### Notes

- Engine pin remains `heliosdb-nano = ">=3.22.2, <4"`. Currently
  resolves to **3.31.2**. Bumping this when FR #1 (FK validation
  throughput) ships will be the next plugin release.
- Compact mode is the default for fresh installs only. Existing users
  with a saved `[serve]` section in their config keep their previous
  profile.
- The 37.2% Full-corpus result is single-trial (TRIALS=1) on
  `qwen3-coder:30b`. Multi-trial validation + Claude-Code agent
  validation are queued for the next release once FR #1 unblocks
  faster bench iteration.

## [0.1.0] — 2026-05-19

Initial release. x86_64 binaries only. MCP stdio + HTTP transports
around the embedded HeliosDB-Nano engine.
