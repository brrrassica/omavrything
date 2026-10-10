//! Peer credential checks for the Unix socket.
//!
//! The socket is created mode `0660` group `omavrything`, so the kernel already
//! restricts who can connect. `SO_PEERCRED` adds an explicit, auditable check:
//! root, the daemon's own uid, and members of the `omavrything` group are
//! allowed.

use std::ffi::{CStr, CString};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

/// The peer's credentials, from `SO_PEERCRED`.
#[derive(Debug, Clone, Copy)]
pub struct PeerCred {
    pub pid: libc::pid_t,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

/// Read the peer credentials of a connected Unix socket.
pub fn peer_cred(stream: &UnixStream) -> Option<PeerCred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a valid `ucred` and `len` its size.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

/// The gid of a named group, if it exists.
pub fn group_gid(name: &str) -> Option<libc::gid_t> {
    let c = CString::new(name).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string.
    let grp = unsafe { libc::getgrnam(c.as_ptr()) };
    if grp.is_null() {
        None
    } else {
        // SAFETY: `grp` is non-null and points to a valid group entry.
        Some(unsafe { (*grp).gr_gid })
    }
}

/// Whether `uid` is a member of `gid` (primary or supplementary).
pub fn uid_in_group(uid: libc::uid_t, gid: libc::gid_t) -> bool {
    // SAFETY: `getpwuid` returns a pointer to a static entry or null.
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return false;
    }
    // SAFETY: `pw` is non-null.
    let primary = unsafe { (*pw).pw_gid };
    if primary == gid {
        return true;
    }
    // SAFETY: `pw_name` is a valid NUL-terminated string.
    let name = unsafe { CStr::from_ptr((*pw).pw_name) };

    let mut groups = vec![0 as libc::gid_t; 32];
    let mut ngroups = groups.len() as libc::c_int;
    // SAFETY: `groups` has `ngroups` elements.
    let rc =
        unsafe { libc::getgrouplist(name.as_ptr(), primary, groups.as_mut_ptr(), &mut ngroups) };
    if rc < 0 {
        // Buffer too small: `ngroups` now holds the required size.
        groups.resize(ngroups.max(0) as usize, 0);
        // SAFETY: `groups` now has `ngroups` elements.
        let rc2 = unsafe {
            libc::getgrouplist(name.as_ptr(), primary, groups.as_mut_ptr(), &mut ngroups)
        };
        if rc2 < 0 {
            return false;
        }
    }
    groups.truncate(ngroups.max(0) as usize);
    groups.contains(&gid)
}

/// Whether the peer may use the socket: root, the daemon's own uid, or a member
/// of the `omavrything` group. When the group does not exist, the socket's file
/// permissions are the only gate.
pub fn peer_allowed(cred: &PeerCred, group: Option<libc::gid_t>) -> bool {
    // SAFETY: `geteuid` has no preconditions.
    if cred.uid == 0 || cred.uid == unsafe { libc::geteuid() } {
        return true;
    }
    match group {
        Some(gid) => uid_in_group(cred.uid, gid),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_and_unknown_groups() {
        assert_eq!(group_gid("root"), Some(0));
        assert_eq!(group_gid("omavrything-nonexistent-xyz"), None);
    }

    #[test]
    fn root_is_in_the_root_group() {
        assert!(uid_in_group(0, 0));
    }

    #[test]
    fn root_and_self_are_allowed() {
        let root = PeerCred {
            pid: 1,
            uid: 0,
            gid: 0,
        };
        assert!(peer_allowed(&root, Some(12345)));

        // SAFETY: `geteuid`/`getegid` have no preconditions.
        let me = PeerCred {
            pid: std::process::id() as libc::pid_t,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        };
        assert!(peer_allowed(&me, Some(12345)));
    }

    #[test]
    fn unknown_group_allows_everyone() {
        let other = PeerCred {
            pid: 1,
            uid: 99999,
            gid: 99999,
        };
        assert!(peer_allowed(&other, None));
    }
}
