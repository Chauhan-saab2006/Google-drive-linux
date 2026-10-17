use crate::drive::{DriveError, GoogleDriveClient};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::watch;
use tracing::{debug, warn};

pub const STREAM_CHUNK_SIZE: u64 = 4 * 1024 * 1024; // 4 MB chunk size for high-speed streaming
pub const MAX_CHUNKS_PER_STREAM: usize = 8; // 32 MB max in-memory window per stream
pub const MAX_ACTIVE_STREAMS: usize = 16; // Up to 16 active concurrent streaming files

#[derive(Clone)]
enum ChunkSlot {
    Ready(Arc<Vec<u8>>),
    Loading(watch::Receiver<Option<Arc<Vec<u8>>>>),
}

struct StreamFileState {
    slots: HashMap<u64, (Instant, ChunkSlot)>,
    last_accessed: Instant,
}

impl StreamFileState {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            last_accessed: Instant::now(),
        }
    }

    fn prune_lru(&mut self, max_chunks: usize) {
        if self.slots.len() <= max_chunks {
            return;
        }

        let mut entries: Vec<(u64, Instant)> = self
            .slots
            .iter()
            .map(|(&idx, &(instant, _))| (idx, instant))
            .collect();
        entries.sort_by_key(|&(_, instant)| instant);

        let to_remove = self.slots.len().saturating_sub(max_chunks);
        for (idx, _) in entries.into_iter().take(to_remove) {
            // Only prune slots that are ready, don't prune active downloads
            if let Some((_, ChunkSlot::Ready(_))) = self.slots.get(&idx) {
                self.slots.remove(&idx);
            }
        }
    }
}

/// In-memory sliding window chunk cache with asynchronous read-ahead prefetching
/// specifically engineered for low-latency media playback (movies/video/audio).
pub struct StreamBufferManager {
    files: Mutex<HashMap<String, Arc<Mutex<StreamFileState>>>>,
}

