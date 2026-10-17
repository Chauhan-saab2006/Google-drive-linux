use crate::cache::{CacheStorage, CachedFileRecord, FileSyncStatus, MetadataCache};
use crate::drive::{DriveFile, GoogleDriveClient};

pub mod stream;
pub use stream::*;

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tracing::{debug, error, info, warn};

pub const FUSER_ROOT_ID: u64 = 1;
const ATTR_TTL: Duration = Duration::from_secs(60);

/// MIME types for Google-native document formats.
const MIME_GOOGLE_DOC: &str = "application/vnd.google-apps.document";
const MIME_GOOGLE_SHEET: &str = "application/vnd.google-apps.spreadsheet";
const MIME_GOOGLE_SLIDES: &str = "application/vnd.google-apps.presentation";
const MIME_GOOGLE_FORM: &str = "application/vnd.google-apps.form";
const MIME_GOOGLE_DRAWING: &str = "application/vnd.google-apps.drawing";
const MIME_GOOGLE_SCRIPT: &str = "application/vnd.google-apps.script";

/// Returns true if `mime_type` is a Google Workspace native document (no binary download).
pub fn is_google_native_doc(mime_type: &str) -> bool {
    matches!(
        mime_type,
        MIME_GOOGLE_DOC
            | MIME_GOOGLE_SHEET
            | MIME_GOOGLE_SLIDES
            | MIME_GOOGLE_FORM
            | MIME_GOOGLE_DRAWING
            | MIME_GOOGLE_SCRIPT
    )
}

/// Generate a JSON stub for a Google Workspace document, compatible with `xdg-open`.
/// Returns (stub_bytes, extension_hint) so callers can serve the right content.
pub fn google_doc_stub(drive_id: &str, name: &str, mime_type: &str, web_url: &str) -> Vec<u8> {
    let type_str = match mime_type {
        MIME_GOOGLE_DOC => "document",
        MIME_GOOGLE_SHEET => "spreadsheet",
        MIME_GOOGLE_SLIDES => "presentation",
        MIME_GOOGLE_FORM => "form",
        MIME_GOOGLE_DRAWING => "drawing",
        MIME_GOOGLE_SCRIPT => "script",
        _ => "file",
    };
    format!(
        "{{
  \"url\": \"{url}\",
  \"doc_id\": \"{id}\",
  \"email\": \"\",
  \"resource_id\": \"{type_str}:{id}\",
  \"title\": \"{name}\"
}}",
        url = web_url,
        id = drive_id,
        type_str = type_str,
        name = name,
    )
    .into_bytes()
}

#[derive(Error, Debug)]
pub enum FsError {
    #[error("IO error during filesystem operation: {0}")]
    Io(#[from] std::io::Error),
    #[error("Mount point does not exist or is not a directory: {0}")]
    InvalidMountPoint(PathBuf),
    #[error("FUSE mount failed: {0}")]
    MountFailed(String),
    #[error("Unmount failed: {0}")]
    UnmountFailed(String),
}

/// Inode entry mapping between POSIX and Google Drive objects.
#[derive(Debug, Clone)]
pub struct InodeEntry {
    pub ino: u64,
    pub drive_id: String,
    pub parent_ino: u64,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_time: SystemTime,
    pub mime_type: String,
}

impl InodeEntry {
    pub fn to_file_attr(&self, uid: u32, gid: u32) -> FileAttr {
        let kind = if self.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };

        let perm = if self.is_dir { 0o755 } else { 0o644 };
        let nlink = if self.is_dir { 2 } else { 1 };

        FileAttr {
            ino: INodeNo(self.ino),
            size: self.size,
            blocks: self.size.div_ceil(512),
            atime: self.modified_time,
            mtime: self.modified_time,
            ctime: self.modified_time,
            crtime: self.modified_time,
            kind,
            perm,
            nlink,
            uid,
            gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }
}

/// Thread-safe in-memory cache mapping POSIX inodes to Google Drive metadata.
#[derive(Debug)]
pub struct InodeTable {
    ino_to_entry: HashMap<u64, InodeEntry>,
    drive_id_to_ino: HashMap<String, u64>,
    parent_name_to_ino: HashMap<(u64, String), u64>,
    children_of: HashMap<u64, Vec<u64>>,
    next_ino: AtomicU64,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    pub fn new() -> Self {
        let mut table = Self {
            ino_to_entry: HashMap::new(),
            drive_id_to_ino: HashMap::new(),
            parent_name_to_ino: HashMap::new(),
            children_of: HashMap::new(),
            next_ino: AtomicU64::new(5),
        };

        // Initialize root directory (ino = 1) mapped to Google Drive "root"
        let root = InodeEntry {
            ino: FUSER_ROOT_ID,
            drive_id: "root".to_string(),
            parent_ino: FUSER_ROOT_ID,
            name: String::new(),
            is_dir: true,
            size: 4096,
            modified_time: SystemTime::now(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
        };

        table.ino_to_entry.insert(FUSER_ROOT_ID, root);
        table
            .drive_id_to_ino
            .insert("root".to_string(), FUSER_ROOT_ID);

        // Virtual .trackerignore, .nomedia & .noindex to prevent desktop indexing engines
        // (GNOME Tracker, KDE Baloo, media indexers) from storming the remote FUSE mount.
        let tracker_entry = InodeEntry {
            ino: 2,
            drive_id: "virtual_trackerignore".to_string(),
            parent_ino: FUSER_ROOT_ID,
            name: ".trackerignore".to_string(),
            is_dir: false,
            size: 0,
            modified_time: SystemTime::now(),
            mime_type: "text/plain".to_string(),
        };
        table.ino_to_entry.insert(2, tracker_entry);
        table
            .drive_id_to_ino
            .insert("virtual_trackerignore".to_string(), 2);
        table
            .parent_name_to_ino
            .insert((FUSER_ROOT_ID, ".trackerignore".to_string()), 2);

        let nomedia_entry = InodeEntry {
            ino: 3,
            drive_id: "virtual_nomedia".to_string(),
            parent_ino: FUSER_ROOT_ID,
            name: ".nomedia".to_string(),
            is_dir: false,
            size: 0,
            modified_time: SystemTime::now(),
            mime_type: "text/plain".to_string(),
        };
        table.ino_to_entry.insert(3, nomedia_entry);
        table
            .drive_id_to_ino
            .insert("virtual_nomedia".to_string(), 3);
        table
            .parent_name_to_ino
            .insert((FUSER_ROOT_ID, ".nomedia".to_string()), 3);

        let noindex_entry = InodeEntry {
            ino: 4,
            drive_id: "virtual_noindex".to_string(),
            parent_ino: FUSER_ROOT_ID,
            name: ".noindex".to_string(),
            is_dir: false,
            size: 0,
            modified_time: SystemTime::now(),
            mime_type: "text/plain".to_string(),
        };
        table.ino_to_entry.insert(4, noindex_entry);
        table
            .drive_id_to_ino
            .insert("virtual_noindex".to_string(), 4);
        table
            .parent_name_to_ino
            .insert((FUSER_ROOT_ID, ".noindex".to_string()), 4);

        table
    }

    pub fn has_children_cached(&self, parent_ino: u64) -> bool {
        self.children_of.contains_key(&parent_ino)
    }

    pub fn get_by_ino(&self, ino: u64) -> Option<InodeEntry> {
        self.ino_to_entry.get(&ino).cloned()
    }

    pub fn get_by_drive_id(&self, drive_id: &str) -> Option<u64> {
        self.drive_id_to_ino.get(drive_id).copied()
    }

    pub fn get_child_ino(&self, parent_ino: u64, name: &str) -> Option<u64> {
        self.parent_name_to_ino
            .get(&(parent_ino, name.to_string()))
            .copied()
    }

    pub fn get_children(&self, parent_ino: u64) -> Option<Vec<InodeEntry>> {
        let child_inos = self.children_of.get(&parent_ino)?;
        let mut entries = Vec::with_capacity(child_inos.len());
        for &ino in child_inos {
            if let Some(entry) = self.ino_to_entry.get(&ino) {
                entries.push(entry.clone());
            }
        }
        Some(entries)
    }

    pub fn insert_or_update(&mut self, parent_ino: u64, file: &DriveFile) -> u64 {
        let mtime = file
            .modified_time
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| SystemTime::UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64))
            .unwrap_or(SystemTime::now());

