//! Source-tree ingestion.
//!
//! Walks a source root with `.gitignore` awareness, classifies each
//! file by extension, and routes it to the right engine API:
//!
//! | Class                                        | Engine path                                   |
//! |----------------------------------------------|-----------------------------------------------|
//! | Code (rs / py / ts / tsx / js / go / sql)    | upsert into `src`, then `db.code_index(...)`  |
//! | Markdown (`.md`)                             | same — engine has tree-sitter Markdown grammar |
//! | Text-like (`.txt`, `.rst`, `.tex`, `.org`)   | upsert into `docs`, then `db.graph_rag_ingest_docs(...)` |
//! | PDF (born-digital)                           | `pdf-extract` → `docs` → graph-rag             |
//! | DOCX                                         | `docx-rs` → `docs` → graph-rag                  |
//! | XLSX                                         | `calamine` → `docs` → graph-rag                 |
//!
//! Files skipped: anything not in the lists above; binaries; files
//! that fail to read or fail to be valid UTF-8 for code paths;
//! anything matched by `.gitignore` or living in a hidden directory
//! (the `ignore` crate handles those by default).
//!
//! Phase-2 scope is the **default tier** — no Docling. Future
//! `--features docling` work routes scanned PDFs / images / audio
//! through `db.graph_rag_ingest_pdf` etc.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use heliosdb_nano::code_graph::{CodeIndexOptions, CodeIndexStats};
use heliosdb_nano::config::{Config as EngineConfig, WalSyncModeConfig};
use heliosdb_nano::graph_rag::{
    ChunkStrategy, IngestDocsOptions, IngestStats as DocStats, LinkerStats,
};
use heliosdb_nano::{EmbeddedDatabase, Value};
use ignore::WalkBuilder;

// Engine dep always has `code-embed` enabled (see Cargo.toml), so the
// in-process FastEmbedder is unconditionally available here.
use heliosdb_nano::code_graph::embed::FastEmbedder;

/// Construct an in-process FastEmbedder and drive the engine's
/// `code_index_with_embedder` directly, bypassing the
/// HttpEmbedder-only construction path inside `db.code_index(opts)`.
/// Lazily initialises the model on first call (~30 MB cache to
/// `$XDG_CACHE_HOME/.fastembed_cache` once).
fn run_code_index_with_inproc_embedder(
    db: &EmbeddedDatabase,
    opts: CodeIndexOptions,
) -> heliosdb_nano::Result<CodeIndexStats> {
    let embedder = FastEmbedder::try_default()?;
    heliosdb_nano::code_graph::storage::code_index_with_embedder(db, opts, Box::new(embedder))
}

/// Open an `EmbeddedDatabase` configured for the bulk-ingest workload.
///
/// Defaults to **Async WAL fsync** (`WalSyncModeConfig::Async`) — for
/// a code-graph index that's regenerable from source, durability is
/// not a property we need to pay for. The engine documents Async as
/// "10–100× throughput" vs the default Sync mode (`src/storage/wal.rs`
/// header comment). Pass `durable = true` to opt back into Sync.
pub fn open_kb_for_ingest(kb_dir: &Path, durable: bool) -> Result<EmbeddedDatabase> {
    let mut cfg = EngineConfig::default();
    cfg.storage.path = Some(kb_dir.to_path_buf());
    cfg.storage.memory_only = false;
    cfg.storage.wal_sync_mode = if durable {
        WalSyncModeConfig::Sync
    } else {
        WalSyncModeConfig::Async
    };
    EmbeddedDatabase::with_config(cfg)
        .with_context(|| format!("failed to open EmbeddedDatabase at {}", kb_dir.display()))
}

const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB; bigger files skipped
const SOURCE_TABLE: &str = "src";
const DOCS_TABLE: &str = "docs";
/// Markdown-only doc table. Split from `docs` so the second
/// `graph_rag_ingest_docs` call can use `ChunkStrategy::Headings`
/// (producing `DocSection` + `PART_OF` edges) without needing
/// per-row chunk selection in the engine.
const DOCS_MD_TABLE: &str = "docs_md";
const MAX_ERROR_SAMPLES: usize = 10;
/// Emit a progress line to stderr roughly every this often during the
/// walk-and-upsert phase. Long ingests (10 k+ file repos at minutes
/// of cold-build wall) need progress feedback so the user doesn't
/// think the binary hung.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Also emit a progress line every N files seen (fast walks where
/// the time-based interval doesn't fire). Catches the case where a
/// 10 k file walk finishes in 5 s — still gets two progress lines.
const PROGRESS_EVERY_FILES: u64 = 250;

/// Directory names that are unconditionally pruned during the walk —
/// ripgrep parity even when the source tree has no `.git` (and
/// therefore no honoured `.gitignore`). Build outputs, vendor dirs,
/// virtualenvs, IDE caches.  Gate-keeps "files seen: 3268" for trees
/// where the actual code tree is ~10 files.
const SKIP_DIRS: &[&str] = &[
    "target",       // Rust / Cargo
    "node_modules", // JS / TS
    "dist",         // generic + Python sdist
    "build",        // CMake / generic
    "out",          // generic
    ".venv",
    "venv",        // Python virtualenvs
    "__pycache__", // Python bytecode
    ".next",
    ".nuxt",  // Next / Nuxt
    ".cache", // tooling caches
    "vendor", // Go / Ruby
    "Pods",   // CocoaPods (iOS)
    ".gradle",
    ".mvn", // JVM
    ".idea",
    ".vscode", // IDE state (not code)
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
];

