//! Shared E2E fixtures for daemon + MCP tests.
//!
//! Layers (per the rmcp testing playbook):
//! 1. In-process duplex — see mcp_inprocess.rs (no binary, fastest).
//! 2. Real binary via CARGO_BIN_EXE_rcd_mcp + TokioChildProcess — see
//!    mcp_daemon_e2e.rs (tests the full stdio transport against the real
//!    daemon).
//!
//! This module: fixture corpus + config builder + readiness helpers.

use std::path::{Path, PathBuf};

pub struct Fixture {
    pub dir: PathBuf,
    pub corpus: PathBuf,
    pub knowledge: PathBuf,
    pub index: PathBuf,
    pub config: PathBuf,
    pub port_dir: PathBuf,
}

impl Fixture {
    /// Create a temp fixture with the given (filename, content) corpus.
    /// Returns None (with a skip note) when the embedding model is absent.
    pub fn new(files: &[(&str, &str)]) -> Option<Fixture> {
        let model = Path::new(
            "D:/search-harness/workspace/models/hf_Qwen_Qwen3-Embedding-0.6B-Q8_0.gguf",
        );
        if !model.is_file() {
            eprintln!("skipping: embedding model not present");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("rcd-e2e-{}-{}", std::process::id(), rand_suffix()));
        let corpus = dir.join("corpus");
        let knowledge = dir.join("knowledge");
        let index = dir.join("index");
        let port_dir = dir.join("portdir");
        for d in [&corpus, &knowledge, &index, &port_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        for (name, content) in files {
            std::fs::write(corpus.join(name), content).unwrap();
        }
        let config = dir.join("cfg.toml");
        // TOML basic strings treat backslash as an escape start (a Windows
        // "\U" becomes a unicode escape) — always emit forward slashes.
        let fix = |p: &Path| p.display().to_string().replace('\\', "/");
        std::fs::write(
            &config,
            format!(
                "[index]\nroots = [\"{}\"]\n[notes]\ndir = \"{}\"\n[embed]\nmodel = \"qwen3-embedding-0.6b\"\ngguf_path = \"{}\"\ntokenizer_repo = \"Qwen/Qwen3-Embedding-0.6B\"\n[storage]\nindex_dir = \"{}\"\n",
                fix(&corpus),
                fix(&knowledge),
                fix(model),
                fix(&index)
            ),
        )
        .unwrap();
        Some(Fixture { dir, corpus, knowledge, index, config, port_dir })
    }

    /// The daemon binary path (cargo sets this for every bin target).
    pub fn daemon_exe() -> &'static str {
        env!("CARGO_BIN_EXE_rcd-mcp")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // zvec holds file locks until process exit; the daemon Drop in the
        // test kills it first. Best-effort, keep temp dirs on failure for
        // post-mortem (they live under %TEMP% anyway).
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Wait until the daemon's admin port file appears and accepts a TCP
/// connection. Panics after `secs`.
pub fn wait_admin_ready(port_dir: &Path, secs: u64) -> u16 {
    use std::net::TcpStream;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let port_file = port_dir.join("admin.port");
    loop {
        if std::time::Instant::now() > deadline {
            panic!("daemon admin port not ready within {secs}s");
        }
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            if let Ok(port) = text.trim().parse::<u16>() {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return port;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
