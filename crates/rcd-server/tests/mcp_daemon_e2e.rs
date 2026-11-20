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

    // 1. tools/list: the 4 search/read/status/index tools present.
    let tools = client.list_tools(Default::default()).await.expect("tools/list");
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "search_content",
        "read_file",
        "get_status",
        "build_index",
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

/// Cold-start E2E: NO daemon pre-spawned. The stdio bridge must spawn one
/// via ensure_daemon on the first tool call, wait out the model load (this
/// is the path that used to fail: admin_request without ensure_daemon, and
/// the 120s ready window), and answer a real search. This is the
/// first-MCP-connection-after-boot user experience.
#[tokio::test]
async fn cold_start_stdio_bridge_spawns_daemon_and_searches() {
    let Some(fixture) = common::Fixture::new(&[
        ("probe.rs", "pub fn zvec_probe() { ZVecInitialize multi_query }"),
        ("docs.md", "# 冷启动测试\nstdio bridge spawns the daemon lazily.\n"),
    ]) else {
        return; // model absent — skip
    };
    // Assert really cold: no port file, no daemon.
    assert!(!fixture.port_dir.join("admin.port").exists(), "port file must not pre-exist");

    let mut cmd = Command::new(common::Fixture::daemon_exe());
    cmd.arg("stdio").arg("--config").arg(&fixture.config);
    cmd.env("RCD_PORT_DIR", &fixture.port_dir);
    cmd.env("RUST_LOG", "rcd_mcp=info");
    cmd.stderr(std::fs::File::create(fixture.dir.join("cold-stderr.log")).unwrap());

    let client = ()
        .serve(TokioChildProcess::new(cmd).unwrap())
        .await
        .expect("MCP handshake (must not require the daemon)");

    // First tool call pays the cold start: ensure_daemon → model load →
    // ready → search. Up to ~4 min on a cold machine (READY_TIMEOUT=300s);
    // rmcp's per-call budget needs to outlive it, so retry until the
    // daemon is up rather than hanging the whole call.
    let t0 = std::time::Instant::now();
    let mut last_err = String::new();
    let ok = loop {
        if let Some(text) = client
            .call_tool(CallToolRequestParams::new("get_status"))
            .await
            .ok()
            .and_then(|r| r.content.first().cloned())
        {
            let _ = text;
            break true;
        } else {
            last_err = "get_status call failed".into();
        }
        if t0.elapsed() > std::time::Duration::from_secs(300) {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    };
    assert!(ok, "cold-start get_status never succeeded: {last_err}; stderr: {}",
        std::fs::read_to_string(fixture.dir.join("cold-stderr.log")).unwrap_or_default()
            .lines().rev().take(15).collect::<Vec<_>>().join("\n"));

    // Daemon is up; a real search must work through the whole chain.
    let mut args = serde_json::Map::new();
    args.insert("query".into(), serde_json::json!("冷启动测试"));
    args.insert("mode".into(), serde_json::json!("keyword"));
    let search = client
        .call_tool(CallToolRequestParams::new("search_content").with_arguments(args))
        .await
        .expect("search after cold start");
    assert!(search.is_error != Some(true), "search returned an error");

    client.cancel().await.ok();
}