        let ino = if let Some(&existing_ino) = self.drive_id_to_ino.get(&file.id) {
            existing_ino
        } else {
            self.next_ino.fetch_add(1, Ordering::SeqCst)
        };

        let entry = InodeEntry {
            ino,
            drive_id: file.id.clone(),
            parent_ino,
            name: file.name.clone(),
            is_dir: file.is_folder(),
            size: if file.is_folder() {
                4096
            } else if is_google_native_doc(&file.mime_type) {
                // Google-native docs have no binary size from the API; use a
                // synthetic stub size so FUSE read() isn't skipped immediately.
                512
            } else {
                file.size_bytes()
            },
            modified_time: mtime,
            mime_type: file.mime_type.clone(),
        };

        self.ino_to_entry.insert(ino, entry);
        self.drive_id_to_ino.insert(file.id.clone(), ino);
        self.parent_name_to_ino
            .insert((parent_ino, file.name.clone()), ino);

        ino
    }

    /// Insert a new locally-created entry not yet confirmed from Drive API.
    pub fn insert_new(
        &mut self,
        parent_ino: u64,
        drive_id: &str,
        name: &str,
        is_dir: bool,
        mime_type: &str,
    ) -> u64 {
        let ino = if let Some(&existing_ino) = self.drive_id_to_ino.get(drive_id) {
            existing_ino
        } else {
            self.next_ino.fetch_add(1, Ordering::SeqCst)
        };

        let entry = InodeEntry {
            ino,
            drive_id: drive_id.to_string(),
            parent_ino,
            name: name.to_string(),
            is_dir,
            size: if is_dir { 4096 } else { 0 },
            modified_time: SystemTime::now(),
            mime_type: mime_type.to_string(),
        };

        self.ino_to_entry.insert(ino, entry);
        self.drive_id_to_ino.insert(drive_id.to_string(), ino);
        self.parent_name_to_ino
            .insert((parent_ino, name.to_string()), ino);

        // Append to parent's children list (invalidate full children cache so next readdir re-fetches)
        self.children_of.remove(&parent_ino);

        ino
    }

    /// Update the size field of an existing inode.
    pub fn update_size(&mut self, ino: u64, new_size: u64) {
        if let Some(entry) = self.ino_to_entry.get_mut(&ino) {
            entry.size = new_size;
            entry.modified_time = SystemTime::now();
        }
    }

    /// Remove an inode from all indexes and from parent's children list.
    pub fn remove_by_ino(&mut self, ino: u64) {
        if let Some(entry) = self.ino_to_entry.remove(&ino) {
            self.drive_id_to_ino.remove(&entry.drive_id);
            self.parent_name_to_ino
                .remove(&(entry.parent_ino, entry.name));
            // Remove from parent's children vector if cached
            if let Some(siblings) = self.children_of.get_mut(&entry.parent_ino) {
                siblings.retain(|&i| i != ino);
            }
        }
        // Also invalidate any children list of this inode (for directories)
        self.children_of.remove(&ino);
    }

    /// Rename/re-parent an inode. Updates all index entries atomically.
    pub fn update_parent_and_name(&mut self, ino: u64, new_parent_ino: u64, new_name: &str) {
        if let Some(entry) = self.ino_to_entry.get_mut(&ino) {
            // Remove old parent-name index
            let old_key = (entry.parent_ino, entry.name.clone());
            self.parent_name_to_ino.remove(&old_key);

            // Invalidate old and new parent children caches so readdir re-fetches
            self.children_of.remove(&entry.parent_ino);
            self.children_of.remove(&new_parent_ino);

            // Apply update
            entry.parent_ino = new_parent_ino;
            entry.name = new_name.to_string();
            entry.modified_time = SystemTime::now();

            // Insert new parent-name index
            self.parent_name_to_ino
                .insert((new_parent_ino, new_name.to_string()), ino);
        }
    }

    pub fn set_children(&mut self, parent_ino: u64, child_inos: Vec<u64>) {
        self.children_of.insert(parent_ino, child_inos);
    }
}

// ---------------------------------------------------------------------------
// Write-buffer: tracks open file handles that have staged local content.
// ---------------------------------------------------------------------------

/// Tracks a single open write file handle against a staging file on disk.
#[derive(Debug)]
pub struct StageFile {
    pub ino: u64,
    pub drive_id: String,
    pub staging_path: PathBuf,
    /// True if this is a brand-new file created via FUSE `create()`.
    pub is_new: bool,
}

// ---------------------------------------------------------------------------
// GoogleDriveFS — FUSE filesystem implementation.
// ---------------------------------------------------------------------------

/// FUSE implementation bridging kernel filesystem requests to the Google Drive API.
pub struct GoogleDriveFS {
    client: Arc<GoogleDriveClient>,
    inodes: Arc<RwLock<InodeTable>>,
    rt_handle: tokio::runtime::Handle,
    uid: u32,
    gid: u32,
    staging_dir: PathBuf,
    write_handles: Arc<RwLock<HashMap<u64, StageFile>>>,
    next_fh: AtomicU64,
    /// Shared metadata + chunk cache (optional — `None` means caching disabled).
    cache: Option<Arc<Mutex<MetadataCache>>>,
    /// High-performance sliding window streaming manager for movies and large media.
    stream_manager: Arc<StreamBufferManager>,
}

