//! Daemon: the long-lived background process owning the index.
//! herdr pattern — probe-then-spawn-then-wait for clients; socket bind is
//! the single-instance mutex; management plane is JSON-RPC over the same
//! local socket (sync sockets + threads — the admin plane is low-traffic);
//! MCP rides the stdio bridge or HTTP on top.

use std::path::PathBuf;
use std::time::Duration;
use std::sync::Arc;

use anyhow::{Context, Result};
use std::net::{TcpListener, TcpStream};
use serde_json::{json, Value};

use rcd_core::config::Config;
use rcd_core::embed::QwenEmbedder;
use rcd_core::search::{SearchRequest, SearchService};
use rcd_core::store::{SearchMode, SearchStore};
use rcd_core::sync::{self, IndexProgress};
use rcd_core::zvec_store::ZvecStore;

// Cold start loads the embedding model twice plus an initial index pass
// before binding the admin port — readiness polling must outlast that.
// 120s was too tight (observed 2026-11-20: model load + first index pass
// exceeded it; the daemon eventually became ready but the client had
// already given up).
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// The daemon's loopback port file. A fresh daemon binds an ephemeral
/// 127.0.0.1 port and writes it here (atomic rename); clients read it to
/// connect. Replaces named pipes: the interprocess crate's Stream read side
/// never returned data that a .NET client received within 1ms on the same
/// pipe — measured (daemon.log 01:22:54: request completed in 0ms, the
/// Rust read hung 20s).
fn port_dir() -> std::path::PathBuf {
    std::env::var_os("RCD_PORT_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(|d| std::path::PathBuf::from(d).join("rcd-mcp"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn port_file() -> std::path::PathBuf {
    let dir = std::env::var_os("RCD_PORT_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(|d| std::path::PathBuf::from(d).join("rcd-mcp"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("admin.port")
}

pub fn connect_admin(timeout: Duration) -> Result<TcpStream> {
    let port: u16 = std::fs::read_to_string(port_file())?
        .trim()
        .parse()
        .context("parsing admin port file")?;
    let stream = TcpStream::connect(("127.0.0.1", port)).context("connecting to daemon")?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

/// Probe: is a daemon already listening? Time-bounded.
pub fn is_daemon_listening() -> bool {
    connect_admin(Duration::from_secs(2)).is_ok()
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
    let mut args = String::from("daemon");
    if let Some(path) = config_path {
        args.push_str(" --config ");
        args.push_str(&path.display().to_string());
    }
    // CRITICAL: WMI Win32_Process.Create does NOT inherit the caller's
    // environment (only variables passed via ProcessStartup). Without
    // echoing RCD_PORT_DIR here, a test-spawned daemon would write its
    // port to the default dir while the caller watches the test dir —
    // two daemons, invisible to each other (observed in smoke).
    if let Some(port_dir) = std::env::var_os("RCD_PORT_DIR") {
        args.push_str(&format!(" --port-dir \"{}\"", port_dir.to_string_lossy()));
    }
    if let Some(log) = std::env::var_os("RUST_LOG") {
        args.push_str(&format!(" --log-level \"{}\"", log.to_string_lossy()));
    }
    #[cfg(windows)]
    {
        // Git Bash / terminal jobs kill children on close; DETACHED_PROCESS
        // + CREATE_BREAKAWAY still dies when the job forbids breakaway
        // (observed: daemon lived exactly as long as its launcher). The
        // reliable escape is WMI Win32_Process.Create — the WMI service
        // host creates the process, outside any caller job (herdr pattern;
        // wmi-rs crate, same call shape as its own doc example).
        let cmdline = format!("\"{}\" {}", exe.display(), args);
        let con = match wmi::WMIConnection::new(wmi::COMLibrary::new()?) {
            Ok(c) => c,
            Err(e) => { anyhow::bail!("WMI connection failed: {e}"); }
        };
        let in_params = con
            .get_object("Win32_Process")?
            .get_method("Create")?
            .unwrap()
            .spawn_instance()?;
        in_params.put_property("CommandLine", cmdline)?;
        let out = con.exec_method("Win32_Process", "Create", Some(&in_params))?;
        let ret = out
            .as_ref()
            .and_then(|o| o.get_property("ReturnValue").ok())
            .and_then(|v| match v {
                wmi::Variant::I4(v) => Some(v),
                wmi::Variant::UI4(v) => Some(v as i32),
                _ => None,
            })
            .unwrap_or(0);
        // Process creation is asynchronous — the authoritative readiness
        // check is wait_for_ready(), so treat 0/unknown as accepted and let
        // the probe loop confirm.
        if ret == 0 {
            crate::logging::daemon_spawned(0, config_path);
            return Ok(());
        }
        let mut props = String::new();
        if let Some(o) = &out {
            if let Ok(names) = o.list_properties() {
                for name in names {
                    if let Ok(val) = o.get_property(&name) {
                        props.push_str(&format!(" {name}={val:?}"));
                    }
                }
            }
        }
        anyhow::bail!("WMI daemon spawn failed (ret={ret};{props})");
    }
    #[cfg(unix)]
    {
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
}

// Unix-only detached spawn helper. Windows spawns via WMI Win32_Process.Create
// (see spawn_daemon_detached), which is inherently detached — no flags needed.
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
    crate::logging::daemon_startup("tcp-loopback", std::process::id());

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

    let state = Arc::new(DaemonState {
        config: config.clone(),
        store,
        embedder,
        query_embedder,
        progress: Arc::new(std::sync::Mutex::new(IndexProgress::default())),
    });

    // Single-instance mutex: atomic lock-file create_new BEFORE the port
    // dance. The old probe-then-bind had a race window the width of the
    // double model load (~90s): two concurrent spawners both passed the
    // probe, both bound (ephemeral ports never collide) and overwrote each
    // other's port file (observed 2026-11-19: daemons on 60340 and 60505).
    // OpenOptions::create_new is the only atomic mutual-exclusion primitive
    // here (plain fs::write truncates an existing lock — it never fails);
    // a stale lock is recovered by PID liveness. Mutex scope = port dir:
    // distinct RCD_PORT_DIR values are distinct daemons by design (tests).
    let port_dir_display = port_dir().display().to_string();
    crate::logging::port_dir_selected(&port_dir_display);
    let lock_path = port_dir().join("daemon.lock");
    let lock_taken = (|| -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .context("opening daemon lock")?;
        writeln!(file, "{}", std::process::id()).ok();
        Ok(())
    })();
    if let Err(err) = lock_taken {
        let already_exists = err
            .downcast_ref::<std::io::Error>()
            .map(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
            .unwrap_or(false);
        if !already_exists {
            return Err(err.into());
        }
        // Lock exists: live holder, or stale lock from a crashed daemon?
        let holder_pid = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let holder_alive = holder_pid.map(is_pid_alive).unwrap_or(false);
        if holder_alive {
            let holder = holder_pid
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "unknown holder".into());
            crate::logging::mutex_lost(&port_dir_display, &holder);
            crate::logging::socket_bind_conflict("tcp-loopback");
            anyhow::bail!("another daemon is already running ({holder})");
        }
        // Holder is dead: take over the stale lock.
        tracing::warn!(holder = ?holder_pid, "stale daemon lock — taking over");
        let _ = std::fs::remove_file(&lock_path);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .context("taking over stale daemon lock")?;
        writeln!(file, "{}", std::process::id()).ok();
    }
    // Lock held; the OS releases it when this process exits (delete on
    // shutdown best-effort — a leftover is recovered by the PID check).
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let tmp = port_file().with_extension("tmp");
    std::fs::write(&tmp, port.to_string())?;
    std::fs::rename(&tmp, port_file())?;
    crate::logging::socket_bound(&format!("127.0.0.1:{port}"));

    // Background incremental index on a thread pool of one.
    spawn_reindex_task(state.clone(), false);

    crate::logging::daemon_ready(&format!("127.0.0.1:{port}"));
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

/// PID liveness probe for the daemon lock: true = a process with that PID
/// is still RUNNING (not merely that the PID can be opened). On Windows an
/// exited process stays "openable" while any handle survives (e.g. a
/// parent's std::process::Child), so the naive OpenProcess probe reports
/// zombies as alive — WaitForSingleObject(0) is the correct check: an
/// exited process's handle is signaled even when other handles remain.
#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: open + zero-timeout wait + close on a numeric pid; no
    // process state is touched.
    #[allow(unsafe_code)]
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return pid == std::process::id(); // our own pid: alive by definition
    }
    // SAFETY: zero timeout — pure query, never blocks.
    #[allow(unsafe_code)]
    let rc = unsafe { WaitForSingleObject(h, 0) };
    // SAFETY: closing the handle we just opened.
    #[allow(unsafe_code)]
    unsafe { CloseHandle(h) };
    rc == WAIT_TIMEOUT // WAIT_OBJECT_0 (signaled) = process has exited
}

#[cfg(not(windows))]
fn is_pid_alive(pid: u32) -> bool {
    // kill(pid, 0): signal 0 is the documented existence probe.
    // SAFETY: no signal is delivered; this only probes existence.
    #[allow(unsafe_code)]
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn daemon_log_path(config: &Config) -> PathBuf {
    std::env::var_os("APPDATA")
        .map(|d| PathBuf::from(d).join("rcd-mcp").join("daemon.log"))
        .unwrap_or_else(|| config.storage.index_dir.join("daemon.log"))
}

/// One JSON-per-line management connection (sync, thread per connection).
fn serve_connection(stream: TcpStream, state: &Arc<DaemonState>) -> Result<()> {
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
        let started = std::time::Instant::now();
        let request_id = crate::logging::next_request_id();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("?").to_string();
        crate::logging::admin_request_start(request_id, &method);
        let reply = handle_admin(request, state).unwrap_or_else(|| {
            json!({"id": Value::Null, "error": "unhandled or malformed request"}).to_string()
        });
        let outcome = if reply.contains("\"error\"") { "error" } else { "ok" };
        crate::logging::admin_request_complete(request_id, &method, outcome, started.elapsed().as_millis());
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
            crate::logging::daemon_stopping();
            let _ = state.store.flush();
            let _ = std::fs::remove_file(port_file());
            let _ = std::fs::remove_file(port_dir().join("daemon.lock"));
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
            let mut embedder = state.query_embedder.lock().ok()?;
            let mut svc = SearchService {
                store: state.store.as_ref(),
                embedder: &mut embedder,
            };
            let started = std::time::Instant::now();
            let mode_str = mode.map(|m| m.as_str()).unwrap_or("auto");
            let result = svc.search(&SearchRequest {
                query: query.clone(),
                mode,
                top_k,
                path_filter: path_filter.clone(),
                kind_filter: None,
            });
            if let Ok(r) = &result {
                crate::logging::search_complete(
                    mode_str,
                    r.hits.len(),
                    r.hits.first().map(|h| h.score).unwrap_or(0.0),
                    started.elapsed().as_millis(),
                );
            }
            result.map(|r| serde_json::to_value(&r).unwrap_or_default())
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

fn status_json(state: &Arc<DaemonState>) -> Value {
    let progress = state.progress.lock().map(|p| p.clone()).unwrap_or_default();
    let doc_count = state.store.doc_count().unwrap_or(0);
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "doc_count": doc_count,
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
