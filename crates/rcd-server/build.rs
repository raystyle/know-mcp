fn main() {
    // Stages the zvec_c_api shared library next to the built artifacts
    // (on Windows: copies zvec_c_api.dll into target/<profile>/ so the
    // exe runs without PATH setup; on unix: emits rpath instead).
    zvec_rust_build::configure();
}
