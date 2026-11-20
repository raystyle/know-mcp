//! Example daemon module for smoke corpus.
pub fn ensure_daemon_ready(port: u16) -> bool {
    // probe -> spawn -> wait for readiness (cold start loads the model)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    false
}
