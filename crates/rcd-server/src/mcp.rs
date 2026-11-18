//! MCP ServerHandler with 8 tools; tool calls forward to the daemon admin
//! plane (the bridge process itself stays light — no model, no index).

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, serve_server,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::sync::Arc;

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
    /// "archive" (source repos/docs only) or "note" (knowledge notes only).
    #[serde(default)]
    pub kind_filter: Option<String>,
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
pub struct SaveNoteArgs {
    /// Note title.
    pub title: String,
    /// Note body (markdown).
    pub body: String,
    /// Tags for retrieval.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Archive file paths this conclusion came from.
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    /// The original query that led to this note.
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListNotesArgs {
    /// Filter by tag.
    #[serde(default)]
    pub tag: Option<String>,
    /// Max notes (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetNoteArgs {
    /// Note id, e.g. KB-20260101-001.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordFeedbackArgs {
    /// The note id the feedback is about.
    pub note_id: String,
    /// true = the note helped solve the problem, false = it did not.
    pub helpful: bool,
    /// Optional comment.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetGapsArgs {
    /// Max gap entries, newest first (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone)]
pub struct RcdMcpServer {
    tool_router: ToolRouter<RcdMcpServer>,
    config_path: Option<std::path::PathBuf>,
}

impl RcdMcpServer {
    pub fn new() -> Self {
        Self { tool_router: Self::tool_router(), config_path: None }
    }

    pub fn with_config_path(config_path: Option<std::path::PathBuf>) -> Self {
        Self { tool_router: Self::tool_router(), config_path }
    }
}

const MAX_LINES: u64 = 2000;
const MAX_BYTES: usize = 256 * 1024;

#[tool_router]
impl RcdMcpServer {
    /// Hybrid keyword+vector search over archived repos, docs, and notes.
    #[tool(description = "Search archived code, documents, and knowledge notes. Modes: keyword (exact/FTS, best for identifiers, CVE ids, Chinese), vector (semantic), hybrid (fused, default via auto). Results carry kind: archive (source of truth) or note (distilled solution with helpful score). Pass path_filter to scope to a subtree; kind_filter for one resource kind.")]
    async fn search_content(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "query": args.query,
            "mode": args.mode.as_deref().unwrap_or("auto"),
            "top_k": args.top_k.unwrap_or(10),
            "path_filter": args.path_filter,
            "kind_filter": args.kind_filter,
        });
        forward("search", params).await
    }

    /// Read a file under a configured root with line numbers.
    #[tool(description = "Read a file from the archive with 1-based line numbers. Optional start_line/end_line; caps at 2000 lines or 256KB. Paths outside the configured roots are rejected.")]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = std::path::PathBuf::from(&args.path);
        let canonical = dunce::canonicalize(&path).map_err(|e| {
            McpError::invalid_params(format!("cannot resolve path {}: {e}", args.path), None)
        })?;

