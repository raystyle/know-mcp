//! Embedding pipeline: Qwen3-Embedding-0.6B GGUF (Q8_0) via llama-cpp-2
//! (llama.cpp native kernels). Pooling type LAST (Qwen3-Embedding recipe) +
//! L2 normalization. Query side uses the instruct template; document side
//! embeds raw.

use std::path::Path;

use anyhow::{Context, Result, bail};
use llama_cpp_2::context::params::LlamaPoolingType;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use sha2::{Digest, Sha256};

pub const QWEN_DIM: usize = 1024;
pub const GGUF_EXPECTED_BYTES: u64 = 639_150_592;
/// Context window per embedding sequence (chunks are ~1000 chars, well under).
const N_CTX: u32 = 1024;

/// sha256(model + dim + chunk params) fingerprint: mismatch triggers rebuild.
pub fn embedding_fingerprint(model: &str, dim: usize, chunk_chars: usize, chunk_overlap: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update(dim.to_le_bytes());
    hasher.update(chunk_chars.to_le_bytes());
    hasher.update(chunk_overlap.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// Validate the GGUF model file before loading.
pub fn validate_gguf(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("embedding model file missing: {}", path.display()))?;
    if meta.len() != GGUF_EXPECTED_BYTES {
        bail!(
            "embedding model file size mismatch at {}: {} bytes (expected {GGUF_EXPECTED_BYTES}). \
             Download Qwen3-Embedding-0.6B-Q8_0.gguf from \
             https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF (rev 370f27d) and place it there.",
            path.display(),
            meta.len()
        );
    }
    Ok(())
}

/// Instruct template for the query side (Qwen3-Embedding convention).
pub fn format_query(query: &str) -> String {
    format!(
        "Instruct: Given a web search query, retrieve relevant passages that answer the query\nQuery: {query}"
    )
}

pub enum EmbedPurpose {
    Query,
    Document,
}

/// The Qwen3-Embedding model over llama.cpp. Loaded once per process;
/// callers keep it behind a `Mutex`.
pub struct QwenEmbedder {
    model: LlamaModel,
    n_threads: i32,
}

unsafe impl Send for QwenEmbedder {}
unsafe impl Sync for QwenEmbedder {}

/// Crate-wide llama backend singleton (llama.cpp global init is once-only).
static BACKEND: std::sync::OnceLock<LlamaBackend> = std::sync::OnceLock::new();

fn backend() -> &'static LlamaBackend {
    BACKEND.get_or_init(|| LlamaBackend::init().expect("llama backend init"))
}

impl QwenEmbedder {
    /// Whether a CUDA GPU backend is available. A CUDA-compiled binary on a
    /// machine without NVIDIA drivers simply sees no CUDA device here and
    /// stays on CPU — no configuration needed for either side.
    pub fn gpu_available() -> bool {
        llama_cpp_2::list_llama_ggml_backend_devices()
            .iter()
            .any(|d| d.backend.eq_ignore_ascii_case("CUDA"))
    }

    pub fn load(gguf_path: &Path, _tokenizer_repo: &str) -> Result<Self> {
        Self::load_with_gpu_layers(gguf_path, None)
    }

