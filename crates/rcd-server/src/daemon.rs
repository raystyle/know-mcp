//! Daemon: the long-lived background process owning the index.
//! herdr pattern — probe-then-spawn-then-wait for clients; socket bind is
//! the single-instance mutex; management plane is JSON-RPC over the same
//! local socket (sync sockets + threads — the admin plane is low-traffic);
//! MCP rides the stdio bridge or HTTP on top.

use std::path::PathBuf;
use std::time::Duration;
use std::sync::Arc;

use anyhow::{Context, Result};
use interprocess::local_socket::prelude::*;
use interprocess::TryClone as _;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream};
use serde_json::{json, Value};

use rcd_core::config::Config;
use rcd_core::embed::QwenEmbedder;
use rcd_core::feedback::Feedback;
use rcd_core::notes::Notes;
use rcd_core::search::{SearchRequest, SearchService};
use rcd_core::store::{SearchMode, SearchStore};
use rcd_core::sync::{self, IndexProgress};
use rcd_core::zvec_store::ZvecStore;

pub const SOCKET_NAME: &str = "rcd-mcp.sock";
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Socket name (namespaced: named pipe on Windows, abstract/fs on Unix).
/// Overridable via RCD_SOCKET_PATH.
pub fn socket_name() -> String {
    std::env::var("RCD_SOCKET_PATH").unwrap_or_else(|_| SOCKET_NAME.to_string())
}

fn ns_name() -> Result<interprocess::local_socket::Name<'static>> {
    socket_name().to_ns_name::<GenericNamespaced>().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Probe: is a daemon already listening? Time-bounded — never hangs.
pub fn is_daemon_listening() -> bool {
    use interprocess::local_socket::ConnectOptions;
    let Ok(name) = ns_name() else { return false };
    ConnectOptions::new()
        .name(name)
        .wait_mode(interprocess::ConnectWaitMode::Timeout(
            Duration::from_secs(2),
        ))
        .connect_sync()
        .is_ok()
}

/// Ensure a daemon is running: probe, then spawn detached, then wait for
/// readiness. Returns when the daemon accepts connections.
pub fn ensure_daemon(config_path: Option<&std::path::Path>) -> Result<()> {
    if is_daemon_listening() {
        return Ok(());
    }
    spawn_daemon_detached(config_path).context("spawning daemon")?;
    wait_for_ready().context("waiting for daemon readiness")
}

fn spawn_daemon_detached(config_path: Option<&std::path::Path>) -> Result<()> {
    let exe = std::env::current_exe().context("resolving current exe")?;
    let mut command = std::process::Command::new(&exe);
    command.arg("daemon");
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach_command(&mut command);
    command.status().context("spawning detached daemon")?;
    Ok(())
}

#[cfg(windows)]
fn detach_command(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    command.creation_flags(DETACHED_PROCESS);
}

