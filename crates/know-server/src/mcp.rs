//! MCP ServerHandler with 4 tools; tool calls forward to the daemon admin
//! plane (the bridge process itself stays light — no model, no index).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{
    schemars, serve_server, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;

use crate::client::admin_request;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Search query text (Chinese or English, keywords or natural language).
    pub query: String,
    /// "auto" (default), "keyword" (FTS/BM25-like), "vector" (semantic), "hybrid" (fused).
    #[serde(default)]
    pub mode: Option<String>,
    /// Max hits to return (default 10).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Case-insensitive substring filter on file paths.
    #[serde(default)]
    pub path_filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFileArgs {
    /// Absolute file path (must be under a configured index root).
    pub path: String,
    /// 1-based first line (default 1).
    #[serde(default)]
    pub start_line: Option<u64>,
    /// 1-based last line (default start+2000, cap 2000 lines / 256KB).
    #[serde(default)]
    pub end_line: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReindexArgs {
    /// Re-embed every file regardless of sync state.
    #[serde(default)]
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ManageIndexArgs {
    /// One of: list_roots, add_root, remove_root, drop.
    pub action: String,
    /// Directory path (add_root / remove_root). Must sit inside the
    /// configured allowed_roots boundary.
    #[serde(default)]
    pub path: Option<String>,
    /// drop requires confirm=true.
    #[serde(default)]
    pub confirm: Option<bool>,
}

#[derive(Clone)]
pub struct KnowMcpServer {
    config_path: Option<std::path::PathBuf>,
}

impl KnowMcpServer {
    pub fn with_config_path(config_path: Option<std::path::PathBuf>) -> Self {
        Self { config_path }
    }
}

const MAX_LINES: u64 = 2000;
const MAX_BYTES: usize = 256 * 1024;

#[tool_router]
impl KnowMcpServer {
    /// Hybrid keyword+vector search over archived repos and docs.
    #[tool(
        description = "Search archived code and documents. Modes: keyword (exact/FTS, best for identifiers, CVE ids, Chinese), vector (semantic), hybrid (fused, default via auto). Pass path_filter to scope to a subtree."
    )]
    async fn search_content(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "query": args.query,
            "mode": args.mode.as_deref().unwrap_or("auto"),
            "top_k": args.top_k.unwrap_or(10),
            "path_filter": args.path_filter,
        });
        forward(self.config_path.clone(), "search", params).await
    }

    /// Read a file under a configured root with line numbers.
    #[tool(
        description = "Read a file from the archive with 1-based line numbers. Optional start_line/end_line; caps at 2000 lines or 256KB. Paths outside the configured roots are rejected."
    )]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = std::path::PathBuf::from(&args.path);
        let canonical = dunce::canonicalize(&path).map_err(|e| {
            McpError::invalid_params(format!("cannot resolve path {}: {e}", args.path), None)
        })?;

        // Root allowlist guard: canonical path must sit under a configured root.
        let config = know_core::config::Config::load(self.config_path.as_deref())
            .map_err(|e| McpError::internal_error(format!("loading config: {e}"), None))?;
        let allowed = config.index.roots.iter().any(|root| {
            dunce::canonicalize(root)
                .map(|r| canonical.starts_with(&r))
                .unwrap_or(false)
        });
        if !allowed {
            return Err(McpError::invalid_params(
                format!("path {} is outside the indexed roots", canonical.display()),
                None,
            ));
        }

        let content = std::fs::read_to_string(&canonical).map_err(|e| {
            McpError::invalid_params(format!("reading {}: {e}", canonical.display()), None)
        })?;
        let start = args.start_line.unwrap_or(1).max(1);
        let end = args
            .end_line
            .unwrap_or(start + MAX_LINES - 1)
            .min(start + MAX_LINES - 1);

        let mut out = String::new();
        let mut taken: usize = 0;
        for (i, line) in content.lines().enumerate() {
            let n = (i + 1) as u64;
            if n < start {
                continue;
            }
            if n > end {
                break;
            }
            out.push_str(&format!("{n:>6}\t{line}\n"));
            taken += line.len() + 8;
            if taken > MAX_BYTES {
                out.push_str("... (truncated at 256KB)\n");
                break;
            }
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
    }

    /// Daemon and index status.
    #[tool(description = "Index and daemon status: doc count, model, roots, indexing progress.")]
    async fn get_status(&self) -> Result<CallToolResult, McpError> {
        forward(self.config_path.clone(), "status", serde_json::json!({})).await
    }

    /// Trigger a background reindex.
    /// Manage index scope and data: add/remove index roots (bounded by
    /// the configured allowed_roots), list them, or drop all index data.
    #[tool(
        description = "Manage the index. Actions: list_roots (show active + allowed roots), add_root <path> (activate a directory for indexing — must be inside allowed_roots from know-mcp.toml; triggers indexing), remove_root <path> (deactivate and delete that root's indexed data), drop (wipe ALL index data, needs confirm=true)."
    )]
    async fn manage_index(
        &self,
        Parameters(args): Parameters<ManageIndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = serde_json::json!({"action": args.action});
        if let Some(p) = &args.path {
            params["path"] = serde_json::json!(p);
        }
        if let Some(c) = args.confirm {
            params["confirm"] = serde_json::json!(c);
        }
        forward(self.config_path.clone(), "manage_index", params).await
    }

    /// Start a background reindex.
    #[tool(
        description = "Start a background incremental reindex (or full re-embed with force=true). Returns immediately; watch progress via get_status."
    )]
    async fn build_index(
        &self,
        Parameters(args): Parameters<ReindexArgs>,
    ) -> Result<CallToolResult, McpError> {
        forward(
            self.config_path.clone(),
            "reindex",
            serde_json::json!({"force": args.force.unwrap_or(false)}),
        )
        .await
    }
}

/// Forward a tool call to the daemon admin plane and wrap the reply.
/// Cold-start tolerant: if no daemon is listening, spawn one and wait for
/// readiness (model load + first index pass can take minutes — that's the
/// daemon's READY_TIMEOUT, not ours; the tool call just takes that long
/// once and is instant afterwards).
async fn forward(
    config_path: Option<std::path::PathBuf>,
    method: &str,
    params: serde_json::Value,
) -> Result<CallToolResult, McpError> {
    let method = method.to_string();
    let response = tokio::task::spawn_blocking(move || {
        // First call (or after a daemon death) pays the cold start; later
        // calls find the daemon listening and skip straight to the request.
        crate::daemon::ensure_daemon(config_path.as_deref())
            .map_err(|e| anyhow::anyhow!("daemon cold start failed: {e}"))?;
        admin_request(&method, params)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("daemon request failed: {e}"), None))?;

    if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "error: {err}"
        ))]));
    }
    let result = &response["result"];
    let text = serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[tool_handler]
impl ServerHandler for KnowMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Hybrid keyword + vector search over archived code and documents.\n\n\
             Start with search (mode auto). Use read_file for exact context with line \
             numbers."
                    .to_string(),
            )
    }
}

/// Serve MCP over stdio (called from `know-mcp mcp`).
pub async fn serve_stdio(config_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    // Logs to stderr only; stdout carries MCP.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = KnowMcpServer::with_config_path(config_path);
    let service = serve_server(server, (tokio::io::stdin(), tokio::io::stdout())).await?;
    service.waiting().await?;
    Ok(())
}
