//! End-to-end ingest contract tests for the `heliosdb-codekb-mcp`
//! binary.  Catches regressions across plugin ↔ engine version
//! transitions: feeds a tiny synthetic corpus through `init
//! --ingest`, opens the produced KB via `EmbeddedDatabase`, and
//! asserts the externally-observable invariants the plugin commits
//! to.  See ROADMAP Tier 1 #3.
//!
//! These tests invoke the actual built binary (`CARGO_BIN_EXE_*`),
//! not the library — that's the layer pilots and CI users hit.
//!
//! Each test sets `XDG_CONFIG_HOME` / `XDG_DATA_HOME` inside a
//! tempdir so it doesn't pollute the developer's user-level config.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use heliosdb_nano::{EmbeddedDatabase, Value};
use tempfile::TempDir;

fn binary() -> &'static Path {
    static BIN: OnceLock<&'static Path> = OnceLock::new();
    BIN.get_or_init(|| {
        let p = env!("CARGO_BIN_EXE_heliosdb-codekb-mcp");
        Box::leak(std::path::PathBuf::from(p).into_boxed_path())
    })
}

/// Build a tiny `(source_root, kb_dir)` pair under a tempdir so
/// each test owns its environment.
struct Fixture {
    _td: TempDir,
    source: std::path::PathBuf,
    kb: std::path::PathBuf,
    xdg_config: std::path::PathBuf,
    xdg_data: std::path::PathBuf,
}

impl Fixture {
    fn new(seed: &str) -> Self {
        let td = tempfile::tempdir().expect("tempdir");
        let source = td.path().join(format!("src-{seed}"));
        let kb = td.path().join(format!("kb-{seed}"));
        let xdg_config = td.path().join(".config");
        let xdg_data = td.path().join(".local-share");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::create_dir_all(&xdg_config).unwrap();
        std::fs::create_dir_all(&xdg_data).unwrap();
        Self {
            _td: td,
            source,
            kb,
            xdg_config,
            xdg_data,
        }
    }

    fn write_corpus(&self) {
        std::fs::write(
            self.source.join("a.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
             pub fn sub(a: i32, b: i32) -> i32 { a - b }\n",
        )
        .unwrap();
        std::fs::create_dir_all(self.source.join("sub")).unwrap();
        std::fs::write(
            self.source.join("sub/b.py"),
            "def hello():\n    return 'hi'\n\
             def goodbye():\n    return 'bye'\n",
        )
        .unwrap();
    }

    /// Corpus that also includes a Markdown file with multiple ATX
    /// headings — exercises the `docs_md` table + `ChunkStrategy::
    /// Headings` projection. The README references `add` so the
    /// entity linker can emit a `MENTIONS` edge into the code symbol.
    fn write_corpus_with_md(&self) {
        self.write_corpus();
        std::fs::write(
            self.source.join("README.md"),
            "# Project\n\n\
             Top-level overview.\n\n\
             ## Math helpers\n\n\
             The `add` function adds two integers and returns the sum.\n\n\
             ## Greetings\n\n\
             `hello` returns a friendly string.\n",
        )
        .unwrap();
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(binary());
        c.env("XDG_CONFIG_HOME", &self.xdg_config);
        c.env("XDG_DATA_HOME", &self.xdg_data);
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let out = self.cmd().args(args).output().expect("spawn binary");
        if !out.status.success() {
            panic!(
                "binary exited {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            );
        }
        out
    }
}

fn count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).expect(sql);
    let v = rows
        .first()
        .and_then(|r| r.values.first())
        .expect("at least one cell");
    match v {
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        other => panic!("count() returned non-int: {other:?}"),
    }
}

