#![allow(clippy::disallowed_methods)] // test daemons are tracked children; the ban targets production code
//! Single-instance mutex regression test.
//!
//! 2026-11-19 production race: two concurrent daemon spawners both passed
//! the probe-then-bind check (the window is the width of the double model
//! load, ~90s), both bound ephemeral ports (which never collide) and
//! overwrote each other's port file — two daemons coexisted (60340 and
//! 60505 observed). The fix is an atomic create-new lock file taken BEFORE
//! the model load; the loser exits immediately.
//!
//! This test starts one daemon, waits for readiness, then launches a
//! second against the SAME port dir and asserts it exits non-zero quickly
//! (mutual exclusion) while the first daemon stays alive and keeps its
//! port file.

mod common;

#[test]
fn second_daemon_exits_when_lock_held() {
    test_support::kill_stray_daemons();
    use std::time::Duration;
    use test_support::with_phase;
    let Some(fixture) =
        common::Fixture::new(&[("probe.rs", "pub fn mutex_probe() { ZVecInitialize }")])
    else {
        return; // model absent — skip
    };

    // First daemon: takes the lock, becomes ready.
    let mut first = std::process::Command::new(common::Fixture::daemon_exe())
        .arg("daemon")
        .arg("--config")
        .arg(&fixture.config)
        .env("KNOW_PORT_DIR", &fixture.port_dir)
        .env("RUST_LOG", "know_mcp=info")
        .stderr(std::fs::File::create(fixture.dir.join("first.log")).unwrap())
        .spawn()
        .expect("spawning first daemon");
    let _port = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        common::wait_admin_ready(&fixture.port_dir, 240)
    }))
    .unwrap_or_else(|_| {
        let log = std::fs::read_to_string(fixture.dir.join("first.log")).unwrap_or_default();
        panic!(
            "first daemon never became ready. log tail:\n{}",
            log.lines().rev().take(20).collect::<Vec<_>>().join("\n")
        );
    });

    // Second daemon against the same port dir: must refuse and exit fast
    // (the lock is checked before the model load — the whole point).
    let t0 = std::time::Instant::now();
    let mut second = std::process::Command::new(common::Fixture::daemon_exe())
        .arg("daemon")
        .arg("--config")
        .arg(&fixture.config)
        .env("KNOW_PORT_DIR", &fixture.port_dir)
        .stderr(std::fs::File::create(fixture.dir.join("second.log")).unwrap())
        .spawn()
        .expect("spawning second daemon");
    let status = loop {
        match second.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                assert!(
                    t0.elapsed() < std::time::Duration::from_secs(30),
                    "second daemon still running after 30s — mutex not enforced"
                );
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            Err(e) => panic!("try_wait: {e}"),
        }
    };
    assert!(
        !status.success(),
        "second daemon exited 0 — mutex not enforced"
    );
    let took = t0.elapsed();
    assert!(
        took < std::time::Duration::from_secs(30),
        "second daemon took {took:?} to refuse — lock check must precede the model load"
    );

    // First daemon must still hold the port file and answer connections.
    let port: u16 = std::fs::read_to_string(fixture.port_dir.join("admin.port"))
        .expect("port file survives")
        .trim()
        .parse()
        .expect("port parses");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "first daemon still serving"
    );

    // Stale-lock recovery: hard-kill the holder (no cleanup — the lock file
    // remains with a dead PID). A third daemon must take over the stale
    // lock and become ready rather than refusing forever.
    let _ = first.kill();
    let _ = first.wait();
    assert!(
        fixture.port_dir.join("daemon.lock").exists(),
        "hard kill leaves the stale lock behind"
    );
    let mut third = std::process::Command::new(common::Fixture::daemon_exe())
        .arg("daemon")
        .arg("--config")
        .arg(&fixture.config)
        .env("KNOW_PORT_DIR", &fixture.port_dir)
        .stderr(std::fs::File::create(fixture.dir.join("third.log")).unwrap())
        .spawn()
        .expect("spawning third daemon");
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        common::wait_admin_ready(&fixture.port_dir, 240)
    }))
    .unwrap_or_else(|_| {
        let log = std::fs::read_to_string(fixture.dir.join("third.log")).unwrap_or_default();
        panic!(
            "third daemon did not take over the stale lock. log tail:\n{}",
            log.lines().rev().take(20).collect::<Vec<_>>().join("\n")
        );
    });
    with_phase("stale_lock_takeover", Duration::from_secs(30), || {
        let _p = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            common::wait_admin_ready(&fixture.port_dir, 240)
        }));
    });
    let _ = third.kill();
    let _ = third.wait(); // reap: no zombie
    let _ = second.kill();
    let _ = second.wait();
}

/// The production race (2026-11-19): concurrent spawners, not sequential
/// ones. Four daemons launched in the same instant against one port dir —
/// exactly one may hold the lock; the rest must exit non-zero FAST (lock
/// check precedes the model load), and the winner must serve.
#[test]
fn concurrent_spawns_yield_exactly_one_daemon() {
    test_support::kill_stray_daemons();
    use std::time::Duration;
    use test_support::with_phase;
    let Some(fixture) =
        common::Fixture::new(&[("probe.rs", "pub fn race_probe() { ZVecInitialize }")])
    else {
        return; // model absent — skip
    };

    let mut children = Vec::new();
    for i in 0..4 {
        let child = std::process::Command::new(common::Fixture::daemon_exe())
            .arg("daemon")
            .arg("--config")
            .arg(&fixture.config)
            .env("KNOW_PORT_DIR", &fixture.port_dir)
            .env("RUST_LOG", "know_mcp=info")
            .stderr(std::fs::File::create(fixture.dir.join(format!("race-{i}.log"))).unwrap())
            .spawn()
            .expect("spawning racer");
        children.push(child);
    }

    // Losers exit non-zero within 30s (no model load on the refusal path).
    let t0 = std::time::Instant::now();
    let mut exited: Vec<(usize, std::process::ExitStatus)> = Vec::new();
    loop {
        for (i, c) in children.iter_mut().enumerate() {
            if exited.iter().any(|(j, _)| *j == i) {
                continue;
            }
            if let Some(status) = c.try_wait().expect("try_wait") {
                exited.push((i, status));
            }
        }
        let alive = 4 - exited.len();
        if alive == 1 {
            break; // exactly the winner still running
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(60),
            "racers did not settle to one winner: {}/4 exited ({:?})",
            exited.len(),
            exited
                .iter()
                .map(|(i, s)| (i, s.code()))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    for (_, status) in &exited {
        assert!(
            !status.success(),
            "a racer exited 0 — mutual exclusion broken"
        );
    }
    // The surviving daemon is reachable on the port file.
    with_phase("winner_serving", Duration::from_secs(30), || {
        let _port = common::wait_admin_ready(&fixture.port_dir, 240);
    });
    for c in children.iter_mut() {
        let _ = c.kill();
    }
}