impl Default for StreamBufferManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBufferManager {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
        }
    }

    fn get_file_state(&self, drive_id: &str) -> Arc<Mutex<StreamFileState>> {
        let mut files = self.files.lock().unwrap();

        // Evict oldest inactive files if exceeding MAX_ACTIVE_STREAMS
        if files.len() >= MAX_ACTIVE_STREAMS {
            let mut file_ages: Vec<(String, Instant)> = files
                .iter()
                .map(|(id, state)| {
                    let last = state
                        .lock()
                        .map(|s| s.last_accessed)
                        .unwrap_or_else(|_| Instant::now());
                    (id.clone(), last)
                })
                .collect();
            file_ages.sort_by_key(|&(_, instant)| instant);

            let excess = files.len() + 1 - MAX_ACTIVE_STREAMS;
            for (id, _) in file_ages.into_iter().take(excess) {
                files.remove(&id);
            }
        }

        files
            .entry(drive_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(StreamFileState::new())))
            .clone()
    }

    /// Free all in-memory chunks associated with a file upon FUSE handle release.
    pub fn release_file(&self, drive_id: &str) {
        if let Ok(mut files) = self.files.lock() {
            files.remove(drive_id);
            debug!("Released in-memory stream buffer for {}", drive_id);
        }
    }

    /// Read an arbitrary byte range from Google Drive with 4MB chunk caching and read-ahead.
    pub async fn read_range(
        &self,
        client: &Arc<GoogleDriveClient>,
        drive_id: &str,
        file_size: u64,
        offset: u64,
        length: u64,
        rt_handle: &tokio::runtime::Handle,
    ) -> Result<Vec<u8>, DriveError> {
        if length == 0 || offset >= file_size {
            return Ok(Vec::new());
        }

        let actual_length = length.min(file_size - offset);
        let start_chunk = offset / STREAM_CHUNK_SIZE;
        let end_chunk = (offset + actual_length - 1) / STREAM_CHUNK_SIZE;

        let file_state = self.get_file_state(drive_id);

        if start_chunk == end_chunk {
            let chunk_data = self
                .get_or_fetch_chunk(
                    file_state.clone(),
                    client,
                    drive_id,
                    start_chunk,
                    file_size,
                    rt_handle,
                )
                .await?;

            let chunk_start_offset = start_chunk * STREAM_CHUNK_SIZE;
            let slice_start = (offset - chunk_start_offset) as usize;
            let slice_end = slice_start + actual_length as usize;

            if slice_start >= chunk_data.len() {
                return Ok(Vec::new());
            }
            let valid_end = slice_end.min(chunk_data.len());
            let result = chunk_data[slice_start..valid_end].to_vec();

            // Trigger background prefetch for next chunk if reader is past midway in this chunk
            if (offset + actual_length) >= chunk_start_offset + (STREAM_CHUNK_SIZE / 2) {
                let next_chunk = start_chunk + 1;
                if next_chunk * STREAM_CHUNK_SIZE < file_size {
                    self.trigger_prefetch(
                        file_state,
                        client.clone(),
                        drive_id.to_string(),
                        next_chunk,
                        file_size,
                        rt_handle,
                    );
                }
            }

            Ok(result)
        } else {
            // Read spans across multiple chunks
            let mut result = Vec::with_capacity(actual_length as usize);
            let mut current_offset = offset;
            let end_offset = offset + actual_length;

            for chunk_idx in start_chunk..=end_chunk {
                let chunk_data = self
                    .get_or_fetch_chunk(
                        file_state.clone(),
                        client,
                        drive_id,
                        chunk_idx,
                        file_size,
                        rt_handle,
                    )
                    .await?;

                let chunk_start_offset = chunk_idx * STREAM_CHUNK_SIZE;
                let chunk_end_offset = (chunk_start_offset + STREAM_CHUNK_SIZE).min(file_size);

                let read_start = current_offset.max(chunk_start_offset);
                let read_end = end_offset.min(chunk_end_offset);

                let slice_start = (read_start - chunk_start_offset) as usize;
                let slice_end = (read_end - chunk_start_offset) as usize;

                if slice_start < chunk_data.len() {
                    let valid_end = slice_end.min(chunk_data.len());
                    result.extend_from_slice(&chunk_data[slice_start..valid_end]);
                }
                current_offset = read_end;
            }

            // Trigger prefetch for the chunk immediately after end_chunk
            let next_chunk = end_chunk + 1;
            if next_chunk * STREAM_CHUNK_SIZE < file_size {
                self.trigger_prefetch(
                    file_state,
                    client.clone(),
                    drive_id.to_string(),
                    next_chunk,
                    file_size,
                    rt_handle,
                );
            }

            Ok(result)
        }
    }

    async fn get_or_fetch_chunk(
        &self,
        file_state: Arc<Mutex<StreamFileState>>,
        client: &Arc<GoogleDriveClient>,
        drive_id: &str,
        chunk_idx: u64,
        file_size: u64,
        rt_handle: &tokio::runtime::Handle,
    ) -> Result<Arc<Vec<u8>>, DriveError> {
        let chunk_start = chunk_idx * STREAM_CHUNK_SIZE;
        let chunk_len = STREAM_CHUNK_SIZE.min(file_size.saturating_sub(chunk_start));

        let slot_action = {
            let mut state = file_state.lock().unwrap();
            state.last_accessed = Instant::now();

            if let Some((instant, slot)) = state.slots.get_mut(&chunk_idx) {
                *instant = Instant::now();
                match slot {
                    ChunkSlot::Ready(data) => SlotAction::Ready(data.clone()),
                    ChunkSlot::Loading(rx) => SlotAction::Wait(rx.clone()),
                }
            } else {
                let (tx, rx) = watch::channel(None);
                state
                    .slots
                    .insert(chunk_idx, (Instant::now(), ChunkSlot::Loading(rx)));
                SlotAction::Fetch(tx)
            }
        };

        match slot_action {
            SlotAction::Ready(data) => Ok(data),
            SlotAction::Wait(mut rx) => {
                while rx.borrow().is_none() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                if let Some(ref data) = *rx.borrow() {
                    return Ok(data.clone());
                }
                // If download sender was dropped without data, retry fetching
                Box::pin(self.get_or_fetch_chunk(
                    file_state, client, drive_id, chunk_idx, file_size, rt_handle,
                ))
                .await
            }
            SlotAction::Fetch(tx) => {
                let fetch_res = client
                    .download_file_range(drive_id, chunk_start, chunk_len)
                    .await;
                match fetch_res {
                    Ok(bytes) => {
                        let arc_data = Arc::new(bytes);
                        let _ = tx.send(Some(arc_data.clone()));

                        let mut state = file_state.lock().unwrap();
                        state
                            .slots
                            .insert(chunk_idx, (Instant::now(), ChunkSlot::Ready(arc_data.clone())));
                        state.prune_lru(MAX_CHUNKS_PER_STREAM);
                        Ok(arc_data)
                    }
                    Err(e) => {
                        let mut state = file_state.lock().unwrap();
                        state.slots.remove(&chunk_idx);
                        let _ = tx.send(None);
                        Err(e)
                    }
                }
            }
        }
    }

    fn trigger_prefetch(
        &self,
        file_state: Arc<Mutex<StreamFileState>>,
        client: Arc<GoogleDriveClient>,
        drive_id: String,
        chunk_idx: u64,
        file_size: u64,
        rt_handle: &tokio::runtime::Handle,
    ) {
        let chunk_start = chunk_idx * STREAM_CHUNK_SIZE;
        if chunk_start >= file_size {
            return;
        }
        let chunk_len = STREAM_CHUNK_SIZE.min(file_size - chunk_start);

        let tx = {
            let mut state = file_state.lock().unwrap();
            if state.slots.contains_key(&chunk_idx) {
                return;
            }
            let (tx, rx) = watch::channel(None);
            state
                .slots
                .insert(chunk_idx, (Instant::now(), ChunkSlot::Loading(rx)));
            tx
        };

        let fs = file_state.clone();
        let did = drive_id.clone();
        rt_handle.spawn(async move {
            debug!("Prefetching chunk {} for {}", chunk_idx, did);
            match client.download_file_range(&did, chunk_start, chunk_len).await {
                Ok(bytes) => {
                    let arc_data = Arc::new(bytes);
                    let _ = tx.send(Some(arc_data.clone()));
                    let mut state = fs.lock().unwrap();
                    state
                        .slots
                        .insert(chunk_idx, (Instant::now(), ChunkSlot::Ready(arc_data)));
                    state.prune_lru(MAX_CHUNKS_PER_STREAM);
                }
                Err(e) => {
                    warn!(
                        "Background prefetch failed for chunk {} of {}: {}",
                        chunk_idx, did, e
                    );
                    let mut state = fs.lock().unwrap();
                    state.slots.remove(&chunk_idx);
                    let _ = tx.send(None);
                }
            }
        });
    }
}