        // Root allowlist guard: canonical path must sit under a configured root.
        let config = rcd_core::config::Config::load(self.config_path.as_deref()).map_err(|e| {
            McpError::internal_error(format!("loading config: {e}"), None)
        })?;
        let allowed = config
            .index
            .roots
            .iter()
            .chain(std::iter::once(&config.notes.dir))
            .any(|root| {
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

        let content = std::fs::read_to_string(&canonical)
            .map_err(|e| McpError::invalid_params(format!("reading {}: {e}", canonical.display()), None))?;
        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(start + MAX_LINES - 1).min(start + MAX_LINES - 1);

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
    #[tool(description = "Index and daemon status: doc count, note count, model, roots, indexing progress.")]
    async fn get_status(&self) -> Result<CallToolResult, McpError> {
        forward("status", serde_json::json!({})).await
    }

    /// Trigger a background reindex.
    #[tool(description = "Start a background incremental reindex (or full re-embed with force=true). Returns immediately; watch progress via get_status.")]
    async fn build_index(
        &self,
        Parameters(args): Parameters<ReindexArgs>,
    ) -> Result<CallToolResult, McpError> {
        forward("reindex", serde_json::json!({"force": args.force.unwrap_or(false)})).await
    }

    /// Save a knowledge note distilled from archive hits.
    #[tool(description = "Distill a solved problem into a knowledge note (markdown under the knowledge dir). Notes are indexed immediately and appear in future searches. Record sources (archive paths) and the triggering query so future readers can trace provenance.")]
    async fn save_note(
        &self,
        Parameters(args): Parameters<SaveNoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let params = serde_json::json!({
            "title": args.title,
            "body": args.body,
            "tags": args.tags.unwrap_or_default(),
            "sources": args.sources.unwrap_or_default(),
            "query": args.query,
        });
        forward("save_note", params).await
    }

    /// List knowledge notes.
    #[tool(description = "List knowledge notes with helpful/not-helpful scores, newest first.")]
    async fn list_notes(
        &self,
        Parameters(args): Parameters<ListNotesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = serde_json::json!({"limit": args.limit.unwrap_or(50)});
        if let Some(tag) = &args.tag {
            params["tag"] = serde_json::json!(tag);
        }
        forward("list_notes", params).await
    }

    /// Get one knowledge note.
    #[tool(description = "Read one knowledge note in full (id like KB-20260101-001), with its helpful score.")]
    async fn get_note(
        &self,
        Parameters(args): Parameters<GetNoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        forward("get_note", serde_json::json!({"id": args.id})).await
    }

    /// Record whether a note helped.
    #[tool(description = "Record whether a knowledge note actually helped solve the problem. helpful=true boosts the note in future searches; helpful=false demotes it. This is the feedback loop that makes the knowledge base smarter.")]
    async fn record_feedback(
        &self,
        Parameters(args): Parameters<RecordFeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = serde_json::json!({"note_id": args.note_id, "helpful": args.helpful});
        if let Some(c) = &args.comment {
            params["comment"] = serde_json::json!(c);
        }
        forward("record_feedback", params).await
    }

    /// Queries that found nothing — what to write notes about.
    #[tool(description = "List recent zero/low-result queries (knowledge gaps), newest first. Use these to decide which notes to write next.")]
    async fn get_gaps(
        &self,
        Parameters(args): Parameters<GetGapsArgs>,
    ) -> Result<CallToolResult, McpError> {
        forward("get_gaps", serde_json::json!({"limit": args.limit.unwrap_or(20)})).await
    }
}

/// Forward a tool call to the daemon admin plane and wrap the reply.
async fn forward(method: &str, params: serde_json::Value) -> Result<CallToolResult, McpError> {
    let method = method.to_string();
    let response = tokio::task::spawn_blocking(move || admin_request(&method, params))
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("daemon request failed: {e}"), None))?;

    if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "error: {err}"
        ))]));
    }
    let result = &response["result"];
    let text = serde_json::to_string_pretty(result)
        .unwrap_or_else(|_| result.to_string());
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[tool_handler]
impl ServerHandler for RcdMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_instructions(
            "Hybrid search over archived code, documents, and knowledge notes.\n\
             \n\
             WORK LOOP: search (archive = source of truth, notes = distilled solutions \
             with helpful scores) → solve the problem → save_note with sources and the \
             original query → next time the note surfaces in search → after using it, \
             record_feedback whether it actually helped → get_gaps shows which queries \
             still find nothing (write those notes).\n\
             \n\
             Start with search (mode auto). Use read_file for exact context with line \
             numbers. Notes are markdown you can also read directly."
                .to_string(),
        )
    }
}

/// Serve MCP over stdio (called from `rcd-mcp mcp`).
pub async fn serve_stdio(config_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    // Logs to stderr only; stdout carries MCP.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = RcdMcpServer::new();
    let service = serve_server(server, (tokio::io::stdin(), tokio::io::stdout())).await?;
    service.waiting().await?;
    Ok(())
}

/// Shared constructor for the HTTP transport.
pub fn make_server() -> Arc<RcdMcpServer> {
    Arc::new(RcdMcpServer::new())
}