#[cfg(unix)]
fn detach_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn wait_for_ready() -> Result<()> {
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if is_daemon_listening() {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    anyhow::bail!("daemon did not become ready within {READY_TIMEOUT:?}");
}

// ---------------------------------------------------------------------------
// Daemon runtime
// ---------------------------------------------------------------------------

pub struct DaemonState {
    pub config: Arc<Config>,
    pub store: Arc<ZvecStore>,
    /// Indexing worker's embedder (held for the whole batch loop).
    pub embedder: std::sync::Mutex<QwenEmbedder>,
    /// Query-side embedder: separate instance so searches never wait on
    /// the indexing loop (same weights, own context).
    pub query_embedder: std::sync::Mutex<QwenEmbedder>,
    pub feedback: Feedback,
    pub notes: Notes,
    pub progress: Arc<std::sync::Mutex<IndexProgress>>,
}

/// Run the daemon: bind the socket (single-instance mutex), serve the JSON
/// management plane, run the initial incremental index in the background.
pub async fn run_daemon(config_path: Option<PathBuf>) -> Result<()> {
    let config = Arc::new(load_config(config_path)?);

    let log_path = daemon_log_path(&config);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();
    tracing::info!(socket = %socket_name(), "daemon starting");

    // Fingerprint check: incompatible index → wipe and rebuild.
    if config.storage.index_dir.exists()
        && !sync::meta_compatible(&config.storage.index_dir, &config)
    {
        tracing::warn!("index fingerprint mismatch — rebuilding");
        std::fs::remove_dir_all(config.storage.index_dir.join("collection")).ok();
    }

    let store = Arc::new(ZvecStore::open(&config.storage.index_dir)?);
    let gpu = config.embed.gpu_layers;
    let gguf = &config.embed.gguf_path;
    let embedder = std::sync::Mutex::new(QwenEmbedder::load_with_gpu_layers(gguf, gpu)?);
    let query_embedder = std::sync::Mutex::new(QwenEmbedder::load_with_gpu_layers(gguf, gpu)?);
    let feedback = Feedback::load(&config.storage.index_dir)?;
    let notes = Notes::new(&config.notes.dir)?;

    let state = Arc::new(DaemonState {
        config: config.clone(),
        store,
        embedder,
        query_embedder,
        feedback,
        notes,
        progress: Arc::new(std::sync::Mutex::new(IndexProgress::default())),
    });

    // Bind: first bind wins; AddrInUse = another daemon is already running.
    let listener = ListenerOptions::new()
        .name(ns_name()?)
        .create_sync()
        .map_err(|e| anyhow::anyhow!("binding daemon socket (already running?): {e}"))?;

    // Background incremental index on a thread pool of one.
    spawn_reindex_task(state.clone(), false);

    tracing::info!("daemon ready");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    if let Err(err) = serve_connection(stream, &state) {
                        tracing::debug!(error = %err, "connection ended");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
    Ok(())
}

fn load_config(config_path: Option<PathBuf>) -> Result<Config> {
    Config::load(config_path.as_deref())
}

fn daemon_log_path(config: &Config) -> PathBuf {
    std::env::var_os("APPDATA")
        .map(|d| PathBuf::from(d).join("rcd-mcp").join("daemon.log"))
        .unwrap_or_else(|| config.storage.index_dir.join("daemon.log"))
}

/// One JSON-per-line management connection (sync, thread per connection).
fn serve_connection(stream: Stream, state: &Arc<DaemonState>) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        let Ok(request) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let reply = handle_admin(request, state).unwrap_or_else(|| {
            json!({"id": Value::Null, "error": "unhandled or malformed request"}).to_string()
        });
        writeln!(writer, "{reply}")?;
        writer.flush()?;
    }
}

