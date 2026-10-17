//! D-Bus control interface (`tech.alatcerdas.omavrything.Manager`).
//!
//! The search hot path stays on the Unix socket; this interface carries control
//! and status. It is served on the system bus and gated by the bus policy and
//! the `tech.alatcerdas.omavrything.control` polkit action.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use omavrything_core::scan::{ScanCancel, ScanConfig, apply_rescan, plan_rescan_cancellable};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};
use zbus::interface;
use zbus::object_server::SignalEmitter;

use crate::config::SharedConfig;
use crate::manager::IndexManager;
use crate::ntfs::{self, NtfsPolicy};
use crate::polkit;
use crate::volumes;

/// The well-known bus name.
pub const SERVICE_NAME: &str = "tech.alatcerdas.omavrything";
/// The object path the manager is registered at.
pub const OBJECT_PATH: &str = "/tech/alatcerdas/omavrything";

/// An index event forwarded to the D-Bus signal task.
#[derive(Debug, Clone)]
pub enum IndexEvent {
    /// The index changed; carries the new live entry count.
    Changed { entries: u64 },
    /// A scan is in progress. `errors` is the traversal failure count (A2).
    ScanProgress {
        phase: String,
        entries: u64,
        errors: u64,
    },
}

/// The `tech.alatcerdas.omavrything.Manager` interface.
pub struct Manager {
    index: Arc<IndexManager>,
    config: Arc<SharedConfig>,
    started: Instant,
    events: UnboundedSender<IndexEvent>,
    policy: Arc<Mutex<NtfsPolicy>>,
}

impl Manager {
    pub fn new(
        index: Arc<IndexManager>,
        config: Arc<SharedConfig>,
        started: Instant,
        events: UnboundedSender<IndexEvent>,
        policy: Arc<Mutex<NtfsPolicy>>,
    ) -> Self {
        Self {
            index,
            config,
            started,
            events,
            policy,
        }
    }
}

