//! Minimal glob matching for exclude patterns.
//!
//! Supports `*` (any run of characters except `/`), `**` (any run including
//! `/`, and it may match zero segments), and `?` (one character except `/`).
//! Patterns and paths are matched segment-by-segment, so `/proc` matches the
//! directory `/proc` and `**/.git/**` matches any `.git` directory and its
//! contents.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

/// A compiled glob pattern.
#[derive(Debug, Clone)]
pub struct Glob {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
enum Segment {
    /// `**`: matches zero or more path segments.
    DoubleStar,
    /// A single segment containing `*` and `?` wildcards.
    Pattern(Vec<u8>),
}

impl Glob {
    /// Compile `pattern`. An empty pattern matches nothing.
    pub fn new(pattern: &str) -> Self {
        let segments = pattern
            .split('/')
            .map(|s| {
                if s == "**" {
                    Segment::DoubleStar
                } else {
                    Segment::Pattern(s.as_bytes().to_vec())
                }
            })
            .collect();
        Self { segments }
    }

    /// Whether `path` matches this pattern.
    pub fn matches(&self, path: &Path) -> bool {
        self.matches_bytes(path_bytes(path))
    }

    fn matches_bytes(&self, path: &[u8]) -> bool {
        let text: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
        match_segments(&self.segments, &text)
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> &[u8] {
    path.to_str().map_or(&[], str::as_bytes)
}

/// A set of exclude globs. An empty set matches nothing.
#[derive(Debug, Clone, Default)]
pub struct GlobSet {
    globs: Vec<Glob>,
}

impl GlobSet {
    pub fn new(patterns: &[String]) -> Self {
        Self {
            globs: patterns.iter().map(|p| Glob::new(p)).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    pub fn matches(&self, path: &Path) -> bool {
        self.globs.iter().any(|g| g.matches(path))
    }
}

fn match_segments(pats: &[Segment], text: &[&[u8]]) -> bool {
    let Some((first, rest)) = pats.split_first() else {
        return text.is_empty();
    };
    match first {
        Segment::DoubleStar => {
            // `**` matches zero or more segments.
            (0..=text.len()).any(|i| match_segments(rest, &text[i..]))
        }
        Segment::Pattern(p) => match text.split_first() {
            Some((head, tail)) => seg_match(p, head) && match_segments(rest, tail),
            None => false,
        },
    }
}

/// Match a single path segment against a `*`/`?` pattern (no `/`).
fn seg_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }

    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        Glob::new(pattern).matches(Path::new(path))
    }

    #[test]
    fn literal_paths_match_exactly() {
        assert!(m("/proc", "/proc"));
        assert!(!m("/proc", "/proc/foo"));
        assert!(!m("/proc", "/process"));
    }

    #[test]
    fn double_star_matches_any_depth() {
        assert!(m("**/.git/**", "/home/sam/repo/.git"));
        assert!(m("**/.git/**", "/home/sam/repo/.git/config"));
        assert!(m("**/.git/**", "/.git/objects/ab/cd"));
        assert!(!m("**/.git/**", "/home/sam/repo/.github/workflows"));
    }

    #[test]
    fn single_star_stays_within_a_segment() {
        assert!(m("/home/*/cache", "/home/sam/cache"));
        assert!(!m("/home/*/cache", "/home/sam/sub/cache"));
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert!(m("/tmp/ab?", "/tmp/abc"));
        assert!(!m("/tmp/ab?", "/tmp/abcd"));
    }

    #[test]
    fn globset_matches_any_pattern() {
        let set = GlobSet::new(&["/proc".into(), "**/.git/**".into()]);
        assert!(set.matches(Path::new("/proc")));
        assert!(set.matches(Path::new("/a/b/.git/x")));
        assert!(!set.matches(Path::new("/home/sam")));
        assert!(GlobSet::default().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn matches_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let path = PathBuf::from(OsString::from_vec(b"/tmp/na\xffme/file".to_vec()));
        assert!(Glob::new("/tmp/*/file").matches(&path));
        assert!(Glob::new("**/file").matches(&path));
        assert!(!Glob::new("/tmp/other/file").matches(&path));
    }
}
