//! zvec-backed SearchStore: one collection holds archive chunks and notes,
//! HNSW vector field + jieba FTS field, engine-side RRF hybrid fusion.

use std::path::Path;

use anyhow::{Context, Result};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, ConfigBuilder, DataType, Doc, FieldSchema, Fts,
    FtsQueryParams, IndexParams, MetricType, MultiQuery, SearchQuery, SubQuery,
};

use crate::store::{ChunkDoc, Hit, SearchOptions, SearchStore};

/// Embedding dimension (Qwen3-Embedding-0.6B).
const DIM: u32 = 1024;
const COLLECTION_NAME: &str = "rcd";
const OUTPUT_FIELDS: &[&str] = &[
    "kind", "path", "root", "rel_path", "title", "chunk_index", "start_line", "end_line",
    "lang", "note_id",
];

pub struct ZvecStore {
    collection: Collection,
}

fn ensure_initialized() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Idempotent per zvec docs; a second init returns "already" which we ignore.
        let _ = zvec_rust::initialize(None);
        let _ = ConfigBuilder::new().enable_console_log(false);
    });
}

impl ZvecStore {
    /// Open the collection at `index_dir/collection`, creating it when absent.
    pub fn open(index_dir: &Path) -> Result<Self> {
        ensure_initialized();
        std::fs::create_dir_all(index_dir)
            .with_context(|| format!("creating index dir {}", index_dir.display()))?;
        let path = index_dir.join("collection");
        let path_str = path.to_str().context("index path must be UTF-8")?;

        if path.exists() {
            let collection = Collection::open(path_str, None)
                .with_context(|| format!("opening collection {}", path_str))?;
            return Ok(Self { collection });
        }

        let schema = build_schema()?;
        let collection = Collection::create_and_open(path_str, &schema, None)
            .with_context(|| format!("creating collection {}", path_str))?;
        Ok(Self { collection })
    }

    pub fn read_only_options() -> Result<CollectionOptions> {
        let mut opts = CollectionOptions::new()?;
        opts.set_read_only(true)?;
        Ok(opts)
    }
}