fn handle_admin(request: Value, state: &Arc<DaemonState>) -> Option<String> {
    let method = request.get("method")?.as_str()?.to_string();
    let id = request.get("id").cloned();
    // Params live under "params" (JSON-RPC style); top-level fields are a
    // legacy fallback. Without this, `params.get("query")?` silently
    // returned None and the client hung forever waiting for a reply.
    let params = request.get("params").cloned().unwrap_or_else(|| {
        let mut top = request.clone();
        top.as_object_mut().map(|m| {
            m.remove("method");
            m.remove("id");
        });
        top
    });
    let result = match method.as_str() {
        "status" => Ok(status_json(state)),
        "stop" => {
            tracing::info!("stop requested");
            let _ = state.store.flush();
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(150));
                std::process::exit(0);
            });
            Ok(json!({"stopping": true}))
        }
        "reindex" => {
            let force = params.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
            spawn_reindex_task(state.clone(), force);
            Ok(json!({"started": true}))
        }
        "search" => {
            let query = params.get("query")?.as_str()?.to_string();
            let mode = params.get("mode").and_then(|m| m.as_str()).and_then(parse_mode);
            let top_k = params.get("top_k").and_then(|k| k.as_u64()).unwrap_or(10) as usize;
            let path_filter = request
                .get("path_filter")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string());
            let kind = params.get("kind_filter").and_then(|k| k.as_str()).and_then(parse_kind);
            let mut embedder = state.query_embedder.lock().ok()?;
            let mut svc = SearchService {
                store: state.store.as_ref(),
                embedder: &mut embedder,
                feedback: &state.feedback,
            };
            svc.search(&SearchRequest { query, mode, top_k, path_filter, kind_filter: kind })
                .map(|r| serde_json::to_value(&r).unwrap_or_default())
        }
        "save_note" => {
            let title = params.get("title")?.as_str()?.to_string();
            let body = params.get("body")?.as_str()?.to_string();
            let tags = request
                .get("tags")
                .and_then(|t| t.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                .unwrap_or_default();
            let sources = request
                .get("sources")
                .and_then(|t| t.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect::<Vec<_>>())
                .unwrap_or_default();
            let query = params.get("query").and_then(|q| q.as_str()).map(|s| s.to_string());
            state
                .notes
                .save(&title, &body, &tags, &sources, query.as_deref())
                .map(|id| json!({"id": id}))
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        "list_notes" => {
            let tag = params.get("tag").and_then(|t| t.as_str());
            let limit = params.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
            let stats = state.feedback.note_stats();
            state
                .notes
                .list(tag, limit)
                .map(|metas| {
                    json!(metas
                        .into_iter()
                        .map(|m| {
                            let (h, n) = stats
                                .iter()
                                .find(|(id, _, _)| *id == m.id)
                                .map(|(_, h, n)| (*h, *n))
                                .unwrap_or((0, 0));
                            json!({
                                "id": m.id, "title": m.title, "tags": m.tags,
                                "helpful": h, "not_helpful": n,
                                "created_at": m.created_at,
                            })
                        })
                        .collect::<Vec<_>>())
                })
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        "get_note" => {
            let note_id = params.get("id")?.as_str()?.to_string();
            state
                .notes
                .get(&note_id)
                .map(|n| {
                    json!({"id": n.meta.id, "title": n.meta.title, "body": n.body,
                           "tags": n.meta.tags, "sources": n.meta.sources, "query": n.meta.query,
                           "helpful_score": state.feedback.helpful_score(&note_id)})
                })
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        "record_feedback" => {
            let note_id = params.get("note_id")?.as_str()?.to_string();
            let helpful = params.get("helpful")?.as_bool()?;
            let comment = params.get("comment").and_then(|c| c.as_str());
            state
                .feedback
                .record(&note_id, helpful, comment)
                .map(|_| json!({"recorded": true}))
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        "get_gaps" => {
            let limit = params.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as usize;
            Ok(json!(state.feedback.gaps(limit)))
        }
        other => Err(anyhow::anyhow!("unknown method: {other}")),
    };

    let payload = match result {
        Ok(value) => json!({"id": id, "result": value}),
        Err(err) => json!({"id": id, "error": format!("{err}")}),
    };
    Some(payload.to_string())
}

fn parse_mode(s: &str) -> Option<SearchMode> {
    match s {
        "keyword" => Some(SearchMode::Keyword),
        "vector" => Some(SearchMode::Vector),
        "hybrid" => Some(SearchMode::Hybrid),
        _ => None,
    }
}

fn parse_kind(s: &str) -> Option<rcd_core::Kind> {
    match s {
        "archive" => Some(rcd_core::Kind::Archive),
        "note" => Some(rcd_core::Kind::Note),
        _ => None,
    }
}

fn status_json(state: &Arc<DaemonState>) -> Value {
    let progress = state.progress.lock().map(|p| p.clone()).unwrap_or_default();
    let doc_count = state.store.doc_count().unwrap_or(0);
    let notes = state.notes.list(None, usize::MAX).map(|n| n.len()).unwrap_or(0);
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "doc_count": doc_count,
        "notes": notes,
        "model": state.config.embed.model,
        "roots": state.config.index.roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "index_dir": state.config.storage.index_dir.display().to_string(),
        "progress": progress,
    })
}

/// Kick a background incremental index pass on a plain thread.
pub fn spawn_reindex_task(state: Arc<DaemonState>, force: bool) {
    std::thread::spawn(move || {
        {
            let mut p = state.progress.lock().unwrap();
            *p = IndexProgress {
                running: true,
                total: 0,
                started_at: Some(chrono::Utc::now()),
                ..Default::default()
            };
        }
        let mut embedder = state.embedder.lock().unwrap();
        let progress_sink = state.progress.clone();
        let report = move |p: IndexProgress| {
            if let Ok(mut slot) = progress_sink.lock() {
                let running = slot.running;
                let started_at = slot.started_at;
                *slot = p;
                slot.running = running;
                slot.started_at = started_at;
            }
        };
        let result = sync::run_incremental_index(
            state.config.as_ref(),
            state.store.as_ref(),
            &mut embedder,
            force,
            Some(&report),
        );
        {
            let mut p = state.progress.lock().unwrap();
            p.running = false;
        }
        match result {
            Ok(outcome) => tracing::info!(
                scanned = outcome.scanned,
                updated = outcome.updated,
                deleted = outcome.deleted,
                failed = outcome.failed,
                "index pass complete"
            ),
            Err(err) => tracing::error!(error = %err, "index pass failed"),
        }
    });
}