#[interface(name = "tech.alatcerdas.omavrything.Manager")]
impl Manager {
    /// Daemon status: version, entries, roots, memory, scan state, uptime.
    async fn get_status(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("version".into(), omavrything_core::VERSION.into());
        map.insert("entries".into(), self.index.len().to_string());
        map.insert("roots".into(), self.index.roots().len().to_string());
        map.insert("memory_bytes".into(), self.index.memory_bytes().to_string());
        // rc7: the resident (capacity) figure, so the benchmark can attribute the
        // gap between the index accounting and RSS to reclaimable slack.
        map.insert(
            "memory_capacity_bytes".into(),
            self.index.capacity_bytes().to_string(),
        );
        let b = self.index.memory_breakdown();
        map.insert("records_len".into(), b.records_len.to_string());
        map.insert("records_cap".into(), b.records_cap.to_string());
        map.insert("paths_len".into(), b.paths_len.to_string());
        map.insert("paths_cap".into(), b.paths_cap.to_string());
        map.insert("map_len".into(), b.map_len.to_string());
        map.insert("map_cap_bytes".into(), b.map_cap_bytes.to_string());
        map.insert("candidate_bytes".into(), b.candidate_bytes.to_string());
        map.insert(
            "candidate_capacity_bytes".into(),
            b.candidate_capacity_bytes.to_string(),
        );
        // Per-component candidate-index breakdown (F11).
        map.insert("candidate_lower".into(), b.candidate.lower.to_string());
        map.insert("candidate_off".into(), b.candidate.off.to_string());
        map.insert("candidate_len".into(), b.candidate.len.to_string());
        map.insert("candidate_parent".into(), b.candidate.parent.to_string());
        map.insert(
            "candidate_postings".into(),
            b.candidate.postings.to_string(),
        );
        map.insert("candidate_map".into(), b.candidate.map.to_string());
        map.insert("candidate_subtree".into(), b.candidate.subtree.to_string());
        map.insert("candidate_stop".into(), b.candidate.stop.to_string());
        // rc7: per-component capacity, so the slack in each arena is visible.
        map.insert(
            "candidate_lower_cap".into(),
            b.candidate.lower_cap.to_string(),
        );
        map.insert("candidate_off_cap".into(), b.candidate.off_cap.to_string());
        map.insert("candidate_len_cap".into(), b.candidate.len_cap.to_string());
        map.insert(
            "candidate_parent_cap".into(),
            b.candidate.parent_cap.to_string(),
        );
        map.insert(
            "candidate_postings_cap".into(),
            b.candidate.postings_cap.to_string(),
        );
        map.insert("candidate_map_cap".into(), b.candidate.map_cap.to_string());
        map.insert(
            "candidate_subtree_cap".into(),
            b.candidate.subtree_cap.to_string(),
        );
        map.insert(
            "candidate_stop_cap".into(),
            b.candidate.stop_cap.to_string(),
        );
        map.insert(
            "candidate_stack_cap".into(),
            b.candidate.stack_cap.to_string(),
        );
        map.insert("scanning".into(), self.config.scanning().to_string());
        // A2: the last scan's traversal failures, with a bounded path sample.
        let (scan_errors, scan_error_paths) = self.config.last_scan_errors();
        map.insert("scan_errors".into(), scan_errors.to_string());
        map.insert("scan_error_paths".into(), scan_error_paths.join(";"));
        // A5: how many subtrees could not be watched (watch-limit exhaustion).
        map.insert(
            "unwatched_dirs".into(),
            self.config.unwatched().len().to_string(),
        );
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
        authorize(&header, connection, polkit::ACTION).await?;
        run_rescan(
            Arc::clone(&self.index),
            Arc::clone(&self.config),
            self.events.clone(),
            &root,
        )
        .await
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
        // Cosmetic keys (the scratchpad opacity) use the ungated-for-active-users
        // `set-config` action; every other key keeps the privileged `control`
        // action, so changing the UI does not also grant MountNtfs.
        let action = if polkit::is_cosmetic_key(&key) {
            polkit::CONFIG_ACTION
        } else {
            polkit::ACTION
        };
        authorize(&header, connection, action).await?;
        self.config
            .set(&key, &value)
            .map_err(zbus::fdo::Error::InvalidArgs)?;
        // Changing the excludes changes what should be indexed; reconcile now so
        // newly excluded paths stop being served without waiting for the next
        // scheduled rescan.
        if key == "excludes" {
            let _ = run_rescan(
                Arc::clone(&self.index),
                Arc::clone(&self.config),
                self.events.clone(),
                "",
            )
            .await;
        }
        // Enabling NTFS detection indexes any partitions that are already
        // present; disabling it only records the preference (nothing is
        // unmounted).
        if key == "ntfs" && self.config.ntfs() {
            let index = Arc::clone(&self.index);
            let policy = Arc::clone(&self.policy);
            crate::spawn_thread("ntfs-startup", crate::CONTROL_STACK, move || {
                crate::run_ntfs_startup(index, policy)
            });
        }
        // F10: apply the candidate-index preference to the running index so the
        // change takes effect without a restart (disabling reclaims its memory).
        if key == "candidate_index" {
            let enabled = self.config.candidate_index();
            self.index
                .with_index_mut(|index| index.set_candidate_enabled(enabled));
        }
        Ok(())
    }

    /// The indexed roots.
    async fn list_roots(&self) -> Vec<String> {
        self.index.roots().iter().map(|r| r.path.clone()).collect()
    }

