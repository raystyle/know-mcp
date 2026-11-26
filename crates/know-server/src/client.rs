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

/// Native self-registration: copy this binary as `know-cli` into
/// `install_dir` (default: next to the exe). The copy dispatches by exe
/// name — same binary, verb-shaped CLI mirroring the MCP tools. No
/// external dispatcher (this is what mcp2cli's Windows link create did
/// for us; now in-tree). Re-run with --force after every upgrade.
pub fn run_register(install_dir: Option<PathBuf>, force: bool) -> Result<()> {
    let exe = std::env::current_exe().context("resolving current exe")?;
    let dir = match install_dir {
        Some(d) => d,
        None => exe
            .parent()
            .context("exe has no parent directory")?
            .to_path_buf(),
    };
    let cli_name = if cfg!(windows) {
        "know-cli.exe"
    } else {
        "know-cli"
    };
    let dest = dir.join(cli_name);
    if dest.exists() && !force {
        anyhow::bail!(
            "{} already exists — pass --force to overwrite (or re-register after upgrading know-mcp)",
            dest.display()
        );
    }
    std::fs::copy(&exe, &dest)
        .with_context(|| format!("copying {} -> {}", exe.display(), dest.display()))?;
    println!("registered: {}", dest.display());
    if !path_contains(&dir) {
        println!(
            "note: {} is not on PATH — add it to use `know-cli` from anywhere",
            dir.display()
        );
    }
    // Self-check: the copy must answer as know-cli.
    let status = std::process::Command::new(&dest)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => println!("self-check ok: know-cli --help"),
        other => {
            let _ = std::fs::remove_file(&dest);
            anyhow::bail!("know-cli copy failed self-check: {other:?} — removed");
        }
    }
    println!("try: know-cli get-status");
    println!("     know-cli search-content --query \"混合检索\" --mode hybrid");
    Ok(())
}

fn path_contains(dir: &std::path::Path) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p == dir))
        .unwrap_or(false)
}

/// CLI read: no roots allowlist check (unlike the MCP tool) — the CLI
/// runs as the local user with the user's own filesystem rights; the
/// MCP guard exists for untrusted client input, not for the operator.
/// Document formats (pdf/docx/…) read through the extraction pipeline
/// (text layer, OCR fallback), not raw bytes.
pub async fn run_read(
    config: Option<PathBuf>,
    path: String,
    start_line: Option<u64>,
    end_line: Option<u64>,
) -> Result<()> {
    ensure_daemon(config.as_deref())?;
    let canonical =
        dunce::canonicalize(&path).with_context(|| format!("cannot resolve path {path}"))?;
    let is_doc = canonical
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| know_core::reader::is_reader_format(&e.to_lowercase()))
        .unwrap_or(false);
    let content = if is_doc {
        let cfg = know_core::config::Config::load(config.as_deref())?;
        know_core::reader::extract_text(&canonical, cfg.reader.ocr)
            .with_context(|| format!("extracting {}", canonical.display()))?
    } else {
        std::fs::read_to_string(&canonical)
            .with_context(|| format!("reading {}", canonical.display()))?
    };
    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(start + 2000 - 1).min(start + 2000 - 1);
    let mut taken: usize = 0;
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as u64;
        if n < start {
            continue;
        }
        if n > end {
            break;
        }
        println!("{n:>6}	{line}");
        taken += line.len() + 8;
        if taken > 256 * 1024 {
            println!("... (truncated at 256KB)");
            break;
        }
    }
    Ok(())
}