fn build_schema() -> Result<CollectionSchema> {
    Ok(CollectionSchema::builder(COLLECTION_NAME)
        .add_indexed_field("kind", DataType::String,
            IndexParams::invert(false, false)?)
        .add_indexed_field("path", DataType::String,
            IndexParams::invert(false, false)?)
        .add_field(FieldSchema::new("root", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("rel_path", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("title", DataType::String, false, 0)?)
        .add_indexed_field("text", DataType::String,
            IndexParams::fts(Some("jieba"), None, None)?)
        .add_field(FieldSchema::new("chunk_index", DataType::Int64, false, 0)?)
        .add_field(FieldSchema::new("start_line", DataType::Int64, false, 0)?)
        .add_field(FieldSchema::new("end_line", DataType::Int64, false, 0)?)
        .add_field(FieldSchema::new("lang", DataType::String, false, 0)?)
        .add_field(FieldSchema::new("note_id", DataType::String, true, 0)?)
        .add_vector_field("embedding", DataType::VectorFp32, DIM,
            IndexParams::hnsw(MetricType::Cosine, 16, 200)?)
        .build()?)
}

fn doc_to_hit(doc: &Doc, fallback_score: f32) -> Hit {
    let text = doc.get_string("text").ok().flatten().unwrap_or_default();
    Hit {
        kind: match doc.get_string("kind").ok().flatten().as_deref() {
            Some("note") => crate::Kind::Note,
            _ => crate::Kind::Archive,
        },
        path: doc.get_string("path").ok().flatten().unwrap_or_default(),
        rel_path: doc.get_string("rel_path").ok().flatten().unwrap_or_default(),
        title: doc.get_string("title").ok().flatten().unwrap_or_default(),
        score: {
            let s = doc.get_score();
            if s.is_finite() && s != 0.0 {
                s
            } else {
                fallback_score
            }
        },
        start_line: doc.get_i64("start_line").ok().flatten().unwrap_or(0) as u64,
        end_line: doc.get_i64("end_line").ok().flatten().unwrap_or(0) as u64,
        chunk_index: doc.get_i64("chunk_index").ok().flatten().unwrap_or(0) as u32,
        snippet: truncate_chars(&text, 500),
        note_id: doc.get_string("note_id").ok().flatten(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn apply_common(query: &mut SearchQuery, opts: &SearchOptions) -> Result<()> {
    if let Some(kind) = opts.kind_filter {
        query.set_filter(&format!("kind = '{}'", kind.as_str()))?;
    }
    query.set_include_vector(false)?;
    query.set_output_fields(OUTPUT_FIELDS)?;
    Ok(())
}

impl SearchStore for ZvecStore {
    fn upsert(&self, docs: &[ChunkDoc]) -> Result<()> {
        for batch in docs.chunks(32) {
            let mut zdocs = Vec::with_capacity(batch.len());
            for chunk in batch {
                let mut doc = Doc::new()?;
                doc.set_pk(&chunk.pk);
                doc.add_string("kind", chunk.kind.as_str())?;
                doc.add_string("path", &chunk.path)?;
                doc.add_string("root", &chunk.root)?;
                doc.add_string("rel_path", &chunk.rel_path)?;
                doc.add_string("title", &chunk.title)?;
                doc.add_string("text", &chunk.text)?;
                doc.add_i64("chunk_index", chunk.chunk_index as i64)?;
                doc.add_i64("start_line", chunk.start_line as i64)?;
                doc.add_i64("end_line", chunk.end_line as i64)?;
                doc.add_string("lang", &chunk.lang)?;
                if let Some(note_id) = &chunk.note_id {
                    doc.add_string("note_id", note_id)?;
                } else {
                    doc.set_field_null("note_id")?;
                }
                doc.add_vector_f32("embedding", &chunk.embedding)?;
                zdocs.push(doc);
            }
            let refs: Vec<&Doc> = zdocs.iter().collect();
            self.collection
                .upsert(&refs)
                .with_context(|| "zvec upsert batch")?;
            self.collection.flush()?;
        }
        Ok(())
    }

    fn delete(&self, pks: &[String]) -> Result<()> {
        if pks.is_empty() {
            return Ok(());
        }
        let refs: Vec<&str> = pks.iter().map(|s| s.as_str()).collect();
        self.collection
            .delete(&refs)
            .with_context(|| "zvec delete")?;
        self.collection.flush()?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        self.collection.flush().context("zvec flush")
    }

    fn optimize(&self) -> Result<()> {
        self.collection.optimize().context("zvec optimize")
    }

    fn doc_count(&self) -> Result<u64> {
        Ok(self.collection.stats().context("zvec stats")?.doc_count)
    }

    fn search_keyword(&self, query: &str, opts: &SearchOptions) -> Result<Vec<Hit>> {
        let topk = topk_fetch(opts);
        let mut fts = Fts::new()?;
        fts.set_match_string(query)?;
        let mut q = SearchQuery::fts("text", &fts, topk as i32)?;
        q.set_fts_params(FtsQueryParams::new(Some("OR"))?)?;
        apply_common(&mut q, opts)?;
        let docs = self.collection.query(&q)?;
        Ok(docs.iter().map(|d| doc_to_hit(d, 0.0)).collect())
    }

    fn search_vector(&self, embedding: &[f32], opts: &SearchOptions) -> Result<Vec<Hit>> {
        let topk = topk_fetch(opts);
        let mut q = SearchQuery::new("embedding", embedding, topk as i32)?;
        apply_common(&mut q, opts)?;
        let docs = self.collection.query(&q)?;
        Ok(docs.iter().map(|d| doc_to_hit(d, 0.0)).collect())
    }

    fn search_hybrid(&self, query: &str, embedding: &[f32], opts: &SearchOptions) -> Result<Vec<Hit>> {
        let topk = topk_fetch(opts);
        let mut sub_vec = SubQuery::new()?;
        sub_vec.set_field_name("embedding")?;
        sub_vec.set_query_vector(embedding)?;
        sub_vec.set_num_candidates(topk as i32)?;

        let mut fts = Fts::new()?;
        fts.set_match_string(query)?;
        let mut sub_fts = SubQuery::new()?;
        sub_fts.set_field_name("text")?;
        sub_fts.set_fts(&fts)?;
        sub_fts.set_num_candidates(topk as i32)?;

        let mut mq = MultiQuery::new()?;
        mq.set_topk(topk as i32)?;
        mq.set_rerank_rrf(60)?;
        if let Some(kind) = opts.kind_filter {
            mq.set_filter(&format!("kind = '{}'", kind.as_str()))?;
        }
        mq.add_sub_query(&sub_vec)?;
        mq.add_sub_query(&sub_fts)?;

        let docs = self.collection.multi_query(&mq)?;
        Ok(docs.iter().map(|d| doc_to_hit(d, 0.0)).collect())
    }
}

/// Over-fetch for Rust-side post-filtering (path prefix), capped at 50.
fn topk_fetch(opts: &SearchOptions) -> usize {
    (opts.top_k.max(1) * 5).min(50)
}
