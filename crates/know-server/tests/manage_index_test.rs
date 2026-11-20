#![allow(clippy::disallowed_methods)] // test daemons are tracked children; the ban targets production code
//! manage_index E2E: root scoping bounded by allowed_roots, add/remove
//! lifecycle with data deletion, and the drop confirmation gate. Runs a
//! real daemon over the admin plane — these are the security-boundary
//! tests for the index-management surface.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

fn admin(port: u16, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let req = serde_json::json!({"id": 1, "method": method, "params": params});
    writeln!(stream, "{}", req).unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).expect("reply");
    serde_json::from_str(line.trim()).expect("json")
}

fn wait_progress_idle(port: u16, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "index pass never finished"
        );
        let st = admin(port, "status", serde_json::json!({}));
        let running = st["result"]["progress"]["running"]
            .as_bool()
            .unwrap_or(true);
        if !running {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

#[test_log::test]
fn manage_index_scopes_and_lifecycle() {
    test_support::kill_stray_daemons();
    // Phase budgets (measured 2026-11-20 on this machine, ~1s each warm):
    // generous headroom for a cold model load; a phase blowing its budget
    // means a leaked daemon or a hung pass — loud, not slow.
    use std::time::Duration;
    use test_support::with_phase;
    let Some(fixture) = with_phase("fixture", Duration::from_secs(10), || {
        common::Fixture::new(&[(
            "probe.md",
            "# boundary probe\nallowed_roots scope test 边界测试\n",
        )])
    }) else {
        return; // model absent — skip
    };
    // Second corpus inside the allowed boundary; a third OUTSIDE it.
    let extra = fixture.dir.join("extra-root");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(
        extra.join("extra.md"),
        "# extra root\nsecond corpus for add_root\n",
    )
    .unwrap();
    let outside = std::env::temp_dir().join(format!("know-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("secret.md"),
        "# outside\nmust never be indexed\n",
    )
    .unwrap();

    let config = common::write_config_with(&fixture, "");
    let mut daemon = std::process::Command::new(common::Fixture::daemon_exe())
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("KNOW_PORT_DIR", &fixture.port_dir)
        .env("RUST_LOG", "know_mcp=info")
        .stderr(std::fs::File::create(fixture.dir.join("daemon.log")).unwrap())
        .spawn()
        .expect("daemon");
    let port = with_phase("daemon_ready", Duration::from_secs(30), || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            common::wait_admin_ready(&fixture.port_dir, 240)
        }))
        .unwrap_or_else(|_| {
            let _ = daemon.kill();
            panic!("daemon not ready");
        })
    });
    fixture.track(&daemon);
    // No auto-index at startup: trigger the initial pass explicitly.
    with_phase("initial_index", Duration::from_secs(60), || {
        admin(port, "reindex", serde_json::json!({}));
        wait_progress_idle(port, 240);
    });

    // 1. list_roots: one active, two allowed.
    let listed = with_phase("list_roots", Duration::from_secs(5), || {
        admin(
            port,
            "manage_index",
            serde_json::json!({"action": "list_roots"}),
        )
    });
    assert!(!listed["result"]["roots"].as_array().unwrap().is_empty());
    assert!(listed["result"]["allowed_roots"].as_array().unwrap().len() >= 2);

    // 2. add_root OUTSIDE allowed_roots → rejected, never indexed.
    let denied = admin(
        port,
        "manage_index",
        serde_json::json!({
            "action": "add_root", "path": outside.display().to_string()
        }),
    );
    assert!(
        denied["error"].is_string(),
        "outside path must be rejected: {denied}"
    );

    // 3. add_root inside the boundary → accepted, indexed.
    let added = admin(
        port,
        "manage_index",
        serde_json::json!({
            "action": "add_root", "path": extra.display().to_string()
        }),
    );
    assert!(
        added["result"]["added"].is_string(),
        "add_root failed: {added}"
    );
    let docs = with_phase("add_root_indexed", Duration::from_secs(60), || {
        wait_progress_idle(port, 240);
        let st = admin(port, "status", serde_json::json!({}));
        st["result"]["doc_count"].as_u64().unwrap_or(0)
    });
    assert!(docs >= 2, "both roots indexed, doc_count={docs}");

    // 4. remove_root → data gone.
    let removed = admin(
        port,
        "manage_index",
        serde_json::json!({
            "action": "remove_root", "path": extra.display().to_string()
        }),
    );
    assert!(
        removed["result"]["removed"].is_string(),
        "remove_root failed: {removed}"
    );
    assert!(
        removed["result"]["docs_deleted"].as_u64().unwrap_or(0) >= 1,
        "chunks deleted: {removed}"
    );
    let after = with_phase("remove_root_dropped", Duration::from_secs(60), || {
        wait_progress_idle(port, 240);
        let st = admin(port, "status", serde_json::json!({}));
        st["result"]["doc_count"].as_u64().unwrap_or(0)
    });
    assert_eq!(
        after,
        docs - removed["result"]["docs_deleted"].as_u64().unwrap_or(0)
    );

    // 5. drop without confirm → refused; with confirm → emptied.
    let refused = admin(port, "manage_index", serde_json::json!({"action": "drop"}));
    assert!(
        refused["error"].is_string(),
        "drop needs confirm: {refused}"
    );
    let dropped = admin(
        port,
        "manage_index",
        serde_json::json!({"action": "drop", "confirm": true}),
    );
    assert_eq!(
        dropped["result"]["dropped"],
        serde_json::json!(true),
        "drop failed: {dropped}"
    );

    // 6. Persisted config survived: the ACTIVE roots array dropped the
    // extra root, while allowed_roots (the authorization list) keeps it.
    let raw = std::fs::read_to_string(&config).unwrap();
    let roots_line = raw
        .lines()
        .find(|l| l.trim_start().starts_with("roots = ["))
        .unwrap_or_default();
    assert!(
        !roots_line.contains("extra-root"),
        "removed root still in the active roots array:\n{roots_line}\n--- full config:\n{raw}"
    );
    assert!(raw.contains("allowed_roots"), "allowed_roots kept: {raw}");
    assert!(
        raw.contains("extra-root"),
        "authorization list keeps the allowed path: {raw}"
    );

    let _ = daemon.kill();
    let _ = daemon.wait(); // reap: no zombie
    let _ = std::fs::remove_dir_all(&outside);
}