/// manage_index CLI: same actions as the MCP tool, over the admin plane.
pub async fn run_manage(config: Option<PathBuf>, action: crate::ManageAction) -> Result<()> {
    ensure_daemon(config.as_deref())?;
    let (method_params, payload_key) = match action {
        crate::ManageAction::ListRoots => (json!({"action": "list_roots"}), "roots"),
        crate::ManageAction::AddRoot { path } => {
            (json!({"action": "add_root", "path": path}), "added")
        }
        crate::ManageAction::RemoveRoot { path } => {
            (json!({"action": "remove_root", "path": path}), "removed")
        }
        crate::ManageAction::Drop { confirm } => {
            (json!({"action": "drop", "confirm": confirm}), "dropped")
        }
    };
    let response = admin_request("manage_index", method_params)?;
    if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("manage_index failed: {err}");
    }
    let result = &response["result"];
    match payload_key {
        "roots" => {
            println!("active roots:");
            for r in result["roots"].as_array().cloned().unwrap_or_default() {
                println!("  {}", r.as_str().unwrap_or("?"));
            }
            println!("allowed_roots (authorization boundary):");
            for r in result["allowed_roots"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                println!("  {}", r.as_str().unwrap_or("?"));
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(result)?),
    }
    Ok(())
}

/// know-cli verb runner: dispatch the tool-shaped subcommands over the
/// daemon admin plane. `json` wraps results in the CLI envelope.
pub async fn run_cli_verb(
    config_path: Option<PathBuf>,
    json: bool,
    verb: crate::KnowCliVerb,
) -> Result<()> {
    use crate::KnowCliVerb;
    type Renderer = Box<dyn FnOnce(&Value) -> String>;
    let (method, params, render): (&str, Value, Renderer) = match verb {
        KnowCliVerb::GetStatus => (
            "status",
            json!({}),
            Box::new(|r| serde_json::to_string_pretty(r).unwrap_or_default()),
        ),
        KnowCliVerb::SearchContent {
            query,
            mode,
            top_k,
            path_filter,
        } => (
            "search",
            json!({
                "query": query,
                "mode": mode.as_deref().unwrap_or("auto"),
                "top_k": top_k.unwrap_or(10),
                "path_filter": path_filter,
            }),
            Box::new(|r| {
                let hits = r["hits"].as_array().cloned().unwrap_or_default();
                let mut out = format!("mode: {}\n", r["mode"].as_str().unwrap_or("?"));
                if hits.is_empty() {
                    out.push_str("no hits\n");
                }
                for hit in hits {
                    out.push_str(&format!(
                        "[{}] {:.3} {}:{}-{} {}\n",
                        hit["kind"].as_str().unwrap_or("?"),
                        hit["score"].as_f64().unwrap_or(0.0),
                        hit["path"].as_str().unwrap_or("?"),
                        hit["start_line"].as_u64().unwrap_or(0),
                        hit["end_line"].as_u64().unwrap_or(0),
                        hit["title"].as_str().unwrap_or(""),
                    ));
                }
                out
            }),
        ),
        KnowCliVerb::BuildIndex { force } => (
            "reindex",
            json!({"force": force}),
            Box::new(|r| r.as_str().unwrap_or("started").to_string()),
        ),
        KnowCliVerb::ManageIndex {
            action,
            path,
            confirm,
        } => {
            let mut params = json!({"action": action});
            if let Some(p) = path {
                params["path"] = json!(p);
            }
            params["confirm"] = json!(confirm);
            (
                "manage_index",
                params,
                Box::new(|r| serde_json::to_string_pretty(r).unwrap_or_default()),
            )
        }
        KnowCliVerb::Daemon => return crate::daemon::run_daemon(config_path).await,
        KnowCliVerb::ReadFile {
            path,
            start_line,
            end_line,
        } => {
            // Direct read (no daemon needed); document formats go through
            // the reader pipeline.
            return run_read(config_path, path, start_line, end_line).await;
        }
    };

    ensure_daemon(config_path.as_deref())?;
    let response = admin_request(method, params)?;
    if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{err}");
    }
    let result = response["result"].clone();
    if json {
        let envelope = json!({
            "app": "know-cli",
            "verb": method,
            "data": result,
        });
        println!("{}", serde_json::to_string(&envelope)?);
    } else {
        print!("{}", render(&result));
    }
    Ok(())
}