#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub source_root: PathBuf,
    /// KB directory.  Threaded down so the checkpoint file
    /// (`.ingest-state.json`) and the quality-progress file land
    /// alongside the engine's RocksDB state.
    pub kb_dir: PathBuf,
    /// When false, PDFs / DOCX / XLSX are skipped (default tier
    /// minus the binary doc decoders, useful if those crates fail
    /// to compile on a particular platform). Effectively forced
    /// false when this crate is built without
    /// `--features native-binary-docs`.
    pub include_binary_docs: bool,
    /// Pass-through to engine `CodeIndexOptions::force_reparse` —
    /// ignore the content-hash gate and re-parse every file.
    pub force_reparse: bool,
    /// When true, opens the KB with `WalSyncModeConfig::Sync` (fsync
    /// every write — slow but durable). Default `false` uses
    /// `WalSyncModeConfig::Async` for 10–100× throughput, accepting
    /// that a crash mid-ingest may corrupt the regenerable index.
    pub durable_writes: bool,
    /// When true, populate `body_vec` on `_hdb_code_symbols` using
    /// the in-process FastEmbedder (BGE-Small-EN-V1.5, 384-dim).
    /// Lifts `helios_graphrag_search` quality for paraphrase-style
    /// queries. Adds engine-side embedding cost during the write
    /// phase; today (post-batched-drain) the budget probably fits
    /// but the bench result is the source of truth — see
    /// ROADMAP.md Tier 0.
    pub with_embeddings: bool,
    /// When true, the binary's parent invocation runs the fast pass
    /// synchronously (no embeddings) and then spawns a detached child
    /// to do the embedding pass. The user gets back control after
    /// ~26 s on the pilot corpus instead of ~3 m 15 s. Progress is
    /// surfaced via `status --source X`. Recommended for repos with
    /// >~1 k files where a blocking embedding pass is awkward. See
    /// > `crate::quality` for the progress-file contract.
    pub background_quality: bool,
    /// Skip the post-ingest exact text-to-symbol linker. The linker
    /// creates high-quality cross-modal MENTIONS edges, but on very
    /// large portfolio KBs it can dominate ingest wall time and
    /// temporary disk usage. Code graph, doc graph, and distill cards
    /// are still built.
    pub skip_linker: bool,
    /// Reserved for the next HeliosDB-Nano code-index API. Accepted for
    /// CLI compatibility, but currently prints a warning and falls back
    /// to the engine default when building against published Nano.
    pub skip_cross_file_resolve: bool,
    /// Reserved for the next HeliosDB-Nano code-index API. Accepted for
    /// CLI compatibility, but currently prints a warning and falls back
    /// to the engine default when building against published Nano.
    pub skip_code_refs: bool,
    /// Skip the engine code-graph indexer entirely. Source rows are
    /// still stored in `src`, and docs/Markdown GraphRAG still runs.
    pub skip_code_graph: bool,
    /// Phase 2 — opt-in LLM distillation pass. `Some(opts)` triggers
    /// `crate::distill::build_llm_summaries` after the heuristic
    /// distill phase. `None` runs heuristic-only Phase 1 distill.
    pub llm_distill: Option<crate::distill::LlmDistillOptions>,
}

#[derive(Debug, Default)]
pub struct IngestSummary {
    pub files_seen: u64,
    pub code_upserts: u64,
    pub doc_upserts: u64,
    pub md_doc_upserts: u64,
    pub binary_upserts: u64,
    pub skipped: u64,
    pub read_errors: u64,
    /// First MAX_ERROR_SAMPLES failure paths + reasons.  Empty when
    /// no errors happened.
    pub read_error_samples: Vec<String>,
    pub elapsed_ms: u128,
    pub code: Option<CodeIndexStats>,
    pub docs: Option<DocStats>,
    /// Stats from the second `graph_rag_ingest_docs` pass over
    /// `docs_md` with `ChunkStrategy::Headings`.
    pub docs_md: Option<DocStats>,
    /// Stats from the post-ingest entity linker (text→symbol
    /// `MENTIONS` edges via `link_exact_qualified`).
    pub links: Option<LinkerStats>,
    /// Stats from the Layer 3 pre-distillation pass. None when the
    /// quality-child path runs (parent owns distill).
    pub distill: Option<DistillSummary>,
    /// Stats from the Phase 2 LLM-distill pass (opt-in via
    /// `--with-llm-distill`).
    pub llm_distill: Option<LlmDistillSummary>,
}

/// Summary of the Layer 3 distill pass — small subset of the full
/// `crate::distill::DistillStats` that we surface to operators.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct DistillSummary {
    pub symbols_written: usize,
    pub symbols_unchanged: usize,
    pub files_written: usize,
    pub pagerank_iters: u32,
    pub pagerank_converged: bool,
}

/// Phase 2 LLM-distill pass summary — token totals come from the
/// upstream chat endpoint's `usage` field.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct LlmDistillSummary {
    pub written: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
}