impl GoogleDriveFS {
    pub fn new(
        client: Arc<GoogleDriveClient>,
        rt_handle: tokio::runtime::Handle,
        staging_dir: PathBuf,
        cache: Option<Arc<Mutex<MetadataCache>>>,
    ) -> Self {
        #[cfg(unix)]
        let (uid, gid) = (unsafe { libc::getuid() }, unsafe { libc::getgid() });
        #[cfg(not(unix))]
        let (uid, gid) = (1000, 1000);

        if let Err(e) = fs::create_dir_all(&staging_dir) {
            warn!(
                "Failed to create staging directory {:?}: {}",
                staging_dir, e
            );
        }

        Self {
            client,
            inodes: Arc::new(RwLock::new(InodeTable::new())),
            rt_handle,
            uid,
            gid,
            staging_dir,
            write_handles: Arc::new(RwLock::new(HashMap::new())),
            next_fh: AtomicU64::new(1),
            cache,
            stream_manager: Arc::new(StreamBufferManager::new()),
        }
    }

    pub fn inode_table(&self) -> Arc<RwLock<InodeTable>> {
        self.inodes.clone()
    }

    fn load_cached_children(
        &self,
        parent_ino: u64,
        parent_drive_id: &str,
    ) -> Option<Vec<InodeEntry>> {
        let arc_cache = self.cache.as_ref()?;
        let cache = arc_cache.lock().ok()?;
        let cached_files = cache.get_children(Some(parent_drive_id)).ok()?;
        if cached_files.is_empty() {
            return None;
        }

        let mut table = self.inodes.write().unwrap();
        let mut child_inos = Vec::with_capacity(cached_files.len() + 2);

        if parent_ino == FUSER_ROOT_ID {
            if let Some(ino) = table.get_child_ino(FUSER_ROOT_ID, ".trackerignore") {
                child_inos.push(ino);
            }
            if let Some(ino) = table.get_child_ino(FUSER_ROOT_ID, ".nomedia") {
                child_inos.push(ino);
            }
            if let Some(ino) = table.get_child_ino(FUSER_ROOT_ID, ".noindex") {
                child_inos.push(ino);
            }
        }

        for rec in &cached_files {
            let drive_file = DriveFile {
                id: rec.drive_id.clone(),
                name: rec.name.clone(),
                mime_type: rec.mime_type.clone(),
                size: Some(rec.size.to_string()),
                modified_time: Some(rec.modified_time.clone()),
                created_time: None,
                parents: rec.parent_id.clone().map(|p| vec![p]),
                trashed: Some(false),
                web_view_link: None,
                md5_checksum: rec.etag.clone(),
                version: None,
            };
            let ino = table.insert_or_update(parent_ino, &drive_file);
            child_inos.push(ino);
        }

        table.set_children(parent_ino, child_inos);
        table.get_children(parent_ino)
    }

    fn fetch_children_from_drive(&self, parent_ino: u64, parent_drive_id: &str) -> Vec<InodeEntry> {
        // Step 1: Check local SQLite cache first (fresh within 120s TTL)
        if let Some(ref arc_cache) = self.cache {
            if let Ok(cache) = arc_cache.lock() {
                if cache
                    .is_directory_fresh(parent_drive_id, Duration::from_secs(120))
                    .unwrap_or(false)
                {
                    drop(cache);
                    if let Some(entries) = self.load_cached_children(parent_ino, parent_drive_id) {
                        debug!(
                            "SQLite directory cache HIT for {}: loaded {} files",
                            parent_drive_id,
                            entries.len()
                        );
                        return entries;
                    }
                }
            }
        }

        // Step 2: SQLite cache miss or stale; query remote Google Drive API
        let client = self.client.clone();
        let drive_id = parent_drive_id.to_string();

        let result = self
            .rt_handle
            .block_on(async move { client.list_all_children(&drive_id).await });

        match result {
            Ok(files) => {
                // Write-through: upsert every fetched file into the metadata cache.
                if let Some(ref arc_cache) = self.cache {
                    if let Ok(cache) = arc_cache.lock() {
                        for file in &files {
                            let rec = CachedFileRecord {
                                drive_id: file.id.clone(),
                                parent_id: Some(parent_drive_id.to_string()),
                                name: file.name.clone(),
                                mime_type: file.mime_type.clone(),
                                size: file.size_bytes(),
                                modified_time: file.modified_time.clone().unwrap_or_default(),
                                etag: file.version.clone(),
                                local_cache_path: None,
                                sync_status: FileSyncStatus::MetadataOnly,
                                last_accessed: CachedFileRecord::now_epoch(),
                            };
                            if let Err(e) = cache.upsert_file_record(&rec) {
                                warn!("Cache upsert failed for {}: {}", file.id, e);
                            }
                        }
                        let _ = cache.mark_directory_fetched(parent_drive_id);
                    }
                }

                let mut table = self.inodes.write().unwrap();
                let mut child_inos = Vec::with_capacity(files.len() + 2);

                if parent_ino == FUSER_ROOT_ID {
                    if let Some(ino) = table.get_child_ino(FUSER_ROOT_ID, ".trackerignore") {
                        child_inos.push(ino);
                    }
                    if let Some(ino) = table.get_child_ino(FUSER_ROOT_ID, ".nomedia") {
                        child_inos.push(ino);
                    }
                    if let Some(ino) = table.get_child_ino(FUSER_ROOT_ID, ".noindex") {
                        child_inos.push(ino);
                    }
                }

                for file in &files {
                    let ino = table.insert_or_update(parent_ino, file);
                    child_inos.push(ino);
                }

                table.set_children(parent_ino, child_inos);
                table.get_children(parent_ino).unwrap_or_default()
            }
            Err(e) => {
                warn!(
                    "Failed to fetch children for {}: {}. Attempting cache fallback...",
                    parent_drive_id, e
                );
                // Fallback to SQLite cache if available, even if TTL expired!
                if let Some(entries) = self.load_cached_children(parent_ino, parent_drive_id) {
                    info!(
                        "Served {} cached entries for {} from local SQLite cache during remote failure.",
                        entries.len(),
                        parent_drive_id
                    );
                    return entries;
                }
                Vec::new()
            }
        }
    }

    /// Allocate a new file handle ID.
    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }

    /// Return the staging file path for a given file handle.
    fn staging_path_for_fh(&self, fh: u64) -> PathBuf {
        self.staging_dir.join(format!("fh_{fh}"))
    }
}

impl Filesystem for GoogleDriveFS {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        info!("Google Drive FUSE filesystem initialized (read-write mode).");
        let _ = config.set_max_readahead(8 * 1024 * 1024); // 8 MB kernel read-ahead for smooth video streaming
        let _ = config.set_max_background(128); // Allow more concurrent background FUSE requests
        let _ = config.set_congestion_threshold(96); // Higher threshold before kernel starts throttling
        let _ = config.set_max_write(1024 * 1024); // 1 MB write buffer
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parent_ino = parent.0;
        debug!("FUSE lookup: parent={}, name={}", parent_ino, name_str);

        // 1. Check if child is cached in memory
        let cached_ino = {
            let table = self.inodes.read().unwrap();
            table.get_child_ino(parent_ino, name_str)
        };

