//! Search service: auto mode routing, path post-filter, note reweighting by
//! feedback score, snippet assembly, gap accounting.

use anyhow::Result;

use crate::embed::{EmbedPurpose, QwenEmbedder};
use crate::feedback::Feedback;
use crate::store::{Hit, SearchMode, SearchOptions, SearchStore};
use crate::Kind;

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    /// None = auto.
    pub mode: Option<SearchMode>,
    pub top_k: usize,
    /// Case-insensitive substring filter on the absolute path.
    pub path_filter: Option<String>,
    pub kind_filter: Option<Kind>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub query: String,
    pub mode: &'static str,
    pub hits: Vec<Hit>,
    /// Recorded when nothing scored above the gap threshold.
    pub gap_recorded: bool,
}

/// Gap threshold on the best cosine similarity: below this we consider the
/// query unserved and record it for get_gaps.
const GAP_THRESHOLD: f32 = 0.15;

/// Auto-mode heuristic: identifier-like tokens or heavy CJK → keyword
/// (FTS exact tokens beat embeddings there); otherwise hybrid.
pub fn resolve_mode(query: &str) -> SearchMode {
    let has_identifier = query
        .split_whitespace()
        .any(|tok| {
            let t = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            (t.contains('_') && t.len() > 3)
                || (t.chars().any(|c| c.is_uppercase()) && t.chars().any(|c| c.is_lowercase()) && t.len() > 3)
                || t.split(|c: char| c == '.' || c == '-').count() > 1 && t.chars().any(|c| c.is_ascii_digit())
        });
    let cjk: usize = query.chars().filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c)).count();
    let cjk_heavy = cjk * 2 >= query.chars().count();
    if has_identifier || cjk_heavy {
        SearchMode::Keyword
    } else {
        SearchMode::Hybrid
    }
}

pub struct SearchService<'a> {
    pub store: &'a dyn SearchStore,
    pub embedder: &'a mut QwenEmbedder,
    pub feedback: &'a Feedback,
}

impl<'a> SearchService<'a> {
    pub fn search(&mut self, req: &SearchRequest) -> Result<SearchResult> {
        let mode = req.mode.unwrap_or_else(|| resolve_mode(&req.query));
        let opts = SearchOptions {
            top_k: req.top_k,
            kind_filter: req.kind_filter,
        };

        let mut hits = match mode {
            SearchMode::Keyword => self.store.search_keyword(&req.query, &opts)?,
            SearchMode::Vector => {
                let emb = self.embedder.embed(&req.query, EmbedPurpose::Query)?;
                self.store.search_vector(&emb, &opts)?
            }
            SearchMode::Hybrid => {
                let emb = self.embedder.embed(&req.query, EmbedPurpose::Query)?;
                self.store.search_hybrid(&req.query, &emb, &opts)?
            }
        };

        // Note reweighting: helpful notes rank up, unhelpful ones down.
        for hit in &mut hits {
            if hit.kind == Kind::Note {
                if let Some(note_id) = &hit.note_id {
                    let score = self.feedback.helpful_score(note_id);
                    let factor = (1.0 + 0.1 * score as f32).clamp(0.5, 2.0);
                    hit.score *= factor;
                }
            }
        }

        // Path post-filter.
        if let Some(filter) = &req.path_filter {
            let f = filter.to_lowercase();
            hits.retain(|h| h.path.to_lowercase().contains(&f));
        }

        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(req.top_k);

        let best = hits.first().map(|h| h.score).unwrap_or(0.0);
        let gap_recorded = hits.is_empty() || best < GAP_THRESHOLD;
        if gap_recorded {
            self.feedback.record_gap(&req.query, mode.as_str());
        }

        Ok(SearchResult {
            query: req.query.clone(),
            mode: mode.as_str(),
            hits,
            gap_recorded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_route_to_keyword() {
        assert!(matches!(resolve_mode("ZVecInitialize"), SearchMode::Keyword));
        assert!(matches!(resolve_mode("zvec_c_api.dll missing"), SearchMode::Keyword));
        assert!(matches!(resolve_mode("CVE-2024-1234"), SearchMode::Keyword));
    }

    #[test]
    fn plain_english_routes_to_hybrid() {
        assert!(matches!(resolve_mode("how to run a background daemon"), SearchMode::Hybrid));
    }

    #[test]
    fn chinese_routes_to_keyword() {
        assert!(matches!(resolve_mode("混合检索方案"), SearchMode::Keyword));
    }
}