#[derive(Debug)]
enum Class<'a> {
    Code(&'a str), // engine `lang` tag — must match `Language::from_name`
    /// Files that benefit from BOTH a code-graph parse (so headings
    /// become navigable symbols via `helios_lsp_document_symbols`)
    /// AND a graph-rag doc projection with heading-aware chunking
    /// (so `helios_graphrag_search` can return the smallest matching
    /// `DocChunk` instead of the whole file). Currently markdown.
    CodeAndDoc(&'a str),
    Text,
    Notebook, // .ipynb — extract code cells, classify by metadata.kernelspec
    Pdf,
    Docx,
    Xlsx,
    Skip,
}

fn classify(path: &Path) -> Class<'static> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    let ext = match ext.as_deref() {
        Some(e) => e,
        None => return Class::Skip,
    };
    match ext {
        "rs" => Class::Code("rust"),
        "py" => Class::Code("python"),
        "ts" => Class::Code("typescript"),
        "tsx" => Class::Code("tsx"),
        "js" | "mjs" | "cjs" => Class::Code("javascript"),
        "go" => Class::Code("go"),
        "sql" => Class::Code("sql"),
        "md" | "markdown" => Class::CodeAndDoc("markdown"),
        // Notebook — special-cased extractor (see `extract_ipynb`).
        "ipynb" => Class::Notebook,
        // Schema/IDL files: registered grammars cover graphql; the rest
        // fall back to text retrieval.
        "graphql" | "gql" => Class::Text, // schema text — searchable
        "proto" | "thrift" => Class::Text, // IDL — searchable
        // Text class — flat retrieval via graph_rag_ingest_docs.
        "txt" | "rst" | "tex" | "org" | "log" | "toml" | "yaml" | "yml" | "json" | "ini"
        | "cfg" => Class::Text,
        "pdf" => Class::Pdf,
        "docx" => Class::Docx,
        "xlsx" | "xlsm" => Class::Xlsx,
        _ => Class::Skip,
    }
}

