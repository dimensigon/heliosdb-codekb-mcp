---
description: First-run setup for the heliosdb-codekb-mcp plugin — installs the binary if needed, registers the KB for the current project, and indexes the source tree.
---

You are guiding the user through first-time setup of the **heliosdb-codekb** plugin for the project at `${CLAUDE_PROJECT_DIR}`. Follow these steps in order; do **not** skip steps, but pause for the user's answer at the marked prompt.

## 1. Verify the binary

Check if `heliosdb-codekb-mcp` is on the user's PATH:

```bash
command -v heliosdb-codekb-mcp || echo "NOT_INSTALLED"
```

If the binary is missing, offer the current crates.io install path first:

```bash
cargo install heliosdb-codekb-mcp
```

If the user has no Rust toolchain and is on Linux x86_64 (`uname -ms` shows `Linux x86_64`), offer the latest published binary artifact set to `~/.local/bin` and `chmod +x` it:

```bash
mkdir -p ~/.local/bin
curl -L https://github.com/HeliosDatabase/HeliosDB-CodeKB-MCP/releases/download/v0.2.3/heliosdb-codekb-mcp-linux-x86_64 \
  -o ~/.local/bin/heliosdb-codekb-mcp
chmod +x ~/.local/bin/heliosdb-codekb-mcp
```

If the user wants this checkout instead of crates.io, offer to run `cargo install --path /abs/path/to/heliosdb-codekb-mcp --features native-binary-docs`. For macOS / aarch64 / other platforms without Rust, explain that native release artifacts are not yet published and source/crates install is required. Confirm before running.

## 2. Ask about compact MCP mode

**Prompt the user verbatim:**

> Pick an MCP tool-surface mode for this project. Compact mode is recommended: it advertises one `helios(action, args)` tool instead of a dozen per-tool schemas, which cuts the per-turn tool catalogue cost.
>
> - **`compact`** *(default)* — one `helios` tool with actions like `ask`, `repo_summary`, `outline_first`, `doc_drill`, `symbol_card`, `git_summary`, and engine passthrough actions. Lowest token overhead; best for Claude Code, Codex, Gemini, and Ollama-style agents.
> - **`minimal`** — individual wrapper tools plus `heliosdb_query`. Useful for clients that strongly prefer per-tool schemas.
> - **`standard`** — wrappers + curated engine tools (read-only LSP, GraphRAG, hybrid search). Good debugging fallback.
> - **`full`** — pass-through, every tool the engine exposes (~33). Use when integrating new tools or when an agent needs the full surface.
>
> Default: **compact** (you can change later by editing `~/.config/heliosdb-codekb-mcp/config.toml`'s `[serve] mega_tool = true|false`, `[serve] profile = "…"`, or by passing `--mega-tool` / `--no-mega-tool` in your `.mcp.json` args).

Wait for the answer. Save as `MODE=compact|minimal|standard|full`. If the user picked `compact`, use `MEGA_TOOL=true` and `PROFILE=standard`; otherwise use `MEGA_TOOL=false` and `PROFILE=<their choice>`.

## 3. Ask about embeddings

**Prompt the user verbatim:**

> Enable in-process embeddings? One-time **~30 MB download** of the FastEmbedder model (BGE-Small-EN-V1.5, 384-dim) to `$XDG_CACHE_HOME/.fastembed_cache`.
>
> **Benefits:** lifts retrieval quality on paraphrase-style queries — "how does auth work" matches code/docs even when "auth" isn't the literal word in the symbol.
> **Skip if:** you mostly do exact-name lookups (`grep`-style) or want the leanest install.
>
> Default: **yes** (you can disable later by re-running `/codekb-setup` and choosing no).

Wait for the user's answer. Save it as `WITH_EMBEDDINGS=yes` or `WITH_EMBEDDINGS=no`.

## 4. Pick the KB location mode

For most users, **co-located** is the right answer: the KB lives at `${CLAUDE_PROJECT_DIR}/.helios-kb` and is auto-added to `.gitignore`. If the project is on a slow / network-mounted filesystem, suggest `global` instead (KB at `~/.local/share/helios-kb/<slug>`).

Default to `co-located` unless the user asks otherwise.

## 5. Register the KB

```bash
heliosdb-codekb-mcp init --source "${CLAUDE_PROJECT_DIR}" --mode co-located
```

(Substitute the chosen mode if the user picked something else.)

## 6. Run the ingest

If `WITH_EMBEDDINGS=yes`, use **background-quality** mode so the agent gets the fast pass in ~26 s on a typical repo and the embedding pass runs detached:

```bash
heliosdb-codekb-mcp ingest --source "${CLAUDE_PROJECT_DIR}" --background-quality
```

If `WITH_EMBEDDINGS=no`, just the fast pass:

```bash
heliosdb-codekb-mcp ingest --source "${CLAUDE_PROJECT_DIR}"
```

## 7. Persist serve defaults

Write the serve defaults to the user-level config TOML so future `serve` invocations pick them up without `.mcp.json` edits.

```bash
if [ "$MEGA_TOOL" = "true" ]; then
  heliosdb-codekb-mcp config set-serve \
    --profile "$PROFILE" \
    --mega-tool \
    --wrapper-cache-size 128 \
    --strip-tool-descriptions 200
else
  heliosdb-codekb-mcp config set-serve \
    --profile "$PROFILE" \
    --no-mega-tool \
    --wrapper-cache-size 0 \
    --strip-tool-descriptions 200
fi
```

## 8. Start or register the HTTP MCP daemon

HTTP is preferred over stdio for embedded KBs because Claude Code, Codex, and other local agents can share one warm process and avoid multi-process KB lock conflicts.

Tell the user to keep this running in a terminal, tmux session, or local service:

```bash
heliosdb-codekb-mcp serve --source "${CLAUDE_PROJECT_DIR}" \
  --http 127.0.0.1:8765 --wrapper-cache-size 128
```

The project `.mcp.json` should point to that local server:

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

## 9. Confirm and hand off

After ingest exits, run:

```bash
heliosdb-codekb-mcp doctor --source "${CLAUDE_PROJECT_DIR}" \
  --mcp-url http://127.0.0.1:8765
```

Tell the user:
- The KB is ready and the compact `helios` MCP tool should appear in their next message's tool list.
- If they ran `--background-quality`, paraphrase queries will improve once the child finishes (typically a few minutes); `/codekb-status` shows progress.
- Suggest a starter query like `helios(action="ask", args={"question":"Where is authentication handled?"})`, adapted to the user's project, to confirm it's working. For exact path checks, use `helios(action="file_lookup", args={"path":"README.md"})`.

## Honest caveat

> Engine FK regression v3.28.0+ (incl. the pinned heliosdb-nano 3.36.1): on large repos (>~500 source files) the indexer's write phase would be very slow (~93 min on a 700-file repo) because per-write FK validation falls back to a linear scan inside the ingest transaction. **Mitigated since plugin v0.2.6:** ingest now wraps `code_index` in `SET bulk_load_mode = true` + `SET fk_validation = deferred`, deferring FK validation to COMMIT — measured ~14× faster code-graph phase on the pilot portfolio. The KB is regenerable from source, so the relaxed durability is acceptable. For very large monorepos, also pass `--background-quality` so the embedding pass runs detached instead of blocking the fast structural pass.
