//! Character-based chunking with token-budget awareness (qmd pattern +
//! CJK adaptation) and context prepend.
//!
//! Token budget: chunks target `tokens_per_chunk` tokens. Char budget is
//! derived per text via a CJK-ratio heuristic (qmd's per-document
//! calibration, cheap form): CJK-dominant text ≈ 1.4 chars/token, ASCII
//! ≈ 4 chars/token. A fixed 1000-char budget overflowed embedding windows
//! on Chinese (700-1300 tokens) and under-filled on code (~250 tokens).
//!
//! Cuts snap back to the last newline past halfway; never inside a ```
//! code fence; overlap keeps boundary context. Callers may prepend a
//! context header (file path / heading breadcrumb / symbol chain) at
//! embed time — `contextualize` builds it.

/// Chunk parameters: token budget and overlap fraction.
#[derive(Debug, Clone, Copy)]
pub struct ChunkParams {
    /// Target tokens per chunk (qmd: 900; we default 600 — Qwen3 window
    /// is 32K but shorter chunks rank better in hybrid retrieval).
    pub tokens_per_chunk: usize,
    /// Overlap as a fraction of the chunk (e.g. 0.12 = 12%).
    pub overlap_fraction: f32,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self { tokens_per_chunk: 600, overlap_fraction: 0.12 }
    }
}

impl ChunkParams {
    pub fn new(tokens_per_chunk: usize, overlap_fraction: f32) -> Self {
        Self { tokens_per_chunk, overlap_fraction }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    /// Context header prepended at embed time (path/breadcrumb/symbol);
    /// stored separately so display text stays clean.
    pub context: String,
    /// 1-based inclusive line range in the source file.
    pub start_line: u64,
    pub end_line: u64,
}

/// Chars per token estimate from the CJK ratio of the text.
///
/// - ratio ≥ 0.5 (CJK-dominant): ~1.4 chars/token
/// - ratio = 0 (pure ASCII): ~4.0 chars/token
/// - linear blend in between
fn chars_per_token(text: &str) -> f32 {
    let total = text.chars().count();
    if total == 0 {
        return 4.0;
    }
    let cjk = text
        .chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
        .count();
    let ratio = cjk as f32 / total as f32;
    4.0 - (4.0 - 1.4) * ratio.min(1.0)
}

/// Derive the char budget for this text from the token budget.
fn char_budget(text: &str, params: ChunkParams) -> usize {
    (params.tokens_per_chunk as f32 * chars_per_token(text)).max(200.0) as usize
}

/// Split into overlapping chunks with newline snap and fence protection.
pub fn chunk_text(text: &str, params_ref: &ChunkParams) -> Vec<Chunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let budget = char_budget(text, *params_ref);
    let overlap_chars = (budget as f32 * params_ref.overlap_fraction) as usize;
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut pos = 0usize;

    while pos < chars.len() {
        let target = pos + budget;
        let mut end = target.min(chars.len());

        if end < chars.len() {
            if inside_fence(&chars[..end]) {
                if let Some(close) = next_fence_close(&chars, end.saturating_sub(1)) {
                    end = close.min(chars.len());
                }
            } else {
                let half = pos + budget / 2;
                if let Some(nl) = (half..end).rev().find(|&i| chars[i] == '\n') {
                    end = nl + 1;
                }
            }
        }

        let chunk_start = pos;
        let chunk_end = end.max(pos + 1).min(chars.len());
        let start_line = line_of(&chars, chunk_start);
        let end_line = line_of(&chars, chunk_end.saturating_sub(1));
        let slice: String = chars[chunk_start..chunk_end].iter().collect();
        chunks.push(Chunk {
            text: slice,
            context: String::new(),
            start_line,
            end_line,
        });

        if chunk_end >= chars.len() {
            break;
        }
        let next = chunk_end.saturating_sub(overlap_chars).max(chunk_start + 1);
        pos = next;
    }
    chunks
}

/// Prepend a context header to every chunk (embed-time only).
///
/// For markdown: the heading breadcrumb (mq path idea). For code: the
/// file path + language. The header is capped so it never crowds the
/// body (~15% of the char budget).
pub fn contextualize(chunks: &mut [Chunk], context: &str, budget_hint: usize) {
    if context.is_empty() {
        return;
    }
    let cap = (budget_hint / 6).max(48);
    let ctx: String = context.chars().take(cap).collect();
    for c in chunks.iter_mut() {
        c.context = ctx.clone();
    }
}

