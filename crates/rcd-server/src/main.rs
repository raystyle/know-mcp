//! rcd-mcp-server: local daemon MCP server for hybrid search over archived
//! code and documents.

mod daemon;
mod client;
mod mcp;
mod http;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rcd-mcp-server", version, about = "Hybrid search MCP over archived code and documents")]
struct Cli {
    /// Path to rcd-mcp.toml (default: RCD_MCP_CONFIG env or exe-adjacent).
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background daemon explicitly (normally auto-spawned).
    Daemon,
    /// MCP over stdio (Claude Code entry point; bridges to the daemon).
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
}

#[derive(Subcommand)]
enum NoteAction {
    List,
    Show { id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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
    }
}
