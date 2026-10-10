//! A minimal recursive directory walker for the walking skeleton.
//!
//! No excludes, no parallelism, no symlink handling. The production scanner
//! (M4) replaces this with a parallel walker, mount discovery, and excludes.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::index::Index;

/// Recursively walk `root` and insert every entry into `index`.
///
/// Unreadable directories are skipped rather than aborting the scan. Metadata
/// is read without following symlinks, so a symlink is indexed as itself.
pub fn scan_into(root: &Path, index: &mut Index) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = if is_dir {
                0
            } else {
                metadata.as_ref().map(|m| m.len()).unwrap_or(0)
            };
            let mtime = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            index.insert(path.clone(), is_dir, size, mtime);
            if is_dir {
                stack.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scans_a_small_tree() {
        let base = std::env::temp_dir().join(format!("omavrything-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("a.txt"), b"hello").unwrap();
        fs::write(base.join("sub/b.txt"), b"world!").unwrap();

        let mut index = Index::new();
        scan_into(&base, &mut index);

        assert!(
            index.len() >= 3,
            "expected at least 3 entries, got {}",
            index.len()
        );

        let a = index.search("a.txt", 10);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].size, 5);
        assert!(!a[0].is_dir);

        let sub = index.search("sub", 10);
        assert!(sub.iter().any(|e| e.is_dir));

        let _ = fs::remove_dir_all(&base);
    }
}
