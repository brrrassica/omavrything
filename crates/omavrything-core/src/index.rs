//! A minimal in-memory index for the M2 walking skeleton.
//!
//! Stores full paths and answers case-insensitive substring queries with
//! space-separated AND terms. The production index (M3) replaces this with a
//! compact, arena-backed structure and ranked results.

use std::path::PathBuf;

/// One indexed filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// A flat, in-memory list of entries.
#[derive(Debug, Default)]
pub struct Index {
    entries: Vec<Entry>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn insert(&mut self, path: PathBuf, is_dir: bool) {
        self.entries.push(Entry { path, is_dir });
    }

    /// Return up to `limit` entries matching every whitespace-separated term
    /// (case-insensitive substring over the full path).
    ///
    /// An empty query matches nothing: the UI shows a hint until the user types.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Entry> {
        let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
        if terms.is_empty() || limit == 0 {
            return Vec::new();
        }

        let mut out = Vec::new();
        for entry in &self.entries {
            let hay = entry.path.to_string_lossy().to_lowercase();
            if terms.iter().all(|t| hay.contains(t.as_str())) {
                out.push(entry);
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut idx = Index::new();
        idx.insert(PathBuf::from("/home/sam/Documents/report-2024.pdf"), false);
        idx.insert(PathBuf::from("/home/sam/Documents/notes.md"), false);
        idx.insert(PathBuf::from("/home/sam/Pictures"), true);
        idx.insert(PathBuf::from("/etc/omavrything/config.toml"), false);
        idx
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(index().search("", 10).is_empty());
        assert!(index().search("   ", 10).is_empty());
    }

    #[test]
    fn substring_is_case_insensitive() {
        let idx = index();
        let hits = idx.search("REPORT", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("report-2024.pdf"));
    }

    #[test]
    fn terms_are_anded() {
        let idx = index();
        assert_eq!(idx.search("documents report", 10).len(), 1);
        assert_eq!(idx.search("documents pictures", 10).len(), 0);
    }

    #[test]
    fn limit_is_respected() {
        let idx = index();
        assert_eq!(idx.search("home", 2).len(), 2);
    }

    #[test]
    fn is_dir_is_preserved() {
        let idx = index();
        let hits = idx.search("Pictures", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_dir);
    }
}