pub fn ingest(db: &EmbeddedDatabase, opts: IngestOptions) -> Result<IngestSummary> {
    let started = Instant::now();
    let mut summary = IngestSummary::default();

    ensure_tables(db)?;

    // Resume-on-interrupt checkpoint — if a previous run left a
    // checkpoint file behind, skip the phases that already
    // completed.  Per-file resume *within* the code_index phase is
    // handled by the engine's content-hash gate; the plugin only
    // gates which top-level phases run.
    let prior = crate::checkpoint::read(&opts.kb_dir)?;
    let resume_from = prior.as_ref().map(|cp| cp.phase);
    let source_root_str = opts.source_root.to_string_lossy().into_owned();
    if let Some(ref cp) = prior {
        eprintln!(
            "ingest: resuming from interrupted run (left at phase = {:?}, started {} s ago)",
            cp.phase,
            crate::quality::now_secs().saturating_sub(cp.started_at_secs),
        );
    }

    // Background-quality child path: parent already populated `src`
    // and `docs`. Skipping the re-walk in the child is now a *perf
    // optimisation* — avoids redundant filesystem walk + per-file
    // upserts that the parent already committed. (Originally a
    // correctness workaround for engine FR
    // `cross_process_on_conflict`; the engine fix landed in branch
    // `feat/cross-process-conflict-and-cache-stats` commit `6ec74d3`,
    // so removing the gate would also be safe — keeping it for the
    // perf win on large repos.)
    let is_quality_child = std::env::var(crate::quality::PROGRESS_ENV).is_ok();

    if !is_quality_child {
        // Skip the walk if a prior run already finished it (resume
        // from CodeIndex or later).  We trust the existing `src` /
        // `docs` row counts; a fresh walk would replay them
        // idempotently anyway, but skipping saves the wall time.
        let skip_walk = matches!(
            resume_from,
            Some(crate::checkpoint::Phase::CodeIndex) | Some(crate::checkpoint::Phase::GraphRag)
        );

        if skip_walk {
            // Probe row counts so the rest of the function still
            // gates on "is there work to do?".
            if let Ok(rows) = db.query("SELECT count(*) FROM src", &[]) {
                if let Some(n) = rows.first().and_then(|r| r.values.first()) {
                    summary.code_upserts = match n {
                        Value::Int4(v) => *v as u64,
                        Value::Int8(v) => *v as u64,
                        _ => 0,
                    };
                }
            }
            if let Ok(rows) = db.query("SELECT count(*) FROM docs", &[]) {
                if let Some(n) = rows.first().and_then(|r| r.values.first()) {
                    let n = match n {
                        Value::Int4(v) => *v as u64,
                        Value::Int8(v) => *v as u64,
                        _ => 0,
                    };
                    summary.doc_upserts = n;
                }
            }
            if let Ok(rows) = db.query("SELECT count(*) FROM docs_md", &[]) {
                if let Some(n) = rows.first().and_then(|r| r.values.first()) {
                    summary.md_doc_upserts = match n {
                        Value::Int4(v) => *v as u64,
                        Value::Int8(v) => *v as u64,
                        _ => 0,
                    };
                }
            }
            eprintln!(
                "ingest phase: walk skipped (resume) — trusting existing src/docs rows ({} src, {} docs, {} docs_md)",
                summary.code_upserts, summary.doc_upserts, summary.md_doc_upserts,
            );
        } else {
            // Mark walk in-flight before touching disk so a kill
            // during the walk leaves a checkpoint to resume from.
            crate::checkpoint::begin(
                &opts.kb_dir,
                &source_root_str,
                crate::checkpoint::Phase::Walk,
            )?;
            // Bulk-upsert path: one transaction around the whole
            // walk so the engine pays durability overhead once
            // instead of per-row. RAII guard rolls back on any
            // error during the loop.
            let txn = TxnGuard::begin(db)?;
            let walk_result = walk_and_upsert(db, &opts, &mut summary);
            match walk_result {
                Ok(()) => txn.commit()?,
                Err(e) => {
                    // ROLLBACK is best-effort — surface the original walk error.
                    let _ = txn.rollback();
                    return Err(e);
                }
            }
        }
    } else {
        // Quality child: probe row counts so the rest of the
        // function still gates on "there is something to index".
        if let Ok(rows) = db.query("SELECT count(*) FROM src", &[]) {
            if let Some(n) = rows.first().and_then(|r| r.values.first()) {
                summary.code_upserts = match n {
                    Value::Int4(v) => *v as u64,
                    Value::Int8(v) => *v as u64,
                    _ => 0,
                };
            }
        }
        if let Ok(rows) = db.query("SELECT count(*) FROM docs", &[]) {
            if let Some(n) = rows.first().and_then(|r| r.values.first()) {
                let n = match n {
                    Value::Int4(v) => *v as u64,
                    Value::Int8(v) => *v as u64,
                    _ => 0,
                };
                summary.doc_upserts = n;
            }
        }
        eprintln!(
            "ingest phase (quality-child): skipping walk; trusting existing src/docs rows ({} src, {} docs)",
            summary.code_upserts, summary.doc_upserts,
        );
    }

    // Step 2: run the code-graph indexer over the `src` table.
    if opts.skip_code_graph && summary.code_upserts > 0 {
        eprintln!("ingest phase: code-graph skipped (--skip-code-graph)");
    } else if summary.code_upserts > 0 {
        // Advance the resume checkpoint — we're about to enter the
        // expensive parse/write phase.
        if !is_quality_child {
            crate::checkpoint::advance(
                &opts.kb_dir,
                &source_root_str,
                crate::checkpoint::Phase::CodeIndex,
            )?;
        }
        eprintln!(
            "ingest phase: walk done in {:.1} s ({} files upserted) — \
             starting code-graph indexer (parse + symbol extract + write to _hdb_code_*){}",
            started.elapsed().as_secs_f64(),
            summary.code_upserts,
            if opts.with_embeddings {
                " + body embeddings"
            } else {
                ""
            }
        );
        let code_started = Instant::now();
        if opts.skip_cross_file_resolve {
            eprintln!(
                "ingest phase: --skip-cross-file-resolve requested, but published \
                 heliosdb-nano does not expose that code-index option yet; using \
                 engine default"
            );
        }
        if opts.skip_code_refs {
            eprintln!(
                "ingest phase: --skip-code-refs requested, but published heliosdb-nano \
                 does not expose that code-index option yet; using engine default"
            );
        }
        let cio = CodeIndexOptions {
            source_table: SOURCE_TABLE.to_string(),
            embed_bodies: opts.with_embeddings,
            embed_endpoint: None,
            embed_bearer: None,
            force_reparse: opts.force_reparse,
            // Engine v3.21.0+ — auto parallelism (min(num_cpus, 8)),
            // single chunk (max parse throughput for the pilot scale).
            parallelism: None,
            chunk_size: None,
        };
        // Bulk-write the code-graph: defer FK validation to COMMIT and put the
        // storage layer in bulk-load mode for the duration of `code_index`. The
        // KB is regenerable from source, so per-row FK enforcement isn't paying
        // for itself here — and on engines with the FK-validation regression
        // (heliosdb-nano v3.28.0+, which includes the pinned 3.36.1) per-write FK
        // validation falls back to a linear scan that dominates ingest wall time
        // (~93 min / 700 files unwrapped). Wrapping `code_index` in these SETs
        // cuts the code-graph phase ~14× on the pilot portfolio. Both SETs are
        // best-effort: an engine that doesn't recognise them leaves them as
        // no-ops, so this is safe across the supported 3.x range. RESET after so
        // the bulk-mode relaxations don't leak into later phases.
        let bulk_enabled = db.execute("SET bulk_load_mode = true").is_ok();
        let fk_deferred = db.execute("SET fk_validation = deferred").is_ok();
        if bulk_enabled || fk_deferred {
            eprintln!(
                "ingest phase: code-graph bulk-write mode (bulk_load_mode={}, fk_validation={})",
                bulk_enabled,
                if fk_deferred { "deferred" } else { "engine-default" }
            );
        }
        let result = if opts.with_embeddings {
            run_code_index_with_inproc_embedder(db, cio)
        } else {
            db.code_index(cio)
        };
        if bulk_enabled {
            let _ = db.execute("SET bulk_load_mode = false");
        }
        if fk_deferred {
            let _ = db.execute("RESET fk_validation");
        }
        // Note: the original code reached here as `match db.code_index(cio) {`.
        // Bridging to keep the rest of the body unchanged below.
        match result.map_err(anyhow::Error::from) {
            Ok(s) => {
                summary.code = Some(s);
                eprintln!(
                    "ingest phase: code-graph done in {:.1} s",
                    code_started.elapsed().as_secs_f64()
                );
            }
            Err(e) => tracing::warn!("code_index failed: {e}"),
        }
    }

    // Step 3: run the graph-rag doc ingester. Two passes:
    //   * `docs`   — chunked one-per-row (unstructured text, configs,
    //                PDFs, DOCX, XLSX). No structural splitter would
    //                give better chunks here.
    //   * `docs_md` — chunked by Markdown ATX headings. Produces
    //                `DocSection` + `DocChunk` nodes connected by
    //                `PART_OF` edges, so `helios_graphrag_search`
    //                can return the smallest matching section instead
    //                of the whole file. This is what makes the plugin
    //                competitive with PageIndex-style doc retrieval.
    let has_row_docs = summary.doc_upserts + summary.binary_upserts > 0;
    let has_md_docs = summary.md_doc_upserts > 0;
    if has_row_docs || has_md_docs {
        if !is_quality_child {
            crate::checkpoint::advance(
                &opts.kb_dir,
                &source_root_str,
                crate::checkpoint::Phase::GraphRag,
            )?;
        }
        eprintln!(
            "ingest phase: starting graph-rag doc projection ({} row-mode + {} heading-mode docs)",
            summary.doc_upserts + summary.binary_upserts,
            summary.md_doc_upserts,
        );
        let docs_started = Instant::now();
        if has_row_docs {
            let opts2 = IngestDocsOptions {
                source_table: DOCS_TABLE.to_string(),
                id_col: "path".to_string(),
                text_col: "content".to_string(),
                title_col: None,
                chunk_by: ChunkStrategy::Row,
            };
            match db.graph_rag_ingest_docs(&opts2) {
                Ok(s) => summary.docs = Some(s),
                Err(e) => tracing::warn!("graph_rag_ingest_docs(row) failed: {e}"),
            }
        }
        if has_md_docs {
            let opts_md = IngestDocsOptions {
                source_table: DOCS_MD_TABLE.to_string(),
                id_col: "path".to_string(),
                text_col: "content".to_string(),
                title_col: None,
                chunk_by: ChunkStrategy::Headings,
            };
            match db.graph_rag_ingest_docs(&opts_md) {
                Ok(s) => summary.docs_md = Some(s),
                Err(e) => tracing::warn!("graph_rag_ingest_docs(headings) failed: {e}"),
            }
        }
        eprintln!(
            "ingest phase: graph-rag done in {:.1} s",
            docs_started.elapsed().as_secs_f64()
        );

        // Step 3b: entity linker — emit `MENTIONS` edges from
        // doc/email/issue text nodes to `_hdb_code_symbols` whose
        // `qualified` name appears as a whole word in the text.
        // This is what lets `helios_graphrag_search "FastEmbedder"`
        // traverse from a README mention to the actual code symbol
        // in one round-trip instead of two.
        //
        // Plugin-side bulk path (see `crate::linker`): the engine's
        // `link_exact_qualified` was ~89 min on the pilot Nano
        // corpus (70k edges via per-row implicit-txn INSERTs). The
        // plugin's `link_mentions_bulk` does the same computation
        // but streams batches to a tempfile and applies them via
        // `execute_batch` under `SET bulk_load_mode = true`.
        if opts.skip_linker {
            eprintln!("ingest phase: linker skipped (--skip-linker)");
        } else {
            let link_started = Instant::now();
            match crate::linker::link_mentions_bulk(db) {
                Ok(stats) => {
                    eprintln!(
                        "ingest phase: linker done in {:.1} s — {} MENTIONS edges added (bulk)",
                        link_started.elapsed().as_secs_f64(),
                        stats.mentions_added
                    );
                    summary.links = Some(stats);
                }
                Err(e) => tracing::warn!("link_mentions_bulk failed: {e}"),
            }
        }

        // Step 3c — pre-distillation (Layer 3). Populates the plugin's
        // `_hdb_plugin_symbol_cards` + `_hdb_plugin_repomap_cards`
        // tables that back the `helios_repo_summary` /
        // `helios_symbol_card` wrappers. Heuristic-only (no LLM); see
        // `src/distill.rs`. Skipped on the quality-child path — the
        // parent owns the distill pass.
        if !is_quality_child {
            crate::checkpoint::advance(
                &opts.kb_dir,
                &source_root_str,
                crate::checkpoint::Phase::Distill,
            )?;
            let distill_started = Instant::now();
            // Order matters: symbol cards first (PageRank's per-symbol
            // doc1l projection wants them already in place for the
            // repomap top-symbol JSON).
            let sym_stats = match crate::distill::build_symbol_cards(db) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("distill::build_symbol_cards failed: {e}");
                    crate::distill::DistillStats::default()
                }
            };
            let repo_stats = match crate::distill::build_repomap_cards(db) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("distill::build_repomap_cards failed: {e}");
                    crate::distill::DistillStats::default()
                }
            };
            eprintln!(
                "ingest phase: distill done in {:.1} s — {} symbol cards ({} unchanged, scanned {}), \
                 {} file cards (pagerank {} iters, converged={})",
                distill_started.elapsed().as_secs_f64(),
                sym_stats.symbols_written,
                sym_stats.symbols_unchanged,
                sym_stats.symbols_scanned,
                repo_stats.files_written,
                repo_stats.pagerank_iters,
                repo_stats.pagerank_converged,
            );
            summary.distill = Some(DistillSummary {
                symbols_written: sym_stats.symbols_written,
                symbols_unchanged: sym_stats.symbols_unchanged,
                files_written: repo_stats.files_written,
                pagerank_iters: repo_stats.pagerank_iters,
                pagerank_converged: repo_stats.pagerank_converged,
            });

            // Phase 2 — opt-in LLM distillation. Runs AFTER the
            // heuristic build_symbol_cards so the LLM has the
            // signature + doc1l + body excerpt available, and so
            // re-runs are cheap (only new/changed symbols hit the
            // LLM endpoint).
            if let Some(ref llm_opts) = opts.llm_distill {
                let llm_started = Instant::now();
                eprintln!(
                    "ingest phase: LLM distill starting — endpoint={} model={} concurrency={}",
                    llm_opts.endpoint, llm_opts.model, llm_opts.concurrency
                );
                match crate::distill::build_llm_summaries(db, llm_opts) {
                    Ok(stats) => {
                        eprintln!(
                            "ingest phase: LLM distill done in {:.1} s — {}/{} written ({} unchanged, {} failed), \
                             tokens prompt={} completion={}",
                            llm_started.elapsed().as_secs_f64(),
                            stats.written,
                            stats.candidates,
                            stats.unchanged,
                            stats.failed,
                            stats.total_prompt_tokens,
                            stats.total_completion_tokens,
                        );
                        summary.llm_distill = Some(LlmDistillSummary {
                            written: stats.written,
                            unchanged: stats.unchanged,
                            failed: stats.failed,
                            total_prompt_tokens: stats.total_prompt_tokens,
                            total_completion_tokens: stats.total_completion_tokens,
                        });
                    }
                    Err(e) => tracing::warn!("build_llm_summaries failed: {e}"),
                }
            }
        }
    }

    // All phases done — clear the resume checkpoint so the next
    // ingest doesn't think it's resuming. Quality-child path
    // doesn't own the checkpoint (parent does), so leave it alone.
    if !is_quality_child {
        let _ = crate::checkpoint::clear(&opts.kb_dir);
    }

    summary.elapsed_ms = started.elapsed().as_millis();
    Ok(summary)
}

