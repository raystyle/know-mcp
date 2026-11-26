//! Document extraction: PDF / Word / EPUB / Office family / images, with
//! PP-OCRv6 OCR fallback for textless pages. Architecture inherited from
//! reader_rs (raystyle/reader_rs): pdf-inspector's markdown layout
//! pipeline for PDF, anydoc's unified engine for the office family, and
//! ppocr-rs as the native OCR kernel. Model cache is know-mcp's own
//! (separate from reader's; see `cache_dir`).

use std::path::Path;

use anyhow::{Context, Result};

/// One extracted unit: a PDF page, a heading section, or a 200-line part.
#[derive(Debug, Clone)]
pub struct TextUnit {
    /// 1-based unit number (PDF page / section ordinal).
    pub no: u32,
    /// Extraction kind (diagnostic label).
    pub kind: &'static str,
    /// Lines in reading order.
    pub lines: Vec<String>,
    /// Some(reason) when the text layer was unreliable and lines came
    /// from (or would need) OCR.
    pub needs_ocr: Option<String>,
}

/// Document-family extensions handled by the anydoc engine.
pub fn is_doc_format(ext: &str) -> bool {
    anydoc::Format::from_extension(ext).is_some()
}

/// Image extensions (no text layer; OCR-only when enabled).
pub fn is_image_format(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tiff" | "tif"
    )
}

/// Anything the reader pipeline handles (docs + images, not markdown —
/// that stays on the plain-text path).
pub fn is_reader_format(ext: &str) -> bool {
    ext == "pdf" || is_doc_format(ext) || is_image_format(ext)
}

/// Extract a document into units. `ocr` backfills textless pages via
/// PP-OCRv6 (models download into know-mcp's own cache on first use).
pub fn extract(path: &Path, ocr: bool) -> Result<Vec<TextUnit>> {
    let ext = ext_of(path);
    if ext == "pdf" {
        return extract_pdf(path, ocr);
    }
    if is_image_format(&ext) {
        return extract_image(path, ocr);
    }
    if is_doc_format(&ext) {
        return extract_office(path);
    }
    anyhow::bail!("unsupported document format: {}", path.display())
}

/// PDF: pdf-inspector layout pipeline (reading order, needs_ocr
/// detection), then OCR backfill for unreliable pages.
fn extract_pdf(path: &Path, ocr: bool) -> Result<Vec<TextUnit>> {
    let result = pdf_inspector::extract_pages_markdown(path, None)
        .with_context(|| format!("reading PDF {}", path.display()))?;
    let mut units: Vec<TextUnit> = result
        .pages
        .into_iter()
        .map(|page| TextUnit {
            no: page.page + 1,
            kind: "page",
            lines: page.markdown.lines().map(str::to_string).collect(),
            needs_ocr: page
                .needs_ocr
                .then(|| page.ocr_reason.unwrap_or_else(|| "unknown".to_string())),
        })
        .collect();
    if ocr {
        let bad: Vec<u32> = units
            .iter()
            .filter(|u| u.needs_ocr.is_some())
            .map(|u| u.no)
            .collect();
        if !bad.is_empty() {
            let backfilled = crate::ocr::ocr_pdf_pages(path, &bad)?;
            for (no, lines) in backfilled {
                if let Some(unit) = units.iter_mut().find(|u| u.no == no) {
                    if !lines.is_empty() {
                        unit.lines = lines;
                    }
                }
            }
        }
    }
    Ok(units)
}

/// Office family: anydoc unified engine → markdown → heading sections
/// (200-line parts for heading-less/overlong sections).
fn extract_office(path: &Path) -> Result<Vec<TextUnit>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading document {}", path.display()))?;
    let format = anydoc::Format::from_bytes(&bytes)
        .or_else(|| anydoc::Format::from_path(path))
        .with_context(|| format!("unrecognized document format: {}", path.display()))?;
    let markdown = anydoc::to_markdown_bytes(&bytes, format)
        .with_context(|| format!("parsing document {}", path.display()))?;
    Ok(sections_to_units(&markdown))
}

/// Image: one unit, no text layer — lines from OCR when enabled.
fn extract_image(path: &Path, ocr: bool) -> Result<Vec<TextUnit>> {
    let lines = if ocr {
        crate::ocr::ocr_image(path)?
    } else {
        Vec::new()
    };
    Ok(vec![TextUnit {
        no: 1,
        kind: "page",
        lines,
        needs_ocr: Some("image".to_string()),
    }])
}