enum SlotAction {
    Ready(Arc<Vec<u8>>),
    Wait(watch::Receiver<Option<Arc<Vec<u8>>>>),
    Fetch(watch::Sender<Option<Arc<Vec<u8>>>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{GoogleOAuthClient, OAuthConfig, StoredCredentials, TokenStore};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_stream_file_state_lru() {
        let mut state = StreamFileState::new();
        // Insert 10 chunks
        for i in 0..10 {
            let data = Arc::new(vec![i as u8; 1024]);
            state.slots.insert(i, (Instant::now(), ChunkSlot::Ready(data)));
        }
        assert_eq!(state.slots.len(), 10);

        // Prune to 4 chunks
        state.prune_lru(4);
        assert_eq!(state.slots.len(), 4);
    }

    #[test]
    fn test_stream_buffer_manager_lifecycle() {
        let manager = StreamBufferManager::new();
        let _state = manager.get_file_state("file_test_123");
        assert_eq!(manager.files.lock().unwrap().len(), 1);

        manager.release_file("file_test_123");
        assert_eq!(manager.files.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_stream_read_range_with_mock_drive() {
        // Setup mock server serving byte ranges
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let port = listener.local_addr().expect("port").port();

        // 10 MB test content filled with sequential pattern
        let mock_file_size: u64 = 10 * 1024 * 1024;

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                if req.contains("GET /drive/v3/files/stream_doc_1?alt=media") {
                    // Extract Range: bytes=start-end
                    let mut start = 0usize;
                    let mut end = mock_file_size as usize - 1;

                    for line in req.lines() {
                        if line.to_lowercase().starts_with("range: bytes=") {
                            let range_part = &line["range: bytes=".len()..];
                            let parts: Vec<&str> = range_part.split('-').collect();
                            if parts.len() == 2 {
                                start = parts[0].parse().unwrap_or(0);
                                end = parts[1].parse().unwrap_or(mock_file_size as usize - 1);
                            }
                        }
                    }

                    let _len = end - start + 1;
                    // Generate payload where byte value is (offset % 256)
                    let body: Vec<u8> = (start..=end).map(|i| (i % 251) as u8).collect();
                    let resp = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        start,
                        end,
                        mock_file_size
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                }
            }
        });

        let dir = tempdir().expect("tempdir");
        let creds_path = dir.path().join("creds.json");
        let store = TokenStore::new(creds_path);
        store
            .save(&StoredCredentials {
                access_token: "mock_token".to_string(),
                refresh_token: "mock_refresh".to_string(),
                token_type: "Bearer".to_string(),
                expires_at: u64::MAX,
                account_email: None,
                account_name: None,
                client_id: None,
                client_secret: None,
            })
            .expect("save creds");

        let oauth = Arc::new(GoogleOAuthClient::new(OAuthConfig::default(), store));
        let client = Arc::new(GoogleDriveClient::new(
            Some(format!("http://127.0.0.1:{port}")),
            None,
            oauth,
        ));

        let manager = StreamBufferManager::new();
        let rt = tokio::runtime::Handle::current();

        // 1. Read first 128 KB
        let chunk1 = manager
            .read_range(&client, "stream_doc_1", mock_file_size, 0, 131072, &rt)
            .await
            .expect("read chunk1");
        assert_eq!(chunk1.len(), 131072);
        assert_eq!(chunk1[0], 0);
        assert_eq!(chunk1[1], 1);

        // 2. Read next 128 KB (should hit in-memory chunk 0)
        let chunk2 = manager
            .read_range(&client, "stream_doc_1", mock_file_size, 131072, 131072, &rt)
            .await
            .expect("read chunk2");
        assert_eq!(chunk2.len(), 131072);
        assert_eq!(chunk2[0], (131072 % 251) as u8);

        // 3. Read spanning across the 4 MB boundary: 4MB - 100 bytes to 4MB + 100 bytes (200 bytes total)
        let span_offset = STREAM_CHUNK_SIZE - 100;
        let span_data = manager
            .read_range(&client, "stream_doc_1", mock_file_size, span_offset, 200, &rt)
            .await
            .expect("read spanning data");
        assert_eq!(span_data.len(), 200);
        for (i, &b) in span_data.iter().enumerate() {
            let expected = ((span_offset as usize + i) % 251) as u8;
            assert_eq!(b, expected);
        }
    }
}

