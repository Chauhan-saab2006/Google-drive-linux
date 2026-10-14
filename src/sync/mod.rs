use crate::cache::{CacheStorage, CachedFileRecord, FileSyncStatus, MetadataCache};
use crate::drive::GoogleDriveClient;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Conflict resolution strategy when remote and local versions differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConflictStrategy {
    #[default]
    KeepBothRenameLocal,
    PreferRemote,
    PreferLocal,
    AskUser,
}

/// Operation queued for background sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncTask {
    Upload {
        file_id: Option<String>,
        local_path: String,
        parent_id: Option<String>,
        name: String,
        mime_type: String,
    },
    Download {
        file_id: String,
        target_path: String,
    },
    Delete {
        file_id: String,
    },
    Rename {
        file_id: String,
        new_name: String,
    },
}

/// Thread-safe, file-persisted FIFO queue for offline write operations.
pub struct OfflineQueue {
    path: PathBuf,
    tasks: Mutex<Vec<SyncTask>>,
}

impl OfflineQueue {
    /// Create or load an offline queue from the specified file path.
    pub fn new(path: PathBuf) -> Self {
        let tasks = if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => serde_json::from_str::<Vec<SyncTask>>(&content).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };

        Self {
            path,
            tasks: Mutex::new(tasks),
        }
    }

    /// Add a task to the queue and persist it to disk.
    pub fn push(&self, task: SyncTask) -> Result<(), String> {
        let mut tasks = self.tasks.lock().map_err(|e| e.to_string())?;
        tasks.push(task);
        self.persist(&tasks)
    }

    /// Drain all queued tasks and clear the disk file.
    pub fn drain(&self) -> Result<Vec<SyncTask>, String> {
        let mut tasks = self.tasks.lock().map_err(|e| e.to_string())?;
        let drained = std::mem::take(&mut *tasks);
        let _ = self.persist(&tasks);
        Ok(drained)
    }

    /// Prepend tasks back to the queue (e.g. if a network flush was interrupted).
    pub fn prepend(&self, mut uncompleted: Vec<SyncTask>) -> Result<(), String> {
        let mut tasks = self.tasks.lock().map_err(|e| e.to_string())?;
        uncompleted.append(&mut *tasks);
        *tasks = uncompleted;
        self.persist(&tasks)
    }

    /// Peek all tasks currently queued without removing them.
    pub fn peek_all(&self) -> Result<Vec<SyncTask>, String> {
        let tasks = self.tasks.lock().map_err(|e| e.to_string())?;
        Ok(tasks.clone())
    }

    /// Returns true if there are no pending tasks.
    pub fn is_empty(&self) -> bool {
        self.tasks.lock().map(|t| t.is_empty()).unwrap_or(true)
    }

    /// Count of pending tasks.
    pub fn len(&self) -> usize {
        self.tasks.lock().map(|t| t.len()).unwrap_or(0)
    }

    fn persist(&self, tasks: &[SyncTask]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let serialized = serde_json::to_string_pretty(tasks).map_err(|e| e.to_string())?;
        fs::write(&self.path, serialized).map_err(|e| e.to_string())
    }
}

/// Synchronization engine responsible for:
/// 1. Polling remote Google Drive changes via Changes API and updating local cache.
/// 2. Processing offline queued writes when connectivity is available.
pub struct SyncEngine {
    client: Arc<GoogleDriveClient>,
    cache: Option<Arc<Mutex<MetadataCache>>>,
    queue: Arc<OfflineQueue>,
    pub sync_interval: Duration,
}

impl SyncEngine {
    pub fn new(
        client: Arc<GoogleDriveClient>,
        cache: Option<Arc<Mutex<MetadataCache>>>,
        queue: Arc<OfflineQueue>,
        sync_interval_secs: u64,
    ) -> Self {
        Self {
            client,
            cache,
            queue,
            sync_interval: Duration::from_secs(sync_interval_secs.max(5)),
        }
    }

    /// Poll remote changes from Google Drive and apply them to the metadata cache.
    /// Returns the number of applied changes.
    pub async fn poll_remote_changes(&self) -> Result<usize, String> {
        let arc_cache = match self.cache {
            Some(ref c) => Arc::clone(c),
            None => return Ok(0),
        };

        // 1. Get or initialize page token
        let current_token = {
            let cache_guard = arc_cache.lock().map_err(|e| e.to_string())?;
            cache_guard.load_page_token()?
        };

        let page_token = match current_token {
            Some(tok) => tok,
            None => {
                debug!("No saved page token; fetching initial start token from Drive.");
                let start_token = self
                    .client
                    .get_start_page_token()
                    .await
                    .map_err(|e| e.to_string())?;
                let cache_guard = arc_cache.lock().map_err(|e| e.to_string())?;
                cache_guard.save_page_token(&start_token)?;
                info!("Saved initial Changes API start token: {}", start_token);
                return Ok(0);
            }
        };

        // 2. Query changes list
        debug!("Polling Google Drive changes with token: {}", page_token);
        let resp = self
            .client
            .list_changes(&page_token)
            .await
            .map_err(|e| e.to_string())?;

        let change_count = resp.changes.len();

        // 3. Apply changes to cache
        {
            let cache_guard = arc_cache.lock().map_err(|e| e.to_string())?;
            for change in resp.changes {
                let file_id = match change.file_id {
                    Some(id) => id,
                    None => continue,
                };

                if change.removed {
                    debug!("Change event: file {} removed remotely", file_id);
                    let _ = cache_guard.delete_chunk(&file_id);
                    let _ = cache_guard.delete_file_record(&file_id);
                } else if let Some(drive_file) = change.file {
                    if drive_file.is_trashed() {
                        debug!("Change event: file {} moved to trash", file_id);
                        let _ = cache_guard.delete_chunk(&file_id);
                        let _ = cache_guard.delete_file_record(&file_id);
                    } else {
                        debug!(
                            "Change event: file {} updated remotely ({})",
                            file_id, drive_file.name
                        );
                        // Invalidate cached chunk since content or metadata changed remotely
                        let _ = cache_guard.delete_chunk(&file_id);
                        let size = drive_file.size_bytes();
                        let parent_id = drive_file.parents_list().first().cloned();

                        let rec = CachedFileRecord {
                            drive_id: drive_file.id,
                            parent_id,
                            name: drive_file.name,
                            mime_type: drive_file.mime_type,
                            size,
                            modified_time: drive_file.modified_time.unwrap_or_default(),
                            etag: drive_file.md5_checksum,
                            local_cache_path: None,
                            sync_status: FileSyncStatus::MetadataOnly,
                            last_accessed: CachedFileRecord::now_epoch(),
                        };
                        let _ = cache_guard.upsert_file_record(&rec);
                    }
                }
            }

            // 4. Update the stored token for subsequent polls
            if let Some(next_tok) = resp.new_start_page_token.or(resp.next_page_token) {
                cache_guard.save_page_token(&next_tok)?;
            }
        }

        if change_count > 0 {
            info!("Applied {} remote Google Drive changes", change_count);
        }

        Ok(change_count)
    }