/// Walk the source tree, classify each file, upsert into `src` /
/// `docs`. The caller wraps this in a transaction (see `ingest`).
fn walk_and_upsert(
    db: &EmbeddedDatabase,
    opts: &IngestOptions,
    summary: &mut IngestSummary,
) -> Result<()> {
    let walker = WalkBuilder::new(&opts.source_root)
        .hidden(true) // skip dot-files / dot-dirs (incl. .git, .helios-kb)
        .git_ignore(true) // honour .gitignore
        .git_global(true) // honour ~/.config/git/ignore
        .git_exclude(true)
        .filter_entry(|entry| {
            // ripgrep parity: skip well-known build / vendor dirs even
            // when there's no .git (so .gitignore wouldn't catch them).
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    if SKIP_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        })
        .build();

    // `.gitattributes linguist-generated` honour. Loaded once before
    // the walk so we don't re-parse per file. Empty / missing →
    // `None`, and we fall back to the `is_generated_file` 4-KiB peek.
    let linguist_skip = load_linguist_generated_globset(&opts.source_root);

    let mut last_progress_at = Instant::now();
    let mut last_progress_files: u64 = 0;
    let walk_started = Instant::now();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                summary.read_errors += 1;
                continue;
            }
        };

        // Periodic progress to stderr — fires whichever of {time-since-last,
        // files-since-last} threshold is hit first. Both quiet for tiny runs.
        if last_progress_at.elapsed() >= PROGRESS_INTERVAL
            || summary.files_seen.saturating_sub(last_progress_files) >= PROGRESS_EVERY_FILES
        {
            eprintln!(
                "ingest progress: walked {} files ({} code, {} text, {} doc upserted) — {:.1} s",
                summary.files_seen,
                summary.code_upserts,
                summary.doc_upserts,
                summary.binary_upserts,
                walk_started.elapsed().as_secs_f64()
            );
            last_progress_at = Instant::now();
            last_progress_files = summary.files_seen;
        }

        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        // Skip anything inside the KB itself (defence in depth — gitignore
        // should have caught it, but the user might run ingest before
        // saving .gitignore).
        if path
            .components()
            .any(|c| c.as_os_str() == ".helios-kb" || c.as_os_str() == ".helios-index")
        {
            continue;
        }

        summary.files_seen += 1;

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                summary.skipped += 1;
                continue;
            }
        };
        if meta.len() > MAX_FILE_BYTES {
            summary.skipped += 1;
            continue;
        }

        let class = classify(path);
        let rel = relative_path(path, &opts.source_root);

        // Generated-file skip path A: `.gitattributes linguist-generated`
        // glob match against the relative path.  Same scope as the
        // content-marker check (Code / Notebook only).
        if matches!(class, Class::Code(_) | Class::Notebook) {
            if let Some(set) = linguist_skip.as_ref() {
                if set.is_match(&rel) {
                    summary.skipped += 1;
                    continue;
                }
            }
        }

        // Generated-file skip path B: peek the first 4 KiB for the
        // canonical "@generated" marker (Facebook / Google / Bazel
        // convention). Only applied to Code / Notebook classes —
        // text and binary doc extraction shouldn't be skipped.
        if matches!(class, Class::Code(_) | Class::Notebook) && is_generated_file(path) {
            summary.skipped += 1;
            continue;
        }

        match class {
            Class::Code(lang) => match read_utf8(path) {
                Ok(content) => {
                    upsert_src(db, &rel, &content, lang)?;
                    summary.code_upserts += 1;
                }
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::CodeAndDoc(lang) => match read_utf8(path) {
                Ok(content) => {
                    upsert_src(db, &rel, &content, lang)?;
                    summary.code_upserts += 1;
                    upsert_doc_md(db, &rel, &content)?;
                    summary.md_doc_upserts += 1;
                }
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::Text => match read_utf8(path) {
                Ok(content) => {
                    upsert_doc(db, &rel, &content, "text")?;
                    summary.doc_upserts += 1;
                }
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::Notebook => match extract_ipynb(path) {
                Ok((src_text, lang)) if !src_text.trim().is_empty() => {
                    upsert_src(db, &rel, &src_text, lang)?;
                    summary.code_upserts += 1;
                }
                Ok(_) => record_read_error(summary, path, "notebook had no code cells"),
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::Pdf if opts.include_binary_docs => match extract_pdf(path) {
                Ok(text) if !text.trim().is_empty() => {
                    upsert_doc(db, &rel, &text, "pdf")?;
                    summary.binary_upserts += 1;
                }
                Ok(_) => record_read_error(summary, path, "PDF produced empty text"),
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::Docx if opts.include_binary_docs => match extract_docx(path) {
                Ok(text) if !text.trim().is_empty() => {
                    upsert_doc(db, &rel, &text, "docx")?;
                    summary.binary_upserts += 1;
                }
                Ok(_) => record_read_error(summary, path, "DOCX produced empty text"),
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::Xlsx if opts.include_binary_docs => match extract_xlsx(path) {
                Ok(text) if !text.trim().is_empty() => {
                    upsert_doc(db, &rel, &text, "xlsx")?;
                    summary.binary_upserts += 1;
                }
                Ok(_) => record_read_error(summary, path, "XLSX produced empty text"),
                Err(e) => record_read_error(summary, path, &e.to_string()),
            },
            Class::Pdf | Class::Docx | Class::Xlsx => {
                // include_binary_docs disabled — silent skip
                summary.skipped += 1;
            }
            Class::Skip => {
                summary.skipped += 1;
            }
        }
    }
    Ok(())
}

/// Tiny RAII guard around BEGIN / COMMIT / ROLLBACK so the upsert
/// loop runs inside a single transaction (Phase 2.5f).
struct TxnGuard<'a> {
    db: &'a EmbeddedDatabase,
    finished: bool,
}

impl<'a> TxnGuard<'a> {
    fn begin(db: &'a EmbeddedDatabase) -> Result<Self> {
        db.execute("BEGIN").context("BEGIN transaction")?;
        Ok(Self {
            db,
            finished: false,
        })
    }
    fn commit(mut self) -> Result<()> {
        self.db.execute("COMMIT").context("COMMIT transaction")?;
        self.finished = true;
        Ok(())
    }
    fn rollback(mut self) -> Result<()> {
        self.db
            .execute("ROLLBACK")
            .context("ROLLBACK transaction")?;
        self.finished = true;
        Ok(())
    }
}

impl<'a> Drop for TxnGuard<'a> {
    fn drop(&mut self) {
        if !self.finished {
            // Best-effort rollback if neither commit nor rollback was
            // called explicitly (e.g. panic).
            let _ = self.db.execute("ROLLBACK");
        }
    }
}

fn ensure_tables(db: &EmbeddedDatabase) -> Result<()> {
    db.execute(
        "CREATE TABLE IF NOT EXISTS src (
            path     TEXT PRIMARY KEY,
            content  TEXT,
            lang     TEXT
        )",
    )
    .context("create src table")?;
    db.execute(
        "CREATE TABLE IF NOT EXISTS docs (
            path     TEXT PRIMARY KEY,
            content  TEXT,
            kind     TEXT
        )",
    )
    .context("create docs table")?;
    db.execute(
        "CREATE TABLE IF NOT EXISTS docs_md (
            path     TEXT PRIMARY KEY,
            content  TEXT,
            kind     TEXT
        )",
    )
    .context("create docs_md table")?;
    Ok(())
}

