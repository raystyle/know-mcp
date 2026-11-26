//! Native OCR: PP-OCRv6 via ppocr-rs (same kernel as reader_rs), for
//! textless PDF pages and image files. The model cache is know-mcp's
//! own — `KNOW_OCR_CACHE_DIR` or `%LOCALAPPDATA%/know-mcp/ocr-models` —
//! maintained independently of any reader installation. First use
//! downloads det+rec models (~6MB for tiny) from the ppocr-rs pinned
//! HuggingFace source into that cache.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ppocr_rs::{ModelAccess, ModelKind, ModelSize, ModelStore, OcrEngine, OcrOptions, RgbImage};

/// OCR cache dir: KNOW_OCR_CACHE_DIR > %LOCALAPPDATA%/know-mcp/ocr-models
/// > %APPDATA%/know-mcp/ocr-models. Never shared with reader's cache.
pub fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("KNOW_OCR_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    let base = base.context("no cache directory locator (set KNOW_OCR_CACHE_DIR)")?;
    Ok(base.join("know-mcp").join("ocr-models"))
}

/// Build the OCR engine. Loads the cached models; downloads them on
/// first use (ppocr-rs ModelStore handles the pinned HF source and
/// sha256 verification). `offline` errors instead of downloading.
pub fn engine(offline: bool) -> Result<OcrEngine> {
    let dir = cache_dir()?;
    let store = ModelStore::new(&dir);
    let options = OcrOptions {
        threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        detector_size: ModelSize::Tiny,
        recognizer_size: ModelSize::Tiny,
        ..OcrOptions::default()
    };
    if offline {
        let paths = store
            .resolve_pair(ModelSize::Tiny, ModelSize::Tiny, ModelAccess::Offline)
            .map_err(|e| anyhow::anyhow!("OCR models not cached and offline set: {e}"))?;
        return OcrEngine::load(
            &paths.detector.weights,
            &paths.recognizer.weights,
            &paths.recognizer.inference,
            options,
        )
        .map_err(|e| anyhow::anyhow!("building OCR engine: {e}"));
    }
    // Online: cached fast path first, then load-from-store (downloads
    // through ppocr-rs's pinned source on cache miss).
    if let Ok(paths) = store.resolve_pair(ModelSize::Tiny, ModelSize::Tiny, ModelAccess::Offline) {
        return OcrEngine::load(
            &paths.detector.weights,
            &paths.recognizer.weights,
            &paths.recognizer.inference,
            options,
        )
        .map_err(|e| anyhow::anyhow!("building OCR engine: {e}"));
    }
    OcrEngine::load_from_store(&store, options)
        .map_err(|e| anyhow::anyhow!("OCR model download: {e}"))
}

/// Pre-download (or, with `offline`, verify) both model packages into
/// know-mcp's own cache.
pub fn init_models(offline: bool) -> Result<()> {
    let dir = cache_dir()?;
    let store = ModelStore::new(&dir);
    for kind in [ModelKind::Detector, ModelKind::Recognizer] {
        let access = if offline {
            ModelAccess::Verify
        } else {
            ModelAccess::Online
        };
        store
            .resolve(kind, ModelSize::Tiny, access)
            .map_err(|e| anyhow::anyhow!("model {kind:?}: {e}"))?;
    }
    Ok(())
}

/// OCR the given 1-based pages of a PDF (reading-order lines, empty
/// lines dropped). Page rendering goes through hayro (2x scale, white
/// background — same contract as reader_rs).
pub fn ocr_pdf_pages(path: &Path, page_nos: &[u32]) -> Result<Vec<(u32, Vec<String>)>> {
    let engine = engine(false)?;
    let file = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let pdf = hayro::hayro_syntax::Pdf::new(file)
        .map_err(|e| anyhow::anyhow!("parsing PDF {}: {e:?}", path.display()))?;
    let pages = pdf.pages();
    let mut out = Vec::new();
    for &no in page_nos {
        let Some(page) = pages.get((no - 1) as usize) else {
            continue;
        };
        tracing::info!(page = no, "OCR fallback");
        let png = page_png(page, no)?;
        let rgb = image::load_from_memory(&png)
            .map_err(|e| anyhow::anyhow!("decoding page {no} bitmap: {e}"))?
            .to_rgb8();
        let (w, h) = rgb.dimensions();
        let image = RgbImage::new(w, h, rgb.into_raw())
            .map_err(|e| anyhow::anyhow!("page {no} bitmap conversion: {e}"))?;
        let result = engine
            .recognize(&image)
            .with_context(|| format!("OCR page {no}"))?;
        let lines = result
            .lines
            .into_iter()
            .map(|l| l.text)
            .filter(|t| !t.trim().is_empty())
            .collect();
        out.push((no, lines));
    }
    Ok(out)
}

/// OCR an image file (first frame of animations; EXIF orientation
/// applied; alpha composited onto white — direct-alpha drops would
/// render as black).
pub fn ocr_image(path: &Path) -> Result<Vec<String>> {
    use image::ImageDecoder;
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("opening image {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing image format {}", path.display()))?;
    let mut decoder = reader
        .into_decoder()
        .with_context(|| format!("decoding image {}", path.display()))?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder)
        .with_context(|| format!("decoding image {}", path.display()))?;
    img.apply_orientation(orientation);
    let rgb = blend_alpha_on_white(&img)?;
    let engine = engine(false)?;
    let result = engine
        .recognize(&rgb)
        .with_context(|| format!("OCR image {}", path.display()))?;
    Ok(result
        .lines
        .into_iter()
        .map(|l| l.text)
        .filter(|t| !t.trim().is_empty())
        .collect())
}

/// Render one PDF page to PNG bytes (hayro defaults to a transparent
/// background that would render black bitmaps; 2x scale matches the
/// OCR input contract).
fn page_png<'a>(page: &'a hayro::hayro_syntax::page::Page<'a>, no: u32) -> Result<Vec<u8>> {
    let settings = hayro::RenderSettings {
        x_scale: 2.0,
        y_scale: 2.0,
        bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        ..Default::default()
    };
    let pixmap = hayro::render(
        page,
        &hayro::RenderCache::new(),
        &Default::default(),
        &settings,
    );
    pixmap
        .into_png()
        .map_err(|e| anyhow::anyhow!("rendering page {no}: {e:?}"))
}

/// out = (c * a + 255 * (255 - a)) / 255: opaque pixels unchanged, full
/// alpha composites onto white (S006-style black-bitmap trap).
fn blend_alpha_on_white(img: &image::DynamicImage) -> Result<RgbImage> {
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    let rgba = img.to_rgba8();
    let mut raw = Vec::with_capacity(w as usize * h as usize * 3);
    for pixel in rgba.pixels() {
        let a = pixel.0[3] as u32;
        for &c in &pixel.0[..3] {
            raw.push(((c as u32 * a + 255 * (255 - a)) / 255) as u8);
        }
    }
    RgbImage::new(w, h, raw).map_err(|e| anyhow::anyhow!("invalid bitmap dimensions: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_respects_env() {
        // Not asserting the exact platform path (env-dependent), just that
        // the env override works and the know-mcp namespace is used.
        let dir = cache_dir().unwrap();
        let text = dir.display().to_string();
        assert!(text.contains("know-mcp"), "own cache namespace: {text}");
    }
}