    /// Flush all pending offline writes to Google Drive.
    /// Returns the number of successfully completed tasks.
    pub async fn flush_offline_queue(&self) -> Result<usize, String> {
        let tasks = self.queue.drain()?;
        if tasks.is_empty() {
            return Ok(0);
        }

        info!(
            "Flushing {} pending offline sync tasks to Google Drive",
            tasks.len()
        );
        let mut completed = 0;
        let mut remaining = Vec::new();

        for task in tasks {
            match self.execute_sync_task(&task).await {
                Ok(_) => {
                    completed += 1;
                }
                Err(e) => {
                    warn!("Failed to execute sync task: {e}; requeuing remaining tasks.");
                    remaining.push(task);
                    let tasks_left = self.queue.drain().unwrap_or_default();
                    remaining.extend(tasks_left);
                    let _ = self.queue.prepend(remaining);
                    return Ok(completed);
                }
            }
        }

        info!("Successfully synced {} tasks to Google Drive", completed);
        Ok(completed)
    }

    async fn execute_sync_task(&self, task: &SyncTask) -> Result<(), String> {
        match task {
            SyncTask::Upload {
                file_id,
                local_path,
                parent_id,
                name,
                mime_type,
            } => {
                let content = fs::read(local_path).map_err(|e| format!("Read error: {e}"))?;
                if let Some(id) = file_id {
                    self.client
                        .update_file_content(id, content, Some(mime_type.as_str()))
                        .await
                        .map_err(|e| e.to_string())?;
                } else {
                    self.client
                        .upload_file(name, parent_id.as_deref(), mime_type, content)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            SyncTask::Delete { file_id } => {
                self.client
                    .delete_file(file_id, false)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            SyncTask::Rename { file_id, new_name } => {
                self.client
                    .rename_file(file_id, new_name)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            SyncTask::Download {
                file_id,
                target_path,
            } => {
                let data = self
                    .client
                    .download_file(file_id)
                    .await
                    .map_err(|e| e.to_string())?;
                fs::write(target_path, data).map_err(|e| format!("Write error: {e}"))?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sync_task_serialization() {
        let task = SyncTask::Upload {
            file_id: None,
            local_path: "/tmp/sample.txt".to_string(),
            parent_id: Some("root".to_string()),
            name: "sample.txt".to_string(),
            mime_type: "text/plain".to_string(),
        };
        let serialized = serde_json::to_string(&task).expect("JSON serialization should succeed");
        let deserialized: SyncTask =
            serde_json::from_str(&serialized).expect("JSON deserialization should succeed");
        assert_eq!(task, deserialized);
    }

    #[test]
    fn test_offline_queue_lifecycle() {
        let dir = tempdir().unwrap();
        let queue_file = dir.path().join("offline_queue.json");
        let queue = OfflineQueue::new(queue_file.clone());

        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);

        let task1 = SyncTask::Delete {
            file_id: "file_del_1".to_string(),
        };
        let task2 = SyncTask::Rename {
            file_id: "file_ren_2".to_string(),
            new_name: "new.txt".to_string(),
        };

        queue.push(task1.clone()).expect("push task 1");
        queue.push(task2.clone()).expect("push task 2");

        assert_eq!(queue.len(), 2);
        assert!(!queue.is_empty());

        // Test persistence reload
        let reloaded = OfflineQueue::new(queue_file);
        assert_eq!(reloaded.len(), 2);
        let peeked = reloaded.peek_all().expect("peek");
        assert_eq!(peeked.len(), 2);
        assert_eq!(peeked[0], task1);
        assert_eq!(peeked[1], task2);

        // Test drain
        let drained = reloaded.drain().expect("drain");
        assert_eq!(drained.len(), 2);
        assert!(reloaded.is_empty());
    }

    #[test]
    fn test_offline_queue_prepend() {
        let dir = tempdir().unwrap();
        let queue_file = dir.path().join("offline_queue.json");
        let queue = OfflineQueue::new(queue_file);

        let task1 = SyncTask::Delete {
            file_id: "file_1".to_string(),
        };
        let task2 = SyncTask::Delete {
            file_id: "file_2".to_string(),
        };

        queue.push(task2.clone()).expect("push task 2");
        queue.prepend(vec![task1.clone()]).expect("prepend task 1");

        let tasks = queue.peek_all().expect("peek");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0], task1);
        assert_eq!(tasks[1], task2);
    }
}