/// Build the markdown heading breadcrumb up to a line, e.g.
/// "Guide > Install > Linux".
pub fn heading_breadcrumb(text: &str, upto_line: u64) -> String {
    let mut stack: Vec<(u8, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // upto_line is 1-based: include only headings on lines strictly
        // before it (the chunk starts at upto_line).
        if (i as u64 + 1) >= upto_line {
            break;
        }
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix('#') else { continue };
        let mut level = 1usize;
        let mut r = rest;
        while let Some(next) = r.strip_prefix('#') {
            level += 1;
            r = next;
        }
        let title = r.trim();
        if title.is_empty() || !r.starts_with(' ') && !title.starts_with(' ') {
            continue;
        }
        // Keep only headings strictly above the chunk start at each level.
        stack.retain(|(l, _)| (*l as usize) < level);
        stack.push((level as u8, title.to_string()));
    }
    stack
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join(" > ")
}

fn line_of(chars: &[char], index: usize) -> u64 {
    1 + chars[..=index.min(chars.len().saturating_sub(1))]
        .iter()
        .filter(|&&c| c == '\n')
        .count() as u64
}

fn inside_fence(prefix: &[char]) -> bool {
    fence_markers(prefix).count() % 2 == 1
}

fn fence_markers(prefix: &[char]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i < prefix.len() {
            if prefix[i] == '`'
                && i + 2 < prefix.len()
                && prefix[i + 1] == '`'
                && prefix[i + 2] == '`'
                && (i == 0 || prefix[i - 1] == '\n')
            {
                let pos = i;
                i += 3;
                return Some(pos);
            }
            i += 1;
        }
        None
    })
}

fn next_fence_close(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`'
            && i + 2 < chars.len()
            && chars[i + 1] == '`'
            && chars[i + 2] == '`'
            && (i == 0 || chars[i - 1] == '\n')
        {
            let mut j = i + 3;
            while j < chars.len() && chars[j] != '\n' {
                j += 1;
            }
            return Some((j + 1).min(chars.len()));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("", &ChunkParams::default()).is_empty());
        assert!(chunk_text("   \n  ", &ChunkParams::default()).is_empty());
    }

    #[test]
    fn cjk_budget_is_smaller_in_chars() {
        let zh = "混合检索方案".repeat(50);
        let en = "hybrid search architecture".repeat(20);
        let zh_budget = char_budget(&zh, ChunkParams::default());
        let en_budget = char_budget(&en, ChunkParams::default());
        assert!(zh_budget < en_budget, "CJK {zh_budget} should be < ASCII {en_budget}");
        // 600 tokens * 1.4 ≈ 840 chars CJK; * 4 ≈ 2400 ASCII.
        assert!((zh_budget as f32 - 840.0).abs() < 60.0, "zh budget {zh_budget}");
        assert!(en_budget > 2000, "en budget {en_budget}");
    }

    #[test]
    fn short_text_single_chunk() {
        let chunks = chunk_text("hello world", &ChunkParams::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn code_fence_not_split() {
        let fence = "```rust\n".to_string() + &"let x = 1;\n".repeat(30) + "```\n";
        let text = "intro\n".to_string() + &fence + &"outro\n".repeat(10);
        let chunks = chunk_text(&text, &ChunkParams::default());
        for c in &chunks {
            let opens = c.text.matches("```rust").count();
            let closes = c.text.matches("```\n").count();
            if opens == 0 {
                continue;
            }
            assert_eq!(opens, closes, "fence split across chunks");
        }
    }

    #[test]
    fn long_unbroken_line_makes_progress() {
        let text = "x".repeat(5000);
        let chunks = chunk_text(&text, &ChunkParams::default());
        let total: usize = chunks.iter().map(|c| c.text.chars().count()).sum();
        // Forward progress + full coverage (overlap may duplicate chars).
        assert!(total >= 5000, "coverage {total} < 5000");
        assert!(chunks.len() >= 2, "no progress: {} chunks", chunks.len());
    }

    #[test]
    fn line_numbers_are_monotonic() {
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let chunks = chunk_text(&text, &ChunkParams::default());
        for w in chunks.windows(2) {
            assert!(w[1].start_line >= w[0].start_line);
            assert!(w[1].end_line >= w[0].end_line);
        }
    }

    #[test]
    fn breadcrumb_nests_headings() {
        let md = "# Guide\n## Install\n### Linux\ntext\n";
        assert_eq!(heading_breadcrumb(md, 4), "Guide > Install > Linux");
        assert_eq!(heading_breadcrumb(md, 2), "Guide");
        assert_eq!(heading_breadcrumb(md, 1), "");
    }

    #[test]
    fn contextualize_caps_header() {
        let mut chunks = chunk_text(&"body ".repeat(500), &ChunkParams::default());
        let n = chunks.len();
        contextualize(&mut chunks, "a > very > long > breadcrumb > path", 2400);
        assert_eq!(chunks.len(), n);
        assert!(chunks[0].context.chars().count() <= 400);
        assert!(chunks[0].context.contains("a > very"));
    }
}
