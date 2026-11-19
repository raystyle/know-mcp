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
const N_CTX_PER_SEQ: u32 = 1024;

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
/// callers keep it behind a `Mutex`. Send/Sync come for free: llama-cpp-2's
/// LlamaModel already implements both.
pub struct QwenEmbedder {
    model: LlamaModel,
    n_threads: i32,
}

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

    /// Context for embedding decode: one sequence, N_CTX_PER_SEQ KV budget.
    /// Flash attention stays at llama.cpp's default (auto): forcing it ON
    /// measured 89.1 vs 92.3 chunks/s on RTX 4060 (2026-11-19) — the ~250-
    /// token chunks are too short for FA's block-skipping to pay for its
    /// kernel overhead on this GPU.
    fn make_context(&self) -> Result<LlamaContext<'_>> {
        self.model
            .new_context(
                backend(),
                LlamaContextParams::default()
                    .with_n_ctx(std::num::NonZeroU32::new(N_CTX_PER_SEQ))
                    .with_n_batch(N_CTX_PER_SEQ)
                    .with_n_ubatch(N_CTX_PER_SEQ)
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
        embed_one(&self.model, &mut ctx, &input)
    }

    /// Batch-embed documents over ONE reused context (single-sequence
    /// decode per text). Multi-sequence batching (seq_id per text in one
    /// pass, correctly configured: n_ctx = n_seq × N_CTX_PER_SEQ, n_seq_max,
    /// n_batch = n_ctx) was measured 2026-11-19 on RTX 4060 after the
    /// llama.cpp LM-head fix (#30445, git rev 0364f5da): width 2 = 1.52x
    /// SLOWER than this loop — dense attention pays the full padded KV
    /// width per query under causal masks, so batching loses once the
    /// logits-copy bottleneck is gone. Don't retry without flash-attn
    /// evidence; see KB-LLAMA-CPP-POOLED-EMBEDDING-SLOW.
    pub fn embed_documents(
        &mut self,
        texts: &[String],
        _max_batch_tokens: usize,
    ) -> Result<Vec<Vec<f32>>> {
        let mut ctx = self.make_context()?;
        texts
            .iter()
            .map(|t| embed_one(&self.model, &mut ctx, t))
            .collect()
    }
}

fn embed_one(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    text: &str,
) -> Result<Vec<f32>> {
    let mut tokens = model.vocab().tokenize(text.as_bytes(), true, true);
    if tokens.is_empty() {
        bail!("empty token sequence");
    }
    if tokens.len() > N_CTX_PER_SEQ as usize {
        tokens.truncate(N_CTX_PER_SEQ as usize);
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

    /// Determinism + unit-norm guard: the same doc embedded twice through
    /// embed_documents (batch call) and once through embed (single call)
    /// must yield identical / cosine≈1 vectors. Replaces the old
    /// batched-vs-serial equivalence test once multi-seq batching was
    /// removed (it lost the 2026-11-19 benchmark: 1.52x slower at width 2
    /// post LM-head fix). Guards against "batching broke quality" class
    /// regressions (e.g. the NonCausal experiment: 0.83 → 0.49).
    #[test]
    fn embed_documents_deterministic_and_consistent_with_embed() {
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
        let docs: Vec<String> = (0..12)
            .map(|i| {
                format!("chunk {i} of the equivalence fixture: hybrid search over archived code 归档代码混合检索 ")
                    .repeat(10)
            })
            .collect();
        let first = emb.embed_documents(&docs, 8192).unwrap();
        let second = emb.embed_documents(&docs, 8192).unwrap();
        assert_eq!(first.len(), docs.len());
        let mut worst = 1.0f32;
        for (i, (a, b)) in first.iter().zip(&second).enumerate() {
            assert_eq!(a.len(), QWEN_DIM, "dim @ {i}");
            assert_eq!(a, b, "non-deterministic embedding @ {i}");
            // L2-normalized ⇒ self-cosine is 1.0 (within float precision).
            let c = cosine(a, a);
            worst = worst.min(c);
        }
        // Batch path must agree with the single-text query-side path.
        let single = emb.embed(&docs[0], EmbedPurpose::Document).unwrap();
        assert_eq!(single.len(), QWEN_DIM);
        let c = cosine(&first[0], &single);
        assert!(
            (c - 1.0).abs() < 1e-3,
            "embed_documents disagrees with embed at doc 0: cosine {c:.6}"
        );
        eprintln!("worst self-cosine: {worst:.7}, batch-vs-single cosine: {c:.7}");
    }

    /// GPU throughput regression guard. 2026-11-19 baseline after the
    /// llama.cpp LM-head fix (#30445, git rev 0364f5da): RTX 4060 ran this
    /// exact fixture at ~92 docs/s. Floor set at 20 to hold the ~3.6x fix
    /// across driver/thermal variance without flaking; the measured rate
    /// prints for eyeballing. (Pre-fix rate was ~25 docs/s with the
    /// full-vocab logits D2H copy dominating.)
    #[test]
    fn embed_documents_throughput_guard() {
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
        // Realistic chunk sizes (~250 tokens): 64 = one EMBED_BATCH flush.
        let docs: Vec<String> = (0..64)
            .map(|i| {
                format!("chunk {i}: walk → read → chunk → embed → store pipeline over code repos and documents 管线分块 ")
                    .repeat(6)
            })
            .collect();
        // Warm up (first call pays model load / graph build / cache warmup).
        let warm: Vec<String> = docs[..8].to_vec();
        let _ = emb.embed_documents(&warm, 8192).unwrap();

        let t0 = std::time::Instant::now();
        let out = emb.embed_documents(&docs, 8192).unwrap();
        let dt = t0.elapsed();
        assert_eq!(out.len(), docs.len());
        let rate = out.len() as f32 / dt.as_secs_f32();
        eprintln!("embed_documents: {dt:?} for {} docs = {rate:.1} chunks/s", docs.len());
        assert!(
            rate >= 20.0,
            "embedding throughput regression: {rate:.1} chunks/s (baseline ~92 on RTX 4060, floor 20)"
        );
    }
}
