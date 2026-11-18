//! SearchStore trait: the storage seam isolating zvec from the rest of the
//! system, so a tantivy+usearch implementation could replace it later.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::Kind;

/// A chunk to be indexed.
#[derive(Debug, Clone)]
pub struct ChunkDoc {
    /// Stable primary key: "{locator}-{chunk_index}".
    pub pk: String,
    pub kind: Kind,
    /// Absolute path (canonical, for display and path filtering).
    pub path: String,
    /// Configured root this file lives under.
    pub root: String,
    /// Path relative to its root.
    pub rel_path: String,
    pub title: String,
    pub text: String,
    pub chunk_index: u32,
    pub start_line: u64,
    pub end_line: u64,
    pub lang: String,
    /// Present only for notes: KB-id for feedback-score joins.
    pub note_id: Option<String>,
    pub embedding: Vec<f32>,
}

/// A search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub kind: Kind,
    pub path: String,
    pub rel_path: String,
    pub title: String,
    pub score: f32,
    pub start_line: u64,
    pub end_line: u64,
    pub chunk_index: u32,
    pub snippet: String,
    pub note_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Keyword,
    Vector,
    Hybrid,
}

impl SearchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Keyword => "keyword",
            SearchMode::Vector => "vector",
            SearchMode::Hybrid => "hybrid",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub top_k: usize,
    /// Filter expression on the `kind` field, e.g. `kind = 'note'`.
    pub kind_filter: Option<Kind>,
}

/// Storage abstraction over the hybrid index.
pub trait SearchStore: Send + Sync {
    /// Batch upsert chunks; deletes are handled by the caller via `delete`.
    fn upsert(&self, docs: &[ChunkDoc]) -> Result<()>;
    /// Delete chunks by primary key.
    fn delete(&self, pks: &[String]) -> Result<()>;
    /// Persist to disk.
    fn flush(&self) -> Result<()>;
    /// Rebuild indexes/merge segments (opt-in only; known to trip on some volumes).
    fn optimize(&self) -> Result<()>;
    /// Number of documents in the collection.
    fn doc_count(&self) -> Result<u64>;

    /// Keyword-only FTS search.
    fn search_keyword(&self, query: &str, opts: &SearchOptions) -> Result<Vec<Hit>>;
    /// Dense vector search.
    fn search_vector(&self, query_embedding: &[f32], opts: &SearchOptions) -> Result<Vec<Hit>>;
    /// Hybrid search fused by the engine (RRF).
    fn search_hybrid(
        &self,
        query: &str,
        query_embedding: &[f32],
        opts: &SearchOptions,
    ) -> Result<Vec<Hit>>;
}
