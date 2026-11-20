//! Daemon client side: thin CLI commands + MCP stdio bridge. All requests go
//! to the daemon's JSON management plane over the local socket.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::daemon::{connect_admin, ensure_daemon, is_daemon_listening};

/// Send one request, read one response line (time-bounded TCP loopback).
pub fn admin_request(method: &str, params: Value) -> Result<Value> {
    let mut stream = connect_admin(Duration::from_secs(10))?;
    let request = json!({"id": 1, "method": method, "params": params});
    use std::io::{BufRead, BufReader, Write};
    writeln!(stream, "{}", request)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("daemon closed the connection without a reply");
    }
    let response: Value = serde_json::from_str(line.trim())?;
    Ok(response)
}

pub async fn run_status(config: Option<PathBuf>) -> Result<()> {
    ensure_daemon(config.as_deref())?;
    let response = admin_request("status", json!({}))?;
    println!("{}", serde_json::to_string_pretty(&response["result"])?);
    Ok(())
}

pub async fn run_stop(_config: Option<PathBuf>) -> Result<()> {
    if !is_daemon_listening() {
        println!("daemon not running");
        return Ok(());
    }
    let _ = admin_request("stop", json!({}));
    // Poll until the socket refuses connections (15s deadline).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if !is_daemon_listening() {
            println!("daemon stopped");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("daemon did not stop within 15s");
}

pub async fn run_index(config: Option<PathBuf>, force: bool) -> Result<()> {
    if is_daemon_listening() {
        let response = admin_request("reindex", json!({"force": force}))?;
        println!("reindex started on daemon: {}", response["result"]);
        return Ok(());
    }
    // Foreground: no daemon — index directly.
    let config = know_core::config::Config::load(config.as_deref())?;
    let store = know_core::zvec_store::ZvecStore::open(&config.storage.index_dir)?;
    let mut embedder = know_core::embed::QwenEmbedder::load(
        &config.embed.gguf_path,
        &config.embed.tokenizer_repo,
    )?;
    let outcome = know_core::sync::run_incremental_index(
        &config,
        &store,
        &mut embedder,
        force,
        Some(&|p| {
            eprintln!(
                "\r{}/{} scanned, {} updated, {} failed",
                p.scanned, p.total, p.updated, p.failed
            );
        }),
    )?;
    eprintln!();
    println!(
        "indexed: {} scanned, {} updated, {} unchanged, {} deleted, {} failed, {} binary-skipped",
        outcome.scanned,
        outcome.updated,
        outcome.unchanged,
        outcome.deleted,
        outcome.failed,
        outcome.skipped_binary
    );
    Ok(())
}

pub async fn run_search(
    config: Option<PathBuf>,
    query: String,
    mode: String,
    top_k: usize,
    path: Option<String>,
) -> Result<()> {
    ensure_daemon(config.as_deref())?;
    let params = json!({
        "query": query,
        "mode": mode,
        "top_k": top_k,
        "path_filter": path,
    });
    let response = admin_request("search", params)?;
    if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("search failed: {err}");
    }
    let result = &response["result"];
    println!("mode: {}", result["mode"].as_str().unwrap_or("?"));
    let hits = result["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        println!("no hits");
        return Ok(());
    }
    for hit in hits {
        println!(
            "[{}] {:.3} {}:{}-{} {}",
            hit["kind"].as_str().unwrap_or("?"),
            hit["score"].as_f64().unwrap_or(0.0),
            hit["path"].as_str().unwrap_or("?"),
            hit["start_line"].as_u64().unwrap_or(0),
            hit["end_line"].as_u64().unwrap_or(0),
            hit["title"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

/// MCP stdio bridge: speak MCP on stdin/stdout, forward tool calls to the
/// daemon's admin plane.
pub async fn run_mcp_stdio(config: Option<PathBuf>) -> Result<()> {
    ensure_daemon(config.as_deref())?;
    crate::mcp::serve_stdio(config).await
}

/// MCP HTTP bridge: ensure daemon, then serve streamable HTTP locally,
/// forwarding tool calls to the daemon.
pub async fn run_mcp_http(config: Option<PathBuf>) -> Result<()> {
    ensure_daemon(config.as_deref())?;
    crate::http::serve(config).await
}

/// Register this server with mcp2cli: named config + `know-cli` command alias.
/// After this, `know search-content --query ...` works as a native CLI verb
/// (mcp2cli turns our MCP tools into typed --flag commands).
pub async fn run_mcp2cli(
    config_path: Option<PathBuf>,
    transport: String,
    endpoint: Option<String>,
    force: bool,
) -> Result<()> {
    let mcp2cli = which_mcp2cli()
        .ok_or_else(|| anyhow::anyhow!(
            "mcp2cli not found on PATH — install it first: cargo install --git https://github.com/mcp2cli/source-code"
        ))?;
    let exe = std::env::current_exe()?;

    let mut cmd = std::process::Command::new(&mcp2cli);
    cmd.args(["config", "init", "--name", "know-cli", "--app", "bridge"]);
    match transport.as_str() {
        "stdio" => {
            // The bridge auto-spawns our daemon; stdio keeps it local-only.
            cmd.args(["--transport", "stdio"]);
            let exe_str = exe.to_string_lossy();
            cmd.args(["--stdio-command", exe_str.as_ref()]);
            cmd.args(["--stdio-arg", "mcp"]);
        }
        "http" => {
            let url = endpoint.unwrap_or_else(|| {
                let bind = know_core::config::Config::load(config_path.as_deref())
                    .map(|c| c.server.http_bind.clone())
                    .unwrap_or_else(|_| "127.0.0.1:8765".to_string());
                format!("http://{bind}/mcp")
            });
            cmd.args(["--transport", "streamable_http"]);
            cmd.args(["--endpoint", &url]);
        }
        other => anyhow::bail!("unknown transport {other:?} (use stdio or http)"),
    }
    if force {
        cmd.arg("--force");
    }
    let status = cmd.status().context("running mcp2cli config init")?;
    if !status.success() {
        anyhow::bail!("mcp2cli config init failed (exit {status})");
    }

    // Activate and create the `know` command alias.
    for args in [
        vec!["use", "know-cli"],
        vec!["link", "create", "--name", "know-cli"],
    ] {
        let status = std::process::Command::new(&mcp2cli)
            .args(&args)
            .status()
            .with_context(|| format!("running mcp2cli {}", args.join(" ")))?;
        if !status.success() {
            anyhow::bail!("mcp2cli {} failed (exit {status})", args.join(" "));
        }
    }

    println!("registered: mcp2cli config `know-cli` + `know-cli` command");
    println!("try: know-cli get-status          # daemon/index status");
    println!("     know-cli search-content --query \"混合检索\" --mode hybrid");
    Ok(())
}

fn which_mcp2cli() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "mcp2cli.exe"
    } else {
        "mcp2cli"
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
