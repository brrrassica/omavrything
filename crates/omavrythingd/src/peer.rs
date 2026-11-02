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
///
/// Uses the reentrant `getgrnam_r`: the non-reentrant `getgrnam` returns a
/// pointer into shared static storage, so concurrent lookups (the D-Bus
/// `GetStatus` path runs on a multi-threaded runtime) could observe another
/// caller's group entry.
pub fn group_gid(name: &str) -> Option<libc::gid_t> {
    let c = CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 1024];
    loop {
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        // SAFETY: `c` is NUL-terminated, `grp` and `buf` are valid, and
        // `result` receives a pointer into `buf` or null.
        let rc = unsafe {
            libc::getgrnam_r(
                c.as_ptr(),
                &mut grp,
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            // Buffer too small: grow (bounded) and retry.
            if buf.len() >= 1 << 20 {
                return None;
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: `result` points at `grp`, which `getgrnam_r` initialized.
        return Some(unsafe { (*result).gr_gid });
    }
}

/// Look up a passwd entry by uid using the reentrant `getpwuid_r`.
///
/// Returns the primary gid and the user name as owned data, so nothing points
/// into shared or soon-to-be-freed storage. This replaces `getpwuid`, whose
/// static entry is overwritten by every concurrent lookup.
fn lookup_passwd(uid: libc::uid_t) -> Option<(libc::gid_t, CString)> {
    let mut buf = vec![0 as libc::c_char; 1024];
    loop {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `pw` and `buf` are valid, and `result` receives a pointer
        // into `buf` or null.
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut result) };
        if rc == libc::ERANGE {
            // Buffer too small: grow (bounded) and retry.
            if buf.len() >= 1 << 20 {
                return None;
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        let gid = unsafe { (*result).pw_gid };
        let name_ptr = unsafe { (*result).pw_name };
        if name_ptr.is_null() {
            return None;
        }
        // SAFETY: `pw_name` is a valid NUL-terminated string in `buf`.
        let name = unsafe { CStr::from_ptr(name_ptr) };
        return Some((gid, CString::new(name.to_bytes()).ok()?));
    }
}

/// Whether `uid` is a member of `gid` (primary or supplementary).
pub fn uid_in_group(uid: libc::uid_t, gid: libc::gid_t) -> bool {
    let Some((primary, name)) = lookup_passwd(uid) else {
        return false;
    };
    if primary == gid {
        return true;
    }

    let mut groups = vec![0 as libc::gid_t; 32];
    let mut ngroups = groups.len() as libc::c_int;
    // SAFETY: `name` is NUL-terminated and `groups` has `ngroups` elements.
    // `getgrouplist` is MT-Safe; only the name lookup above needed the
    // reentrant form.
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
    fn unknown_uid_is_not_in_the_root_group() {
        // A uid that `getpwuid_r` cannot resolve must fail closed, not read
        // another entry left over from a concurrent lookup.
        assert!(!uid_in_group(u32::MAX, 0));
    }

    #[test]
    fn concurrent_user_and_group_lookups_do_not_cross_talk() {
        // Regression guard for the reentrancy fix: many threads resolve the
        // same and different accounts/groups at once. With the old
        // `getpwuid`/`getgrnam` static buffers this raced; with the reentrant
        // forms each result is independent.
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(std::thread::spawn(|| {
                for _ in 0..200 {
                    assert!(uid_in_group(0, 0));
                    assert_eq!(group_gid("root"), Some(0));
                    assert!(!uid_in_group(u32::MAX, 0));
                }
            }));
        }
        for handle in handles {
            handle.join().expect("lookup thread panicked");
        }
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
