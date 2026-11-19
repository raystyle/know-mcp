//! rcd-mcp — Reference Code & Documentation: local daemon MCP server for
//! hybrid search over archived reference code and documentation.

mod daemon;
mod client;
mod logging;
mod mcp;
mod http;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rcd-mcp", version, about = "Hybrid search MCP over archived code and documents")]
struct Cli {
    /// Path to rcd-mcp.toml (default: RCD_MCP_CONFIG env or exe-adjacent).
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    /// Admin port-file directory (default: RCD_PORT_DIR or %APPDATA%/rcd-mcp).
    /// Echoed through WMI spawns, which do not inherit the environment.
    #[arg(long, global = true)]
    port_dir: Option<std::path::PathBuf>,

    /// Log level override (default: RUST_LOG), echoed through WMI spawns.
    #[arg(long, global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background daemon explicitly (normally auto-spawned).
    Daemon,
    /// MCP over stdio (Claude Code entry point; bridges to the daemon).
    #[command(name = "stdio")]
    Mcp,
    /// MCP over streamable HTTP (served by the daemon).
    Http,
    /// Stop the running daemon.
    Stop,
    /// Daemon/index/notes status (starts the daemon if needed).
    Status,
    /// (Re)build the index. Foreground when no daemon is running.
    Index {
        /// Re-embed every file regardless of sync state.
        #[arg(long)]
        force: bool,
        /// Run zvec optimize after indexing (opt-in; known to be fragile).
        #[arg(long)]
        optimize: bool,
    },
    /// Debug search from the CLI.
    Search {
        query: String,
        #[arg(long, default_value = "auto")]
        mode: String,
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        #[arg(long)]
        path: Option<String>,
    },
    /// Notes inspection: list or show with feedback scores.
    Note {
        #[command(subcommand)]
        action: NoteAction,
    },
    /// Register this server as a native CLI: `rcd-cli` (via mcp2cli binding).
    #[command(name = "register")]
    Mcp2Cli {
        /// Transport for the binding: stdio bridge (default) or http.
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// HTTP endpoint when --transport http (default from config).
        #[arg(long)]
        endpoint: Option<String>,
        /// Overwrite an existing named config.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum NoteAction {
    List,
    Show { id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    configure_lib_search_path();

    let cli = Cli::parse();
    // Flags beat env (the WMI spawn path passes them explicitly because
    // Win32_Process.Create drops the caller's environment).
    if let Some(dir) = cli.port_dir.clone() {
        std::env::set_var("RCD_PORT_DIR", dir);
    }
    if let Some(level) = cli.log_level.clone() {
        std::env::set_var("RUST_LOG", level);
    }
    match cli.command {
        Command::Daemon => daemon::run_daemon(cli.config).await,
        Command::Mcp => client::run_mcp_stdio(cli.config).await,
        Command::Http => client::run_mcp_http(cli.config).await,
        Command::Stop => client::run_stop(cli.config).await,
        Command::Status => client::run_status(cli.config).await,
        Command::Index { force, optimize } => client::run_index(cli.config, force, optimize).await,
        Command::Search { query, mode, top_k, path } => {
            client::run_search(cli.config, query, mode, top_k, path).await
        }
        Command::Note { action } => client::run_note(cli.config, action).await,
        Command::Mcp2Cli { transport, endpoint, force } => {
            client::run_mcp2cli(cli.config, transport, endpoint, force).await
        }
    }
}

/// Minimal .env loader: `KEY=VALUE` lines from `.env` next to the exe or in
/// the working directory. Real environment variables always win.
fn load_dotenv() {
    for candidate in [
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(".env"))),
        std::env::current_dir().ok().map(|d| d.join(".env")),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(raw) = std::fs::read_to_string(&candidate) else { continue };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            if key.is_empty() {
                continue;
            }
            // Real env wins over .env.
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, value);
            }
        }
        break;
    }
}

/// Add RCD_LIB_DIR to the DLL search path so dependency libraries (zvec,
/// CUDA runtime) load from a configured directory without PATH setup.
/// Windows: SetDllDirectoryW injects the directory into the loader's search
/// list (the CUDA DLLs are delay-loaded, so early registration suffices).
#[cfg(windows)]
fn configure_lib_search_path() {
    let Some(dir) = std::env::var_os("RCD_LIB_DIR") else { return };
    if dir.is_empty() {
        return;
    }
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new(&dir)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // Best effort: a missing directory is reported by the loader later.
        windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(wide.as_ptr());
    }
}

#[cfg(not(windows))]
fn configure_lib_search_path() {
    // Unix: LD_LIBRARY_PATH must be set before process start; document that
    // .env users export it in their shell profile instead.
    if let Some(dir) = std::env::var_os("RCD_LIB_DIR") {
        let existing = std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default();
        let combined = if existing.is_empty() {
            dir
        } else {
            std::env::join_paths([dir, existing]).unwrap_or(existing)
        };
        std::env::set_var("LD_LIBRARY_PATH", combined);
    }
}
