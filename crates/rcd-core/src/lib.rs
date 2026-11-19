//! rcd-core — Reference Code & Documentation: hybrid keyword+vector search
//! core over archived reference code and documentation.
//!
//! Index + search only: walk → chunk → embed → store → hybrid search.
//! (The notes/feedback knowledge loop was removed 2026-11-19 — this MCP
//! focuses purely on indexing and searching code and documents. The `Kind`
//! enum keeps its `Note` variant for zvec schema compatibility with
//! indexes built before the removal.)

pub mod config;
pub mod walk;
pub mod chunker;
pub mod ast_chunker;
pub mod embed;
pub mod store;
pub mod zvec_store;
pub mod sync;
pub mod search;

/// Resource kind of an indexed document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Archive,
    Note,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Archive => "archive",
            Kind::Note => "note",
        }
    }
}