#[test]
fn init_then_ingest_produces_expected_row_counts() {
    let f = Fixture::new("basic");
    f.write_corpus();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
    ]);

    let db = EmbeddedDatabase::new(&f.kb).unwrap();
    assert_eq!(
        count(&db, "SELECT count(*) FROM src"),
        2,
        "src should hold one row per source file"
    );
    assert!(
        count(&db, "SELECT count(*) FROM _hdb_code_symbols") >= 4,
        "expect at least 4 symbols (2 fns × 2 files)"
    );
    // Default fast tier: no embeddings.  Engine's
    // `ensure_body_vec_column` only ALTERs `_hdb_code_symbols` to
    // add `body_vec` lazily when an embedder runs; in fast tier the
    // column is absent entirely (the engine returns
    // "Column 'body_vec' not found in schema" instead of just an
    // empty result).  Either outcome — error or zero rows — is
    // acceptable for "no embeddings".
    // Probe the body_vec COLUMN directly — don't infer its presence from a
    // `WHERE body_vec IS NOT NULL` count. heliosdb-nano 3.36.1's planner
    // evaluates that predicate as always-true when the column is absent
    // (returning every row) instead of erroring, so the count is not a
    // reliable signal. `SELECT body_vec` *does* error when the column is
    // missing, which is the authoritative check.
    match db.query("SELECT body_vec FROM _hdb_code_symbols LIMIT 1", &[]) {
        Err(e) => {
            // Column absent ⇒ no embedder ran. Correct for the fast tier.
            let msg = e.to_string();
            assert!(
                msg.contains("body_vec") && msg.contains("not found"),
                "unexpected error probing body_vec on fast tier: {msg}"
            );
        }
        Ok(_) => {
            // Column exists ⇒ every value must still be NULL (no embedder
            // populated it in the fast tier).
            let n = count(
                &db,
                "SELECT count(*) FROM _hdb_code_symbols WHERE body_vec IS NOT NULL",
            );
            assert_eq!(n, 0, "fast tier should leave body_vec NULL, found {n}");
        }
    }
}

#[test]
fn second_ingest_does_not_duplicate_rows() {
    // Cross-process correctness contract: the second ingest opens
    // a fresh `EmbeddedDatabase` on the same on-disk KB.  Without
    // engine FR `cross_process_on_conflict` (`6ec74d3`), `src`
    // would double.  With it, counts hold steady.
    let f = Fixture::new("idempotent");
    f.write_corpus();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
    ]);
    f.run(&["ingest", "--source", f.source.to_str().unwrap()]);

    let db = EmbeddedDatabase::new(&f.kb).unwrap();
    assert_eq!(
        count(&db, "SELECT count(*) FROM src"),
        2,
        "second ingest must not duplicate src rows (FR cross_process_on_conflict)"
    );
}

#[test]
fn with_embeddings_populates_body_vec() {
    // Quality tier — synchronous embedding pass via in-process
    // FastEmbedder.  Slow on first run (model download), fast on
    // subsequent (cache).
    let f = Fixture::new("emb");
    f.write_corpus();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
        "--with-embeddings",
    ]);

    let db = EmbeddedDatabase::new(&f.kb).unwrap();
    let symbols = count(&db, "SELECT count(*) FROM _hdb_code_symbols");
    let with_vec = count(
        &db,
        "SELECT count(*) FROM _hdb_code_symbols WHERE body_vec IS NOT NULL",
    );
    assert_eq!(
        with_vec, symbols,
        "every symbol must have body_vec populated under --with-embeddings"
    );
}

#[test]
fn background_quality_writes_progress_json_and_finalises() {
    // Parent runs fast pass synchronously; spawns detached child
    // for the embedding pass; writes `<kb>/quality-progress.json`
    // with `pid` and `started_at_secs`.  Child finalises with
    // `completed_at_secs` on success.
    let f = Fixture::new("bgq");
    f.write_corpus();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
        "--background-quality",
    ]);

    let progress_path = f.kb.join("quality-progress.json");
    assert!(
        progress_path.exists(),
        "expected {} after --background-quality",
        progress_path.display()
    );

    // Wait up to 30 s for the child to finalise (small corpus +
    // cached FastEmbedder normally finishes in a couple of seconds;
    // give head-room for the model-cache cold path).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut completed = false;
    while std::time::Instant::now() < deadline {
        let body = std::fs::read_to_string(&progress_path).expect("read progress");
        if body.contains("\"completed_at_secs\":") && !body.contains("\"completed_at_secs\": null")
        {
            completed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(
        completed,
        "background quality child did not finalise within 30 s"
    );

    // Child should have populated body_vec.
    let db = EmbeddedDatabase::new(&f.kb).unwrap();
    let symbols = count(&db, "SELECT count(*) FROM _hdb_code_symbols");
    let with_vec = count(
        &db,
        "SELECT count(*) FROM _hdb_code_symbols WHERE body_vec IS NOT NULL",
    );
    assert_eq!(
        with_vec, symbols,
        "body_vec must be populated after background quality completes"
    );
    // src must not have grown (FR cross_process_on_conflict — no
    // duplication even though the child opens the KB in a fresh
    // process and the workaround skip is now perf-only).
    assert_eq!(
        count(&db, "SELECT count(*) FROM src"),
        2,
        "src must not grow when child re-opens the KB"
    );
}

#[test]
fn checkpoint_cleared_on_successful_ingest() {
    let f = Fixture::new("ckpt-clear");
    f.write_corpus();
    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
    ]);
    let path = f.kb.join(".ingest-state.json");
    assert!(!path.exists(), "checkpoint must be cleared on success");
}

