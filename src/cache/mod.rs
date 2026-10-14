//! Local metadata and file-content cache backed by SQLite.
//!
//! # Architecture
//!
//! ```text
//! Google Drive API
//!       │
//!       ▼
//! MetadataCache (SQLite)
//!   ├── file_metadata   — Drive file records (id, name, mime, size, mtime…)
//!   ├── directory_cache — Maps parent_id → list of child drive_ids + fetch timestamp
//!   └── page_tokens     — Persists Drive Change API page tokens across restarts
//!       │
//!       ▼
//! ChunkCache (filesystem)
//!   └── ~/.cache/google-drive-linux/chunks/<drive_id>
//!         Evicted when cache_size_mb is exceeded (LRU by last-access time)
//! ```
//!
//! Thread safety: `MetadataCache` wraps a `rusqlite::Connection` behind a `Mutex`
//! and is designed to be shared via `Arc<Mutex<MetadataCache>>`.

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Cache not initialized")]
    NotInitialized,
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Sync / download state for a cached file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FileSyncStatus {
    /// Only metadata is stored — no local content file.
    #[default]
    MetadataOnly,
    /// Content download is in progress.
    Downloading,
    /// Full content is cached locally.
    Cached,
    /// Local file has been modified and the upload is pending.
    ModifiedLocally,
    /// Upload is in progress.
    Uploading,
    /// A write conflict was detected.
    Conflict,
}

impl FileSyncStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::MetadataOnly => "metadata_only",
            Self::Downloading => "downloading",
            Self::Cached => "cached",
            Self::ModifiedLocally => "modified_locally",
            Self::Uploading => "uploading",
            Self::Conflict => "conflict",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "downloading" => Self::Downloading,
            "cached" => Self::Cached,
            "modified_locally" => Self::ModifiedLocally,
            "uploading" => Self::Uploading,
            "conflict" => Self::Conflict,
            _ => Self::MetadataOnly,
        }
    }
}

/// Full metadata record for one Drive item, mirroring the Google Drive API fields
/// we care about, plus local cache bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedFileRecord {
    /// Google Drive file ID (stable across renames).
    pub drive_id: String,
    /// Parent Drive folder ID (`None` for root).
    pub parent_id: Option<String>,
    /// Display name.
    pub name: String,
    /// MIME type string.
    pub mime_type: String,
    /// File size in bytes (0 for folders / Google-native docs).
    pub size: u64,
    /// RFC 3339 modification timestamp from Drive API.
    pub modified_time: String,
    /// Drive ETag / version string for change detection.
    pub etag: Option<String>,
    /// Absolute path to the locally cached content file, if downloaded.
    pub local_cache_path: Option<String>,
    /// Current synchronisation state.
    pub sync_status: FileSyncStatus,
    /// Unix epoch seconds of when this record was last accessed for eviction.
    pub last_accessed: u64,
}

impl CachedFileRecord {
    pub fn now_epoch() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn touch(&mut self) {
        self.last_accessed = Self::now_epoch();
    }
}

/// Trait defining the metadata cache contract. Both the real SQLite backend and
/// any test fakes implement this so the FUSE layer stays testable without a DB.
pub trait CacheStorage: Send {
    fn get_file_record(&self, drive_id: &str) -> Result<Option<CachedFileRecord>, String>;
    fn upsert_file_record(&self, record: &CachedFileRecord) -> Result<(), String>;
    fn delete_file_record(&self, drive_id: &str) -> Result<(), String>;
    fn get_children(&self, parent_id: Option<&str>) -> Result<Vec<CachedFileRecord>, String>;
    /// Mark a directory listing as fetched at the current timestamp.
    fn mark_directory_fetched(&self, parent_id: &str) -> Result<(), String>;
    /// Return whether the directory cache is still fresh (within `ttl`).
    fn is_directory_fresh(&self, parent_id: &str, ttl: Duration) -> Result<bool, String>;
    /// Persist the Drive Changes API page token.
    fn save_page_token(&self, token: &str) -> Result<(), String>;
    /// Retrieve the saved page token, if any.
    fn load_page_token(&self) -> Result<Option<String>, String>;
}

// ---------------------------------------------------------------------------
// SQLite backend
// ---------------------------------------------------------------------------

