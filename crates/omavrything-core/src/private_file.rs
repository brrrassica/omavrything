//! Creating files that must never be readable by another local user.
//!
//! The index snapshot and the daemon's state configuration are both written by
//! creating a temporary file and renaming it into place. Two rules make that
//! safe, and both are needed:
//!
//! 1. The mode is applied by the `open` call that *creates* the file.
//!    Permissions are checked only when a file is opened, so a file created as
//!    `0666 & ~umask` and tightened by a later `chmod` can be opened by another
//!    local process during the interval between the two. That process keeps its
//!    descriptor, and with it read access to everything written through it
//!    afterwards, whatever the mode becomes in the end.
//! 2. The file the caller writes to is always a **new inode**. `open` ignores
//!    `mode` for a file that already exists, so reopening a leftover temp file
//!    would use whatever mode it already had: reopening a `0644` leftover and
//!    tightening it afterwards is rule 1's bug again, for the same reason. A
//!    leftover is removed instead, and `O_EXCL` refuses to write through an inode
//!    this call did not create.

use std::fs::{self, File, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Create `path` as a fresh file with `mode`, replacing any leftover at that path.
///
/// Fails rather than writing through a file that appears at `path` between the
/// removal and the creation: the temp path is a shared, predictable name, so only
/// the writer's own new inode is safe to write to.
pub fn open_private(path: &Path, mode: u32) -> io::Result<File> {
    // A leftover temp file from an interrupted run is removed, never reopened:
    // `open` would ignore `mode` and hand back an inode that may still be
    // readable by someone else. Removing it also disposes of a symlink planted at
    // the path, rather than following it.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;

    // `mode` is masked by the umask, so force the exact mode the caller asked for.
    // This cannot open a window: the inode was created by this call, so no other
    // process can already hold a descriptor to it, and the caller writes no data
    // until this returns.
    file.set_permissions(Permissions::from_mode(mode))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
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
    fn a_leftover_temp_file_is_replaced_rather_than_reused() {
        // A temp file left behind by an interrupted run, readable by everyone.
        let dir = temp_dir("leftover");
        let path = dir.join("snapshot");
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        // Stand in for another local process that opened it beforehand: it holds a
        // descriptor, and a descriptor outlives the mode that allowed it.
        let eavesdropper = File::open(&path).unwrap();
        let stale_inode = eavesdropper.metadata().unwrap().ino();

        let mut file = open_private(&path, 0o600).unwrap();
        file.write_all(b"index of every file on the disk").unwrap();
        file.sync_all().unwrap();

        // Writing must have gone to a new inode, so the descriptor the eavesdropper
        // already holds cannot read what was written. Reopening the leftover would
        // leave both of these failing.
        assert_ne!(
            fs::metadata(&path).unwrap().ino(),
            stale_inode,
            "the leftover inode must not be reused"
        );
        let mut seen = Vec::new();
        (&eavesdropper).read_to_end(&mut seen).unwrap();
        assert_eq!(
            seen, b"stale",
            "the old inode must not receive the new data"
        );
        assert_eq!(mode_of(&path), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_at_the_temp_path_is_removed_and_not_written_through() {
        let dir = temp_dir("symlink");
        let target = dir.join("target");
        fs::write(&target, b"do not touch").unwrap();
        let path = dir.join("snapshot");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let mut file = open_private(&path, 0o600).unwrap();
        file.write_all(b"new").unwrap();
        file.sync_all().unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"do not touch");
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(mode_of(&path), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }
}