    /// Load with an explicit GPU layer override; `None` = auto (all layers
    /// when a CUDA device exists, 0 otherwise).
    pub fn load_with_gpu_layers(gguf_path: &Path, gpu_layers: Option<u32>) -> Result<Self> {
        validate_gguf(gguf_path)?;
        let backend = backend();
        let auto = if Self::gpu_available() { 999 } else { 0 };
        let n_gpu_layers = gpu_layers.unwrap_or(auto);
        let model = LlamaModel::load_from_file(
            backend,
            gguf_path,
            &LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers),
        )
        .with_context(|| format!("loading GGUF weights (llama.cpp, n_gpu_layers={n_gpu_layers})"))?;

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);
        Ok(Self { model, n_threads })
    }

    fn make_context(&self) -> Result<LlamaContext> {
        self.model
            .new_context(
                backend(),
                LlamaContextParams::default()
                    .with_n_ctx(std::num::NonZeroU32::new(N_CTX))
                                                           .with_n_batch(1024)
                    .with_n_ubatch(1024)
                    .with_n_threads(self.n_threads)
                    .with_n_threads_batch(self.n_threads)
                    .with_embeddings(true)
                    .with_pooling_type(LlamaPoolingType::Last),
            )
            .with_context(|| "creating llama context")
    }

    /// Embed one text with the given purpose.
    pub fn embed(&mut self, text: &str, purpose: EmbedPurpose) -> Result<Vec<f32>> {
        let input = match purpose {
            EmbedPurpose::Query => format_query(text),
            EmbedPurpose::Document => text.to_string(),
        };
        let mut ctx = self.make_context()?;
        embed_one(&self.model, &mut ctx, &input, self.n_threads)
    }

    /// Batch-embed documents using llama.cpp multi-sequence batches: up to
    /// SEQ_PER_BATCH sequences share ONE forward pass (each with its own
    /// seq_id and position space), then embeddings_seq_ith(i) reads each
    /// pooled vector. This is the real GPU batching — the previous
    /// one-sequence-per-decode loop left the GPU idle between round trips.
    pub fn embed_documents(
        &mut self,
        texts: &[String],
        _max_batch_tokens: usize,
    ) -> Result<Vec<Vec<f32>>> {
        // Single-sequence pipeline (proven ~15 files/s with the batched
        // sync loop). Multi-sequence batching was tried and rejected:
        // causal attention (required for embedding quality) clamps
        // n_batch to n_ctx and yields no net GPU gain — see git history.
        let mut ctx = self.make_context()?;
        texts
            .iter()
            .map(|t| embed_one(&self.model, &mut ctx, t, self.n_threads))
            .collect()
    }
}

/// One forward pass over several sequences. Positions restart per sequence
/// (seq_id-scoped KV cache); n_ctx bounds each sequence independently.
fn embed_group(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    texts: &[&str],
    _n_threads: i32,
) -> Result<Vec<Vec<f32>>> {
    use std::collections::HashMap;
    let mut token_lists = Vec::with_capacity(texts.len());
    let mut total = 0usize;
    for text in texts {
        let mut tokens = model.vocab().tokenize(text.as_bytes(), true, true);
        if tokens.is_empty() {
            anyhow::bail!("empty token sequence");
        }
        tokens.truncate(N_CTX as usize);
        total += tokens.len();
        token_lists.push(tokens);
    }
    // Each group is independent: reset the KV cache so every sequence
    // starts at position 0 (decode requires pos continuity per seq).
    ctx.clear_kv_cache();
    let mut batch = LlamaBatch::new(total, token_lists.len() as i32);
    for (seq, tokens) in token_lists.iter().enumerate() {
        for (pos, token) in tokens.iter().enumerate() {
            batch.add(*token, pos as i32, &[seq as i32], false)?;
        }
    }
    ctx.decode(&mut batch)
        .with_context(|| "llama multi-sequence decode")?;
    let mut result = Vec::with_capacity(token_lists.len());
    for seq in 0..token_lists.len() {
        let emb = ctx
            .embeddings_seq_ith(seq as i32)
            .map_err(|e| anyhow::anyhow!("reading seq embedding {seq}: {e:?}"))?;
        if emb.len() != QWEN_DIM {
            anyhow::bail!("embedding dim {} != expected {QWEN_DIM}", emb.len());
        }
        result.push(l2_normalize(emb.to_vec()));
    }
    let _ = HashMap::<(), ()>::new; // silence unused import if edited
    Ok(result)
}

