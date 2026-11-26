#![allow(clippy::disallowed_methods)] // test daemons are tracked children
//! E2E: the native reader pipeline inside the real index flow. The
//! committed text-layer PDF joins a fixture corpus; after a daemon
//! reindex, keyword search must hit it — guarding pdf-inspector
//! extraction → chunk → embed → store → search end to end.

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
        if !st["result"]["progress"]["running"]
            .as_bool()
            .unwrap_or(true)
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

#[test_log::test]
fn pdf_text_layer_is_indexed_and_searchable() {
    test_support::kill_stray_daemons();
    use std::time::Duration;
    use test_support::with_phase;

    let Some(fixture) = with_phase("fixture", Duration::from_secs(10), || {
        common::Fixture::new(&[("plain.md", "# plain anchor\n普通文本锚点\n")])
    }) else {
        return; // model absent — skip
    };

    // The committed text-layer PDF joins the corpus (binary: copy, not
    // the text-file constructor).
    let pdf_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../know-core/tests/fixtures/corpus/docs/hybrid_baseline.pdf");
    let pdf_dst = fixture.corpus.join("hybrid_baseline.pdf");
    std::fs::copy(&pdf_src, &pdf_dst).expect("copying pdf fixture");

    let mut daemon = std::process::Command::new(common::Fixture::daemon_exe())
        .arg("daemon")
        .arg("--config")
        .arg(&fixture.config)
        .env("KNOW_PORT_DIR", &fixture.port_dir)
        .env("RUST_LOG", "know_mcp=info")
        .stderr(std::fs::File::create(fixture.dir.join("daemon.log")).unwrap())
        .spawn()
        .expect("daemon");
    let port = with_phase("daemon_ready", Duration::from_secs(30), || {
        common::wait_admin_ready(&fixture.port_dir, 240)
    });
    fixture.track(&daemon);

    with_phase("index_pdf", Duration::from_secs(90), || {
        admin(port, "reindex", serde_json::json!({}));
        wait_progress_idle(port, 240);
    });

    // Keyword hit on the PDF's body text (only the extraction pipeline
    // can produce this content — the raw bytes are a binary container).
    let hit = with_phase("search_pdf", Duration::from_secs(10), || {
        admin(
            port,
            "search",
            serde_json::json!({"query": "quantization", "mode": "keyword", "top_k": 5}),
        )
    });
    let hits = hit["result"]["hits"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        hits.iter().any(|h| h["path"]
            .as_str()
            .unwrap_or_default()
            .contains("hybrid_baseline.pdf")),
        "keyword search must hit the indexed PDF: {hit}"
    );

    // The plain-text fixture still indexes alongside the document
    // pipeline (regression: mixed corpora).
    let mixed = admin(
        port,
        "search",
        serde_json::json!({"query": "锚点", "mode": "keyword", "top_k": 5}),
    );
    assert!(
        mixed["result"]["hits"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "plain-text fixture must remain searchable: {mixed}"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}
