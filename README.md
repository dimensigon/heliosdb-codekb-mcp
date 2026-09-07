# heliosdb-codekb-mcp

Local MCP server for code+docs knowledge bases. Embeds
[HeliosDB-Nano](https://crates.io/crates/heliosdb-nano) as a Rust
library (`code-graph`, `graph-rag`, `mcp-endpoint`, `code-embed`
features) and exposes its LSP-shaped + GraphRAG tools to Claude Code,
Cursor, Codex, Aider, and any other MCP-aware agent. The recommended
transport is a loopback HTTP daemon (`127.0.0.1`) so local clients share
one warm embedded KB process instead of starting competing stdio
processes.

**What this buys you:** fewer broad-repo discovery turns, smaller
tool payloads, reusable code/doc context across Claude Code and Codex,
and evidence-carrying answers instead of ad hoc `Read`/`Grep` tours.
It is strongest on monorepos and portfolio-scale work where the agent
would otherwise repeatedly rediscover the same architecture, docs, and
file map.

**Published release:** `0.2.5` on crates.io. It adds HTTP-first setup,
`doctor`, compact `helios(action, args)` routing, exact `file_lookup` /
`doc_lookup`, and repo-inventory fallback for fast portfolio KBs.

**Benchmark context:** v0.2.1 headline (qwen3-coder:30b on
`/home/gpc/HDB/Full`): **−37.2 % model tokens vs no-MCP across 15 dev
questions** (full report
in [`MCP_ECOSYSTEM_BENCHMARK_REPORT_2026-05-27.md`](./MCP_ECOSYSTEM_BENCHMARK_REPORT_2026-05-27.md)).
Biggest single-question wins: **−76 k, −69 k, −47 k tokens** on
broad-architecture queries. See [Honest caveats](#honest-caveats) for
the workloads where Read+Grep is still cheaper.

On the newer `/home/app/Helios` portfolio re-test for v0.2.4/v0.2.5, the best
full run observed **−28.7 % model tokens** with a warm loopback HTTP MCP
daemon. A later routing run measured **−19.4 %**. That is still a real
improvement, but not a universal win; exact narrow lookups can still be
cheaper with raw local search.

## Why it improves answer quality, not just token count

The bench measures cost. But the reason MCP wins on broad questions
isn't just compression — it's that the wrapper layer **gives the model
better-grounded raw material to reason from**. Every quality lever
also reduces tokens; both directions point the same way.

| Quality lever | What changes for the agent |
|---|---|
| **Pre-distilled answer cards** (`helios.answer_card.v1`) | Every wrapper response carries a `summary` + `evidence` array (file:line citations, qualified symbol names, doc-section IDs) + `omitted` metadata. The model doesn't have to remember where it found something across N turns — the citation rides with the answer. Less hallucinated provenance, fewer "I think this was in some file…" lapses. |
| **`helios_ask` question router** | One entry point that inspects the question and picks the right sub-wrapper (repo summary / outline-first / symbol card / doc drill). Stops the model from going down the wrong path (e.g. grep when it should outline-first). On the Full bench this picked correctly on most questions; the report calls out where it didn't and what would fix it. |
| **Exact file/doc lookup wrappers** | `helios_file_lookup` and `helios_doc_lookup` answer "show README.md", "where is Cargo.toml", and direct filename questions from the KB's compact ingest tables before falling back to GraphRAG. This removes a major regression path from earlier benches: exact file questions no longer need broad graph traversal. |
| **LLM-distilled symbol summaries** (Phase 2 ingest) | Each public symbol gets a one-sentence purpose summary generated ONCE at ingest by a code-aware model (e.g. `qwen3-coder:30b`), stored in `_hdb_plugin_symbol_cards.llm_summary`, reused across every agent query. A Haiku 4.5 query against a qwen-distilled card often produces a more accurate answer than Haiku reading the raw body and re-deriving the purpose itself. |
| **Cross-modal `MENTIONS` edges** | When the agent searches "FastEmbedder", the response includes the symbol AND the doc passages that name it. The agent verifies the doc claim against the actual code in one round-trip instead of having to grep both surfaces independently and stitch results. |
| **PageRank-ranked file index** | `helios_repo_summary` orders files by the symbol-graph PageRank, so the agent investigates the load-bearing implementation files before tests or examples. Without this, "describe the architecture" agents reliably waste 5–10 turns walking peripheral code. |
| **Typed AST diff for "what changed"** | `helios_ast_diff` / `helios_git_summary` return structured `{added,removed,moved,signature_changed}` rows, not raw line diffs. The model parses 200 bytes of structured rows instead of pages of `+/-` lines, with the same information density. |
| **Bounded-by-design responses** | Wrappers cap result counts (`max_callers=5`, `max_chunks=10`, `fields=[…]` projection). The model never gets a half-truncated response with `…[truncated]` and has to guess what came next — every response is complete relative to its declared bounds. |
| **Compact one-tool surface** | `helios(action, args)` default replaces the 12-tool catalogue. The model sees ~720 bytes of tool descriptions per turn instead of ~6 KB. Less "what tool should I call?" deliberation, more answer reasoning. |

## When the plugin earns its keep

| Scenario | What you get |
|---|---|
| **Broad architecture / cross-cutting questions** | "Describe the WAL flushing path", "where does foreign-key validation happen", "what modules depend on storage" — these are the questions where MCP saved 47-76 k tokens per question on the Full bench. Read+Grep here sends the agent on a 20+ turn grep tour; MCP lands in 3-6 turns with citations. |
| **Cross-modal queries** ("which doc section mentions the `FastEmbedder` symbol?") | `Read`+`Grep` literally can't answer in one shot. The plugin's text → code `MENTIONS` edges traverse both sides in one tool call. |
| **"What did this look like before"** (time-travel / branch / commit diff) | `helios_ast_diff`, `heliosdb_branch_*`, `heliosdb_time_travel` answer "what did this symbol look like at commit X / on branch Y" with typed AST deltas. Read+Grep would need a checkout + reindex + grep cycle. |
| **Catastrophe prevention on multi-turn tours** | On the canonical Haiku bench, Opus hit its budget cap on 5/10 questions WITHOUT MCP (no answer); Haiku burned 22-32 grep+read turns on the same questions. WITH MCP, the same questions land in 3-6 turns. This stabilises the *tail*, not always the median. |
| **Doc-heavy workflows** | Heading-chunked `.md` ingest means `helios_graphrag_search` returns the matching `DocSection` instead of the full file. Doc retrieval shrinks to one section per question. |
| **Exact path questions** | `helios_file_lookup` / `helios_doc_lookup` return bounded KB snippets and evidence for known paths, avoiding both full-file `Read` payloads and broad graph searches. |

## Honest caveats

- **Direct symbol lookups can still be cheaper through raw Read+Grep when the agent already knows the exact file and only needs a tiny span.** The v0.2.4 `file_lookup` / `doc_lookup` wrappers remove the worst exact-path regression, but symbol-card coverage still matters for "public types in crate X" style questions.
- **Symbol-card population is content-hash-gated.** If `helios_symbol_card` returns `{"status": "not_found"}`, re-run `ingest` to backfill the cards. The Full benchmark report has a "Recommended Next Work" section calling out the cases where card coverage matters most.
- **Cold-start cost is high on benchmarks** because each WITH-MCP run launches a fresh `serve` subprocess. Real agent sessions keep the server warm; the per-question cold start ratio improves with longer sessions.
- **Full portfolio code-graph indexing still has an engine bottleneck.** Nano 3.36.1's cross-file resolver can become a long serial phase on very large KBs. Fast ingest mode remains useful for file/doc lookup and GraphRAG, but deep symbol cards need the engine-side resolver work to become adoption-grade on the largest portfolios.
- **Engine-side FRs are queued.** Four engine improvements would unlock another step-change in both quality and cost — tracked in [`ENGINE_FRS_FROM_CODEKB_2026-05-26.md`](./ENGINE_FRS_FROM_CODEKB_2026-05-26.md). FR #1 (FK validation throughput) is the prerequisite for benching on `/home/gpc/HDB`-scale (10 k+ files) corpora; FR #4 (`tools/list verbose=false`) would compose with the existing `--mega-tool` for even smaller catalogue payloads.

Use the plugin when the cross-modal / time-travel / catastrophe-prevention / answer-grounding value matters. The aggregate token win on broad workloads (−37 % on Full) is a bonus; the per-answer quality lift is the durable value.

## Install

### Step 1 — install the `heliosdb-codekb-mcp` binary

```bash
cargo install heliosdb-codekb-mcp
# binary lands at ~/.cargo/bin/heliosdb-codekb-mcp
```

This is the recommended path for every platform (Linux x86_64,
Linux aarch64, macOS Intel, macOS Apple Silicon). Current source
version and crates.io release:
**[0.2.5](https://crates.io/crates/heliosdb-codekb-mcp)**.
First build pulls the engine (`heliosdb-nano`) and is slow (~10 min);
subsequent updates are cached.

<details>
<summary>Alternative: pre-built Linux x86_64 binary</summary>

A stripped Linux x86_64 binary is attached to the GitHub release for
tagged versions that publish binary artifacts. Use this if your machine
doesn't have a Rust toolchain handy. The links below currently point at
the latest binary artifact set (`v0.2.3`); use `cargo install
heliosdb-codekb-mcp` for the crates.io `0.2.5` release.

```bash
curl -L \
  https://github.com/HeliosDatabase/HeliosDB-CodeKB-MCP/releases/download/v0.2.3/heliosdb-codekb-mcp-linux-x86_64 \
  -o ~/.local/bin/heliosdb-codekb-mcp
chmod +x ~/.local/bin/heliosdb-codekb-mcp
# Optional but recommended: verify
curl -sL https://github.com/HeliosDatabase/HeliosDB-CodeKB-MCP/releases/download/v0.2.3/heliosdb-codekb-mcp-linux-x86_64.sha256 \
  | sha256sum -c -
```

macOS x86_64 binaries are also attached to tagged releases. Linux
aarch64 / macOS Apple Silicon users should use
`cargo install heliosdb-codekb-mcp` until native release artifacts are
published.

</details>

<details>
<summary>Alternative: build from source</summary>

```bash
git clone https://github.com/HeliosDatabase/HeliosDB-CodeKB-MCP
cd heliosdb-codekb-mcp
cargo build --release --features native-binary-docs
# binary: ./target/release/heliosdb-codekb-mcp
```

</details>

### Step 2 — index your project

Once per source-tree you want indexed:

```bash
heliosdb-codekb-mcp init --source /abs/path/to/your/project \
  --mode co-located --ingest
```

`co-located` puts the KB at `<project>/.helios-kb` (auto-gitignored).
See [KB-location modes](#kb-location-modes) for `global` / `hybrid`.

### Step 3 — wire it into your agent

The binary is now ready. Pick your agent below.

This repository also ships ready-to-merge templates under
[`install/`](./install/) for the `/home/app/Helios` portfolio KB:
`install/claude-code.mcp.json` and `install/codex.config.toml`.

#### Start the local MCP daemon

Run one server per KB and keep it running for all local agents:

```bash
heliosdb-codekb-mcp serve --source /abs/path/to/your/project \
  --http 127.0.0.1:8765 --wrapper-cache-size 128
```

The HTTP transport refuses non-loopback binds by default. To expose it
outside the machine, set `HELIOS_ALLOW_NON_LOOPBACK_HTTP=1`
intentionally and only on a trusted network.

#### Claude Code (`claude-code`)

**Per-project `.mcp.json`**:

```json
{
  "mcpServers": {
    "helios": {
      "type": "http",
      "url": "http://127.0.0.1:8765/"
    }
  }
}
```

`--mega-tool` is the default since v0.2.0, so the daemon advertises the
compact one-tool surface automatically unless you explicitly configure
profile mode.

**As a Claude Code plugin** (gets you the `/codekb-setup`,
`/codekb-ingest`, `/codekb-status` slash commands + the
`codekb-pro-features` skill on top of the MCP server):

```bash
# One-time install from the repo's plugin manifest
claude --plugin-dir /abs/path/to/heliosdb-codekb-mcp
```

Or, after a plugin marketplace publish:
`/plugin install heliosdb-codekb-mcp`. First run `/codekb-setup`
walks through binary install / embeddings / ingest in one flow.

#### OpenAI CODEX (`codex` CLI)

Add the server to `~/.codex/config.toml`:

```toml
[mcp_servers.helios]
url = "http://127.0.0.1:8765/"
```

To switch projects, run a different loopback port and add another named
server (`helios-foo`, `helios-bar`, …). Verify the server is registered
with:

```bash
codex --print mcp_servers
```

Then start a session as usual; the `helios(action, args)` tool should
appear in the tool list. If CODEX reports the server is unreachable,
run:

```bash
heliosdb-codekb-mcp doctor --source /abs/path/to/your/project \
  --mcp-url http://127.0.0.1:8765
```

#### Cursor / Continue / other MCP clients

```bash
heliosdb-codekb-mcp serve --source /abs/path \
  --http 127.0.0.1:8765 --wrapper-cache-size 128
```

Then point the client at:

| Route          | Method | Purpose                             |
|----------------|--------|-------------------------------------|
| `POST /`       | JSON-RPC 2.0 | Standard MCP request channel  |
| `GET /ws`      | WebSocket upgrade | Bidirectional stream      |
| `GET /sse`     | Server-sent events | Progress notifications  |
| `GET /info`    | One-shot discovery + cache stats         |

The HTTP gateway honours `--mega-tool`, `--profile`, and
`--strip-tool-descriptions` the same way the stdio path does.

### Step 4 — verify

```bash
heliosdb-codekb-mcp status --source /abs/path/to/your/project
```

You should see `kb-on-disk : exists` and a non-zero row count. If
`status` reports an interrupted ingest, just re-run
`heliosdb-codekb-mcp ingest --source <path>` — the checkpoint resumes
from the last completed phase.

## What it is

Three things at once:

1. **A user-level config tool.** Run `init --source <PATH> --mode
   <co-located|global|hybrid>` once per source-directory you want
   indexed.  The choice persists in
   `${XDG_CONFIG_HOME:-~/.config}/heliosdb-codekb-mcp/config.toml`.
2. **A KB resolver.** Given a source path, finds the right KB
   directory using the persisted config (with longest-prefix match
   for hybrid setups that span multiple sub-trees).
3. **The local MCP server.** `serve --source <PATH>` opens an
   `EmbeddedDatabase` rooted at that source's KB. Prefer
   `--http 127.0.0.1:<port>` for Claude Code/Codex multi-session use;
   stdio remains available as a fallback. Most query tools
   (`helios_lsp_*`, `helios_graphrag_search`, `helios_ast_diff`, …)
   come from the engine library, while this binary owns config,
   transport, compact wrappers, and answer-card shaping.

## KB-location modes

| Mode         | Where the KB lives                                       | Use when                                    |
|--------------|----------------------------------------------------------|---------------------------------------------|
| `co-located` | `<source>/.helios-kb` (auto-added to `.gitignore`)       | Single repo, KB travels with the code.       |
| `global`     | `${XDG_DATA_HOME}/helios-kb/<slug>` (slug = source path) | Many independent projects on one machine.   |
| `hybrid`     | An explicit `--kb <PATH>` you can register many sources to | Multi-repo aggregate (e.g. `~/Helios/*`).   |

## Document ingestion (default tier — no Docling)

Out of the box, ingestion uses pure-Rust crates.  Single static
binary, no Python, no Docker.  Covers ~80% of real-world content:

| Format                                | Backend                  |
|---------------------------------------|--------------------------|
| `.rs` / `.py` / `.ts` / `.tsx` / `.js` / `.go` / `.sql` | tree-sitter via the engine's `code-graph` |
| `.md` / `.txt`                         | tree-sitter Markdown / generic text |
| `.pdf` (born-digital)                  | [`pdf-extract`](https://crates.io/crates/pdf-extract) |
| `.docx`                                | [`docx-rs`](https://crates.io/crates/docx-rs) |
| `.xlsx`                                | [`calamine`](https://crates.io/crates/calamine) |

## Document ingestion (`--features docling`)

Build with `--features docling` and the binary additionally routes:

| Format                              | Backend (via engine `graph_rag::ingest_*`) |
|-------------------------------------|--------------------------------------------|
| Scanned / OCR-required PDFs         | Docling layout + OCR                       |
| Multi-column scientific PDFs        | Docling table & formula extraction         |
| `.pptx` / complex Office            | Docling Office pipeline                    |
| Audio (`.mp3` / `.wav` / `.m4a`)    | Docling Whisper-pipeline                   |
| Images (`.png` / `.jpg` / `.tiff`)  | Docling OCR                                |

This tier requires a `docling-serve` HTTP endpoint to be running
(host-managed, or via the `helios-code-graph` plugin's bundled
compose stack).

## Day-2 operations

After Install:

```bash
# Refresh the KB after meaningful source changes
heliosdb-codekb-mcp ingest --source /abs/path/to/your/project

# Inspect / sanity-check
heliosdb-codekb-mcp status                              # global summary
heliosdb-codekb-mcp status --source /abs/path           # per-KB row counts
heliosdb-codekb-mcp status --source /abs/path \
    --mcp-url http://127.0.0.1:8765                     # live cache stats (HTTP mode)
heliosdb-codekb-mcp config show                         # raw TOML config

# Re-ingest with LLM distillation (one-sentence symbol summaries via
# a self-hosted Ollama / qwen3-coder endpoint). Opt-in; slow first
# pass, incremental after.
heliosdb-codekb-mcp ingest --source /abs/path \
    --with-llm-distill \
    --llm-distill-endpoint http://ollama:11434 \
    --llm-distill-model qwen3-coder:30b \
    --llm-distill-batch-size 8
```

## Ingest tiers

Three knobs, picked at `init --ingest` or `ingest` time, scaling
quality vs. wall time on the pilot corpus
(`~/Helios/Nano`, 666 files / 18 k symbols / 115 k refs):

| Tier                       | Flag                    | Pilot wall   | What lights up                                  |
|----------------------------|-------------------------|--------------|-------------------------------------------------|
| **fast** (default)         | *(none)*                | **26 s**     | BM25 + hop-distance ranking on `helios_graphrag_search` |
| **quality** (blocking)     | `--with-embeddings`     | 3 m 15 s     | Adds in-process FastEmbedder body vectors → paraphrase queries |
| **background-quality**     | `--background-quality`  | **26 s parent** + ~2 m 50 s detached child | User-wait stays at 26 s; paraphrase quality lifts when the child finishes |

Recommended for repos `>~1 000 files`: `--background-quality`.
Track via `status --source X` ("quality phase : running pid X" →
"complete — took Y").

## Resume on interrupt

`ingest` writes `<kb_dir>/.ingest-state.json` at each phase
transition (`walk → code_index → graph_rag`).  If a process is
killed mid-flight (Ctrl-C, OOM, reboot), the next `ingest` reads
the file at startup and skips already-completed phases — the
walk is skipped if `phase >= code_index`.  Per-file resume *inside*
`code_index` is the engine's content-hash gate.  Cleared on
successful completion; surfaced by `status --source X` as
`ingest resume : interrupted at phase = ...` until then.

## Generated-file skip

Two layers, both honoured during the walk:

- **`@generated` content-marker scan** of the first 4 KiB
  (Facebook / Google / Bazel / Go convention).
- **`<root>/.gitattributes` linguist-generated globs** —
  patterns flagged with `linguist-generated`,
  `linguist-generated=true`, or `linguist-generated=set` are
  matched against relative paths and skipped.  Long-tail
  coverage of generated files (`*.pb.rs`, `codegen/**`,
  vendored bundles) that don't carry an in-file marker.

## CLI surface

```text
heliosdb-codekb-mcp <SUBCOMMAND>

  init     [--source PATH] [--mode co-located|global|hybrid] [--kb PATH]
           [--ingest] [--include-binary-docs BOOL]
           [--force] [--durable-writes]
           [--with-embeddings] [--background-quality]

  ingest   [--source PATH] [--include-binary-docs BOOL]
           [--force] [--durable-writes]
           [--with-embeddings] [--background-quality]

  serve    [--source PATH] [--http <ADDR>]

  status   [--source PATH] [--mcp-url <URL>]

  config   show | set-default-mode <MODE> | path
```

## Why a separate binary

HeliosDB-Nano is a generic database; baking one specific MCP
transport into its binary would adapt the engine to one consumer.
This crate keeps that boundary clean: the engine stays generic and
publishable to crates.io for any downstream consumer; this binary
holds the MCP packaging, transport, and per-source KB-location
ergonomics.

## License

Apache-2.0 — see [LICENSE](LICENSE).