    /// Every detected block volume with its indexing status.
    ///
    /// NTFS volumes are omitted while NTFS support is disabled, matching the
    /// detection gate. Read-only: no polkit authorization is required.
    async fn list_volumes(&self) -> Vec<HashMap<String, String>> {
        let ntfs_enabled = self.config.ntfs();
        let detected = tokio::task::spawn_blocking(volumes::list_block_volumes)
            .await
            .unwrap_or_default();
        detected
            .into_iter()
            .filter(|v| ntfs_enabled || !v.is_ntfs())
            .map(|v| {
                let mut map = HashMap::new();
                map.insert("device".into(), v.device.display().to_string());
                map.insert("uuid".into(), v.uuid.clone().unwrap_or_default());
                map.insert("label".into(), v.label.clone().unwrap_or_default());
                map.insert("fstype".into(), v.fstype.clone());
                map.insert(
                    "mount_point".into(),
                    v.mount_point
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                );
                map.insert("status".into(), volumes::status(&v, &self.index).into());
                map
            })
            .collect()
    }

    /// Mount a volume by device path and index it. Returns the mount point.
    ///
    /// NTFS uses the MFT path (read-only mount); every other filesystem is
    /// mounted through udisks2 and added as an index root.
    async fn mount_volume(
        &self,
        device: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        authorize(&header, connection, polkit::ACTION).await?;
        let device = PathBuf::from(device);
        let probe_device = device.clone();
        let fstype = tokio::task::spawn_blocking(move || volumes::probe_fstype(&probe_device))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs(format!(
                    "no filesystem detected on {}",
                    device.display()
                ))
            })?;
        let is_ntfs = matches!(fstype.as_str(), "ntfs" | "ntfs3");
        if is_ntfs && !self.config.ntfs() {
            return Err(zbus::fdo::Error::AccessDenied(
                "NTFS support is disabled".into(),
            ));
        }

        if is_ntfs {
            let partition = omavrything_ntfs::list_ntfs_partitions()
                .into_iter()
                .find(|p| p.device == device)
                .ok_or_else(|| {
                    zbus::fdo::Error::InvalidArgs(format!(
                        "no NTFS partition at {}",
                        device.display()
                    ))
                })?;
            let index = Arc::clone(&self.index);
            let policy = Arc::clone(&self.policy);
            let mount_point = tokio::task::spawn_blocking(move || {
                ntfs::mount_index_and_remember(&index, &policy, &partition)
            })
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)?;
            let entries = self.index.len() as u64;
            let _ = self.events.send(IndexEvent::Changed { entries });
            let _ = self.config.remember_volume(&device.to_string_lossy());
            return Ok(mount_point.display().to_string());
        }

        // Non-NTFS: mount through udisks2, add the mount point as a root, and
        // walk it. The indexing guard makes the settings UI show "indexing"
        // while the walk runs.
        let mount_device = device.clone();
        let mount_point =
            tokio::task::spawn_blocking(move || volumes::mount_generic(&mount_device))
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
                .map_err(zbus::fdo::Error::Failed)?;

        let kind = omavrything_core::mounts::fs_kind(&fstype);
        self.index
            .with_index_mut(|idx| idx.add_root(&mount_point, kind));
        let _indexing = volumes::IndexingGuard::new(&device);
        run_rescan(
            Arc::clone(&self.index),
            Arc::clone(&self.config),
            self.events.clone(),
            &mount_point.display().to_string(),
        )
        .await?;
        let _ = self.config.remember_volume(&device.to_string_lossy());
        Ok(mount_point.display().to_string())
    }

    /// Mount an NTFS partition by UUID and index it via the MFT. Returns the
    /// mount point.
    async fn mount_ntfs(
        &self,
        uuid: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        authorize(&header, connection, polkit::ACTION).await?;
        let partition = omavrything_ntfs::list_ntfs_partitions()
            .into_iter()
            .find(|p| p.uuid.as_deref() == Some(uuid.as_str()))
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs(format!("no NTFS partition with uuid {uuid}"))
            })?;

        let index = Arc::clone(&self.index);
        let policy = Arc::clone(&self.policy);
        let device = partition.device.clone();
        let mount_point = tokio::task::spawn_blocking(move || {
            ntfs::mount_index_and_remember(&index, &policy, &partition)
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        .map_err(zbus::fdo::Error::Failed)?;

        let entries = self.index.len() as u64;
        let _ = self.events.send(IndexEvent::Changed { entries });
        let _ = self.config.remember_volume(&device.to_string_lossy());
        Ok(mount_point.display().to_string())
    }

    /// Unmount a volume by device path and drop its index entries.
    async fn unmount_volume(
        &self,
        device: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        authorize(&header, connection, polkit::ACTION).await?;
        let device = PathBuf::from(device);
        let Some(mount_point) = volumes::existing_mount_point(&device) else {
            return Ok(());
        };
        let unmount_device = device.clone();
        let unmount_point = mount_point.clone();
        tokio::task::spawn_blocking(move || volumes::unmount(&unmount_device, &unmount_point))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)?;

        // The volume is gone: stop serving its records and forget the root.
        self.index
            .with_index_mut(|idx| idx.remove_root(&mount_point));
        let mut ownership = ntfs::MountOwnership::load(ntfs::default_ownership_path());
        let _ = ownership.forget(&mount_point);
        let _ = self.config.forget_volume(&device.to_string_lossy());
        let entries = self.index.len() as u64;
        let _ = self.events.send(IndexEvent::Changed { entries });
        Ok(())
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
        errors: u64,
    ) -> zbus::Result<()>;
}

