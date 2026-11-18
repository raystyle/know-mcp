fn main() {
    // Stages the zvec_c_api shared library next to the built artifacts
    // (on Windows: copies zvec_c_api.dll into target/<profile>/ so the
    // exe runs without PATH setup; on unix: emits rpath instead).
    zvec_rust_build::configure();

    // Stage dependency DLLs archived in the project's vendor/dlls/ (created
    // by the developer; gitignored because NVIDIA's cublasLt exceeds
    // GitHub's 100MB file limit). No system paths are guessed — the runtime
    // side (RCD_LIB_DIR / .env) covers arbitrary locations.
    #[cfg(target_os = "windows")]
    stage_vendor_dlls();
}

#[cfg(target_os = "windows")]
fn stage_vendor_dlls() {
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else { return };
    let vendor = std::path::Path::new(&manifest_dir)
        .ancestors()
        .nth(2)
        .map(|p| p.join("vendor").join("dlls"));
    let Some(vendor) = vendor else { return };
    if !vendor.is_dir() {
        println!("cargo:warning=vendor/dlls/ absent — put runtime DLLs there (or set RCD_LIB_DIR in .env) for self-contained runs");
        return;
    }

    let Some(out_dir) = std::env::var_os("OUT_DIR") else { return };
    // target/<profile>/ is three levels above OUT_DIR.
    let profile_dir = std::path::Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();

    let mut staged = 0;
    for entry in std::fs::read_dir(&vendor).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let dst = profile_dir.join(entry.file_name());
        if std::fs::copy(&path, &dst).is_ok() {
            staged += 1;
        }
    }
    if staged > 0 {
        println!("cargo:warning=staged {staged} DLLs from vendor/dlls next to the binary");
    }
}
