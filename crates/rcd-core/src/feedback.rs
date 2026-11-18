//! Feedback and gaps accounting: append-only event log (feedback.json),
//! aggregated helpful scores per note, zero/low-result query gaps.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::sync::atomic_write;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEvent {
    pub note_id: String,
    pub helpful: bool,
    pub comment: Option<String>,
    pub time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapEvent {
    pub query: String,
    pub mode: String,
    pub time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeedbackLog {
    pub events: Vec<FeedbackEvent>,
    pub gaps: Vec<GapEvent>,
}

pub struct Feedback {
    path: PathBuf,
    state: std::sync::Mutex<FeedbackLog>,
}

impl Feedback {
    pub fn load(index_dir: &Path) -> Result<Self> {
        let path = index_dir.join("feedback.json");
        let state = if path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&path)?)?
        } else {
            FeedbackLog::default()
        };
        Ok(Self { path, state: std::sync::Mutex::new(state) })
    }

    pub fn record(&self, note_id: &str, helpful: bool, comment: Option<&str>) -> Result<()> {
        let mut guard = self.state.lock().unwrap();
        guard.events.push(FeedbackEvent {
            note_id: note_id.to_string(),
            helpful,
            comment: comment.map(|s| s.to_string()),
            time: chrono::Utc::now(),
        });
        let snapshot = guard.clone();
        atomic_write(&self.path, serde_json::to_string_pretty(&snapshot)?.as_bytes())
    }

    pub fn record_gap(&self, query: &str, mode: &str) {
        let mut guard = self.state.lock().unwrap();
        guard.gaps.push(GapEvent {
            query: query.to_string(),
            mode: mode.to_string(),
            time: chrono::Utc::now(),
        });
        // Gaps can be frequent; cap the log so it cannot grow unboundedly.
        if guard.gaps.len() > 1000 {
            let drop = guard.gaps.len() - 1000;
            guard.gaps.drain(0..drop);
        }
        let snapshot = guard.clone();
        if let Ok(bytes) = serde_json::to_string_pretty(&snapshot) {
            let _ = atomic_write(&self.path, bytes.as_bytes());
        }
    }

    /// helpful - not_helpful for a note.
    pub fn helpful_score(&self, note_id: &str) -> i64 {
        let guard = self.state.lock().unwrap();
        guard
            .events
            .iter()
            .filter(|e| e.note_id == note_id)
            .map(|e| if e.helpful { 1 } else { -1 })
            .sum()
    }

    /// Aggregated note scores: (note_id, helpful_count, not_helpful_count).
    pub fn note_stats(&self) -> Vec<(String, u64, u64)> {
        let guard = self.state.lock().unwrap();
        use std::collections::BTreeMap;
        let mut map: BTreeMap<String, (u64, u64)> = Default::default();
        for e in &guard.events {
            let entry = map.entry(e.note_id.clone()).or_default();
            if e.helpful {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        map.into_iter().map(|(k, (h, n))| (k, h, n)).collect()
    }

    /// Recent gaps, newest first.
    pub fn gaps(&self, limit: usize) -> Vec<GapEvent> {
        let guard = self.state.lock().unwrap();
        let mut gaps = guard.gaps.clone();
        gaps.reverse();
        gaps.truncate(limit);
        gaps
    }
}
