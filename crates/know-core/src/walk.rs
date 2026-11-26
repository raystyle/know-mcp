//! File discovery: walk configured roots, honor gitignore, filter by
//! extension/size/excludes.

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

/// Canonicalize a root for matching (case-insensitive on Windows is not
/// handled here; we compare canonical forms on both sides).
pub fn canonical_root(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Walk all configured roots and return indexable files.
pub fn collect_files(config: &Config) -> anyhow::Result<Vec<FoundFile>> {
    let extensions = config.extensions();
    let ext_set: std::collections::HashSet<&str> = extensions.iter().map(|s| s.as_str()).collect();
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
            .add_custom_ignore_filename(".know-ignore")
            .filter_entry(move |entry| {
                let depth = entry.depth();
                if depth == 0 {
                    return true;
                }
                // Hard excludes that gitignore cannot cover (no .git in some trees).
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if matches!(
                    name.as_str(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | "vendor"
                        | "dist"
                        | "build"
                        | ".know-index"
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
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !ext_set.contains(ext.to_lowercase().as_str()) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if meta.len() > config.index.max_file_bytes {
                tracing::debug!(path = %path.display(), "skipping large file");
                continue;
            }
            out.push(FoundFile {
                path: path.to_path_buf(),
                kind: Kind::Archive,
                root: canonical.clone(),
            });
        }
    }
    Ok(out)
}