/// SQLite-backed metadata cache.  The database is created at first use.
pub struct MetadataCache {
    conn: Mutex<Connection>,
    /// Root directory for locally cached file chunks.
    pub chunk_dir: PathBuf,
    /// Maximum total size of cached chunks in bytes.
    pub max_cache_bytes: u64,
    /// Track current chunk cache bytes to avoid recursive disk walks on every chunk write.
    pub current_cache_bytes: std::sync::atomic::AtomicU64,
}

impl MetadataCache {
    /// Open (or create) the SQLite database at `db_path`, storing chunks in `chunk_dir`.
    pub fn open(db_path: &Path, chunk_dir: &Path, max_cache_mb: u64) -> Result<Self, CacheError> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(chunk_dir)?;

        let conn = Connection::open(db_path)?;
        // Performance pragmas — safe for a cache that can be rebuilt from Drive.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA cache_size   = -8000;", // 8 MB page cache
        )?;

        let initial_bytes = dir_size_bytes(chunk_dir);
        let cache = Self {
            conn: Mutex::new(conn),
            chunk_dir: chunk_dir.to_path_buf(),
            max_cache_bytes: max_cache_mb * 1024 * 1024,
            current_cache_bytes: std::sync::atomic::AtomicU64::new(initial_bytes),
        };
        cache.init_schema()?;
        info!("MetadataCache opened at {:?}", db_path);
        Ok(cache)
    }

    /// Create all required tables if they do not already exist.
    fn init_schema(&self) -> Result<(), CacheError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_metadata (
                drive_id          TEXT PRIMARY KEY NOT NULL,
                parent_id         TEXT,
                name              TEXT NOT NULL,
                mime_type         TEXT NOT NULL,
                size              INTEGER NOT NULL DEFAULT 0,
                modified_time     TEXT NOT NULL DEFAULT '',
                etag              TEXT,
                local_cache_path  TEXT,
                sync_status       TEXT NOT NULL DEFAULT 'metadata_only',
                last_accessed     INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_file_metadata_parent
                ON file_metadata (parent_id);

            CREATE TABLE IF NOT EXISTS directory_cache (
                parent_id         TEXT PRIMARY KEY NOT NULL,
                fetched_at        INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS page_tokens (
                key    TEXT PRIMARY KEY NOT NULL,
                value  TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Chunk cache helpers
    // ------------------------------------------------------------------

    /// Return the path where a file's content should be stored locally.
    pub fn chunk_path(&self, drive_id: &str) -> PathBuf {
        // Use a two-level subdirectory to avoid huge flat dirs.
        let prefix = &drive_id[..drive_id.len().min(2)];
        self.chunk_dir.join(prefix).join(drive_id)
    }

    /// Save downloaded content bytes to the local chunk store.
    pub fn save_chunk(&self, drive_id: &str, data: &[u8]) -> Result<PathBuf, CacheError> {
        let path = self.chunk_path(drive_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let old_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        fs::write(&path, data)?;
        let new_size = data.len() as u64;
        if new_size > old_size {
            self.current_cache_bytes
                .fetch_add(new_size - old_size, std::sync::atomic::Ordering::Relaxed);
        } else if old_size > new_size {
            self.current_cache_bytes
                .fetch_sub(old_size - new_size, std::sync::atomic::Ordering::Relaxed);
        }
        debug!(
            "Saved chunk for drive_id={} ({} bytes)",
            drive_id,
            data.len()
        );
        self.maybe_evict()?;
        Ok(path)
    }

    /// Read a locally cached chunk.  Returns `None` if not present.
    pub fn load_chunk(&self, drive_id: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let path = self.chunk_path(drive_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        Ok(Some(bytes))
    }

    /// Delete a locally cached chunk if present.
    pub fn delete_chunk(&self, drive_id: &str) -> Result<(), CacheError> {
        let path = self.chunk_path(drive_id);
        if path.exists() {
            if let Ok(meta) = fs::metadata(&path) {
                self.current_cache_bytes
                    .fetch_sub(meta.len(), std::sync::atomic::Ordering::Relaxed);
            }
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Return total bytes used by the chunk cache directory (fast atomic lookup).
    pub fn chunk_cache_bytes(&self) -> u64 {
        self.current_cache_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Evict the least-recently-accessed cached chunks until usage drops below
    /// `max_cache_bytes`.  Eviction order is determined by `last_accessed` in
    /// the metadata database.
    pub fn maybe_evict(&self) -> Result<(), CacheError> {
        let used = self.chunk_cache_bytes();
        if used <= self.max_cache_bytes {
            return Ok(());
        }

        info!(
            "Chunk cache at {} MB / {} MB — running eviction.",
            used / (1024 * 1024),
            self.max_cache_bytes / (1024 * 1024)
        );

        // Collect candidates sorted by last_accessed ascending (evict oldest first)
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT drive_id FROM file_metadata
             WHERE sync_status = 'cached' AND local_cache_path IS NOT NULL
             ORDER BY last_accessed ASC",
        )?;
        let candidates: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(SqlResult::ok)
            .collect();
        drop(stmt);

        let mut remaining = used;
        for drive_id in candidates {
            if remaining <= self.max_cache_bytes {
                break;
            }
            let path = self.chunk_path(&drive_id);
            if let Ok(meta) = fs::metadata(&path) {
                let file_size = meta.len();
                if let Err(e) = fs::remove_file(&path) {
                    warn!("Eviction: failed to remove {:?}: {}", path, e);
                    continue;
                }
                remaining = remaining.saturating_sub(file_size);
                self.current_cache_bytes
                    .fetch_sub(file_size, std::sync::atomic::Ordering::Relaxed);
                // Update sync_status back to MetadataOnly
                conn.execute(
                    "UPDATE file_metadata
                     SET sync_status = 'metadata_only', local_cache_path = NULL
                     WHERE drive_id = ?1",
                    params![drive_id],
                )?;
                debug!("Evicted chunk: {} ({} bytes freed)", drive_id, file_size);
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CacheStorage implementation for MetadataCache
// ---------------------------------------------------------------------------

impl CacheStorage for MetadataCache {
    fn get_file_record(&self, drive_id: &str) -> Result<Option<CachedFileRecord>, String> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row(
                "SELECT drive_id, parent_id, name, mime_type, size, modified_time,
                        etag, local_cache_path, sync_status, last_accessed
                 FROM file_metadata WHERE drive_id = ?1",
                params![drive_id],
                row_to_record,
            )
            .optional()
            .map_err(|e| e.to_string())?;

        // Update last_accessed in background (best-effort, ignore errors)
        if result.is_some() {
            let now = CachedFileRecord::now_epoch() as i64;
            let _ = conn.execute(
                "UPDATE file_metadata SET last_accessed = ?1 WHERE drive_id = ?2",
                params![now, drive_id],
            );
        }

        Ok(result)
    }

    fn upsert_file_record(&self, record: &CachedFileRecord) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO file_metadata
                (drive_id, parent_id, name, mime_type, size, modified_time,
                 etag, local_cache_path, sync_status, last_accessed)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(drive_id) DO UPDATE SET
                parent_id        = excluded.parent_id,
                name             = excluded.name,
                mime_type        = excluded.mime_type,
                size             = excluded.size,
                modified_time    = excluded.modified_time,
                etag             = excluded.etag,
                local_cache_path = excluded.local_cache_path,
                sync_status      = excluded.sync_status,
                last_accessed    = excluded.last_accessed",
            params![
                record.drive_id,
                record.parent_id,
                record.name,
                record.mime_type,
                record.size as i64,
                record.modified_time,
                record.etag,
                record.local_cache_path,
                record.sync_status.as_str(),
                record.last_accessed as i64,
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn delete_file_record(&self, drive_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM file_metadata WHERE drive_id = ?1",
            params![drive_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn get_children(&self, parent_id: Option<&str>) -> Result<Vec<CachedFileRecord>, String> {
        let conn = self.conn.lock().unwrap();
        let (query, parent_val) = match parent_id {
            Some(id) => (
                "SELECT drive_id, parent_id, name, mime_type, size, modified_time,
                         etag, local_cache_path, sync_status, last_accessed
                  FROM file_metadata WHERE parent_id = ?1
                  ORDER BY mime_type DESC, name ASC",
                id.to_string(),
            ),
            None => (
                "SELECT drive_id, parent_id, name, mime_type, size, modified_time,
                         etag, local_cache_path, sync_status, last_accessed
                  FROM file_metadata WHERE parent_id IS NULL
                  ORDER BY mime_type DESC, name ASC",
                String::new(),
            ),
        };

        let mut stmt = conn.prepare(query).map_err(|e| e.to_string())?;

        let records = if parent_id.is_some() {
            stmt.query_map(params![parent_val], row_to_record)
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], row_to_record)
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect()
        };

        Ok(records)
    }

    fn mark_directory_fetched(&self, parent_id: &str) -> Result<(), String> {
        let now = CachedFileRecord::now_epoch() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO directory_cache (parent_id, fetched_at)
             VALUES (?1, ?2)
             ON CONFLICT(parent_id) DO UPDATE SET fetched_at = excluded.fetched_at",
            params![parent_id, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn is_directory_fresh(&self, parent_id: &str, ttl: Duration) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let fetched_at: Option<i64> = conn
            .query_row(
                "SELECT fetched_at FROM directory_cache WHERE parent_id = ?1",
                params![parent_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        match fetched_at {
            None => Ok(false),
            Some(ts) => {
                let age_secs = CachedFileRecord::now_epoch().saturating_sub(ts as u64);
                Ok(age_secs < ttl.as_secs())
            }
        }
    }

    fn save_page_token(&self, token: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO page_tokens (key, value) VALUES ('changes', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![token],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn load_page_token(&self) -> Result<Option<String>, String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM page_tokens WHERE key = 'changes'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Row-mapping helper
// ---------------------------------------------------------------------------

fn row_to_record(row: &rusqlite::Row<'_>) -> SqlResult<CachedFileRecord> {
    let size: i64 = row.get(4)?;
    let last_accessed: i64 = row.get(9)?;
    let status_str: String = row.get(8)?;
    Ok(CachedFileRecord {
        drive_id: row.get(0)?,
        parent_id: row.get(1)?,
        name: row.get(2)?,
        mime_type: row.get(3)?,
        size: size.max(0) as u64,
        modified_time: row.get(5)?,
        etag: row.get(6)?,
        local_cache_path: row.get(7)?,
        sync_status: FileSyncStatus::from_str(&status_str),
        last_accessed: last_accessed.max(0) as u64,
    })
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Recursively sum file sizes in `dir`.
fn dir_size_bytes(dir: &Path) -> u64 {
    if !dir.exists() {
        return 0;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .map(|e| {
            let meta_res = e.metadata();
            match meta_res {
                Ok(m) if m.is_file() => m.len(),
                Ok(m) if m.is_dir() => dir_size_bytes(&e.path()),
                _ => 0,
            }
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Convenience: build the cache from config paths
// ---------------------------------------------------------------------------

/// Open the default metadata cache in `~/.cache/google-drive-linux/`.
pub fn open_default_cache(max_cache_mb: u64) -> Result<MetadataCache, CacheError> {
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("google-drive-linux");

    let db_path = cache_dir.join("metadata.db");
    let chunk_dir = cache_dir.join("chunks");
    MetadataCache::open(&db_path, &chunk_dir, max_cache_mb)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_cache(dir: &Path) -> MetadataCache {
        let db = dir.join("test.db");
        let chunks = dir.join("chunks");
        MetadataCache::open(&db, &chunks, 100).expect("open cache")
    }

    fn sample_record(id: &str, parent: Option<&str>) -> CachedFileRecord {
        CachedFileRecord {
            drive_id: id.to_string(),
            parent_id: parent.map(str::to_string),
            name: format!("{id}.txt"),
            mime_type: "text/plain".to_string(),
            size: 1024,
            modified_time: "2026-09-04T00:00:00Z".to_string(),
            etag: Some(format!("etag_{id}")),
            local_cache_path: None,
            sync_status: FileSyncStatus::MetadataOnly,
            last_accessed: CachedFileRecord::now_epoch(),
        }
    }

    #[test]
    fn test_schema_created() {
        let dir = tempdir().unwrap();
        let _cache = make_cache(dir.path());
        assert!(dir.path().join("test.db").exists());
        assert!(dir.path().join("chunks").exists());
    }

    #[test]
    fn test_upsert_and_get() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        let rec = sample_record("file_abc", Some("root"));
        cache.upsert_file_record(&rec).expect("upsert");

        let loaded = cache
            .get_file_record("file_abc")
            .expect("get")
            .expect("exists");
        assert_eq!(loaded.drive_id, "file_abc");
        assert_eq!(loaded.name, "file_abc.txt");
        assert_eq!(loaded.size, 1024);
        assert_eq!(loaded.sync_status, FileSyncStatus::MetadataOnly);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        let rec = sample_record("file_xyz", Some("root"));
        cache.upsert_file_record(&rec).expect("insert");

        let mut updated = rec.clone();
        updated.size = 9999;
        updated.sync_status = FileSyncStatus::Cached;
        cache.upsert_file_record(&updated).expect("update");

        let loaded = cache
            .get_file_record("file_xyz")
            .expect("get")
            .expect("must exist");
        assert_eq!(loaded.size, 9999);
        assert_eq!(loaded.sync_status, FileSyncStatus::Cached);
    }

    #[test]
    fn test_delete_file_record() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        cache
            .upsert_file_record(&sample_record("del_me", Some("root")))
            .expect("upsert");
        cache.delete_file_record("del_me").expect("delete");

        assert!(cache.get_file_record("del_me").expect("get").is_none());
    }

    #[test]
    fn test_get_children() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        cache
            .upsert_file_record(&sample_record("child_a", Some("folder_1")))
            .expect("upsert a");
        cache
            .upsert_file_record(&sample_record("child_b", Some("folder_1")))
            .expect("upsert b");
        cache
            .upsert_file_record(&sample_record("child_c", Some("folder_2")))
            .expect("upsert c");

        let children = cache.get_children(Some("folder_1")).expect("children");
        assert_eq!(children.len(), 2);
        let ids: Vec<&str> = children.iter().map(|r| r.drive_id.as_str()).collect();
        assert!(ids.contains(&"child_a"));
        assert!(ids.contains(&"child_b"));
        assert!(!ids.contains(&"child_c"));
    }

    #[test]
    fn test_directory_freshness() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        // Not fetched yet
        assert!(!cache
            .is_directory_fresh("root", Duration::from_secs(300))
            .expect("fresh check"));

        cache.mark_directory_fetched("root").expect("mark");

        // Should be fresh now
        assert!(cache
            .is_directory_fresh("root", Duration::from_secs(300))
            .expect("fresh check"));

        // With 0-second TTL it's already stale
        assert!(!cache
            .is_directory_fresh("root", Duration::from_secs(0))
            .expect("fresh check"));
    }

    #[test]
    fn test_page_token_persistence() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        assert!(cache.load_page_token().expect("load").is_none());

        cache.save_page_token("token_abc123").expect("save token");
        let loaded = cache.load_page_token().expect("load").expect("must exist");
        assert_eq!(loaded, "token_abc123");

        // Overwrite
        cache.save_page_token("token_updated").expect("update");
        assert_eq!(
            cache.load_page_token().expect("load2").unwrap(),
            "token_updated"
        );
    }

    #[test]
    fn test_chunk_save_load_delete() {
        let dir = tempdir().unwrap();
        let cache = make_cache(dir.path());

        let data = b"hello cached world";
        let path = cache.save_chunk("drive_file_1", data).expect("save");
        assert!(path.exists());

        let loaded = cache
            .load_chunk("drive_file_1")
            .expect("load")
            .expect("some");
        assert_eq!(loaded, data);

        cache.delete_chunk("drive_file_1").expect("delete");
        assert!(cache.load_chunk("drive_file_1").expect("load2").is_none());
    }

    #[test]
    fn test_eviction_respects_limit() {
        let dir = tempdir().unwrap();
        // Tiny 1-byte limit to force eviction immediately
        let db = dir.path().join("test.db");
        let chunks = dir.path().join("chunks");
        let cache = MetadataCache::open(&db, &chunks, 0).expect("open");

        // Upsert a record that claims to be cached
        let mut rec = sample_record("evict_me", Some("root"));
        rec.sync_status = FileSyncStatus::Cached;
        rec.local_cache_path = Some(cache.chunk_path("evict_me").to_string_lossy().to_string());
        cache.upsert_file_record(&rec).expect("upsert");

        // Save a chunk (which triggers eviction)
        cache
            .save_chunk("evict_me", b"some data that must be evicted")
            .expect("save");

        // After eviction, chunk should be gone
        assert!(cache.load_chunk("evict_me").expect("load").is_none());
    }

    #[test]
    fn test_cache_record_defaults() {
        let record = sample_record("id_123", Some("root"));
        assert_eq!(record.sync_status, FileSyncStatus::MetadataOnly);
        assert_eq!(record.size, 1024);
    }

    #[test]
    fn test_sync_status_roundtrip() {
        let statuses = [
            FileSyncStatus::MetadataOnly,
            FileSyncStatus::Downloading,
            FileSyncStatus::Cached,
            FileSyncStatus::ModifiedLocally,
            FileSyncStatus::Uploading,
            FileSyncStatus::Conflict,
        ];
        for s in statuses {
            assert_eq!(FileSyncStatus::from_str(s.as_str()), s);
        }
    }
}
