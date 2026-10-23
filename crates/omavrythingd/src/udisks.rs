//! udisks2 D-Bus client for mounting and unmounting block devices.
//!
//! M19 (B3): the daemon prefers the udisks2 D-Bus API over parsing English
//! `udisksctl` output. The API returns the mount point directly, so the mount
//! point is never inferred from localized text. The daemon runs as the
//! dedicated `omavrything` service account; udisks2/polkit may deny a mount
//! that needs an interactive desktop session, in which case the caller falls
//! back to the private read-only `ntfs3` mount.
//!
//! The daemon is synchronous on its mount threads, so each call builds a small
//! current-thread Tokio runtime. Mounts are infrequent, so the runtime cost is
//! irrelevant next to the mount itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use zbus::Variant;

/// B5: a udisks2 D-Bus call that hangs must not wedge the mount thread.
const UDISKS_DBUS_TIMEOUT: Duration = Duration::from_secs(15);

/// The udisks2 well-known bus name.
const UDISKS_SERVICE: &str = "org.freedesktop.UDisks2";
/// The object path prefix for block devices.
const UDISKS_BLOCK_DEVICES: &str = "/org/freedesktop/UDisks2/block_devices";
/// The filesystem interface that carries `Mount`/`Unmount`.
const UDISKS_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";

/// The udisks2 object path for `device`, e.g. `/dev/sda1` ->
/// `/org/freedesktop/UDisks2/block_devices/sda1`.
///
/// A `/dev/disk/by-*` symlink is resolved first so the object path names the
/// real block device.
fn object_path(device: &Path) -> Result<String, String> {
    let resolved = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    let name = resolved
        .file_name()
        .ok_or_else(|| format!("no device basename for {}", device.display()))?;
    Ok(format!("{UDISKS_BLOCK_DEVICES}/{}", name.to_string_lossy()))
}

/// Run `future` on a fresh current-thread runtime.
fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    Ok(runtime.block_on(future))
}

/// Mount `device` through udisks2 and return the mount point.
///
/// `options` is passed as the udisks2 `options` mount option string (for
/// example `ro,nosuid,nodev,noexec`); `None` uses the desktop's default
/// (writable) mount. `auth.no_user_interaction` is set so a system service
/// never blocks on a polkit prompt.
pub fn mount(device: &Path, options: Option<&str>) -> Result<PathBuf, String> {
    let path = object_path(device)?;
    let options = options.map(str::to_string);
    block_on(async move {
        tokio::time::timeout(UDISKS_DBUS_TIMEOUT, async move {
            let connection = zbus::Connection::system()
                .await
                .map_err(|e| format!("udisks2 system bus: {e}"))?;
            let proxy = zbus::Proxy::new(
                &connection,
                UDISKS_SERVICE,
                path.as_str(),
                UDISKS_FILESYSTEM,
            )
            .await
            .map_err(|e| format!("udisks2 proxy: {e}"))?;
            let mut opts: HashMap<&str, Variant<'_>> = HashMap::new();
            opts.insert("auth.no_user_interaction", Variant::Bool(true));
            if let Some(options) = options.as_deref() {
                opts.insert("options", Variant::Str(options.into()));
            }
            let mount_point: String = proxy
                .call("Mount", &(opts,))
                .await
                .map_err(|e| format!("udisks2 Mount: {e}"))?;
            Ok(PathBuf::from(mount_point))
        })
        .await
        .map_err(|_| "udisks2 Mount timed out".to_string())?
    })?
}

/// Unmount `device` through udisks2.
pub fn unmount(device: &Path) -> Result<(), String> {
    let path = object_path(device)?;
    block_on(async move {
        tokio::time::timeout(UDISKS_DBUS_TIMEOUT, async move {
            let connection = zbus::Connection::system()
                .await
                .map_err(|e| format!("udisks2 system bus: {e}"))?;
            let proxy = zbus::Proxy::new(
                &connection,
                UDISKS_SERVICE,
                path.as_str(),
                UDISKS_FILESYSTEM,
            )
            .await
            .map_err(|e| format!("udisks2 proxy: {e}"))?;
            let mut opts: HashMap<&str, Variant<'_>> = HashMap::new();
            opts.insert("auth.no_user_interaction", Variant::Bool(true));
            let _: () = proxy
                .call("Unmount", &(opts,))
                .await
                .map_err(|e| format!("udisks2 Unmount: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|_| "udisks2 Unmount timed out".to_string())?
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_path_uses_the_device_basename() {
        assert_eq!(
            object_path(Path::new("/dev/sda1")).unwrap(),
            "/org/freedesktop/UDisks2/block_devices/sda1"
        );
        assert_eq!(
            object_path(Path::new("/dev/nvme0n1p2")).unwrap(),
            "/org/freedesktop/UDisks2/block_devices/nvme0n1p2"
        );
    }

    #[test]
    fn object_path_rejects_a_path_without_a_basename() {
        assert!(object_path(Path::new("/")).is_err());
    }
}