/// Parse `<root>/.gitattributes` and build a `GlobSet` of patterns
/// flagged with `linguist-generated` (or `linguist-generated=true`).
/// Returns `None` if the file is absent, empty of relevant entries,
/// or fails to parse — all are non-fatal: callers degrade to the
/// content-marker check (`is_generated_file`) only.
///
/// gitattributes line format we support:
///   `<pattern> linguist-generated`
///   `<pattern> linguist-generated=true`
///   `<pattern> linguist-generated linguist-vendored`  (any-position attr)
///
/// Lines starting with `#` and blank lines are skipped. We do not
/// honour `linguist-generated=false` (no negation today; rare in
/// practice).  We also check `<root>/.git/info/attributes` for repo-
/// scoped overrides — same parser.
fn load_linguist_generated_globset(root: &Path) -> Option<GlobSet> {
    let candidates = [
        root.join(".gitattributes"),
        root.join(".git").join("info").join("attributes"),
    ];
    let mut builder = GlobSetBuilder::new();
    let mut count = 0usize;
    for p in &candidates {
        let body = match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for raw in body.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let pattern = match parts.next() {
                Some(p) => p,
                None => continue,
            };
            let mut linguist_generated = false;
            for attr in parts {
                if attr == "linguist-generated"
                    || attr == "linguist-generated=true"
                    || attr == "linguist-generated=set"
                {
                    linguist_generated = true;
                    break;
                }
            }
            if !linguist_generated {
                continue;
            }
            // gitattributes patterns are already glob-like (`*.pb.rs`,
            // `vendor/**`, etc.); a few have `[abc]` ranges that
            // globset handles natively.  Build with literal_separator=false
            // so `*` matches across slashes the way users expect for
            // `**/*.pb.rs`.
            if let Ok(glob) = Glob::new(pattern) {
                builder.add(glob);
                count += 1;
            }
        }
    }
    if count == 0 {
        return None;
    }
    builder.build().ok()
}