#[test]
fn checkpoint_resume_skips_walk_when_phase_is_code_index() {
    // Drop a synthetic checkpoint at phase=code_index, then run
    // `ingest`. Plugin must skip the walk and still complete
    // cleanly (engine's content-hash gate makes the code-graph
    // pass a no-op).
    let f = Fixture::new("ckpt-resume");
    f.write_corpus();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
    ]);
    let ckpt = f.kb.join(".ingest-state.json");
    let body = format!(
        r#"{{"source_root":"{}","started_at_secs":1,"phase":"code_index"}}"#,
        f.source.display(),
    );
    std::fs::write(&ckpt, body).unwrap();

    let out = f.run(&["ingest", "--source", f.source.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("walk skipped (resume)"),
        "expected 'walk skipped (resume)' in stderr, got:\n{stderr}"
    );
    // Resume cleared the checkpoint on completion.
    assert!(
        !ckpt.exists(),
        "checkpoint must be cleared after a successful resume"
    );
}

#[test]
fn force_reparse_does_not_grow_symbols() {
    // `--force` re-parses every file.  Engine's
    // delete-by-file_id path inside code_index keeps symbol
    // counts stable across runs.
    let f = Fixture::new("force");
    f.write_corpus();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
    ]);
    let db = EmbeddedDatabase::new(&f.kb).unwrap();
    let baseline = count(&db, "SELECT count(*) FROM _hdb_code_symbols");
    drop(db);

    f.run(&["ingest", "--source", f.source.to_str().unwrap(), "--force"]);
    let db = EmbeddedDatabase::new(&f.kb).unwrap();
    let after_force = count(&db, "SELECT count(*) FROM _hdb_code_symbols");
    assert_eq!(
        after_force, baseline,
        "force-reparse must not duplicate symbols (delete-by-file_id stable)"
    );
}

#[test]
fn markdown_uses_heading_chunking_and_links_to_code() {
    // The retrieval-quality contract that makes the plugin
    // competitive with PageIndex (for doc navigation) and tree-sitter
    // alone (for code navigation):
    //
    //   1. `.md` files land in `docs_md` and are projected via
    //      `ChunkStrategy::Headings`, producing `DocSection` and
    //      `DocChunk` graph nodes connected by `PART_OF` edges.
    //   2. The entity linker emits `MENTIONS` edges from doc text
    //      nodes to `_hdb_code_symbols` whose `qualified` name
    //      appears verbatim in the body — so the README's reference
    //      to `add` traverses straight into the Rust function.
    //
    // See ROADMAP.md "Retrieval-quality audit (2026-05-18)" items A + B.
    let f = Fixture::new("md-headings-linker");
    f.write_corpus_with_md();

    f.run(&[
        "init",
        "--source",
        f.source.to_str().unwrap(),
        "--mode",
        "hybrid",
        "--kb",
        f.kb.to_str().unwrap(),
        "--ingest",
    ]);

    let db = EmbeddedDatabase::new(&f.kb).unwrap();

    // docs_md table populated.
    assert_eq!(
        count(&db, "SELECT count(*) FROM docs_md"),
        1,
        "docs_md should hold the README.md row"
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM docs"),
        0,
        "row-mode docs table should be empty when corpus has only markdown",
    );

    // Heading projection: at least one DocSection (for `# Project`,
    // `## Math helpers`, `## Greetings`) and at least one DocChunk
    // (per-heading body). Exact counts depend on the engine's
    // splitter; assert ≥1 to stay robust to splitter tweaks.
    let sections = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_nodes WHERE node_kind = 'DocSection'",
    );
    assert!(
        sections >= 1,
        "expected at least one DocSection node, got {sections}"
    );
    let chunks = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_nodes WHERE node_kind = 'DocChunk'",
    );
    assert!(
        chunks >= 1,
        "expected at least one DocChunk node, got {chunks}"
    );
    let part_of = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'PART_OF'",
    );
    assert!(
        part_of >= 1,
        "expected at least one PART_OF edge (DocChunk → DocSection), got {part_of}"
    );

    // Entity linker: README mentions `add`, which is a Rust symbol in
    // `a.rs`. The exact-qualified linker should emit at least one
    // MENTIONS edge.
    let mentions = count(
        &db,
        "SELECT count(*) FROM _hdb_graph_edges WHERE edge_kind = 'MENTIONS'",
    );
    assert!(
        mentions >= 1,
        "expected at least one MENTIONS edge from README to code symbol, got {mentions}"
    );
}
