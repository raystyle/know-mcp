//! Integration test: native document extraction (reader pipeline) against
//! the committed PDF fixture. No embedding model required — this guards
//! the pdf-inspector path (text layer, page units) and the format tables.
//! OCR paths need the PP-OCRv6 models and are covered by manual probes;
//! the extraction contract itself is model-free and stays guarded here.

fn corpus() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus")
}

#[test]
fn pdf_fixture_extracts_text_layer() {
    let pdf = corpus().join("docs/hybrid_baseline.pdf");
    let units = know_core::reader::extract(&pdf, false).expect("pdf extraction");
    assert!(!units.is_empty(), "fixture PDF must yield units");
    let text: Vec<&str> = units
        .iter()
        .flat_map(|u| u.lines.iter().map(String::as_str))
        .collect();
    let joined = text.join("\n");
    assert!(
        joined.contains("Hybrid Search Baseline"),
        "title line missing: {joined:?}"
    );
    assert!(
        joined.contains("quantization"),
        "body keyword missing (search fixtures depend on it): {joined:?}"
    );
    // Text-layer page: no OCR needed, no needs_ocr flag.
    assert!(
        units.iter().all(|u| u.needs_ocr.is_none()),
        "text-layer page must not be flagged needs_ocr: {units:?}"
    );
    // Page contract: unit kind is page, lines non-empty.
    assert_eq!(units[0].kind, "page");
    assert!(!units[0].lines.is_empty());
}

#[test]
fn extract_text_joins_units() {
    let pdf = corpus().join("docs/hybrid_baseline.pdf");
    let text = know_core::reader::extract_text(&pdf, false).expect("pdf extract_text");
    assert!(text.contains("fused keyword and vector retrieval"));
}

#[test]
fn unsupported_format_errors() {
    let fake = corpus().join("README.md"); // md is NOT a reader format (text path owns it)
    assert!(know_core::reader::extract(&fake, false).is_err());
}

#[test]
fn format_dispatch_table() {
    assert!(know_core::reader::is_reader_format("pdf"));
    assert!(know_core::reader::is_reader_format("docx"));
    assert!(know_core::reader::is_reader_format("epub"));
    assert!(know_core::reader::is_reader_format("png"));
    assert!(!know_core::reader::is_reader_format("md"));
    assert!(!know_core::reader::is_reader_format("rs"));
    // Case-insensitivity is the caller's contract (ext lowercased in sync).
    assert!(know_core::reader::is_doc_format("csv"));
    assert!(!know_core::reader::is_doc_format("exe"));
}

/// Office-family smoke: anydoc engine against a minimal real docx.
/// Built here (zip of minimal OOXML) so the fixture corpus stays model-
/// and binary-free while the anydoc path is still guarded.
#[test]
fn docx_minimal_extracts_sections() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let docx = tmp.path().join("minimal.docx");
    let body = concat!(
        "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
        "<w:body>",
        "<w:p><w:r><w:t>SENTINEL_DOCX_TEXT delta gamma</w:t></w:r></w:p>",
        "</w:body></w:document>"
    );
    let content_types = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
        "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
        "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
        "<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
        "</Types>"
    );
    let rels = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>",
        "</Relationships>"
    );
    let file = std::fs::File::create(&docx).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", options).unwrap();
    zip.write_all(content_types.as_bytes()).unwrap();
    zip.start_file("_rels/.rels", options).unwrap();
    zip.write_all(rels.as_bytes()).unwrap();
    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(body.as_bytes()).unwrap();
    zip.finish().unwrap();

    let text = know_core::reader::extract_text(&docx, false).expect("docx extraction");
    assert!(
        text.contains("SENTINEL_DOCX_TEXT"),
        "anydoc must surface the paragraph text: {text:?}"
    );
    assert!(text.contains("delta"));
}