/// Peek the first 4 KiB of the file and return true iff it contains
/// one of the canonical machine-generated markers.  Cheap defence
/// against indexing protobuf-generated `*.pb.rs`, OpenAPI clients,
/// vendored bundles etc. that aren't caught by `.gitignore`.
fn is_generated_file(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 4096];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    // Match the case-sensitive markers Linguist + Bazel + many tools use.
    head.contains("@generated")
        || head.contains("DO NOT EDIT")
        || head.contains("AUTO-GENERATED")
        || head.contains("Code generated by") // Go convention
}

fn record_read_error(summary: &mut IngestSummary, path: &Path, reason: &str) {
    summary.read_errors += 1;
    if summary.read_error_samples.len() < MAX_ERROR_SAMPLES {
        summary
            .read_error_samples
            .push(format!("{}: {}", path.display(), reason));
    }
}

fn upsert_src(db: &EmbeddedDatabase, path: &str, content: &str, lang: &str) -> Result<()> {
    db.execute_params(
        "INSERT INTO src (path, content, lang) VALUES ($1, $2, $3) \
         ON CONFLICT(path) DO UPDATE SET content = excluded.content, lang = excluded.lang",
        &[
            Value::String(path.to_string()),
            Value::String(content.to_string()),
            Value::String(lang.to_string()),
        ],
    )
    .with_context(|| format!("upsert_src {path}"))?;
    Ok(())
}

