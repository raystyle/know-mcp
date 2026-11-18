//! Daemon client side: thin CLI commands + MCP stdio bridge. All requests go
//! to the daemon's JSON management plane over the local socket.

use std::path::PathBuf;

use anyhow::{Context, Result};
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, Stream};
use serde_json::{json, Value};

use crate::daemon::{ensure_daemon, is_daemon_listening, socket_name};

fn connect() -> Result<Stream> {
    let name = socket_name().to_ns_name::<GenericNamespaced>()?;
    Stream::connect(name).context("connecting to daemon socket")
}

/// Send one request, read one response line.
pub fn admin_request(method: &str, params: Value) -> Result<Value> {
    let mut stream = connect()?;
    let request = json!({"id": 1, "method": method, "params": params});
    use std::io::{BufRead, BufReader, Write};
    writeln!(stream, "{}", request)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: Value = serde_json::from_str(line.trim())?;
    Ok(response)
}

pub async fn run_status(_config: Option<PathBuf>) -> Result<()> {
    ensure_daemon()?;
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

pub async fn run_index(config: Option<PathBuf>, force: bool, optimize: bool) -> Result<()> {
    if is_daemon_listening() {
        let response = admin_request("reindex", json!({"force": force}))?;
        println!("reindex started on daemon: {}", response["result"]);
        if optimize {
            println!("note: optimize runs on the daemon side at the end of the pass");
        }
        return Ok(());
    }
    // Foreground: no daemon — index directly.
    let config = rcd_core::config::Config::load(config.as_deref())?;
    use rcd_core::store::SearchStore as _;
    let store = rcd_core::zvec_store::ZvecStore::open(&config.storage.index_dir)?;
    let mut embedder = rcd_core::embed::QwenEmbedder::load(
        &config.embed.gguf_path,
        &config.embed.tokenizer_repo,
    )?;
    let outcome = rcd_core::sync::run_incremental_index(
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
        "indexed: {} scanned, {} updated, {} unchanged, {} deleted, {} failed",
        outcome.scanned, outcome.updated, outcome.unchanged, outcome.deleted, outcome.failed
    );
    if optimize {
        store.optimize().context("zvec optimize (known to be fragile on some volumes)")?;
    }
    Ok(())
}

pub async fn run_search(
    _config: Option<PathBuf>,
    query: String,
    mode: String,
    top_k: usize,
    path: Option<String>,
) -> Result<()> {
    ensure_daemon()?;
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
    if result["gap_recorded"].as_bool() == Some(true) {
        println!("(gap recorded: no hit scored above threshold)");
    }
    Ok(())
}

pub async fn run_note(_config: Option<PathBuf>, action: crate::NoteAction) -> Result<()> {
    ensure_daemon()?;
    match action {
        crate::NoteAction::List => {
            let response = admin_request("list_notes", json!({}))?;
            println!("{}", serde_json::to_string_pretty(&response["result"])?);
        }
        crate::NoteAction::Show { id } => {
            let response = admin_request("get_note", json!({"id": id}))?;
            if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
                anyhow::bail!("{err}");
            }
            println!("{}", serde_json::to_string_pretty(&response["result"])?);
        }
    }
    Ok(())
}

/// MCP stdio bridge: speak MCP on stdin/stdout, forward tool calls to the
/// daemon's admin plane.
pub async fn run_mcp_stdio(_config: Option<PathBuf>) -> Result<()> {
    ensure_daemon()?;
    crate::mcp::serve_stdio().await
}

/// MCP HTTP bridge: ensure daemon, then serve streamable HTTP locally,
/// forwarding tool calls to the daemon.
pub async fn run_mcp_http(_config: Option<PathBuf>) -> Result<()> {
    ensure_daemon()?;
    crate::http::serve().await
}
