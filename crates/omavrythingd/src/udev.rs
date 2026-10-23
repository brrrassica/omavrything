//! Minimal udev netlink monitor for block-device add/remove events.
//!
//! M19 (B4): the daemon subscribes to the kernel's uevent netlink group and
//! wakes the NTFS hotplug pass when a block device is added, removed, or
//! changed. This is a real udev event source (the same one `udevadm monitor`
//! uses), not a mount-table poll. If the socket cannot be opened, the caller
//! falls back to polling.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const NETLINK_KOBJECT_UEVENT: i32 = 15;
const UDEV_MONITOR_GROUP: u32 = 1;

/// One parsed uevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdevEvent {
    pub action: String,
    pub devname: Option<String>,
    pub subsystem: Option<String>,
    pub devtype: Option<String>,
}

impl UdevEvent {
    /// Whether this event is a block-device add/remove/change.
    pub fn is_block_event(&self) -> bool {
        self.subsystem.as_deref() == Some("block")
            && matches!(self.action.as_str(), "add" | "remove" | "change")
    }
}

/// Start the udev monitor. The returned receiver yields parsed events; the
/// monitor thread exits when the receiver is dropped or the socket fails.
pub fn monitor() -> io::Result<Receiver<UdevEvent>> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, owned socket descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as u16;
    addr.nl_groups = UDEV_MONITOR_GROUP;
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            let text = String::from_utf8_lossy(&buf[..n as usize]);
            if let Some(event) = parse_event(&text) {
                if tx.send(event).is_err() {
                    break;
                }
            }
        }
    });
    Ok(rx)
}

/// Parse a NUL-separated uevent message.
fn parse_event(text: &str) -> Option<UdevEvent> {
    let mut action = None;
    let mut devname = None;
    let mut subsystem = None;
    let mut devtype = None;
    for field in text.split('\0') {
        if let Some(value) = field.strip_prefix("ACTION=") {
            action = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("DEVNAME=") {
            devname = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("SUBSYSTEM=") {
            subsystem = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("DEVTYPE=") {
            devtype = Some(value.to_string());
        }
    }
    Some(UdevEvent {
        action: action?,
        devname,
        subsystem,
        devtype,
    })
}

/// Wait for the next block event, or `timeout` if none arrives.
pub fn wait_for_block_event(rx: &Receiver<UdevEvent>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match rx.recv_timeout(remaining) {
            Ok(event) if event.is_block_event() => return,
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_block_add_event() {
        let text = "add@/devices/pci0000:00/0000:00:14.0/usb1/1-1/1-1:1.0/host0/target0:0:0/0:0:0:0/block/sda/sda1\0ACTION=add\0DEVPATH=/devices/...\0SUBSYSTEM=block\0DEVNAME=/dev/sda1\0DEVTYPE=partition\0";
        let event = parse_event(text).unwrap();
        assert_eq!(event.action, "add");
        assert_eq!(event.devname.as_deref(), Some("/dev/sda1"));
        assert_eq!(event.subsystem.as_deref(), Some("block"));
        assert_eq!(event.devtype.as_deref(), Some("partition"));
        assert!(event.is_block_event());
    }

    #[test]
    fn ignores_non_block_events() {
        let text = "add\0ACTION=add\0SUBSYSTEM=usb\0DEVNAME=/dev/bus/usb/001/002\0";
        let event = parse_event(text).unwrap();
        assert!(!event.is_block_event());
    }

    #[test]
    fn ignores_events_without_an_action() {
        assert!(parse_event("SUBSYSTEM=block\0DEVNAME=/dev/sda1\0").is_none());
    }
}
