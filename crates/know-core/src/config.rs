//! Configuration: `know-mcp.toml` parsing, defaults, validation.
//!
//! Lookup order: `--config` path > `KNOW_MCP_CONFIG` env > `know-mcp.toml`
//! next to the current exe.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// File extensions indexed when `[index].extensions` is empty.
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "mjs", "cjs", "py", "go", "c", "h", "cpp", "hpp", "java", "rb", "php",
    "lua", "sh", "ps1", "toml", "yaml", "yml", "json", "md", "txt",
];

/// Directories always excluded, on top of gitignore rules.
/// `claude-src` has no `.git`, so node_modules must be excluded explicitly.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "**/.git/**",
    "**/node_modules/**",
    "**/target/**",
    "**/vendor/**",
    "**/dist/**",
    "**/build/**",
    "**/.know-index/**",
];

pub fn default_config_path() -> Option<PathBuf> {
    // 1. Next to the executable (deployed layout).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("know-mcp.toml");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // 2. Current working directory (dev layout: run from the project root).
    let cwd_candidate = std::env::current_dir().ok()?.join("know-mcp.toml");
    cwd_candidate.exists().then_some(cwd_candidate)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub index: IndexConfig,
    pub chunk: ChunkConfig,
    pub embed: EmbedConfig,
    pub search: SearchConfig,
    pub server: ServerConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexConfig {
    /// Root directories to index: code repositories and document folders.
    pub roots: Vec<PathBuf>,
    /// Allowlist BOUNDARY: the only directories manage_roots may ever
    /// activate. Paths outside allowed_roots are rejected by the admin
    /// plane regardless of what an MCP client asks for. Empty = roots can
    /// only shrink, never grow (management is config-file-only).
    pub allowed_roots: Vec<PathBuf>,
    /// Extra exclude globs on top of DEFAULT_EXCLUDES.
    pub exclude: Vec<String>,
    pub max_file_bytes: u64,
    /// Empty = built-in whitelist (DEFAULT_EXTENSIONS).
    pub extensions: Vec<String>,
}

impl IndexConfig {
    /// Is `candidate` inside the allowlist boundary? Canonicalized prefix
    /// comparison (case-insensitive filesystems rely on canonicalize).
    pub fn root_allowed(&self, candidate: &Path) -> bool {
        let canon = dunce::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
        self.allowed_roots.iter().any(|allowed| {
            let a = dunce::canonicalize(allowed).unwrap_or_else(|_| allowed.clone());
            canon.starts_with(&a)
        })
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            allowed_roots: Vec::new(),
            exclude: Vec::new(),
            max_file_bytes: 10_000_000,
            extensions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChunkConfig {
    pub chars: usize,
    pub overlap: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            chars: 1000,
            overlap: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbedConfig {
    pub model: String,
    pub gguf_path: PathBuf,
    /// HF repo id or a local directory containing tokenizer.json.
    pub tokenizer_repo: String,
    pub max_batch_tokens: usize,
    /// GPU layer offload. null/absent = auto (all layers when a CUDA device
    /// is detected, CPU otherwise); 0 = force CPU; N = first N layers.
    pub gpu_layers: Option<u32>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model: "qwen3-embedding-0.6b".to_string(),
            // Repository-relative: `models/` is archived in-repo (gitignored).
            // Real deployments always set this in know-mcp.toml.
            gguf_path: crate::embed::default_gguf_path(),
            tokenizer_repo: "Qwen/Qwen3-Embedding-0.6B".to_string(),
            max_batch_tokens: 8192,
            gpu_layers: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SearchConfig {
    pub default_top_k: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self { default_top_k: 10 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub http_bind: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_bind: "127.0.0.1:8765".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    pub index_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            index_dir: PathBuf::from("E:/know-mcp/.know-index"),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = explicit
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("KNOW_MCP_CONFIG").map(PathBuf::from))
            .or_else(default_config_path);
        let Some(path) = path else {
            bail!(
                "no config found: pass --config, set KNOW_MCP_CONFIG, or place know-mcp.toml next to the executable"
            );
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.index.roots.is_empty() {
            bail!("config: [index].roots must list at least one root directory");
        }
        if self.chunk.chars < 200 || self.chunk.chars > 8000 {
            bail!("config: [chunk].chars must be within 200..=8000");
        }
        if self.chunk.overlap >= self.chunk.chars / 2 {
            bail!("config: [chunk].overlap must be less than half of chars");
        }
        Ok(())
    }

    /// Effective extension whitelist (lowercased, without leading dot).
    pub fn extensions(&self) -> Vec<String> {
        if self.index.extensions.is_empty() {
            DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect()
        } else {
            self.index
                .extensions
                .iter()
                .map(|s| s.trim_start_matches('.').to_lowercase())
                .collect()
        }
    }

    /// All exclude globs: defaults + config extras.
    pub fn excludes(&self) -> Vec<String> {
        let mut all: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
        all.extend(self.index.exclude.iter().cloned());
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let raw = r#"
[index]
roots = ["E:/a", "E:/b"]
[storage]
index_dir = "E:/idx"
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.index.roots.len(), 2);
        assert_eq!(config.chunk.chars, 1000);
        assert_eq!(config.embed.model, "qwen3-embedding-0.6b");
        assert!(config.excludes().iter().any(|g| g.contains("node_modules")));
    }

    #[test]
    fn rejects_overlap_too_large() {
        let raw = r#"
[index]
roots = ["E:/a"]
[chunk]
chars = 200
overlap = 150
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert!(config.validate().is_err());
    }
}
