#![allow(clippy::disallowed_methods)] // test daemons are tracked children
//! E2E: native self-registration. `register` copies the binary as
//! know-cli; the copy dispatches by exe name into the verb form; verbs
//! answer over the daemon admin plane with a --json envelope. Guards
//! the argv[0] routing that replaced mcp2cli.

mod common;

#[test_log::test]
fn register_copies_know_cli_and_verb_answers() {
    test_support::kill_stray_daemons();
    use std::time::Duration;
    use test_support::with_phase;

    let Some(fixture) = with_phase("fixture", Duration::from_secs(10), || {
        common::Fixture::new(&[("anchor.md", "# register anchor\n注册锚点\n")])
    }) else {
        return; // model absent — skip
    };

    // Isolated port dir: the daemon that know-cli cold-spawns binds
    // here (WMI echo), and its lock file gives us the PID for cleanup.
    let install_dir = fixture.dir.join("bin");
    std::fs::create_dir_all(&install_dir).unwrap();

    let registered = with_phase("register", Duration::from_secs(60), || {
        std::process::Command::new(common::Fixture::daemon_exe())
            .arg("register")
            .arg("--install-dir")
            .arg(&install_dir)
            .env("RUST_LOG", "warn")
            .output()
            .expect("running register")
    });
    assert!(
        registered.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let know_cli = install_dir.join("know-cli.exe");
    assert!(know_cli.is_file(), "know-cli copy missing");

    // know-cli get-status: verb dispatch + --json envelope + daemon
    // cold start (WMI-spawned know-cli.exe daemon, lazy model — the
    // port binds in milliseconds, no model wait).
    let status = with_phase("know_cli_get_status", Duration::from_secs(120), || {
        std::process::Command::new(&know_cli)
            .arg("get-status")
            .arg("--json")
            .arg("--config")
            .arg(&fixture.config)
            .env("KNOW_PORT_DIR", &fixture.port_dir)
            .env("RUST_LOG", "warn")
            .output()
            .expect("running know-cli get-status")
    });
    assert!(
        status.status.success(),
        "know-cli get-status failed: {}\nstdout: {}\nstderr: {}",
        status.status,
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("json envelope on stdout");
    assert_eq!(envelope["app"], "know-cli", "envelope app: {envelope}");
    assert_eq!(envelope["verb"], "status", "envelope verb: {envelope}");
    assert!(
        envelope["data"]["doc_count"].as_u64().is_some(),
        "envelope data.doc_count: {envelope}"
    );
    assert_eq!(
        envelope["data"]["roots"][0]
            .as_str()
            .map(|r| r.replace('\\', "/")),
        Some(fixture.corpus.display().to_string().replace('\\', "/")),
        "daemon picked up the fixture config"
    );

    // register --force overwrites; without it, refuses.
    let refused = std::process::Command::new(common::Fixture::daemon_exe())
        .arg("register")
        .arg("--install-dir")
        .arg(&install_dir)
        .env("RUST_LOG", "warn")
        .output()
        .expect("register without force");
    assert!(
        !refused.status.success(),
        "re-register without --force must refuse"
    );

    // Cleanup: the WMI-spawned know-cli.exe daemon is not a tracked
    // child — kill it by the PID in the lock file.
    let lock = fixture.port_dir.join("daemon.lock");
    if let Ok(pid) = std::fs::read_to_string(&lock) {
        if let Ok(pid) = pid.trim().parse::<u32>() {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}