/// Rescan all roots (empty `root`) or a single root, returning entries added.
///
/// The rescan runs against a clone of the index which is swapped in on
/// completion, so socket searches keep serving the previous index instead of
/// blocking on a long write lock. A non-empty `root` must be one of the
/// configured roots; otherwise the scan would inject records for an unmanaged
/// root that the snapshot loader never validates or drops.
async fn run_rescan(
    index: Arc<IndexManager>,
    config: Arc<SharedConfig>,
    events: UnboundedSender<IndexEvent>,
    root: &str,
) -> zbus::fdo::Result<u64> {
    if !root.is_empty() {
        let known = index.roots().iter().any(|r| r.path == root);
        if !known {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown root: {root}"
            )));
        }
    }

    let _ = events.send(IndexEvent::ScanProgress {
        phase: "start".into(),
        entries: 0,
        errors: 0,
    });
    // A3: supersede any running scan before starting this one.
    config.bump_scan_generation();
    config.set_scanning(true);

    let root = root.to_string();
    let task_index = Arc::clone(&index);
    let task_config = Arc::clone(&config);
    let (added, errors) = tokio::task::spawn_blocking(move || {
        let scan = task_config.scan();
        // A3: this walk stops promptly on shutdown or when a newer rescan
        // supersedes it.
        let cancel = ScanCancel::new(task_config.generation_cell(), task_config.shutdown_cell());
        // Walk outside the lock, then reconcile under it. This avoids cloning the
        // whole index (which doubled peak memory) while keeping searches
        // unblocked during the filesystem traversal.
        let plan = if root.is_empty() {
            plan_rescan_cancellable(&scan, Some(&cancel))
        } else {
            let sub = ScanConfig {
                roots: vec![PathBuf::from(&root)],
                excludes: scan.excludes.clone(),
                skip_pseudo: scan.skip_pseudo,
                metadata: scan.metadata,
                // A targeted rescan of a volume the user explicitly mounted may
                // live under /run/media; the full scan still excludes it.
                allow_removable: omavrything_core::mounts::is_removable_media_path(Path::new(
                    &root,
                )),
            };
            plan_rescan_cancellable(&sub, Some(&cancel))
        };
        if cancel.cancelled() {
            // Superseded or shutting down: discard the partial plan.
            return (0u64, 0u64);
        }
        let report = task_index.with_index_mut(|idx| apply_rescan(idx, plan));
        task_config.set_last_scan_errors(report.errors, report.error_paths.clone());
        (report.entries as u64, report.errors as u64)
    })
    .await
    .unwrap_or((0, 0));

    config.set_scanning(false);
    let entries = index.len() as u64;
    let _ = events.send(IndexEvent::ScanProgress {
        phase: "done".into(),
        entries,
        errors,
    });
    let _ = events.send(IndexEvent::Changed { entries });
    Ok(added)
}