fn upsert_doc_md(db: &EmbeddedDatabase, path: &str, content: &str) -> Result<()> {
    db.execute_params(
        "INSERT INTO docs_md (path, content, kind) VALUES ($1, $2, 'markdown') \
         ON CONFLICT(path) DO UPDATE SET content = excluded.content, kind = excluded.kind",
        &[
            Value::String(path.to_string()),
            Value::String(content.to_string()),
        ],
    )
    .with_context(|| format!("upsert_doc_md {path}"))?;
    Ok(())
}

fn upsert_doc(db: &EmbeddedDatabase, path: &str, content: &str, kind: &str) -> Result<()> {
    db.execute_params(
        "INSERT INTO docs (path, content, kind) VALUES ($1, $2, $3) \
         ON CONFLICT(path) DO UPDATE SET content = excluded.content, kind = excluded.kind",
        &[
            Value::String(path.to_string()),
            Value::String(content.to_string()),
            Value::String(kind.to_string()),
        ],
    )
    .with_context(|| format!("upsert_doc {path}"))?;
    Ok(())
}

fn read_utf8(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("not utf-8: {e}"))
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Extract concatenated code from a Jupyter `.ipynb` notebook + the
/// language tag from its `metadata.kernelspec.language`. Falls back
/// to `python` if the kernel-spec is missing (the dominant case).
fn extract_ipynb(path: &Path) -> Result<(String, &'static str)> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("read notebook {}", path.display()))?;
    // Lightweight: parse as serde_json::Value, walk cells.
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parse notebook {} as JSON", path.display()))?;
    let lang_tag = v
        .pointer("/metadata/kernelspec/language")
        .and_then(|x| x.as_str())
        .and_then(|s| match s.to_lowercase().as_str() {
            "python" | "python3" => Some("python"),
            "typescript" => Some("typescript"),
            "javascript" => Some("javascript"),
            "rust" => Some("rust"),
            "go" => Some("go"),
            "sql" => Some("sql"),
            _ => None,
        })
        .unwrap_or("python");

    let mut out = String::new();
    if let Some(cells) = v.get("cells").and_then(|c| c.as_array()) {
        for cell in cells {
            let kind = cell.get("cell_type").and_then(|c| c.as_str()).unwrap_or("");
            if kind != "code" {
                continue;
            }
            if let Some(src) = cell.get("source") {
                // `source` is either a single string or an array of strings.
                if let Some(s) = src.as_str() {
                    out.push_str(s);
                    out.push('\n');
                } else if let Some(arr) = src.as_array() {
                    for line in arr {
                        if let Some(s) = line.as_str() {
                            out.push_str(s);
                        }
                    }
                    out.push('\n');
                }
            }
        }
    }
    Ok((out, lang_tag))
}

#[cfg(feature = "native-binary-docs")]
fn extract_pdf(path: &Path) -> Result<String> {
    pdf_extract::extract_text(path).map_err(|e| anyhow::anyhow!("pdf-extract: {e}"))
}

#[cfg(not(feature = "native-binary-docs"))]
fn extract_pdf(_path: &Path) -> Result<String> {
    anyhow::bail!(
        "PDF ingestion not enabled — rebuild with `--features native-binary-docs` (or use Docling)"
    )
}

#[cfg(feature = "native-binary-docs")]
fn extract_docx(path: &Path) -> Result<String> {
    use docx_rs::*;
    let bytes = std::fs::read(path)?;
    let docx = read_docx(&bytes).map_err(|e| anyhow::anyhow!("docx-rs read: {e}"))?;
    let mut out = String::new();
    for child in &docx.document.children {
        if let DocumentChild::Paragraph(p) = child {
            for run in &p.children {
                if let ParagraphChild::Run(r) = run {
                    for c in &r.children {
                        if let RunChild::Text(t) = c {
                            out.push_str(&t.text);
                        }
                    }
                }
            }
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(not(feature = "native-binary-docs"))]
fn extract_docx(_path: &Path) -> Result<String> {
    anyhow::bail!(
        "DOCX ingestion not enabled — rebuild with `--features native-binary-docs` (or use Docling)"
    )
}

#[cfg(feature = "native-binary-docs")]
fn extract_xlsx(path: &Path) -> Result<String> {
    use calamine::{open_workbook_auto, Reader};
    let mut wb = open_workbook_auto(path).map_err(|e| anyhow::anyhow!("calamine open: {e}"))?;
    let mut out = String::new();
    let names: Vec<String> = wb.sheet_names().to_owned();
    for name in &names {
        if let Ok(range) = wb.worksheet_range(name) {
            out.push_str(&format!("# Sheet: {name}\n"));
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(|c| c.to_string()).collect();
                out.push_str(&cells.join("\t"));
                out.push('\n');
            }
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(not(feature = "native-binary-docs"))]
fn extract_xlsx(_path: &Path) -> Result<String> {
    anyhow::bail!(
        "XLSX ingestion not enabled — rebuild with `--features native-binary-docs` (or use Docling)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn classify_extensions() {
        assert!(matches!(classify(Path::new("a.rs")), Class::Code("rust")));
        assert!(matches!(classify(Path::new("a.py")), Class::Code("python")));
        assert!(matches!(classify(Path::new("a.tsx")), Class::Code("tsx")));
        assert!(matches!(
            classify(Path::new("a.md")),
            Class::CodeAndDoc("markdown")
        ));
        assert!(matches!(classify(Path::new("a.txt")), Class::Text));
        assert!(matches!(classify(Path::new("a.pdf")), Class::Pdf));
        assert!(matches!(classify(Path::new("a.docx")), Class::Docx));
        assert!(matches!(classify(Path::new("a.xlsx")), Class::Xlsx));
        assert!(matches!(classify(Path::new("a.png")), Class::Skip));
        assert!(matches!(classify(Path::new("a")), Class::Skip));
    }
}
