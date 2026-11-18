//! Knowledge notes: markdown files with YAML-ish frontmatter under the notes
//! dir. save_note writes atomically; id = KB-YYYYMMDD-NNN.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sync::atomic_write;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteMeta {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub query: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct Note {
    pub meta: NoteMeta,
    pub body: String,
}

pub struct Notes {
    dir: PathBuf,
}

impl Notes {
    pub fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating notes dir {}", dir.display()))?;
        Ok(Self { dir: dir.to_path_buf() })
    }

    /// Save a note; returns its id. The file becomes immediately indexable
    /// (the sync layer picks it up as Kind::Note under the notes root).
    pub fn save(
        &self,
        title: &str,
        body: &str,
        tags: &[String],
        sources: &[String],
        query: Option<&str>,
    ) -> Result<String> {
        let today = chrono::Utc::now().format("%Y%m%d");
        let mut seq = 1u32;
        let mut id = format!("KB-{today}-{seq:03}");
        while self.dir.join(format!("{id}.md")).exists() {
            seq += 1;
            id = format!("KB-{today}-{seq:03}");
        }
        let meta = NoteMeta {
            id: id.clone(),
            title: title.to_string(),
            tags: tags.to_vec(),
            sources: sources.to_vec(),
            query: query.map(|s| s.to_string()),
            created_at: chrono::Utc::now(),
            status: "published".to_string(),
        };
        let frontmatter = to_frontmatter(&meta);
        let content = format!("---\n{frontmatter}---\n\n# {title}\n\n{body}\n");
        atomic_write(&self.dir.join(format!("{id}.md")), content.as_bytes())?;
        Ok(id)
    }

    pub fn list(&self, tag: Option<&str>, limit: usize) -> Result<Vec<NoteMeta>> {
        let mut notes = Vec::new();
        let entries = std::fs::read_dir(&self.dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(note) = self.read(&path) {
                if let Some(t) = tag {
                    if !note.meta.tags.iter().any(|nt| nt.eq_ignore_ascii_case(t)) {
                        continue;
                    }
                }
                notes.push(note.meta);
            }
        }
        notes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        notes.truncate(limit);
        Ok(notes)
    }

    pub fn get(&self, id: &str) -> Result<Note> {
        let path = self.dir.join(format!("{id}.md"));
        if !path.exists() {
            anyhow::bail!("note {id} not found");
        }
        self.read(&path).ok_or_else(|| anyhow::anyhow!("note {id} unreadable"))
    }

    fn read(&self, path: &Path) -> Option<Note> {
        let raw = std::fs::read_to_string(path).ok()?;
        let raw = raw.strip_prefix("---\n")?;
        let (fm, body) = raw.split_once("\n---\n")?;
        let meta: NoteMeta = serde_yamlish(&mut fm.lines())?;
        Some(Note { meta, body: body.trim_start_matches('\n').to_string() })
    }
}

/// Minimal YAML subset writer (key: value lists only — enough for notes,
/// avoids a yaml dependency).
fn to_frontmatter(meta: &NoteMeta) -> String {
    let mut s = String::new();
    s.push_str(&format!("id: {}\n", meta.id));
    s.push_str(&format!("title: {}\n", escape(&meta.title)));
    if !meta.tags.is_empty() {
        s.push_str(&format!("tags: [{}]\n", meta.tags.iter().map(|t| escape(t)).collect::<Vec<_>>().join(", ")));
    }
    if !meta.sources.is_empty() {
        s.push_str(&format!("sources: [{}]\n", meta.sources.iter().map(|t| escape(t)).collect::<Vec<_>>().join(", ")));
    }
    if let Some(q) = &meta.query {
        s.push_str(&format!("query: {}\n", escape(q)));
    }
    s.push_str(&format!("created_at: {}\n", meta.created_at.to_rfc3339()));
    s.push_str(&format!("status: {}\n", meta.status));
    s
}

fn escape(s: &str) -> String {
    if s.contains(',') || s.contains(':') || s.contains('[') || s.contains(']') {
        format!("{:?}", s)
    } else {
        s.to_string()
    }
}

/// Parse the frontmatter subset back. Handles `key: value`, `key: [a, b]`,
/// quoted strings.
fn serde_yamlish<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Option<NoteMeta> {
    let mut id = String::new();
    let mut title = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut query: Option<String> = None;
    let mut created_at = None;
    let mut status = "published".to_string();

    for line in lines {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        match key.trim() {
            "id" => id = value.to_string(),
            "title" => title = unquote(value),
            "tags" => tags = parse_list(value),
            "sources" => sources = parse_list(value),
            "query" => query = Some(unquote(value)).filter(|s| !s.is_empty()),
            "created_at" => created_at = chrono::DateTime::parse_from_rfc3339(value).ok().map(|t| t.with_timezone(&chrono::Utc)),
            "status" => status = value.to_string(),
            _ => {}
        }
    }
    Some(NoteMeta {
        id,
        title,
        tags,
        sources,
        query,
        created_at: created_at.unwrap_or_else(chrono::Utc::now),
        status,
    })
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn parse_list(s: &str) -> Vec<String> {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']');
    if t.is_empty() {
        return Vec::new();
    }
    t.split(',').map(|i| unquote(i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_list_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let notes = Notes::new(dir.path()).unwrap();
        let id = notes
            .save("zvec DLL 手动获取", "下载 prebuilt tar.gz 并设 ZVEC_LIB_DIR", &["zvec".into()], &["E:/x/y.rs".into()], Some("zvec dll missing"))
            .unwrap();
        assert!(id.starts_with("KB-"));

        let listed = notes.list(None, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "zvec DLL 手动获取");
        assert_eq!(listed[0].tags, vec!["zvec".to_string()]);

        let got = notes.get(&id).unwrap();
        assert!(got.body.contains("ZVEC_LIB_DIR"));
        assert_eq!(got.meta.query.as_deref(), Some("zvec dll missing"));
    }

    #[test]
    fn second_note_same_day_gets_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let notes = Notes::new(dir.path()).unwrap();
        let a = notes.save("a", "b", &[], &[], None).unwrap();
        let b = notes.save("a2", "b2", &[], &[], None).unwrap();
        assert_ne!(a, b);
    }
}