/// Authorize a control call via polkit for `action`.
async fn authorize(
    header: &zbus::message::Header<'_>,
    connection: &zbus::Connection,
    action: &str,
) -> zbus::fdo::Result<()> {
    let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
    let uid = polkit::peer_uid(connection, &sender)
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
    if polkit::is_authorized(connection, uid, &sender, action).await {
        Ok(())
    } else {
        Err(zbus::fdo::Error::AccessDenied(format!(
            "not authorized for {action}"
        )))
    }
}

/// Run the D-Bus service on a dedicated thread with a tokio runtime.
///
/// If the system bus is unavailable the service logs and returns, leaving the
/// socket search path unaffected.
pub fn run(
    index: Arc<IndexManager>,
    config: Arc<SharedConfig>,
    started: Instant,
    mut events: tokio::sync::mpsc::UnboundedReceiver<IndexEvent>,
    event_tx: UnboundedSender<IndexEvent>,
    policy: Arc<Mutex<NtfsPolicy>>,
) {
    // The D-Bus service is event-driven and does no CPU-bound work, so a small
    // fixed worker pool is enough; the default (one worker per core) spawns a
    // thread per core that is idle for the daemon's whole life (rc6 RSS spike).
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            error!(error = %e, "dbus: failed to build runtime");
            return;
        }
    };

    runtime.block_on(async move {
        let connection = match zbus::Connection::system().await {
            Ok(connection) => connection,
            Err(e) => {
                warn!(error = %e, "dbus: cannot connect to the system bus");
                return;
            }
        };

        let manager = Manager::new(index, config, started, event_tx, policy);
        if let Err(e) = connection.object_server().at(OBJECT_PATH, manager).await {
            error!(error = %e, "dbus: failed to register object");
            return;
        }
        if let Err(e) = connection.request_name(SERVICE_NAME).await {
            error!(service = SERVICE_NAME, error = %e, "dbus: failed to request name");
            return;
        }
        info!(service = SERVICE_NAME, path = OBJECT_PATH, "dbus: serving");

        let iface = match connection
            .object_server()
            .interface::<_, Manager>(OBJECT_PATH)
            .await
        {
            Ok(iface) => iface,
            Err(e) => {
                error!(error = %e, "dbus: interface lookup failed");
                return;
            }
        };

        while let Some(event) = events.recv().await {
            match event {
                IndexEvent::Changed { entries } => {
                    let _ = Manager::index_changed(iface.signal_emitter(), entries).await;
                }
                IndexEvent::ScanProgress {
                    phase,
                    entries,
                    errors,
                } => {
                    let _ = Manager::scan_progress(iface.signal_emitter(), phase, entries, errors)
                        .await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use omavrything_core::index::{FsKind, Index};
    use std::path::Path;
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test]
    async fn rescan_rejects_an_unknown_root() {
        let mut index = Index::new();
        index.add_root(Path::new("/known"), FsKind::Other);
        let index = Arc::new(IndexManager::new(index, Vec::new(), None));
        let config = Arc::new(SharedConfig::new(crate::config::ConfigInit {
            scan: ScanConfig::default(),
            scan_interval: 900,
            snapshot_interval: 300,
            evict_idle_secs: 120,
            snapshot: None,
            watch: true,
            ntfs: true,
            log_level: crate::logging::LogLevel::Info,
            background_opacity: 0.75,
            candidate_index: true,
            state: crate::config_file::FileConfig::default(),
            state_path: None,
        }));
        let (tx, _rx) = unbounded_channel();

        let err = run_rescan(Arc::clone(&index), config, tx, "/unknown")
            .await
            .unwrap_err();
        assert!(matches!(err, zbus::fdo::Error::InvalidArgs(_)));
    }
}
