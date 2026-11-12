//! NTFS partition detection.
//!
//! Detection has two paths: a dependency-free boot-sector check (the OEM
//! identifier), and a `blkid` probe that also yields the UUID and label. The
//! `blkid` binary is used rather than binding `libblkid` so the crate has no
//! build-time C dependency.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::boot;

/// Wall-clock budget for one `blkid` probe. A hung `blkid` (a failing device, a
/// stuck udev) must not wedge the caller: the hotplug monitor calls
/// [`list_ntfs_partitions`] on every pass, so an unbounded child would stall the
/// whole monitor thread. The child is killed and reaped on timeout.
const BLKID_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `blkid` with `args`, killing and reaping the child if it does not finish
/// within [`BLKID_TIMEOUT`]. Returns `None` on spawn failure or timeout.
fn run_blkid_timeout(args: &[&str]) -> Option<Output> {
    run_command_timeout("blkid", args, BLKID_TIMEOUT)
}

/// Run `prog` with `args`, capturing its output, and kill it if it does not
/// finish within `timeout`. Returns `None` on spawn failure or timeout.
///
/// The `blkid` probe is the only external command this crate runs, and it can
/// hang on a failing device or a stuck udev; the hotplug monitor calls it on
/// every pass, so an unbounded child would stall the monitor thread.
fn run_command_timeout(prog: &str, args: &[&str], timeout: Duration) -> Option<Output> {
    let mut child = Command::new(prog)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    // Drain both pipes concurrently with the wait loop. A child that writes
    // more than the pipe capacity (about 64 KiB) blocks in `write` until a
    // reader consumes it, so polling `try_wait` alone would never see it exit
    // and would report a spurious timeout.
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // The readers are deliberately not joined: a descendant that
            // inherited the pipe could hold it open past the deadline.
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    Some(Output {
        status,
        stdout,
        stderr,
    })
}

/// Read `pipe` to EOF on its own thread so a blocked child can still be reaped.
fn spawn_reader<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// A detected NTFS partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsPartition {
    /// The block device path, e.g. `/dev/sda1`.
    pub device: PathBuf,
    /// The filesystem UUID, when known.
    pub uuid: Option<String>,
    /// The volume label, when known.
    pub label: Option<String>,
}

/// Whether `path` begins with an NTFS boot sector.
pub fn is_ntfs_device(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 512];
    file.read_exact(&mut buf)?;
    Ok(boot::is_ntfs(&buf))
}

/// Probe a single device with `blkid`, returning it when it is NTFS.
pub fn probe_blkid(device: &Path) -> Option<NtfsPartition> {
    let device_str = device.to_string_lossy();
    let output = run_blkid_timeout(&["-o", "export", &device_str])?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut uuid = None;
    let mut label = None;
    let mut fstype = None;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "UUID" => uuid = Some(value.to_string()),
                "LABEL" => label = Some(value.to_string()),
                "TYPE" => fstype = Some(value.to_string()),
                _ => {}
            }
        }
    }
    if is_ntfs_type(fstype.as_deref()) {
        Some(NtfsPartition {
            device: device.to_path_buf(),
            uuid,
            label,
        })
    } else {
        None
    }
}

/// Enumerate all NTFS partitions on the system via `blkid`.
///
/// Bounded by [`BLKID_TIMEOUT`]: the hotplug monitor calls this on every pass,
/// so an unbounded child would stall the monitor thread. A timeout yields an
/// empty list, matching the previous "no partitions detected" behaviour.
pub fn list_ntfs_partitions() -> Vec<NtfsPartition> {
    let output = match run_blkid_timeout(&["-o", "export"]) {
        Some(output) if output.status.success() => output.stdout,
        _ => return Vec::new(),
    };
    parse_blkid_export(&String::from_utf8_lossy(&output))
}

/// Parse `blkid -o export` output (blank-line-separated device blocks).
pub fn parse_blkid_export(text: &str) -> Vec<NtfsPartition> {
    let mut out = Vec::new();
    let mut device: Option<String> = None;
    let mut uuid: Option<String> = None;
    let mut label: Option<String> = None;
    let mut fstype: Option<String> = None;

    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut out, &mut device, &mut uuid, &mut label, &mut fstype);
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "DEVNAME" => device = Some(value.to_string()),
                "UUID" => uuid = Some(value.to_string()),
                "LABEL" => label = Some(value.to_string()),
                "TYPE" => fstype = Some(value.to_string()),
                _ => {}
            }
        }
    }
    flush(&mut out, &mut device, &mut uuid, &mut label, &mut fstype);
    out
}

fn flush(
    out: &mut Vec<NtfsPartition>,
    device: &mut Option<String>,
    uuid: &mut Option<String>,
    label: &mut Option<String>,
    fstype: &mut Option<String>,
) {
    if let Some(dev) = device.take() {
        if is_ntfs_type(fstype.as_deref()) {
            out.push(NtfsPartition {
                device: PathBuf::from(dev),
                uuid: uuid.take(),
                label: label.take(),
            });
        }
    }
    *uuid = None;
    *label = None;
    *fstype = None;
}

fn is_ntfs_type(fstype: Option<&str>) -> bool {
    matches!(fstype, Some("ntfs") | Some("ntfs3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blkid_export_and_filters_ntfs() {
        let text = "\
DEVNAME=/dev/sda1
UUID=AAAA-BBBB
TYPE=ntfs
LABEL=Windows

DEVNAME=/dev/sda2
UUID=CCCC-DDDD
TYPE=ext4

DEVNAME=/dev/sdb1
UUID=EEEE-FFFF
TYPE=ntfs3
LABEL=Data
";
        let parts = parse_blkid_export(text);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].device, PathBuf::from("/dev/sda1"));
        assert_eq!(parts[0].uuid.as_deref(), Some("AAAA-BBBB"));
        assert_eq!(parts[0].label.as_deref(), Some("Windows"));
        assert_eq!(parts[1].device, PathBuf::from("/dev/sdb1"));
        assert_eq!(parts[1].label.as_deref(), Some("Data"));
    }

    #[test]
    fn empty_output_yields_no_partitions() {
        assert!(parse_blkid_export("").is_empty());
    }

    #[test]
    fn command_timeout_kills_a_hung_process() {
        // The hotplug monitor calls `list_ntfs_partitions` on every pass; a hung
        // `blkid` must be killed and reaped rather than stalling the monitor
        // thread. `sleep` stands in for a hung probe.
        let start = Instant::now();
        assert!(run_command_timeout("sleep", &["30"], Duration::from_millis(100)).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the timeout must fire promptly, not wait for the child"
        );
    }

    #[test]
    fn command_timeout_returns_output_for_a_fast_process() {
        let output = run_command_timeout("true", &[], Duration::from_secs(5))
            .expect("a fast command must return its output");
        assert!(output.status.success());
    }

    #[test]
    fn command_timeout_drains_output_larger_than_a_pipe_buffer() {
        // A child that writes more than the ~64 KiB pipe buffer blocks in
        // `write` until the reader drains it. The wait loop must not mistake
        // that for a hang.
        let output = run_command_timeout(
            "sh",
            &[
                "-c",
                "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2",
            ],
            Duration::from_secs(5),
        )
        .expect("a fast command with large output must not time out");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 200_000);
        assert_eq!(output.stderr.len(), 200_000);
    }
}
