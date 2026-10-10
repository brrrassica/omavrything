//! A minimal recursive directory walker for the M2 walking skeleton.
//!
//! No excludes, no parallelism, no symlink handling. The production scanner
//! (M4) replaces this with a parallel walker, mount discovery, and excludes.

use std::fs;
use std::path::Path;

use crate::index::Index;

/// Recursively walk `root` and insert every entry into `index`.
///
/// Unreadable directories are skipped rather than aborting the scan.
pub fn scan_into(root: &Path, index: &mut Index) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            index.insert(path.clone(), is_dir);
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
        fs::write(base.join("a.txt"), b"a").unwrap();
        fs::write(base.join("sub/b.txt"), b"b").unwrap();

        let mut index = Index::new();
        scan_into(&base, &mut index);

        assert!(
            index.len() >= 3,
            "expected at least 3 entries, got {}",
            index.len()
        );
        assert_eq!(index.search("a.txt", 10).len(), 1);
        assert_eq!(index.search("b.txt", 10).len(), 1);

        let _ = fs::remove_dir_all(&base);
    }
}