        let target_ino = if let Some(ino) = cached_ino {
            ino
        } else {
            // Fast negative lookup: if parent's children are already loaded,
            // the requested file definitely does not exist! Do not call remote API.
            let already_fetched = {
                let table = self.inodes.read().unwrap();
                table.has_children_cached(parent_ino)
            };

            if already_fetched {
                debug!(
                    "Fast negative lookup HIT: parent={}, name={}",
                    parent_ino, name_str
                );
                reply.error(Errno::ENOENT);
                return;
            }

            // 2. Not cached; fetch directory (checking SQLite first, then Drive API)
            let parent_drive_id = {
                let table = self.inodes.read().unwrap();
                table.get_by_ino(parent_ino).map(|e| e.drive_id)
            };

            if let Some(drive_id) = parent_drive_id {
                self.fetch_children_from_drive(parent_ino, &drive_id);
                let table = self.inodes.read().unwrap();
                match table.get_child_ino(parent_ino, name_str) {
                    Some(ino) => ino,
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                }
            } else {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let table = self.inodes.read().unwrap();
        if let Some(entry) = table.get_by_ino(target_ino) {
            let attr = entry.to_file_attr(self.uid, self.gid);
            reply.entry(&ATTR_TTL, &attr, Generation(0));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino_val = ino.0;
        debug!("FUSE getattr: ino={}", ino_val);
        let table = self.inodes.read().unwrap();
        if let Some(entry) = table.get_by_ino(ino_val) {
            let attr = entry.to_file_attr(self.uid, self.gid);
            reply.attr(&ATTR_TTL, &attr);
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino_val = ino.0;
        debug!("FUSE setattr: ino={}, size={:?}", ino_val, size);

        // Handle truncation: update inode size, truncate staging file if open
        if let Some(new_size) = size {
            {
                let mut table = self.inodes.write().unwrap();
                table.update_size(ino_val, new_size);
            }

            // Also truncate staging file if a write handle is open for this inode
            let handles = self.write_handles.read().unwrap();
            for stage in handles.values() {
                if stage.ino == ino_val {
                    if let Ok(f) = OpenOptions::new().write(true).open(&stage.staging_path) {
                        let _ = f.set_len(new_size);
                    }
                    break;
                }
            }
        }

        let table = self.inodes.read().unwrap();
        if let Some(entry) = table.get_by_ino(ino_val) {
            reply.attr(&ATTR_TTL, &entry.to_file_attr(self.uid, self.gid));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino_val = ino.0;
        debug!("FUSE readdir: ino={}, offset={}", ino_val, offset);

        let (parent_ino, drive_id, is_dir) = {
            let table = self.inodes.read().unwrap();
            match table.get_by_ino(ino_val) {
                Some(entry) => (entry.parent_ino, entry.drive_id, entry.is_dir),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        if !is_dir {
            reply.error(Errno::ENOTDIR);
            return;
        }

        // Check if children are cached, else fetch from Drive
        let children = {
            let table = self.inodes.read().unwrap();
            table.get_children(ino_val)
        };

        let entries = match children {
            Some(list) => list,
            None => self.fetch_children_from_drive(ino_val, &drive_id),
        };

        let mut list: Vec<(u64, FileType, String)> = Vec::with_capacity(entries.len() + 2);
        list.push((ino_val, FileType::Directory, ".".to_string()));
        list.push((parent_ino, FileType::Directory, "..".to_string()));

        for child in entries {
            let kind = if child.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            list.push((child.ino, kind, child.name));
        }

        for (i, (child_ino, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, name) {
                break;
            }
        }

        reply.ok();
    }

    /// Open a file. Supports both read-only and write-mode opens.
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino_val = ino.0;
        debug!("FUSE open: ino={}, flags={:#x}", ino_val, flags.0);

        let entry = {
            let table = self.inodes.read().unwrap();
            match table.get_by_ino(ino_val) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        if entry.is_dir {
            reply.error(Errno::EISDIR);
            return;
        }

        // Google Workspace native files (Docs, Sheets, etc.) cannot be opened for writing
        // through the FUSE layer. Reject write-mode opens with EPERM.
        if is_google_native_doc(&entry.mime_type) {
            let access_mode = flags.0 & libc::O_ACCMODE;
            let writable = access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR;
            if writable {
                warn!(
                    "Rejected write-mode open on Google native doc: {} (mime={})",
                    entry.name, entry.mime_type
                );
                reply.error(Errno::EPERM);
                return;
            }
            // Read-only open of a Google Doc: serve the stub content directly
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }

        let access_mode = flags.0 & libc::O_ACCMODE;
        let writable = access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR;

        if writable {
            // Stage existing content locally so writes can be seeked
            let fh = self.alloc_fh();
            let staging_path = self.staging_path_for_fh(fh);

            let drive_id = entry.drive_id.clone();
            let sp = staging_path.clone();
            let is_trunc = (flags.0 & libc::O_TRUNC) != 0;

            if is_trunc || entry.size == 0 {
                // If truncated or empty, avoid downloading the old file!
                if let Err(e) = fs::write(&staging_path, b"") {
                    error!("Failed to create empty staging file: {}", e);
                    reply.error(Errno::EIO);
                    return;
                }
            } else {
                // Check if already in local chunk cache
                let local_chunk = if let Some(ref arc_cache) = self.cache {
                    arc_cache
                        .lock()
                        .ok()
                        .and_then(|c| c.load_chunk(&drive_id).ok().flatten())
                } else {
                    None
                };

                if let Some(bytes) = local_chunk {
                    debug!("Staging from local chunk cache for ino={}", ino_val);
                    if let Err(e) = fs::write(&sp, &bytes) {
                        error!("Failed to stage cached file for writing: {}", e);
                        reply.error(Errno::EIO);
                        return;
                    }
                } else {
                    let client = self.client.clone();
                    let did = drive_id.clone();
                    let download_res = self
                        .rt_handle
                        .block_on(async move { client.download_file(&did).await });
                    match download_res {
                        Ok(bytes) => {
                            if let Err(e) = fs::write(&sp, &bytes) {
                                error!("Failed to stage file for writing: {}", e);
                                reply.error(Errno::EIO);
                                return;
                            }
                        }
                        Err(e) => {
                            error!("Failed to download file for write-staging: {}", e);
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                }
            }

            let stage = StageFile {
                ino: ino_val,
                drive_id: entry.drive_id.clone(),
                staging_path,
                is_new: false,
            };
            self.write_handles.write().unwrap().insert(fh, stage);
            debug!("Opened ino={} for writing as fh={}", ino_val, fh);
            reply.opened(FileHandle(fh), FopenFlags::empty());
        } else {
            // Read-only open: instruct the kernel to keep page cache across opens
            reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino_val = ino.0;
        debug!(
            "FUSE read: ino={}, offset={}, size={}",
            ino_val, offset, size
        );

        // If there's a staging file for this fh, read from it
        if fh.0 != 0 {
            let handles = self.write_handles.read().unwrap();
            if let Some(stage) = handles.get(&fh.0) {
                match File::open(&stage.staging_path) {
                    Ok(mut f) => {
                        if f.seek(SeekFrom::Start(offset)).is_err() {
                            reply.data(&[]);
                            return;
                        }
                        let mut buf = vec![0u8; size as usize];
                        match f.read(&mut buf) {
                            Ok(n) => {
                                reply.data(&buf[..n]);
                                return;
                            }
                            Err(e) => {
                                error!("Read from staging file failed: {}", e);
                                reply.error(Errno::EIO);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to open staging file: {}", e);
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
        }

        let entry = {
            let table = self.inodes.read().unwrap();
            match table.get_by_ino(ino_val) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        // Virtual control files (.trackerignore, .nomedia) return empty content
        if entry.drive_id.starts_with("virtual_") {
            reply.data(&[]);
            return;
        }

        if offset >= entry.size {
            reply.data(&[]);
            return;
        }

        // For Google Workspace native documents, serve a JSON stub referencing the web URL.
        // This allows `xdg-open` to open the file in the default browser.
        if is_google_native_doc(&entry.mime_type) {
            let web_url = self
                .rt_handle
                .block_on(async { self.client.get_metadata(&entry.drive_id).await })
                .ok()
                .and_then(|m| m.web_view_link)
                .unwrap_or_else(|| format!("https://docs.google.com/open?id={}", entry.drive_id));

            let stub = google_doc_stub(&entry.drive_id, &entry.name, &entry.mime_type, &web_url);
            let start = offset as usize;
            let end = (start + size as usize).min(stub.len());
            if start >= stub.len() {
                reply.data(&[]);
            } else {
                reply.data(&stub[start..end]);
            }
            return;
        }

        let read_len = (size as u64).min(entry.size - offset);
        let drive_id = entry.drive_id.clone();

        // --- 1. Direct local chunk cache read (zero-copy, lock-free slice read) ---
        // Reads directly from the locally cached file on disk using read_exact_at without
        // loading the entire file into memory or holding the SQLite cache lock.
        if let Some(ref arc_cache) = self.cache {
            let chunk_path = arc_cache.lock().ok().map(|c| c.chunk_path(&drive_id));
            if let Some(path) = chunk_path {
                if path.exists() {
                    if let Ok(file) = File::open(&path) {
                        if let Ok(meta) = file.metadata() {
                            let file_len = meta.len();
                            if offset < file_len {
                                let to_read = (read_len as usize).min((file_len - offset) as usize);
                                let mut buf = vec![0u8; to_read];
                                use std::os::unix::fs::FileExt;
                                if let Ok(()) = file.read_exact_at(&mut buf, offset) {
                                    reply.data(&buf);
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }

        // --- 2. Full download & disk cache for files <= 32 MB (images, audio, docs) ---
        // For files up to 32 MB, downloading the full file once enables instant zero-latency
        // local disk access for all subsequent reads and future opens.
        const CACHE_THRESHOLD: u64 = 32 * 1024 * 1024;
        let did = drive_id.clone();

        if entry.size > 0 && entry.size <= CACHE_THRESHOLD {
            let client_for_full = self.client.clone();
            let did_clone = did.clone();
            let full_res = self
                .rt_handle
                .block_on(async move { client_for_full.download_file(&did_clone).await });

            if let Ok(full_data) = full_res {
                if let Some(ref arc_cache) = self.cache {
                    if let Ok(cache) = arc_cache.lock() {
                        if let Ok(path) = cache.save_chunk(&did, &full_data) {
                            let mut rec = CachedFileRecord {
                                drive_id: did.clone(),
                                parent_id: None,
                                name: entry.name.clone(),
                                mime_type: entry.mime_type.clone(),
                                size: entry.size,
                                modified_time: String::new(),
                                etag: None,
                                local_cache_path: Some(path.to_string_lossy().to_string()),
                                sync_status: FileSyncStatus::Cached,
                                last_accessed: CachedFileRecord::now_epoch(),
                            };
                            if let Ok(Some(existing)) = cache.get_file_record(&did) {
                                rec.parent_id = existing.parent_id;
                                rec.modified_time = existing.modified_time;
                                rec.etag = existing.etag;
                                rec.name = existing.name;
                            }
                            let _ = cache.upsert_file_record(&rec);
                            debug!(
                                "Cached full file ({} bytes) for drive_id={}",
                                full_data.len(),
                                did
                            );
                        }
                    }
                }
                let start = offset as usize;
                let end = (start + read_len as usize).min(full_data.len());
                if start < full_data.len() {
                    reply.data(&full_data[start..end]);
                } else {
                    reply.data(&[]);
                }
                return;
            }
        }

        // --- 3. High-performance streaming buffer with read-ahead (movies & large files) ---
        // Uses 4 MB chunk caching and proactive background prefetching to ensure smooth,
        // buffer-free video playback and instant seeking.
        let client = self.client.clone();
        let sm = self.stream_manager.clone();
        let rt = self.rt_handle.clone();
        let file_size = entry.size;
        let did_stream = drive_id.clone();

        let chunk_res = self.rt_handle.block_on(async move {
            sm.read_range(&client, &did_stream, file_size, offset, read_len, &rt)
                .await
        });

        match chunk_res {
            Ok(data) => {
                reply.data(&data);
            }
            Err(e) => {
                error!(
                    "FUSE read failed for file {} at offset {}: {}",
                    drive_id, offset, e
                );
                reply.error(Errno::EIO);
            }
        }
    }

    /// Create a new file. Creates metadata on Drive, stages locally, returns open handle.
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_ino = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        debug!("FUSE create: parent={}, name={}", parent_ino, name_str);

        let parent_drive_id = {
            let table = self.inodes.read().unwrap();
            match table.get_by_ino(parent_ino).map(|e| e.drive_id) {
                Some(id) => id,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let client = self.client.clone();
        let n = name_str.to_string();
        let pdid = parent_drive_id.clone();
        let create_res = self.rt_handle.block_on(async move {
            client
                .create_empty_file(&n, &pdid, "application/octet-stream")
                .await
        });

        let drive_file = match create_res {
            Ok(f) => f,
            Err(e) => {
                error!("Drive create_empty_file failed: {}", e);
                reply.error(Errno::EIO);
                return;
            }
        };

        let ino = {
            let mut table = self.inodes.write().unwrap();
            table.insert_new(
                parent_ino,
                &drive_file.id,
                name_str,
                false,
                "application/octet-stream",
            )
        };

        let fh = self.alloc_fh();
        let staging_path = self.staging_path_for_fh(fh);
        if let Err(e) = fs::write(&staging_path, b"") {
            error!("Failed to create staging file: {}", e);
            reply.error(Errno::EIO);
            return;
        }

        let stage = StageFile {
            ino,
            drive_id: drive_file.id.clone(),
            staging_path,
            is_new: true,
        };
        self.write_handles.write().unwrap().insert(fh, stage);

        let attr = {
            let table = self.inodes.read().unwrap();
            table
                .get_by_ino(ino)
                .unwrap()
                .to_file_attr(self.uid, self.gid)
        };

        info!(
            "Created new file: {} (ino={}, drive_id={})",
            name_str, ino, drive_file.id
        );
        reply.created(
            &ATTR_TTL,
            &attr,
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    /// Create a directory. Creates folder on Drive, allocates inode, returns entry.
    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_ino = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        debug!("FUSE mkdir: parent={}, name={}", parent_ino, name_str);

        let parent_drive_id = {
            let table = self.inodes.read().unwrap();
            match table.get_by_ino(parent_ino).map(|e| e.drive_id) {
                Some(id) => id,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let client = self.client.clone();
        let n = name_str.to_string();
        let pdid = parent_drive_id.clone();
        let create_res = self
            .rt_handle
            .block_on(async move { client.create_folder(&n, Some(&pdid)).await });

        let drive_folder = match create_res {
            Ok(f) => f,
            Err(e) => {
                error!("Drive create_folder failed: {}", e);
                reply.error(Errno::EIO);
                return;
            }
        };

        let ino = {
            let mut table = self.inodes.write().unwrap();
            table.insert_new(
                parent_ino,
                &drive_folder.id,
                name_str,
                true,
                "application/vnd.google-apps.folder",
            )
        };

        let attr = {
            let table = self.inodes.read().unwrap();
            table
                .get_by_ino(ino)
                .unwrap()
                .to_file_attr(self.uid, self.gid)
        };

        info!(
            "Created folder: {} (ino={}, drive_id={})",
            name_str, ino, drive_folder.id
        );
        reply.entry(&ATTR_TTL, &attr, Generation(0));
    }

    /// Write bytes into the staging file for this file handle.
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino_val = ino.0;
        debug!(
            "FUSE write: ino={}, fh={}, offset={}, len={}",
            ino_val,
            fh.0,
            offset,
            data.len()
        );

        let handles = self.write_handles.read().unwrap();
        let stage = match handles.get(&fh.0) {
            Some(s) => s,
            None => {
                warn!("FUSE write: no staging handle for fh={}", fh.0);
                reply.error(Errno::EBADF);
                return;
            }
        };

        let result = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&stage.staging_path)
            .and_then(|mut f| {
                f.seek(SeekFrom::Start(offset))?;
                f.write_all(data)?;
                Ok(())
            });

        match result {
            Ok(()) => {
                // Update inode size if write extended the file
                let new_end = offset + data.len() as u64;
                {
                    let mut table = self.inodes.write().unwrap();
                    if let Some(entry) = table.ino_to_entry.get(&ino_val) {
                        if new_end > entry.size {
                            table.update_size(ino_val, new_end);
                        }
                    }
                }
                debug!(
                    "FUSE write: staged {} bytes at offset {}",
                    data.len(),
                    offset
                );
                reply.written(data.len() as u32);
            }
            Err(e) => {
                error!("FUSE write staging failed: {}", e);
                reply.error(Errno::EIO);
            }
        }
    }

    /// Flush: no-op. Upload happens on release.
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        debug!("FUSE flush: ino={}, fh={}", ino.0, fh.0);
        reply.ok();
    }

    /// Fsync: no-op. Upload happens on release.
    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        debug!("FUSE fsync: ino={}, fh={}", ino.0, fh.0);
        reply.ok();
    }

    /// Release: last close of a write handle — upload staged content to Drive.
    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino_val = ino.0;
        let fh_val = fh.0;
        debug!("FUSE release: ino={}, fh={}", ino_val, fh_val);

        // Only upload if this was a writable handle (fh != 0)
        if fh_val == 0 {
            if let Some(entry) = self.inodes.read().unwrap().get_by_ino(ino_val) {
                self.stream_manager.release_file(&entry.drive_id);
            }
            reply.ok();
            return;
        }

        let stage = {
            let mut handles = self.write_handles.write().unwrap();
            handles.remove(&fh_val)
        };

        if let Some(stage) = stage {
            // Read the staged bytes
            let content = match fs::read(&stage.staging_path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    error!("Failed to read staging file for upload: {}", e);
                    reply.error(Errno::EIO);
                    return;
                }
            };

            let content_len = content.len() as u64;
            let drive_id = stage.drive_id.clone();
            let client = self.client.clone();
            let did = drive_id.clone();

            // Update local inode size immediately
            {
                let mut table = self.inodes.write().unwrap();
                table.update_size(ino_val, content_len);
            }

            // Save chunk locally in cache immediately so subsequent reads hit local SSD/RAM
            if let Some(ref arc_cache) = self.cache {
                if let Ok(cache) = arc_cache.lock() {
                    let _ = cache.save_chunk(&drive_id, &content);
                    if let Ok(Some(mut rec)) = cache.get_file_record(&drive_id) {
                        rec.size = content_len;
                        rec.sync_status = FileSyncStatus::Uploading;
                        rec.last_accessed = CachedFileRecord::now_epoch();
                        let _ = cache.upsert_file_record(&rec);
                    }
                }
            }

            // Clean up staging file
            let _ = fs::remove_file(&stage.staging_path);

            // Spawn background upload onto Tokio runtime without blocking the FUSE thread!
            let cache_clone = self.cache.clone();
            let inodes_clone = self.inodes.clone();
            self.rt_handle.spawn(async move {
                match client.update_file_content(&did, content, None).await {
                    Ok(updated_file) => {
                        let authoritative_size = updated_file.size_bytes();
                        let final_size = if authoritative_size > 0 {
                            authoritative_size
                        } else {
                            content_len
                        };
                        if let Ok(mut table) = inodes_clone.write() {
                            table.update_size(ino_val, final_size);
                        }
                        if let Some(ref arc_cache) = cache_clone {
                            if let Ok(cache) = arc_cache.lock() {
                                if let Ok(Some(mut rec)) = cache.get_file_record(&did) {
                                    rec.sync_status = FileSyncStatus::Cached;
                                    rec.size = final_size;
                                    rec.last_accessed = CachedFileRecord::now_epoch();
                                    let _ = cache.upsert_file_record(&rec);
                                }
                            }
                        }
                        info!(
                            "Background upload complete for ino={} drive_id={} ({} bytes)",
                            ino_val, did, final_size
                        );
                    }
                    Err(e) => {
                        error!("Background upload failed for {}: {}", did, e);
                        if let Some(ref arc_cache) = cache_clone {
                            if let Ok(cache) = arc_cache.lock() {
                                if let Ok(Some(mut rec)) = cache.get_file_record(&did) {
                                    rec.sync_status = FileSyncStatus::ModifiedLocally;
                                    let _ = cache.upsert_file_record(&rec);
                                }
                            }
                        }
                    }
                }
            });

            reply.ok();
        } else {
            reply.ok();
        }
    }

    /// Delete a file. Trashes on Drive, removes from inode table.
    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_ino = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        debug!("FUSE unlink: parent={}, name={}", parent_ino, name_str);

        let (ino, drive_id) = {
            let table = self.inodes.read().unwrap();
            match table.get_child_ino(parent_ino, name_str) {
                Some(ino) => match table.get_by_ino(ino) {
                    Some(entry) => (ino, entry.drive_id),
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                },
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let client = self.client.clone();
        let did = drive_id.clone();
        let delete_res = self
            .rt_handle
            .block_on(async move { client.delete_file(&did, false).await });

        match delete_res {
            Ok(()) => {
                // Evict from metadata + chunk cache
                if let Some(ref arc_cache) = self.cache {
                    if let Ok(cache) = arc_cache.lock() {
                        let _ = cache.delete_chunk(&drive_id);
                        let _ = cache.delete_file_record(&drive_id);
                    }
                }
                let mut table = self.inodes.write().unwrap();
                table.remove_by_ino(ino);
                info!(
                    "Deleted file: {} (ino={}, drive_id={})",
                    name_str, ino, drive_id
                );
                reply.ok();
            }
            Err(e) => {
                error!("Drive delete_file failed for {}: {}", drive_id, e);
                reply.error(Errno::EIO);
            }
        }
    }

    /// Remove a directory. Trashes on Drive, removes from inode table.
    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_ino = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        debug!("FUSE rmdir: parent={}, name={}", parent_ino, name_str);

        let (ino, drive_id) = {
            let table = self.inodes.read().unwrap();
            match table.get_child_ino(parent_ino, name_str) {
                Some(ino) => match table.get_by_ino(ino) {
                    Some(entry) if entry.is_dir => (ino, entry.drive_id),
                    Some(_) => {
                        reply.error(Errno::ENOTDIR);
                        return;
                    }
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                },
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let client = self.client.clone();
        let did = drive_id.clone();
        let delete_res = self
            .rt_handle
            .block_on(async move { client.delete_file(&did, false).await });

        match delete_res {
            Ok(()) => {
                let mut table = self.inodes.write().unwrap();
                table.remove_by_ino(ino);
                info!(
                    "Removed directory: {} (ino={}, drive_id={})",
                    name_str, ino, drive_id
                );
                reply.ok();
            }
            Err(e) => {
                error!("Drive delete_file (rmdir) failed for {}: {}", drive_id, e);
                reply.error(Errno::EIO);
            }
        }
    }

    /// Rename or move a file/folder within or across directories.
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent_ino = parent.0;
        let newparent_ino = newparent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        debug!(
            "FUSE rename: parent={}, name={}, newparent={}, newname={}",
            parent_ino, name_str, newparent_ino, newname_str
        );

        let (ino, drive_id, old_parent_drive_id, new_parent_drive_id) = {
            let table = self.inodes.read().unwrap();
            let child_ino = match table.get_child_ino(parent_ino, name_str) {
                Some(i) => i,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let entry = match table.get_by_ino(child_ino) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let old_parent = table
                .get_by_ino(parent_ino)
                .map(|e| e.drive_id)
                .unwrap_or_else(|| "root".to_string());
            let new_parent = table
                .get_by_ino(newparent_ino)
                .map(|e| e.drive_id)
                .unwrap_or_else(|| "root".to_string());
            (child_ino, entry.drive_id, old_parent, new_parent)
        };

        let client = self.client.clone();
        let did = drive_id.clone();
        let same_parent = parent_ino == newparent_ino;
        let new_n = newname_str.to_string();
        let add_p = new_parent_drive_id.clone();
        let rem_p = old_parent_drive_id.clone();

        let rename_res = self.rt_handle.block_on(async move {
            if same_parent {
                // Rename only
                client.rename_file(&did, &new_n).await
            } else {
                // Move across directories (and rename simultaneously)
                let moved = client.move_file(&did, &add_p, &rem_p).await?;
                if moved.name != new_n {
                    client.rename_file(&did, &new_n).await
                } else {
                    Ok(moved)
                }
            }
        });

        match rename_res {
            Ok(_) => {
                let mut table = self.inodes.write().unwrap();
                table.update_parent_and_name(ino, newparent_ino, newname_str);
                info!(
                    "Renamed/moved {} -> {} (ino={}, drive_id={})",
                    name_str, newname_str, ino, drive_id
                );
                reply.ok();
            }
            Err(e) => {
                error!("Drive rename/move failed: {}", e);
                reply.error(Errno::EIO);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mount / unmount helpers.
// ---------------------------------------------------------------------------

/// Active background FUSE mount session.
pub struct ActiveMount {
    mount_point: PathBuf,
    session: Option<fuser::BackgroundSession>,
}

impl ActiveMount {
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Explicitly unmount the FUSE filesystem.
    pub fn unmount(mut self) -> Result<(), FsError> {
        if let Some(session) = self.session.take() {
            drop(session);
        }
        if is_path_mounted(&self.mount_point) {
            unmount_path(&self.mount_point)
        } else {
            Ok(())
        }
    }
}

impl Drop for ActiveMount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            drop(session);
        }
        if is_path_mounted(&self.mount_point) {
            let _ = unmount_path(&self.mount_point);
        }
    }
}

/// Default staging directory: `~/.cache/google-drive-linux/staging/`
pub fn default_staging_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("google-drive-linux")
        .join("staging")
}

/// Mount Google Drive as a read-write virtual filesystem at `mount_point`.
pub fn mount_filesystem(
    mount_point: &Path,
    client: Arc<GoogleDriveClient>,
    rt_handle: tokio::runtime::Handle,
    cache: Option<Arc<Mutex<MetadataCache>>>,
) -> Result<ActiveMount, FsError> {
    if !mount_point.exists() {
        fs::create_dir_all(mount_point)?;
    }

    if !mount_point.is_dir() {
        return Err(FsError::InvalidMountPoint(mount_point.to_path_buf()));
    }

    info!("Mounting Google Drive at {:?} (read-write)", mount_point);

    let staging_dir = default_staging_dir();
    let fs = GoogleDriveFS::new(client, rt_handle, staging_dir, cache);
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("GoogleDrive".to_string()),
        MountOption::Subtype("google-drive-linux".to_string()),
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoAtime,
        MountOption::Async,
    ];

    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get().clamp(8, 32))
        .unwrap_or(8);
    config.n_threads = Some(n_threads);
    config.clone_fd = true;

    let session = fuser::spawn_mount(fs, mount_point, &config)
        .map_err(|e| FsError::MountFailed(e.to_string()))?;

    info!("Google Drive successfully mounted at {:?}", mount_point);

    Ok(ActiveMount {
        mount_point: mount_point.to_path_buf(),
        session: Some(session),
    })
}

/// Safely unmount a target path using `fusermount3 -u`.
pub fn unmount_path(mount_point: &Path) -> Result<(), FsError> {
    info!("Unmounting FUSE filesystem at {:?}", mount_point);
    let status = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(mount_point)
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("Successfully unmounted {:?}", mount_point);
            Ok(())
        }
        Ok(s) => {
            warn!("fusermount3 returned non-zero code: {:?}", s.code());
            // Attempt lazy unmount if normal unmount failed
            let lazy = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg("-z")
                .arg(mount_point)
                .status();
            if let Ok(ls) = lazy {
                if ls.success() {
                    return Ok(());
                }
            }
            Err(FsError::UnmountFailed(format!("Exit code {:?}", s.code())))
        }
        Err(e) => Err(FsError::UnmountFailed(e.to_string())),
    }
}

/// Check whether `path` is currently an active mount point.
pub fn is_path_mounted(path: &Path) -> bool {
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        let path_str = path.to_string_lossy();
        for line in mounts.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1] == path_str {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inode_table_root() {
        let table = InodeTable::new();
        let root = table.get_by_ino(FUSER_ROOT_ID).expect("root exists");
        assert_eq!(root.drive_id, "root");
        assert!(root.is_dir);
        assert_eq!(table.get_by_drive_id("root"), Some(FUSER_ROOT_ID));
    }

    #[test]
    fn test_inode_table_insertion_and_lookup() {
        let mut table = InodeTable::new();
        let file = DriveFile {
            id: "drive_file_1".to_string(),
            name: "Document.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: Some("2048".to_string()),
            modified_time: Some("2026-09-03T00:00:00Z".to_string()),
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };

        let ino = table.insert_or_update(FUSER_ROOT_ID, &file);
        assert!(ino > 1);

        let entry = table.get_by_ino(ino).expect("must exist");
        assert_eq!(entry.name, "Document.pdf");
        assert_eq!(entry.size, 2048);
        assert!(!entry.is_dir);

        let lookup_ino = table.get_child_ino(FUSER_ROOT_ID, "Document.pdf");
        assert_eq!(lookup_ino, Some(ino));
    }

    #[test]
    fn test_file_attr_conversion() {
        let entry = InodeEntry {
            ino: 42,
            drive_id: "id_42".to_string(),
            parent_ino: 1,
            name: "test.txt".to_string(),
            is_dir: false,
            size: 100,
            modified_time: SystemTime::now(),
            mime_type: "text/plain".to_string(),
        };

        let attr = entry.to_file_attr(1000, 1000);
        assert_eq!(attr.ino, INodeNo(42));
        assert_eq!(attr.size, 100);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o644);
    }

    #[test]
    fn test_mount_path_check() {
        assert!(!is_path_mounted(Path::new("/nonexistent_drive_test_path")));
    }

    #[test]
    fn test_inode_table_remove() {
        let mut table = InodeTable::new();
        let file = DriveFile {
            id: "to_remove_id".to_string(),
            name: "removeme.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("512".to_string()),
            modified_time: None,
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };

        let ino = table.insert_or_update(FUSER_ROOT_ID, &file);
        assert!(table.get_by_ino(ino).is_some());
        assert!(table.get_child_ino(FUSER_ROOT_ID, "removeme.txt").is_some());
        assert_eq!(table.get_by_drive_id("to_remove_id"), Some(ino));

        table.remove_by_ino(ino);

        assert!(table.get_by_ino(ino).is_none());
        assert!(table.get_child_ino(FUSER_ROOT_ID, "removeme.txt").is_none());
        assert_eq!(table.get_by_drive_id("to_remove_id"), None);
    }

    #[test]
    fn test_inode_table_update_name() {
        let mut table = InodeTable::new();

        // Insert a dummy parent folder
        let folder_id = "subfolder_id";
        let folder_ino = table.insert_new(
            FUSER_ROOT_ID,
            folder_id,
            "subfolder",
            true,
            "application/vnd.google-apps.folder",
        );

        let file = DriveFile {
            id: "rename_me_id".to_string(),
            name: "old_name.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("0".to_string()),
            modified_time: None,
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };

        let ino = table.insert_or_update(FUSER_ROOT_ID, &file);
        assert_eq!(
            table.get_child_ino(FUSER_ROOT_ID, "old_name.txt"),
            Some(ino)
        );

        // Rename and move to sub-folder
        table.update_parent_and_name(ino, folder_ino, "new_name.txt");

        // Old entry gone
        assert!(table.get_child_ino(FUSER_ROOT_ID, "old_name.txt").is_none());
        // New entry exists
        assert_eq!(table.get_child_ino(folder_ino, "new_name.txt"), Some(ino));
        // Drive ID mapping unchanged
        assert_eq!(table.get_by_drive_id("rename_me_id"), Some(ino));
    }

    #[test]
    fn test_inode_table_update_size() {
        let mut table = InodeTable::new();
        let file = DriveFile {
            id: "size_test_id".to_string(),
            name: "size_test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size: Some("100".to_string()),
            modified_time: None,
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };
        let ino = table.insert_or_update(FUSER_ROOT_ID, &file);
        assert_eq!(table.get_by_ino(ino).unwrap().size, 100);

        table.update_size(ino, 2048);
        assert_eq!(table.get_by_ino(ino).unwrap().size, 2048);
    }

    #[test]
    fn test_insert_new_directory() {
        let mut table = InodeTable::new();
        let ino = table.insert_new(
            FUSER_ROOT_ID,
            "new_dir_id",
            "new_folder",
            true,
            "application/vnd.google-apps.folder",
        );
        assert!(ino > 1);
        let entry = table.get_by_ino(ino).expect("entry should exist");
        assert_eq!(entry.name, "new_folder");
        assert!(entry.is_dir);
        assert_eq!(entry.drive_id, "new_dir_id");
        assert_eq!(table.get_child_ino(FUSER_ROOT_ID, "new_folder"), Some(ino));
    }

    #[test]
    fn test_staging_dir_default_path() {
        let path = default_staging_dir();
        assert!(path.to_string_lossy().contains("google-drive-linux"));
        assert!(path.to_string_lossy().contains("staging"));
    }

    #[test]
    fn test_virtual_trackerignore_and_nomedia() {
        let table = InodeTable::new();
        let tracker_ino = table.get_child_ino(FUSER_ROOT_ID, ".trackerignore");
        assert_eq!(tracker_ino, Some(2));
        let tracker_entry = table.get_by_ino(2).expect("tracker entry");
        assert_eq!(tracker_entry.name, ".trackerignore");
        assert_eq!(tracker_entry.size, 0);

        let nomedia_ino = table.get_child_ino(FUSER_ROOT_ID, ".nomedia");
        assert_eq!(nomedia_ino, Some(3));
        let nomedia_entry = table.get_by_ino(3).expect("nomedia entry");
        assert_eq!(nomedia_entry.name, ".nomedia");
        assert_eq!(nomedia_entry.size, 0);

        let noindex_ino = table.get_child_ino(FUSER_ROOT_ID, ".noindex");
        assert_eq!(noindex_ino, Some(4));
        let noindex_entry = table.get_by_ino(4).expect("noindex entry");
        assert_eq!(noindex_entry.name, ".noindex");
        assert_eq!(noindex_entry.size, 0);
    }

    #[test]
    fn test_has_children_cached() {
        let mut table = InodeTable::new();
        assert!(!table.has_children_cached(FUSER_ROOT_ID));

        table.set_children(FUSER_ROOT_ID, vec![2, 3, 4]);
        assert!(table.has_children_cached(FUSER_ROOT_ID));
    }
}
