//! Creating files that must never be readable by another local user.
//!
//! The index snapshot and the daemon's state configuration are both written by
//! creating a temporary file and renaming it into place. Permissions are checked
//! only when a file is *opened*, so a file that is created as `0666 & ~umask` and
//! tightened by a later `chmod` can be opened by another local process during the
//! interval between the two. That process keeps its descriptor, and with it read
//! access to everything written through it afterwards, whatever the mode becomes
//! in the end. Creating with the final mode is the only way to close that window.

use std::fs::{self, File, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Create (or truncate) `path` with `mode`, applied by the `open` call itself, so
/// the file is never readable beyond `mode` -- not even briefly.
///
/// `.mode()` applies only when the file is created, so a temporary file left
/// behind by an interrupted run keeps whatever mode it already had; the mode is
/// therefore forced again for that case. This is safe because the file is still
/// empty here: the caller writes no data until this returns.
pub fn open_private(path: &Path, mode: u32) -> io::Result<File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(mode))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omavrything-private-file-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_mode_comes_from_the_open_call_and_not_from_the_umask() {
        let dir = temp_dir("umask");
        let path = dir.join("snapshot");

        // `umask 0` is the worst case: a file created as `0666 & ~umask` would be
        // world-writable, so this pins the mode to the `open` call rather than to
        // whatever the process umask happens to be. The umask is process-wide, so
        // restore it immediately; the window is a single syscall wide.
        let previous = unsafe { libc::umask(0) };
        let file = open_private(&path, 0o600).unwrap();
        let observed = file.metadata().unwrap().permissions().mode() & 0o777;
        unsafe { libc::umask(previous) };

        assert_eq!(observed, 0o600, "the file must be owner-only as created");
        assert_eq!(mode_of(&path), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leftover_temp_file_is_tightened_before_any_data_is_written() {
        // A temp file left by an interrupted run is merely reopened, which keeps
        // its existing mode, so opening alone is not enough.
        let dir = temp_dir("leftover");
        let path = dir.join("snapshot");
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        let _file = open_private(&path, 0o600).unwrap();
        assert_eq!(mode_of(&path), 0o600, "the widened mode must not survive");
        let _ = fs::remove_dir_all(&dir);
    }
}
