//! D-Bus control interface (`org.omarchy.omavrything.Manager`).
//!
//! The search hot path stays on the Unix socket; this interface carries control
//! and status. It is served on the system bus and gated by the bus policy and
//! the `org.omarchy.omavrything.control` polkit action.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use omavrything_core::index::Index;
use omavrything_core::scan::{ScanConfig, rescan_into_index};
use tokio::sync::mpsc::UnboundedSender;
use zbus::interface;
use zbus::object_server::SignalEmitter;

use crate::config::SharedConfig;
use crate::polkit;

/// The well-known bus name.
pub const SERVICE_NAME: &str = "org.omarchy.omavrything";
/// The object path the manager is registered at.
pub const OBJECT_PATH: &str = "/org/omarchy/omavrything";

/// An index event forwarded to the D-Bus signal task.
#[derive(Debug, Clone)]
pub enum IndexEvent {
    /// The index changed; carries the new live entry count.
    Changed { entries: u64 },
    /// A scan is in progress.
    ScanProgress { phase: String, entries: u64 },
}

/// The `org.omarchy.omavrything.Manager` interface.
pub struct Manager {
    index: Arc<RwLock<Index>>,
    config: Arc<SharedConfig>,
    started: Instant,
    events: UnboundedSender<IndexEvent>,
}

impl Manager {
    pub fn new(
        index: Arc<RwLock<Index>>,
        config: Arc<SharedConfig>,
        started: Instant,
        events: UnboundedSender<IndexEvent>,
    ) -> Self {
        Self {
            index,
            config,
            started,
            events,
        }
    }
}

#[interface(name = "org.omarchy.omavrything.Manager")]
impl Manager {
    /// Daemon status: version, entries, roots, uptime.
    async fn get_status(&self) -> HashMap<String, String> {
        let index = self.index.read().unwrap_or_else(|e| e.into_inner());
        let mut map = HashMap::new();
        map.insert("version".into(), omavrything_core::VERSION.into());
        map.insert("entries".into(), index.len().to_string());
        map.insert("roots".into(), index.roots().len().to_string());
        map.insert(
            "uptime_secs".into(),
            self.started.elapsed().as_secs().to_string(),
        );
        map
    }

    /// Rescan all roots (empty `root`) or a single root; returns entries added.
    async fn rescan(
        &self,
        root: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<u64> {
        authorize(&header, connection).await?;
        let _ = self.events.send(IndexEvent::ScanProgress {
            phase: "start".into(),
            entries: 0,
        });
        let index = Arc::clone(&self.index);
        let config = Arc::clone(&self.config);
        let added = tokio::task::spawn_blocking(move || {
            let scan = config.scan();
            let mut idx = index.write().unwrap_or_else(|e| e.into_inner());
            let report = if root.is_empty() {
                rescan_into_index(&mut idx, &scan)
            } else {
                let sub = ScanConfig {
                    roots: vec![PathBuf::from(&root)],
                    excludes: scan.excludes.clone(),
                    skip_pseudo: scan.skip_pseudo,
                };
                rescan_into_index(&mut idx, &sub)
            };
            report.entries as u64
        })
        .await
        .unwrap_or(0);

        let entries = self.index.read().unwrap_or_else(|e| e.into_inner()).len() as u64;
        let _ = self.events.send(IndexEvent::ScanProgress {
            phase: "done".into(),
            entries,
        });
        let _ = self.events.send(IndexEvent::Changed { entries });
        Ok(added)
    }

    /// The current configuration.
    async fn get_config(&self) -> HashMap<String, String> {
        self.config.as_map()
    }

    /// Set a configuration value.
    async fn set_config(
        &self,
        key: String,
        value: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        authorize(&header, connection).await?;
        self.config
            .set(&key, &value)
            .map_err(zbus::fdo::Error::InvalidArgs)
    }

    /// The indexed roots.
    async fn list_roots(&self) -> Vec<String> {
        self.index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .roots()
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }

    /// Mount an NTFS partition by UUID (implemented in M8).
    async fn mount_ntfs(
        &self,
        uuid: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        authorize(&header, connection).await?;
        Err(zbus::fdo::Error::NotSupported(format!(
            "NTFS mounting arrives in M8 (uuid {uuid})"
        )))
    }

    /// Emitted when the index changes.
    #[zbus(signal)]
    async fn index_changed(emitter: &SignalEmitter<'_>, entries: u64) -> zbus::Result<()>;

    /// Emitted during a scan.
    #[zbus(signal)]
    async fn scan_progress(
        emitter: &SignalEmitter<'_>,
        phase: String,
        entries: u64,
    ) -> zbus::Result<()>;
}

/// Authorize a control call via polkit.
async fn authorize(
    header: &zbus::message::Header<'_>,
    connection: &zbus::Connection,
) -> zbus::fdo::Result<()> {
    let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
    let uid = polkit::peer_uid(connection, &sender)
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
    if polkit::is_authorized(connection, uid).await {
        Ok(())
    } else {
        Err(zbus::fdo::Error::AccessDenied(format!(
            "not authorized for {}",
            polkit::ACTION
        )))
    }
}

/// Run the D-Bus service on a dedicated thread with a tokio runtime.
///
/// If the system bus is unavailable the service logs and returns, leaving the
/// socket search path unaffected.
pub fn run(
    index: Arc<RwLock<Index>>,
    config: Arc<SharedConfig>,
    started: Instant,
    mut events: tokio::sync::mpsc::UnboundedReceiver<IndexEvent>,
    event_tx: UnboundedSender<IndexEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("dbus: failed to build runtime: {e}");
            return;
        }
    };

    runtime.block_on(async move {
        let connection = match zbus::Connection::system().await {
            Ok(connection) => connection,
            Err(e) => {
                eprintln!("dbus: cannot connect to the system bus: {e}");
                return;
            }
        };

        let manager = Manager::new(index, config, started, event_tx);
        if let Err(e) = connection.object_server().at(OBJECT_PATH, manager).await {
            eprintln!("dbus: failed to register object: {e}");
            return;
        }
        if let Err(e) = connection.request_name(SERVICE_NAME).await {
            eprintln!("dbus: failed to request name {SERVICE_NAME}: {e}");
            return;
        }
        eprintln!("dbus: serving {SERVICE_NAME} at {OBJECT_PATH}");

        let iface = match connection
            .object_server()
            .interface::<_, Manager>(OBJECT_PATH)
            .await
        {
            Ok(iface) => iface,
            Err(e) => {
                eprintln!("dbus: interface lookup failed: {e}");
                return;
            }
        };

        while let Some(event) = events.recv().await {
            match event {
                IndexEvent::Changed { entries } => {
                    let _ = Manager::index_changed(iface.signal_emitter(), entries).await;
                }
                IndexEvent::ScanProgress { phase, entries } => {
                    let _ = Manager::scan_progress(iface.signal_emitter(), phase, entries).await;
                }
            }
        }
    });
}
