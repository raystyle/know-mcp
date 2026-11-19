//! rcd-core — Reference Code & Documentation: hybrid keyword+vector search
//! core over archived reference code and documentation.
//!
//! Two resource kinds share one index:
//! - `archive` (read-only facts): cloned repos, docs, reverse-engineered sources
//! - `note` (writable solutions): knowledge notes distilled from archive hits
//!
//! The search loop (mcp-knowledge-base pattern): search archive + notes →
//! solve the problem → save a note → the note is searchable → feedback scores
//! whether the note actually helped → gaps show what to write next.

pub mod config;
pub mod walk;
pub mod chunker;
pub mod ast_chunker;
pub mod embed;
pub mod store;
pub mod zvec_store;
pub mod sync;
pub mod search;
pub mod notes;
pub mod feedback;

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