fn embed_one(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    text: &str,
    n_threads: i32,
) -> Result<Vec<f32>> {
    let mut tokens = model.vocab().tokenize(text.as_bytes(), true, true);
    if tokens.is_empty() {
        bail!("empty token sequence");
    }
    if tokens.len() > N_CTX as usize {
        tokens.truncate(N_CTX as usize);
    }
    // Each text is an independent sequence: reset the KV cache so positions
    // restart at 0 for sequence 0.
    ctx.clear_kv_cache();
    let mut batch = LlamaBatch::new(tokens.len(), 1);
    for (pos, token) in tokens.iter().enumerate() {
        batch.add(*token, pos as i32, &[0], false)?;
    }
    ctx.decode(&mut batch)
        .with_context(|| "llama decode (embedding)")?;
    let _ = n_threads;
    let emb = ctx
        .embeddings_seq_ith(0)
        .map_err(|e| anyhow::anyhow!("reading embeddings: {e:?}"))?;
    if emb.len() != QWEN_DIM {
        bail!("embedding dim {} != expected {QWEN_DIM}", emb.len());
    }
    Ok(l2_normalize(emb.to_vec()))
}

pub fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable() {
        let a = embedding_fingerprint("qwen3-embedding-0.6b", 1024, 1000, 120);
        let b = embedding_fingerprint("qwen3-embedding-0.6b", 1024, 1000, 120);
        let c = embedding_fingerprint("qwen3-embedding-0.6b", 1024, 800, 120);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn query_template() {
        assert!(format_query("zvec dll").starts_with("Instruct:"));
    }

    #[test]
    fn l2_normalize_works() {
        let v = l2_normalize(vec![3.0, 4.0]);
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }

    /// Requires the local GGUF model file; skipped when absent.
    #[test]
    fn smoke_embed_similarity_structure() {
        let gguf = std::path::PathBuf::from(
            "D:/search-harness/workspace/models/hf_Qwen_Qwen3-Embedding-0.6B-Q8_0.gguf",
        );
        if !gguf.is_file() {
            eprintln!("skipping: model file not present");
            return;
        }
        let Ok(mut emb) = QwenEmbedder::load(&gguf, "") else {
            eprintln!("skipping: embedder failed to load");
            return;
        };
        let q = emb.embed("如何配置 MCP 服务器", EmbedPurpose::Query).unwrap();
        let d_relevant = emb
            .embed("MCP server 配置: 在 settings.json 中添加 mcpServers 条目", EmbedPurpose::Document)
            .unwrap();
        let d_irrelevant = emb
            .embed("The chef added chopped onions to the simmering stew", EmbedPurpose::Document)
            .unwrap();
        let rel = cosine(&q, &d_relevant);
        let irr = cosine(&q, &d_irrelevant);
        eprintln!("relevant={rel:.4} irrelevant={irr:.4}");
        assert!(rel > irr, "similarity structure broken: {rel} vs {irr}");
        assert!(rel > 0.5, "relevant similarity suspiciously low: {rel}");
    }

    /// Throughput floor: catches perf regressions (candle was ~0.05 chunks/s;
    /// llama.cpp should be orders of magnitude faster).
    #[test]
    fn embed_throughput_floor() {
        let gguf = std::path::PathBuf::from(
            "D:/search-harness/workspace/models/hf_Qwen_Qwen3-Embedding-0.6B-Q8_0.gguf",
        );
        if !gguf.is_file() {
            eprintln!("skipping: model file not present");
            return;
        }
        let Ok(mut emb) = QwenEmbedder::load(&gguf, "") else {
            eprintln!("skipping: embedder failed to load");
            return;
        };
        let docs: Vec<String> = (0..8)
            .map(|i| format!("chunk {i} about hybrid vector search 混合检索 architecture ").repeat(20))
            .collect();
        let t0 = std::time::Instant::now();
        let out = emb.embed_documents(&docs, 8192).unwrap();
        let dt = t0.elapsed().as_secs_f32();
        let rate = out.len() as f32 / dt;
        eprintln!("throughput: {rate:.1} chunks/s");
        assert!(rate >= 0.3, "embedding too slow: {rate:.2} chunks/s (candle-class regression)");
    }
}
