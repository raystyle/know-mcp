//! File discovery: walk configured roots, honor gitignore, filter by
//! extension/size/excludes. Classify each file as archive or note (by
//! whether it lives under the notes dir).

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::config::Config;
use crate::Kind;

#[derive(Debug, Clone)]
pub struct FoundFile {
    pub path: PathBuf,
    pub kind: Kind,
    /// Which configured root this file belongs to (canonicalized).
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WalkSummary {
    pub files: usize,
    pub archive: usize,
    pub notes: usize,
    pub skipped_large: usize,
    pub per_root: Vec<(PathBuf, usize)>,
}

/// Canonicalize a root for matching (case-insensitive on Windows is not
/// handled here; we compare canonical forms on both sides).
pub fn canonical_root(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Walk all configured roots and return indexable files.
pub fn collect_files(config: &Config) -> anyhow::Result<Vec<FoundFile>> {
    let extensions = config.extensions();
    let ext_set: std::collections::HashSet<&str> =
        extensions.iter().map(|s| s.as_str()).collect();
    let notes_root = canonical_root(&config.notes.dir);
    let mut out = Vec::new();

    for root in &config.index.roots {
        if !root.is_dir() {
            tracing::warn!(root = %root.display(), "root missing, skipping");
            continue;
        }
        let canonical = canonical_root(root);
        let walker = WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .parents(true)
            .ignore(true)
            .add_custom_ignore_filename(".rcdignore")
            .filter_entry(move |entry| {
                let depth = entry.depth();
                if depth == 0 {
                    return true;
                }
                // Hard excludes that gitignore cannot cover (no .git in some trees).
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if matches!(
                    name.as_str(),
                    ".git" | "node_modules" | "target" | "vendor" | "dist" | "build" | ".rcd-index"
                ) {
                    return false;
                }
                true
            })
            .build();

        for entry in walker {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
            if !ext_set.contains(ext.to_lowercase().as_str()) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(path) else { continue };
            if meta.len() > config.index.max_file_bytes {
                tracing::debug!(path = %path.display(), "skipping large file");
                continue;
            }
            let canonical_file = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let kind = if canonical_file.starts_with(&notes_root) {
                Kind::Note
            } else {
                Kind::Archive
            };
            out.push(FoundFile { path: path.to_path_buf(), kind, root: canonical.clone() });
        }
    }
    Ok(out)
}

/// Dry-run summary for `index --dry-run`.
pub fn summarize(files: &[FoundFile], skipped_large: usize) -> WalkSummary {
    let mut per_root: std::collections::BTreeMap<PathBuf, usize> = Default::default();
    let mut archive = 0;
    let mut notes = 0;
    for f in files {
        *per_root.entry(f.root.clone()).or_default() += 1;
        match f.kind {
            Kind::Archive => archive += 1,
            Kind::Note => notes += 1,
        }
    }
    WalkSummary {
        files: files.len(),
        archive,
        notes,
        skipped_large,
        per_root: per_root.into_iter().collect(),
    }
}
