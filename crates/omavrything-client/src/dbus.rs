//! Client for the daemon's D-Bus control interface.

use std::collections::HashMap;

use zbus::proxy;

/// The control interface exposed by `omavrythingd` on the system bus.
#[proxy(
    interface = "org.omarchy.omavrything.Manager",
    default_service = "org.omarchy.omavrything",
    default_path = "/org/omarchy/omavrything"
)]
pub trait Manager {
    /// Daemon status: version, entries, roots, uptime.
    fn get_status(&self) -> zbus::Result<HashMap<String, String>>;
    /// Rescan all roots (empty `root`) or a single root; returns entries added.
    fn rescan(&self, root: &str) -> zbus::Result<u64>;
    /// The current configuration.
    fn get_config(&self) -> zbus::Result<HashMap<String, String>>;
    /// Set a configuration value.
    fn set_config(&self, key: &str, value: &str) -> zbus::Result<()>;
    /// The indexed roots.
    fn list_roots(&self) -> zbus::Result<Vec<String>>;
    /// Mount an NTFS partition by UUID (M8).
    fn mount_ntfs(&self, uuid: &str) -> zbus::Result<()>;

    /// Emitted when the index changes.
    #[zbus(signal)]
    fn index_changed(&self, entries: u64) -> zbus::Result<()>;
    /// Emitted during a scan.
    #[zbus(signal)]
    fn scan_progress(&self, phase: &str, entries: u64) -> zbus::Result<()>;
}

/// A client for the D-Bus control interface.
pub struct DbusClient {
    proxy: ManagerProxy<'static>,
}

impl DbusClient {
    /// Connect to the system bus and bind the manager proxy.
    pub async fn connect() -> zbus::Result<Self> {
        let connection = zbus::Connection::system().await?;
        let proxy = ManagerProxy::new(&connection).await?;
        Ok(Self { proxy })
    }

    /// Daemon status.
    pub async fn status(&self) -> zbus::Result<HashMap<String, String>> {
        self.proxy.get_status().await
    }

    /// Rescan all roots (empty `root`) or a single root.
    pub async fn rescan(&self, root: &str) -> zbus::Result<u64> {
        self.proxy.rescan(root).await
    }

    /// The current configuration.
    pub async fn config(&self) -> zbus::Result<HashMap<String, String>> {
        self.proxy.get_config().await
    }

    /// Set a configuration value.
    pub async fn set_config(&self, key: &str, value: &str) -> zbus::Result<()> {
        self.proxy.set_config(key, value).await
    }

    /// The indexed roots.
    pub async fn roots(&self) -> zbus::Result<Vec<String>> {
        self.proxy.list_roots().await
    }

    /// Mount an NTFS partition by UUID.
    pub async fn mount_ntfs(&self, uuid: &str) -> zbus::Result<()> {
        self.proxy.mount_ntfs(uuid).await
    }
}
