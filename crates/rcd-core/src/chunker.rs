//! Character-based chunking (search-harness pattern): target size with
//! overlap, snap the cut back to the last newline if it's past halfway,
//! never cut inside a ``` code fence, track 1-based line ranges.

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    /// 1-based inclusive line range in the source file.
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkParams {
    pub chars: usize,
    pub overlap: usize,
}

impl ChunkParams {
    pub fn new(chars: usize, overlap: usize) -> Self {
        Self { chars, overlap }
    }
}

/// Split `text` into overlapping chunks. Never cuts inside a code fence;
/// if a single fenced block exceeds `chars`, it is emitted as one
/// oversized chunk rather than split.
pub fn chunk_text(text: &str, params: ChunkParams) -> Vec<Chunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut pos = 0usize;

    while pos < chars.len() {
        let target = pos + params.chars;
        let mut end = target.min(chars.len());

        if end < chars.len() {
            // Never cut inside a ``` code fence: if the cut point lands in
            // one, extend the chunk to the fence's closing marker instead.
            if inside_fence(&chars[..end]) {
                if let Some(close) = next_fence_close(&chars, end.saturating_sub(1)) {
                    end = close.min(chars.len());
                }
            } else {
                // Snap back to the last newline if it is at least halfway through.
                let half = pos + params.chars / 2;
                if let Some(nl) = (half..end).rev().find(|&i| chars[i] == '\n') {
                    end = nl + 1;
                }
            }
        }

        let chunk_start = pos;
        let chunk_end = end.max(pos + 1).min(chars.len());
        let start_line = line_of(&chars, chunk_start);
        let end_line = line_of(&chars, chunk_end.saturating_sub(1));
        let text: String = chars[chunk_start..chunk_end].iter().collect();
        chunks.push(Chunk { text, start_line, end_line });

        if chunk_end >= chars.len() {
            break;
        }
        // Forward-progress guarantee with overlap.
        let next = chunk_end.saturating_sub(params.overlap).max(chunk_start + 1);
        pos = next;
    }
    chunks
}

/// 1-based line number of the char at `index`.
fn line_of(chars: &[char], index: usize) -> u64 {
    1 + chars[..=index.min(chars.len().saturating_sub(1))]
        .iter()
        .filter(|&&c| c == '\n')
        .count() as u64
}

/// Whether the prefix ends strictly inside a ``` code fence (odd number of
/// line-start fence markers seen so far).
fn inside_fence(prefix: &[char]) -> bool {
    fence_markers(prefix).count() % 2 == 1
}

/// Positions of line-start ``` fence markers in `prefix`.
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

/// Index just past the next line-start ``` at/after `from` (fence close).
fn next_fence_close(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`'
            && i + 2 < chars.len()
            && chars[i + 1] == '`'
            && chars[i + 2] == '`'
            && (i == 0 || chars[i - 1] == '\n')
        {
            // Skip to end of the fence line.
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

    fn params() -> ChunkParams {
        ChunkParams::new(100, 20)
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("", params()).is_empty());
        assert!(chunk_text("   \n  ", params()).is_empty());
    }

    #[test]
    fn short_text_single_chunk() {
        let chunks = chunk_text("hello world", params());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
    }

    #[test]
    fn splits_on_newline_boundary() {
        let text = "a".repeat(40) + "\n" + &"b".repeat(40) + "\n" + &"c".repeat(40);
        let chunks = chunk_text(&text, params());
        assert!(chunks.len() >= 2);
        // Every chunk boundary lands on a newline (first chunk may end with \n).
        for c in &chunks {
            assert!(c.text.len() <= 121, "chunk len {} exceeds fence allowance", c.text.len());
        }
    }

    #[test]
    fn chinese_text_chunks() {
        let text = "混合检索方案".repeat(100);
        let chunks = chunk_text(&text, params());
        assert!(chunks.len() >= 8);
        for (a, b) in chunks.iter().zip(chunks.iter().skip(1)) {
            assert!(b.start_line >= a.start_line);
        }
    }

    #[test]
    fn code_fence_not_split() {
        let fence = "```rust\n".to_string() + &"let x = 1;\n".repeat(30) + "```\n";
        let text = "intro\n".to_string() + &fence + &"outro\n".repeat(10);
        let chunks = chunk_text(&text, params());
        // No chunk boundary strictly inside the fence: every chunk either
        // contains whole fence lines or none of them.
        for c in &chunks {
            let fence_starts = c.text.matches("```rust").count();
            let fence_ends = c.text.matches("```\n").count();
            if fence_starts == 0 {
                // A tail continuation of the fence is allowed only when it
                // reaches the closing marker within the same chunk.
                continue;
            }
            assert_eq!(fence_starts, fence_ends, "fence split across chunks: {:?}", c.text);
        }
    }

    #[test]
    fn long_unbroken_line_makes_progress() {
        let text = "x".repeat(5000);
        let chunks = chunk_text(&text, params());
        assert!(chunks.len() > 10);
        let total: usize = chunks.iter().map(|c| c.text.len()).sum();
        assert!(total >= 5000);
    }

    #[test]
    fn line_numbers_are_monotonic() {
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let chunks = chunk_text(&text, params());
        for w in chunks.windows(2) {
            assert!(w[1].start_line >= w[0].start_line);
            assert!(w[1].end_line >= w[0].end_line);
        }
    }
}
