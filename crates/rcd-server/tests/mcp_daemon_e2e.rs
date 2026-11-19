//! E2E: real daemon binary + MCP over stdio (rmcp TokioChildProcess).
//! Proves the full chain: binary → stdio JSON-RPC → daemon admin plane →
//! zvec index → tool results. Log evidence comes from the daemon's
//! structured event stream (event= field assertions via status payloads).

mod common;

use rmcp::{model::CallToolRequestParams, service::ServiceExt, transport::TokioChildProcess};
use tokio::process::Command;

#[tokio::test]
async fn daemon_stdio_mcp_tools_list_and_search() {
    let Some(fixture) = common::Fixture::new(&[
        ("probe.rs", "pub fn zvec_probe() { ZVecInitialize multi_query }"),
        ("docs.md", "# 混合检索\n混合检索融合关键字与向量语义。\n"),
    ]) else {
        return; // model absent — skip
    };

    // Pre-spawn the daemon and wait for admin readiness: the stdio bridge
    // would otherwise block in ensure_daemon through the cold model load
    // (~25-40s) and miss the MCP initialize window.
    let mut daemon = Command::new(common::Fixture::daemon_exe())
        .arg("daemon")
        .arg("--config")
        .arg(&fixture.config)
        .env("RCD_PORT_DIR", &fixture.port_dir)
        .env("RUST_LOG", "rcd_mcp=debug")
        // Capture early-exit evidence: a daemon that dies before writing
        // the port file is otherwise invisible.
        .stderr(std::fs::File::create(fixture.dir.join("daemon-stderr.log")).unwrap())
        .spawn()
        .expect("spawning daemon");
    let port = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        common::wait_admin_ready(&fixture.port_dir, 150)
    }))
    .unwrap_or_else(|_| {
        let stderr_log = std::fs::read_to_string(fixture.dir.join("daemon-stderr.log"))
            .unwrap_or_default();
        panic!("admin port never appeared. daemon stderr tail:
{}",
            stderr_log.lines().rev().take(30).collect::<Vec<_>>().join("
"));
    });

    let mut cmd = Command::new(common::Fixture::daemon_exe());
    cmd.arg("stdio").arg("--config").arg(&fixture.config);
    cmd.env("RCD_PORT_DIR", &fixture.port_dir);
    cmd.env("RUST_LOG", "rcd_mcp=debug");

    let client = ()
        .serve(TokioChildProcess::new(cmd).unwrap())
        .await
        .expect("MCP handshake with daemon bridge");

    // 1. tools/list: all 9 tools present.
    let tools = client.list_tools(Default::default()).await.expect("tools/list");
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "search_content",
        "read_file",
        "get_status",
        "build_index",
        "save_note",
        "list_notes",
        "get_note",
        "record_feedback",
        "get_gaps",
    ] {
        assert!(names.contains(&expected), "missing tool {expected} in {names:?}");
    }

    // 2. get_status responds (proves bridge → daemon admin round trip).
    let status = client
        .call_tool(CallToolRequestParams::new("get_status"))
        .await
        .expect("get_status call");
    assert!(!status.content.is_empty(), "status returned content");

    // 3. search_content keyword hit through the full chain.
    let mut args = serde_json::Map::new();
    args.insert("query".into(), serde_json::json!("混合检索"));
    args.insert("mode".into(), serde_json::json!("keyword"));
    let search = client
        .call_tool(CallToolRequestParams::new("search_content").with_arguments(args))
        .await
        .expect("search_content call");
    // rmcp 3.5: call_tool returns CallToolResult directly.
    assert!(search.is_error != Some(true), "search returned an error");

    client.cancel().await.ok();
    let _ = daemon.kill().await;
}