/// Plain-text render (units joined by blank lines; line numbers come
/// from the chunker, which counts real text lines).
pub fn extract_text(path: &Path, ocr: bool) -> Result<String> {
    let units = extract(path, ocr)?;
    Ok(units
        .into_iter()
        .filter(|u| !u.lines.is_empty())
        .map(|u| u.lines.join("\n"))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// Markdown → (sections, has_heading); headings split sections, fenced
/// code blocks keep `#` lines inert, empty sections dropped. (Reader_rs
/// split_markdown, same contract.)
fn split_markdown(markdown: &str) -> (Vec<Vec<String>>, bool) {
    let mut sections: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut has_heading = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence && is_atx_heading(trimmed) {
            has_heading = true;
            sections.push(std::mem::take(&mut current));
        }
        current.push(line.trim_end().to_string());
    }
    sections.push(current);
    let sections = sections
        .into_iter()
        .map(|lines| lines.into_iter().filter(|l| !l.is_empty()).collect())
        .filter(|lines: &Vec<String>| !lines.is_empty())
        .collect();
    (sections, has_heading)
}

const PART_LINE_BUDGET: usize = 200;

fn sections_to_units(markdown: &str) -> Vec<TextUnit> {
    let (sections, has_heading) = split_markdown(markdown);
    let bodies: Vec<(&'static str, Vec<String>)> = if !has_heading {
        chunk_lines(sections.into_iter().next().unwrap_or_default())
            .into_iter()
            .map(|l| ("part", l))
            .collect()
    } else {
        sections
            .into_iter()
            .flat_map(|lines| {
                if lines.len() > PART_LINE_BUDGET {
                    chunk_lines(lines)
                        .into_iter()
                        .map(|l| ("part", l))
                        .collect::<Vec<_>>()
                } else {
                    vec![("section", lines)]
                }
            })
            .collect()
    };
    bodies
        .into_iter()
        .enumerate()
        .map(|(i, (kind, lines))| TextUnit {
            no: i as u32 + 1,
            kind,
            lines,
            needs_ocr: None,
        })
        .collect()
}

fn chunk_lines(lines: Vec<String>) -> Vec<Vec<String>> {
    if lines.is_empty() {
        return Vec::new();
    }
    lines
        .chunks(PART_LINE_BUDGET)
        .map(<[String]>::to_vec)
        .collect()
}

/// GFM ATX heading: 1–6 leading `#` then space or EOL.
fn is_atx_heading(trimmed: &str) -> bool {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return false;
    }
    let rest = &trimmed[hashes..];
    rest.is_empty() || rest.starts_with([' ', '\t'])
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tables() {
        assert!(is_doc_format("docx"));
        assert!(is_doc_format("epub"));
        assert!(is_doc_format("csv"));
        assert!(is_reader_format("pdf"));
        assert!(!is_reader_format("md"));
        assert!(!is_reader_format("rs"));
        assert!(is_image_format("png"));
        assert!(!is_image_format("svg"));
    }

    #[test]
    fn markdown_splits_at_top_level_headings() {
        let (units, has_heading) = split_markdown("# Title\npara one\n\n## Sub\npara two\n");
        assert!(has_heading);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0][0], "# Title");
    }

    #[test]
    fn hash_inside_fence_stays_in_section() {
        let md = "# Title\n```rust\n# not a heading\nlet x = 1;\n```\nafter\n";
        let (units, has_heading) = split_markdown(md);
        assert!(has_heading);
        assert_eq!(units.len(), 1);
        assert!(units[0].contains(&"# not a heading".to_string()));
    }

    #[test]
    fn headingless_document_is_parts() {
        let units = sections_to_units("line one\n\nline two\n");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "part");
        assert_eq!(units[0].lines.len(), 2);
    }

    #[test]
    fn overlong_section_splits_into_parts() {
        let long: Vec<String> = (0..450).map(|i| format!("l{i}")).collect();
        let md = format!("# A\nx\n\n# B\n{}\n\n# C\ny\n", long.join("\n"));
        let units = sections_to_units(&md);
        assert_eq!(units.len(), 5, "short + long(3 parts) + short");
        assert_eq!(units[0].kind, "section");
        assert_eq!(units[1].kind, "part");
        assert_eq!(units[4].kind, "section");
    }

    #[test]
    fn atx_requires_space_or_end() {
        assert!(is_atx_heading("# a"));
        assert!(is_atx_heading("###### a"));
        assert!(!is_atx_heading("####### a"));
        assert!(!is_atx_heading("#a"));
        assert!(is_atx_heading("#"));
    }

    #[test]
    fn empty_markdown_yields_no_parts() {
        let units = sections_to_units("");
        assert!(units.is_empty());
    }
}
