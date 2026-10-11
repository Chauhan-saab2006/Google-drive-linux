use crate::auth::{GoogleOAuthClient, OAuthConfig, TokenStore};
use crate::cache::{self, MetadataCache};
use crate::config::AppConfig;
use crate::drive::GoogleDriveClient;
use crate::filesystem;
use crate::sync::{OfflineQueue, SyncEngine};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Lifecycle controller for the background daemon service.
pub struct DaemonService {
    config: AppConfig,
    running: Arc<AtomicBool>,
}

impl DaemonService {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run the daemon event loop until a shutdown signal is received.
    ///
    /// Responsibilities:
    /// 1. Verify stored credentials are present and valid.
    /// 2. Open the local metadata + chunk cache.
    /// 3. Auto-mount Google Drive if `auto_mount` is enabled.
    /// 4. Periodically check mount health and remount on failure.
    /// 5. Cleanly unmount and exit on SIGINT / SIGTERM.
    pub async fn run(&self) -> Result<(), String> {
        self.running.store(true, Ordering::SeqCst);
        info!(
            "Google Drive Linux daemon starting. Mount point: {:?}",
            self.config.mount_point
        );

        // Load credentials
        let creds_path = AppConfig::default_credentials_path()
            .map_err(|e| format!("Failed to resolve credentials path: {e}"))?;
        let token_store = TokenStore::new(creds_path);

        let creds = match token_store.load() {
            Ok(Some(c)) => c,
            Ok(None) => {
                warn!("No stored credentials found. Please run `google-drive-linux login` first.");
                return Err(
                    "Daemon cannot start without credentials. Run `google-drive-linux login`."
                        .to_string(),
                );
            }
            Err(e) => {
                error!("Failed to load credentials: {}", e);
                return Err(format!("Credential load error: {e}"));
            }
        };

        if creds.is_expired() && creds.refresh_token.is_empty() {
            error!("Stored credentials are expired and cannot be refreshed.");
            return Err(
                "Credentials expired. Please run `google-drive-linux login` again.".to_string(),
            );
        }

        // Build shared OAuth + Drive clients
        let oauth = Arc::new(GoogleOAuthClient::new(
            OAuthConfig {
                client_id: creds.client_id.clone().unwrap_or_default(),
                client_secret: creds.client_secret.clone(),
                ..OAuthConfig::default()
            },
            token_store,
        ));
        let drive_client = Arc::new(GoogleDriveClient::new(None, None, oauth));

        // Open metadata + chunk cache (shared across reconnects)
        let meta_cache: Option<Arc<Mutex<MetadataCache>>> =
            match cache::open_default_cache(self.config.cache_size_mb) {
                Ok(c) => {
                    info!(
                        "Daemon: metadata cache opened ({} MB limit).",
                        self.config.cache_size_mb
                    );
                    Some(Arc::new(Mutex::new(c)))
                }
                Err(e) => {
                    warn!(
                        "Daemon: failed to open metadata cache: {}. Running without cache.",
                        e
                    );
                    None
                }
            };

        // Initialize OfflineQueue & SyncEngine for remote polling and offline write flush
        let queue_path = dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("google-drive-linux")
            .join("offline_queue.json");
        let offline_queue = Arc::new(OfflineQueue::new(queue_path));
        let sync_engine = Arc::new(SyncEngine::new(
            drive_client.clone(),
            meta_cache.clone(),
            offline_queue.clone(),
            self.config.sync_interval_secs,
        ));

        // Attempt initial auto-mount
        let mount_path = self.config.mount_point.clone();
        let mut active_mount = None;

        if self.config.auto_mount {
            if filesystem::is_path_mounted(&mount_path) {
                info!("Google Drive already mounted at {:?}.", mount_path);
            } else {
                info!("Auto-mounting Google Drive at {:?}...", mount_path);
                match self.try_mount(drive_client.clone(), meta_cache.clone()) {
                    Ok(m) => {
                        info!("✓ Google Drive successfully mounted at {:?}", mount_path);
                        active_mount = Some(m);
                    }
                    Err(e) => {
                        warn!("Initial mount failed: {}. Will retry automatically.", e);
                    }
                }
            }
        } else {
            info!("Auto-mount disabled. Start mount manually via CLI or GUI.");
        }

        // Register shutdown handler
        let running_flag = self.running.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                info!("Daemon received shutdown signal (Ctrl-C / SIGINT).");
                running_flag.store(false, Ordering::SeqCst);
            }
        });

        // Heartbeat & Sync loop — check mount health and sync changes every 30 seconds
        let check_interval = Duration::from_secs(self.config.sync_interval_secs.max(10));
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(check_interval).await;

            if !self.running.load(Ordering::SeqCst) {
                break;
            }

            let is_mounted = filesystem::is_path_mounted(&mount_path);
            if !is_mounted {
                warn!(
                    "Google Drive is no longer mounted at {:?}. Attempting remount...",
                    mount_path
                );
                active_mount = None;

                match self.try_mount(drive_client.clone(), meta_cache.clone()) {
                    Ok(m) => {
                        info!("✓ Remounted Google Drive at {:?}", mount_path);
                        active_mount = Some(m);
                    }
                    Err(e) => {
                        error!("Remount failed: {}. Will retry in {:?}.", e, check_interval);
                    }
                }
            } else {
                info!("Mount health check OK: {:?} is mounted.", mount_path);

                // Run background sync
                if let Err(e) = sync_engine.flush_offline_queue().await {
                    warn!("Daemon: error flushing offline queue: {}", e);
                }
                if let Err(e) = sync_engine.poll_remote_changes().await {
                    debug!("Daemon: error polling remote changes: {}", e);
                }
            }
        }

        // Graceful shutdown
        info!("Daemon shutting down gracefully...");
        if let Some(mount) = active_mount.take() {
            if let Err(e) = mount.unmount() {
                error!("Error unmounting on daemon shutdown: {}", e);
            } else {
                info!("✓ Google Drive safely unmounted.");
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!("Google Drive Linux daemon stopped cleanly.");
        Ok(())
    }

    /// Attempt to mount Google Drive with the shared cache, returning the active mount handle.
    fn try_mount(
        &self,
        drive_client: Arc<GoogleDriveClient>,
        cache: Option<Arc<Mutex<MetadataCache>>>,
    ) -> Result<filesystem::ActiveMount, String> {
        let handle = tokio::runtime::Handle::current();
        filesystem::mount_filesystem(&self.config.mount_point, drive_client, handle, cache)
            .map_err(|e| e.to_string())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_initialization() {
        let config = AppConfig::default();
        let daemon = DaemonService::new(config);
        assert!(!daemon.is_running());
    }

    #[test]
    fn test_daemon_config_propagation() {
        let mut config = AppConfig::default();
        config.auto_mount = false;
        let daemon = DaemonService::new(config.clone());
        assert!(!daemon.config.auto_mount);
    }
}
