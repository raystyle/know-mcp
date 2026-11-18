//! Incremental sync: per-file sync state (mtime/size/content-hash/chunk-count)
//! with mtime+size fast path, content-hash second level, deterministic chunk
//! ids and delete-then-upsert replacement. Orchestration wires walk → read →
//! chunk → embed → store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunker::{chunk_text, ChunkParams};
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

#[derive(Debug, Clone, Serialize)]
pub struct IndexOutcome {
    pub scanned: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub failed: usize,
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
    meta.fingerprint == fingerprint_of(config) && meta.schema_version == 1
}

pub fn write_meta(index_dir: &Path, config: &Config) -> Result<()> {
    let meta = IndexMeta { fingerprint: fingerprint_of(config), schema_version: 1 };
    atomic_write(&meta_path(index_dir), serde_json::to_string_pretty(&meta)?.as_bytes())
}

/// Full incremental index pass. Blocking; callers run it on a worker thread.
/// `progress` receives live counters; `force` re-embeds every file.
pub fn run_incremental_index(
    config: &Config,
    store: &dyn SearchStore,
    embedder: &mut QwenEmbedder,
    force: bool,
    progress: Option<&dyn Fn(IndexProgress)>,
) -> Result<IndexOutcome> {
    let index_dir = &config.storage.index_dir;
    let mut state = SyncState::load(&sync_state_path(index_dir))?;
    let files = collect_files(config)?;
    let chunk_params = ChunkParams::new(config.chunk.chars, config.chunk.overlap);

    let mut outcome = IndexOutcome { scanned: 0, updated: 0, unchanged: 0, deleted: 0, failed: 0 };
    let mut report = |progress: Option<&dyn Fn(IndexProgress)>, scanned: usize, outcome: &IndexOutcome| {
        if let Some(cb) = progress {
            cb(IndexProgress {
                running: true,
                scanned,
                updated: outcome.updated,
                deleted: outcome.deleted,
                failed: outcome.failed,
                total: files.len(),
                started_at: None,
            });
        }
    };

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
            let pks: Vec<String> =
                (0..fs.chunk_count).map(|i| format!("{loc}-{i}")).collect();
            if store.delete(&pks).is_ok() {
                outcome.deleted += 1;
            }
        }
    }

    // 2. Upsert new/changed files.
    for found in &files {
        outcome.scanned += 1;
        if outcome.scanned % 10 == 0 {
            tracing::info!(
                scanned = outcome.scanned,
                total = files.len(),
                updated = outcome.updated,
                "indexing progress"
            );
            report(progress, outcome.scanned, &outcome);
        }
        let canonical = dunce::canonicalize(&found.path).unwrap_or_else(|_| found.path.clone());
        let Some(canon_str) = canonical.to_str() else {
            outcome.failed += 1;
            continue;
        };
        let Ok(meta) = std::fs::metadata(&found.path) else {
            outcome.failed += 1;
            continue;
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let prior = state.files.get(canon_str);
        // Fast path: mtime + size unchanged.
        if !force {
            if let Some(p) = prior {
                if p.mtime_ms == mtime_ms && p.size == meta.len() {
                    outcome.unchanged += 1;
                    continue;
                }
            }
        }

        let Ok(raw) = std::fs::read(&found.path) else {
            outcome.failed += 1;
            continue;
        };
        let text = match String::from_utf8(raw) {
            Ok(t) => t,
            Err(_) => {
                // Skip non-UTF8 files silently (binaries with known ext).
                outcome.failed += 1;
                continue;
            }
        };
        let content_hash = {
            let mut h = Sha256::new();
            h.update(text.as_bytes());
            format!("{:x}", h.finalize())
        };
        // Second level: mtime changed but content identical → refresh cache only.
        if let Some(p) = prior {
            if p.content_hash == content_hash {
                state.files.insert(
                    canon_str.to_string(),
                    FileSyncState {
                        mtime_ms,
                        size: meta.len(),
                        content_hash,
                        chunk_count: p.chunk_count,
                    },
                );
                outcome.unchanged += 1;
                continue;
            }
        }

        let chunks = chunk_text(&text, chunk_params);
        if chunks.is_empty() {
            // File became empty: drop old chunks if any.
            if let Some(p) = prior {
                let loc = locator_of(canon_str);
                let pks: Vec<String> =
                    (0..p.chunk_count).map(|i| format!("{loc}-{i}")).collect();
                let _ = store.delete(&pks);
            }
            state.files.remove(canon_str);
            outcome.updated += 1;
            continue;
        }

        let embeddings = match embedder.embed_documents(
            &chunks.iter().map(|c| c.text.clone()).collect::<Vec<_>>(),
            config.embed.max_batch_tokens,
        ) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(path = canon_str, error = %err, "embedding failed");
                outcome.failed += 1;
                continue;
            }
        };

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

        // Replacement semantics: delete surplus chunks when the file shrank.
        if let Some(p) = prior {
            if p.chunk_count > chunks.len() as u32 {
                let surplus: Vec<String> = (chunks.len() as u32..p.chunk_count)
                    .map(|i| format!("{loc}-{i}"))
                    .collect();
                let _ = store.delete(&surplus);
            }
        }

        let mut docs = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            docs.push(ChunkDoc {
                pk: format!("{loc}-{i}"),
                kind: found.kind,
                path: canon_str.to_string(),
                root: found.root.to_string_lossy().to_string(),
                rel_path: rel_path.clone(),
                title: title.clone(),
                text: chunk.text.clone(),
                chunk_index: i as u32,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                lang: lang.clone(),
                note_id: note_id_of(&title, found.kind),
                embedding: embeddings[i].clone(),
            });
        }
        if let Err(err) = store.upsert(&docs) {
            tracing::warn!(path = canon_str, error = %err, "upsert failed");
            outcome.failed += 1;
            continue;
        }
        state.files.insert(
            canon_str.to_string(),
            FileSyncState {
                mtime_ms,
                size: meta.len(),
                content_hash,
                chunk_count: chunks.len() as u32,
            },
        );
        outcome.updated += 1;
    }

    report(progress, outcome.scanned, &outcome);
    state.save(&sync_state_path(index_dir))?;
    write_meta(index_dir, config)?;
    Ok(outcome)
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

/// Note files carry a note_id derived from their file stem (KB-*.md).
fn note_id_of(title: &str, kind: crate::Kind) -> Option<String> {
    match kind {
        crate::Kind::Note => Some(title.to_string()),
        crate::Kind::Archive => None,
    }
}
