//! Incremental sync: per-file sync state (mtime/size/content-hash/chunk-count)
//! with mtime+size fast path, content-hash second level, deterministic chunk
//! ids and delete-then-upsert replacement. Orchestration wires walk → read →
//! chunk → embed → store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunker::{chunk_text_code, heading_breadcrumb, ChunkParams};
use crate::config::Config;
use crate::embed::QwenEmbedder;
use crate::store::{ChunkDoc, SearchStore};
use crate::walk::collect_files;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileSyncState {
    pub mtime_ms: i64,
    pub size: u64,
    pub content_hash: String,
    pub chunk_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncState {
    /// Canonical path → state.
    pub files: HashMap<String, FileSyncState>,
}

impl SyncState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Stable locator: first 32 hex of sha256 over the canonical path.
pub fn locator_of(canonical_path: &str) -> String {
    let mut h = Sha256::new();
    h.update(canonical_path.as_bytes());
    let digest = format!("{:x}", h.finalize());
    digest[..32].to_string()
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexProgress {
    pub running: bool,
    pub scanned: usize,
    pub updated: usize,
    pub deleted: usize,
    pub failed: usize,
    pub total: usize,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// One file's stage-1 output, queued for batch embedding.
#[derive(Debug, Clone)]
struct PendingFile {
    loc: String,
    canon: String,
    root: String,
    rel_path: String,
    title: String,
    lang: String,
    kind: crate::Kind,
    mtime_ms: i64,
    size: u64,
    content_hash: String,
    chunks: Vec<crate::chunker::Chunk>,
    /// Final embed inputs (context + body), assembled by the producer so
    /// the embed thread only decodes: context-empty → bare text, else
    /// "context\ntext".
    embed_texts: Vec<String>,
    /// Chunk count of the previous version, for the shrink-surplus delete
    /// (applied by the store stage before the new upsert).
    prior_chunk_count: Option<u32>,
}

/// Atomic per-stage counters shared across the pipeline threads.
#[derive(Default)]
struct IndexCounters {
    scanned: AtomicUsize,
    updated: AtomicUsize,
    unchanged: AtomicUsize,
    deleted: AtomicUsize,
    failed: AtomicUsize,
    skipped_binary: AtomicUsize,
    /// Total chunks handed to the store this pass (drives the end-of-pass
    /// optimize decision).
    upserted_chunks: AtomicUsize,
}

impl IndexCounters {
    fn snapshot(&self) -> IndexOutcome {
        IndexOutcome {
            scanned: self.scanned.load(Ordering::Relaxed),
            updated: self.updated.load(Ordering::Relaxed),
            unchanged: self.unchanged.load(Ordering::Relaxed),
            deleted: self.deleted.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped_binary: self.skipped_binary.load(Ordering::Relaxed),
        }
    }
}

/// Stage A → B message: one file's read/chunk outcome.
enum Produce {
    /// Chunks ready for embedding.
    Batch(PendingFile),
    /// mtime/size changed but content identical: refresh the state entry.
    Refresh(String, FileSyncState),
    /// File became empty: drop prior chunks and the state entry.
    Dropped(String, u32),
}

/// Stage B → C message: embedding results to persist.
enum StoreMsg {
    Embedded {
        files: Vec<PendingFile>,
        embeddings: Vec<Vec<f32>>,
        /// Files in this group whose embedding failed (store stage skips them).
        failed: usize,
    },
    Refresh(String, FileSyncState),
    Dropped(String, u32),
}

#[derive(Debug)]
pub struct IndexOutcome {
    pub scanned: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub failed: usize,
    /// Binary-looking files skipped by content sniffing (not failures).
    pub skipped_binary: usize,
}

/// meta.json: embedding fingerprint guard; mismatch → caller rebuilds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub fingerprint: String,
    pub schema_version: u32,
}

pub fn meta_path(index_dir: &Path) -> PathBuf {
    index_dir.join("meta.json")
}

pub fn sync_state_path(index_dir: &Path) -> PathBuf {
    index_dir.join("sync_state.json")
}

pub fn fingerprint_of(config: &Config) -> String {
    crate::embed::embedding_fingerprint(
        &config.embed.model,
        crate::embed::QWEN_DIM,
        config.chunk.chars,
        config.chunk.overlap,
    )
}

/// Check meta.json: returns true when the index is compatible and can be
/// reused; false when it must be rebuilt (fingerprint mismatch).
pub fn meta_compatible(index_dir: &Path, config: &Config) -> bool {
    let Ok(raw) = std::fs::read_to_string(meta_path(index_dir)) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<IndexMeta>(&raw) else {
        return false;
    };
    meta.fingerprint == fingerprint_of(config) && meta.schema_version == 2
}

pub fn write_meta(index_dir: &Path, config: &Config) -> Result<()> {
    let meta = IndexMeta {
        fingerprint: fingerprint_of(config),
        schema_version: 2,
    };
    atomic_write(
        &meta_path(index_dir),
        serde_json::to_string_pretty(&meta)?.as_bytes(),
    )
}

/// Full incremental index pass. Blocking; callers run it on a worker thread.
/// `progress` receives live counters; `force` re-embeds every file.
pub fn run_incremental_index(
    config: &Config,
    store: &dyn SearchStore,
    embedder: &mut QwenEmbedder,
    force: bool,
    progress: Option<&(dyn Fn(IndexProgress) + Send + Sync)>,
) -> Result<IndexOutcome> {
    let index_dir = &config.storage.index_dir;
    let mut state = SyncState::load(&sync_state_path(index_dir))?;
    let files = collect_files(config)?;
    // Token-budget chunking (CJK-adaptive). Legacy char config maps to a
    // token budget: chars/4 ≈ tokens for the ASCII-ish legacy default.
    let chunk_params = ChunkParams::new(config.chunk.chars / 4, 0.12);

    let counters = IndexCounters::default();

    // 1. Handle deletions: files previously indexed but no longer present.
    let present: std::collections::HashSet<String> = files
        .iter()
        .map(|f| dunce::canonicalize(&f.path).unwrap_or_else(|_| f.path.clone()))
        .filter_map(|p| p.to_str().map(|s| s.to_string()))
        .collect();
    let stale: Vec<String> = state
        .files
        .keys()
        .filter(|k| !present.contains(*k))
        .cloned()
        .collect();
    for key in stale {
        if let Some(fs) = state.files.remove(&key) {
            let loc = locator_of(&key);
            let pks: Vec<String> = (0..fs.chunk_count).map(|i| format!("{loc}-{i}")).collect();
            if store.delete(&pks).is_ok() {
                counters.deleted.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // 2. Upsert new/changed files — three-stage pipeline.
    //
    // Stage A (N producer threads): read/validate/chunk — pure CPU+IO, the
    // measured bottleneck once GPU embedding was fixed (1.75 cores busy,
    // GPU 0-29% bursts). Stage B (dedicated thread): GPU batch embedding,
    // the only stage that needs &mut embedder. Stage C (1 store thread):
    // all store mutations in arrival order, so deletes/upserts stay
    // consistent. Bounded channels give backpressure: A stalls if B can't
    // keep up, B stalls if C can't. Scoped threads + Send/Sync bounds make
    // the sharing safe without unsafe.
    const EMBED_BATCH: usize = 64;
    let state_snapshot = state.clone();
    let next = AtomicUsize::new(0);
    let drain_done = std::sync::atomic::AtomicBool::new(false);

    let (tx_a, rx_a) = sync_channel::<Produce>(128);
    let (tx_b, rx_b) = sync_channel::<StoreMsg>(4);

    let total = files.len();
    let report = |progress: Option<&(dyn Fn(IndexProgress) + Send + Sync)>,
                  counters: &IndexCounters| {
        if let Some(cb) = progress {
            cb(IndexProgress {
                running: true,
                scanned: counters.scanned.load(Ordering::Relaxed),
                updated: counters.updated.load(Ordering::Relaxed),
                deleted: counters.deleted.load(Ordering::Relaxed),
                failed: counters.failed.load(Ordering::Relaxed),
                total,
                started_at: None,
            });
        }
    };

    std::thread::scope(|scope| -> Result<()> {
        // ---- Stage C: store mutations, single consumer (ordered) ----
        let store_handle = {
            let store = &*store;
            let state = &mut state;
            let counters = &counters;
            let rx = rx_b;
            scope.spawn(move || {
                let mut upsert_buf: Vec<ChunkDoc> = Vec::with_capacity(2048);
                let mut pending_since_flush: usize = 0;
                while let Ok(msg) = rx.recv() {
                    match msg {
                        StoreMsg::Embedded {
                            files,
                            embeddings,
                            failed,
                        } => {
                            counters.failed.fetch_add(failed, Ordering::Relaxed);
                            let mut off = 0usize;
                            for p in files {
                                let n = p.chunks.len();
                                // Shrink-surplus delete must land before the
                                // fresh upsert of the same pk range.
                                if let Some(prior_n) = p.prior_chunk_count {
                                    if prior_n > n as u32 {
                                        let surplus: Vec<String> = (n as u32..prior_n)
                                            .map(|i| format!("{}-{i}", p.loc))
                                            .collect();
                                        let _ = store.delete(&surplus);
                                    }
                                }
                                upsert_buf.reserve(n);
                                for (i, chunk) in p.chunks.iter().enumerate() {
                                    upsert_buf.push(ChunkDoc {
                                        pk: format!("{}-{i}", p.loc),
                                        kind: p.kind,
                                        path: p.canon.clone(),
                                        root: p.root.clone(),
                                        rel_path: p.rel_path.clone(),
                                        title: p.title.clone(),
                                        text: chunk.text.clone(),
                                        chunk_index: i as u32,
                                        start_line: chunk.start_line,
                                        end_line: chunk.end_line,
                                        lang: p.lang.clone(),
                                        embedding: embeddings[off + i].clone(),
                                    });
                                }
                                off += n;
                                state.files.insert(
                                    p.canon.clone(),
                                    FileSyncState {
                                        mtime_ms: p.mtime_ms,
                                        size: p.size,
                                        content_hash: p.content_hash.clone(),
                                        chunk_count: n as u32,
                                    },
                                );
                                counters.updated.fetch_add(1, Ordering::Relaxed);
                                // Large upserts, rare flushes: zvec's flush()
                                // seals the vector-index block (alibaba/zvec
                                // #869), so flushing per batch fragments the
                                // index. Durability comes from the final
                                // flush + sync_state; pk upserts are
                                // idempotent after any mid-pass crash.
                                if upsert_buf.len() >= 2048 {
                                    if let Err(err) = store.upsert(&upsert_buf) {
                                        tracing::warn!(error = %err, "upsert failed");
                                        counters.failed.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        counters
                                            .upserted_chunks
                                            .fetch_add(upsert_buf.len(), Ordering::Relaxed);
                                    }
                                    pending_since_flush += n;
                                    if pending_since_flush >= 16384 {
                                        let _ = store.flush();
                                        pending_since_flush = 0;
                                    }
                                    upsert_buf.clear();
                                }
                            }
                        }
                        StoreMsg::Refresh(canon, fs) => {
                            state.files.insert(canon, fs);
                            counters.unchanged.fetch_add(1, Ordering::Relaxed);
                        }
                        StoreMsg::Dropped(canon, prior_n) => {
                            let loc = locator_of(&canon);
                            let pks: Vec<String> =
                                (0..prior_n).map(|i| format!("{loc}-{i}")).collect();
                            let _ = store.delete(&pks);
                            state.files.remove(&canon);
                            counters.updated.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if !upsert_buf.is_empty() {
                    if let Err(err) = store.upsert(&upsert_buf) {
                        tracing::warn!(error = %err, "final upsert failed");
                    } else {
                        counters
                            .upserted_chunks
                            .fetch_add(upsert_buf.len(), Ordering::Relaxed);
                    }
                }
                let _ = store.flush();
            })
        };

        // ---- Stage B: GPU embedding (owns the embedder via &mut) ----
        {
            let embedder = &mut *embedder;
            let tx_b = tx_b.clone();
            let max_batch_tokens = config.embed.max_batch_tokens;
            scope.spawn(move || {
                let mut batch: Vec<PendingFile> = Vec::new();
                let mut pending_chunks = 0usize;
                while let Ok(msg) = rx_a.recv() {
                    let pf = match msg {
                        Produce::Batch(pf) => pf,
                        Produce::Refresh(c, fs) => {
                            let _ = tx_b.send(StoreMsg::Refresh(c, fs));
                            continue;
                        }
                        Produce::Dropped(c, n) => {
                            let _ = tx_b.send(StoreMsg::Dropped(c, n));
                            continue;
                        }
                    };
                    pending_chunks += pf.chunks.len();
                    batch.push(pf);
                    if pending_chunks >= EMBED_BATCH {
                        flush_embed_batch(&batch, embedder, &tx_b, max_batch_tokens);
                        batch.clear();
                        pending_chunks = 0;
                    }
                }
                if !batch.is_empty() {
                    flush_embed_batch(&batch, embedder, &tx_b, max_batch_tokens);
                }
                // tx_b (clone) dropped here; the original in main drops at
                // scope end → store thread's recv() terminates.
            });
        }

        // ---- Stage A: N producer threads (IO + chunking) ----
        {
            // Read-only snapshot for the mtime/hash fast paths: stage C owns
            // &mut state, so producers can't borrow the live map. Entries
            // written by stage C during the pass are invisible here — fine,
            // each file is produced exactly once per pass.
            let state = &state_snapshot;
            let counters = &counters;
            let chunk_params = &chunk_params;
            let n_workers = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(2))
                .unwrap_or(4);
            for _ in 0..n_workers {
                // Each producer OWNS a sender clone: the channel closes only
                // when the last producer finishes. (Borrowing one shared
                // sender deadlocks shutdown — the original would stay alive
                // in main until after join(), which waits for the pipeline
                // to drain, which waits for the channel to close. Observed
                // 2026-11-19: pass completed, process hung at 100% forever.)
                let tx = tx_a.clone();
                let next = &next;
                let files = &files;
                scope.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() {
                            return;
                        }
                        let found = &files[i];
                        counters.scanned.fetch_add(1, Ordering::Relaxed);
                        if let Err(err) = produce_one(found, state, force, chunk_params, &tx) {
                            tracing::warn!(path = %found.path.display(), error = %err, "produce failed");
                            counters.failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // tx (clone) dropped here as this producer finishes.
                });
            }
        }
        // Shutdown signal chain: dropping the originals lets each stage's
        // recv() return Err once its producers are done → B flushes the
        // final batch and drops its tx_b clone → C drains, final-flushes and
        // exits → join() below returns. Skipping either drop deadlocks.
        drop(tx_a);
        drop(tx_b);

        // Liveness heartbeat while the store thread drains: a silent join()
        // made the 2026-11-19 shutdown deadlock look like work. Every 30s:
        // log + emit progress so a stall surfaces within a minute.
        let monitor = {
            let counters = &counters;
            let done = &drain_done;
            scope.spawn(move || {
                let mut last = std::time::Instant::now();
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    if last.elapsed() >= std::time::Duration::from_secs(30) {
                        last = std::time::Instant::now();
                        tracing::info!(
                            scanned = counters.scanned.load(Ordering::Relaxed),
                            updated = counters.updated.load(Ordering::Relaxed),
                            failed = counters.failed.load(Ordering::Relaxed),
                            "indexing progress (waiting for pipeline drain)"
                        );
                        report(progress, counters);
                    }
                }
            })
        };
        let _ = store_handle.join();
        drain_done.store(true, Ordering::Relaxed);
        let _ = monitor.join();
        Ok(())
    })?;

    let outcome = counters.snapshot();
    report(progress, &counters);
    state.save(&sync_state_path(index_dir))?;
    write_meta(index_dir, config)?;

    // Merge the flat ingestion buffers into the configured HNSW index.
    // zvec stages fresh vectors in brute-force buffers (fast to write, slow
    // to search as they grow); optimize() folds them into the graph in the
    // background — reads and writes continue during the merge. Skipped for
    // small incremental passes (docs: consider at 100k+ unindexed docs; we
    // threshold on a chunk count well below that to keep hybrid search
    // fast after any meaningful pass).
    if counters.upserted_chunks.load(Ordering::Relaxed) >= 10_000 {
        tracing::info!(
            chunks = counters.upserted_chunks.load(Ordering::Relaxed),
            "optimizing vector index"
        );
        if let Err(err) = store.optimize() {
            // Degraded, not failed: data is durable; queries just scan the
            // flat buffers until a later pass optimizes.
            tracing::warn!(error = %err, "zvec optimize failed — search runs on the flat buffer until the next pass");
        }
    }

    Ok(outcome)
}

/// Stage A worker: read/validate/chunk one file, send it downstream.
fn produce_one(
    found: &crate::walk::FoundFile,
    state: &SyncState,
    force: bool,
    chunk_params: &ChunkParams,
    tx: &SyncSender<Produce>,
) -> Result<()> {
    let canonical = dunce::canonicalize(&found.path).unwrap_or_else(|_| found.path.clone());
    let Some(canon_str) = canonical.to_str() else {
        anyhow::bail!("non-UTF8 path");
    };
    let Ok(meta) = std::fs::metadata(&found.path) else {
        anyhow::bail!("metadata failed");
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let prior = state.files.get(canon_str);
    // Fast path: mtime + size unchanged → refresh state only.
    if !force {
        if let Some(p) = prior {
            if p.mtime_ms == mtime_ms && p.size == meta.len() {
                tx.send(Produce::Refresh(
                    canon_str.to_string(),
                    FileSyncState {
                        mtime_ms,
                        size: meta.len(),
                        content_hash: p.content_hash.clone(),
                        chunk_count: p.chunk_count,
                    },
                ))
                .ok();
                return Ok(());
            }
        }
    }

    let Ok(raw) = std::fs::read(&found.path) else {
        anyhow::bail!("read failed");
    };
    // Text detection, layer 3: NUL sniff in the first 8KB — binary files
    // with whitelisted extensions are skipped, not failures.
    let head = &raw[..raw.len().min(8192)];
    if head.contains(&0u8) {
        return Ok(());
    }
    let raw = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        &raw[..]
    };
    let text = match String::from_utf8(raw.to_vec()) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    let content_hash = {
        let mut h = Sha256::new();
        h.update(text.as_bytes());
        format!("{:x}", h.finalize())
    };
    // Second level: mtime changed but content identical → refresh only.
    if let Some(p) = prior {
        if p.content_hash == content_hash {
            tx.send(Produce::Refresh(
                canon_str.to_string(),
                FileSyncState {
                    mtime_ms,
                    size: meta.len(),
                    content_hash,
                    chunk_count: p.chunk_count,
                },
            ))
            .ok();
            return Ok(());
        }
    }

    let ext = found
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let mut chunks = chunk_text_code(&text, ext, chunk_params);
    // Context prepend: markdown gets the heading breadcrumb, code gets
    // file path + language.
    if !chunks.is_empty() {
        let ctx = if lang_is_markdown(&found.path) {
            heading_breadcrumb(&text, chunks[0].start_line)
        } else {
            format!(
                "{} [{}]",
                rel_path_of(&canonical, &found.root),
                ext_of(&found.path)
            )
        };
        let budget = 600 * 4;
        crate::chunker::contextualize(&mut chunks, &ctx, budget);
    }
    if chunks.is_empty() {
        if let Some(p) = prior {
            tx.send(Produce::Dropped(canon_str.to_string(), p.chunk_count))
                .ok();
        }
        return Ok(());
    }

    let loc = locator_of(canon_str);
    let title = title_of(&text, &found.path);
    let lang = found
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let rel_path = canonical
        .strip_prefix(&found.root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| canonical.to_string_lossy().to_string());

    let embed_texts = chunks
        .iter()
        .map(|c| {
            if c.context.is_empty() {
                c.text.clone()
            } else {
                format!("{}\n{}", c.context, c.text)
            }
        })
        .collect();

    tx.send(Produce::Batch(PendingFile {
        loc,
        canon: canon_str.to_string(),
        root: found.root.to_string_lossy().to_string(),
        rel_path,
        title: title.clone(),
        lang,
        kind: found.kind,
        mtime_ms,
        size: meta.len(),
        content_hash,
        chunks,
        embed_texts,
        prior_chunk_count: prior.map(|p| p.chunk_count),
    }))
    .ok();
    Ok(())
}

/// Stage B flush: embed one batch and hand results to the store thread.
fn flush_embed_batch(
    batch: &[PendingFile],
    embedder: &mut QwenEmbedder,
    tx_b: &SyncSender<StoreMsg>,
    max_batch_tokens: usize,
) {
    let mut texts = Vec::with_capacity(batch.iter().map(|p| p.embed_texts.len()).sum::<usize>());
    for p in batch.iter() {
        texts.extend(p.embed_texts.iter().cloned());
    }
    match embedder.embed_documents(&texts, max_batch_tokens) {
        Ok(embeddings) => {
            let _ = tx_b.send(StoreMsg::Embedded {
                files: batch.to_vec(),
                embeddings,
                failed: 0,
            });
        }
        Err(err) => {
            tracing::warn!(error = %err, n_files = batch.len(), "batch embedding failed");
            let _ = tx_b.send(StoreMsg::Embedded {
                files: Vec::new(),
                embeddings: Vec::new(),
                failed: batch.len(),
            });
        }
    }
}

/// Title: first markdown heading, else file name.
fn title_of(text: &str, path: &Path) -> String {
    for line in text.lines().take(30) {
        let trimmed = line.trim_start();
        if let Some(h) = trimmed.strip_prefix("# ") {
            let t = h.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Whether the path looks like a markdown file.
fn lang_is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
        .unwrap_or(false)
}

/// Path relative to the root, forward-slash form.
fn rel_path_of(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// File extension lowercased.
fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}
