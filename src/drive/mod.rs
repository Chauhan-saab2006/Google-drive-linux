use crate::auth::{AuthError, GoogleOAuthClient};
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, warn};

#[derive(Error, Debug)]
pub enum DriveError {
    #[error("Authentication error: {0}")]
    Auth(#[from] AuthError),
    #[error("HTTP network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON serialization or parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Google API error (status {code}): {message}")]
    Api {
        code: u16,
        message: String,
        reason: Option<String>,
    },
    #[error("Google API rate limit exceeded. Retry after {retry_after_secs:?} seconds")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("Drive item not found: {0}")]
    NotFound(String),
    #[error("Invalid byte range requested: start={0}, length={1}")]
    InvalidRange(u64, u64),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Representation of a Google Drive item (file, folder, or shortcut).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub parents: Option<Vec<String>>,
    #[serde(default)]
    pub trashed: Option<bool>,
    #[serde(default)]
    pub web_view_link: Option<String>,
    #[serde(default)]
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

impl DriveFile {
    /// Return file size in bytes, or 0 if size is unspecified (such as for folders).
    pub fn size_bytes(&self) -> u64 {
        self.size
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Returns whether this item is a Google Drive folder.
    pub fn is_folder(&self) -> bool {
        self.mime_type == "application/vnd.google-apps.folder"
    }

    /// Returns whether this item is a native Google Doc, Sheet, Slide, or Form.
    pub fn is_google_doc(&self) -> bool {
        self.mime_type.starts_with("application/vnd.google-apps.") && !self.is_folder()
    }

    /// Returns whether this file is marked as trashed.
    pub fn is_trashed(&self) -> bool {
        self.trashed.unwrap_or(false)
    }

    /// Returns slice of parent IDs.
    pub fn parents_list(&self) -> &[String] {
        self.parents.as_deref().unwrap_or(&[])
    }
}

/// Paginated file listing response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileListResponse {
    pub files: Vec<DriveFile>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// A single file change event returned by the Google Drive Changes API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveChange {
    pub kind: Option<String>,
    pub change_type: Option<String>,
    pub file_id: Option<String>,
    pub file: Option<DriveFile>,
    #[serde(default)]
    pub removed: bool,
    pub time: Option<String>,
}

/// Response returned by `changes.list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveChangesResponse {
    pub kind: Option<String>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
    #[serde(default)]
    pub changes: Vec<DriveChange>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartPageTokenResponse {
    start_page_token: String,
}

/// Storage quota and user information returned by Google Drive `about.get`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveAboutResponse {
    pub user: Option<DriveUser>,
    pub storage_quota: Option<StorageQuota>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveUser {
    pub display_name: Option<String>,
    pub email_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuota {
    #[serde(default)]
    pub limit: Option<String>,
    #[serde(default)]
    pub usage: Option<String>,
    #[serde(default)]
    pub usage_in_drive: Option<String>,
    #[serde(default)]
    pub usage_in_drive_trash: Option<String>,
}

impl StorageQuota {
    pub fn limit_bytes(&self) -> Option<u64> {
        self.limit.as_deref().and_then(|s| s.parse::<u64>().ok())
    }

    pub fn usage_bytes(&self) -> u64 {
        self.usage
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    }

    pub fn usage_percent(&self) -> Option<f64> {
        let limit = self.limit_bytes()?;
        if limit == 0 {
            return None;
        }
        Some((self.usage_bytes() as f64 / limit as f64) * 100.0)
    }
}

/// Google Drive API v3 Client with resilient retries, rate limiting, and range streaming.
pub struct GoogleDriveClient {
    base_url: String,
    upload_base_url: String,
    http: Client,
    oauth: Arc<GoogleOAuthClient>,
}

const DEFAULT_BASE_URL: &str = "https://www.googleapis.com";
const DEFAULT_UPLOAD_URL: &str = "https://www.googleapis.com/upload";
const FILE_FIELDS: &str = "id,name,mimeType,size,modifiedTime,createdTime,parents,trashed,webViewLink,md5Checksum,version";

impl GoogleDriveClient {
    pub fn new(
        base_url: Option<String>,
        upload_base_url: Option<String>,
        oauth: Arc<GoogleOAuthClient>,
    ) -> Self {
        Self {
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            upload_base_url: upload_base_url.unwrap_or_else(|| DEFAULT_UPLOAD_URL.to_string()),
            http: Client::new(),
            oauth,
        }
    }

    /// Helper to acquire authorization headers with a fresh access token.
    async fn auth_headers(&self) -> Result<HeaderMap, DriveError> {
        let token = self.oauth.get_valid_access_token().await?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| DriveError::Api {
                code: 400,
                message: format!("Invalid header: {e}"),
                reason: None,
            })?,
        );
        Ok(headers)
    }

    /// Query account information and storage quota from `about.get`.
    pub async fn get_about(&self) -> Result<DriveAboutResponse, DriveError> {
        let url = format!("{}/drive/v3/about", self.base_url);
        let resp = self
            .execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[("fields", "user,storageQuota")])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let about: DriveAboutResponse = resp.json().await?;
        Ok(about)
    }

    /// Retrieve metadata for a specific Drive file or folder by ID.
    pub async fn get_metadata(&self, file_id: &str) -> Result<DriveFile, DriveError> {
        let url = format!("{}/drive/v3/files/{file_id}", self.base_url);
        let resp = self
            .execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[("fields", FILE_FIELDS)])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Err(DriveError::NotFound(file_id.to_string()));
        }

        let file: DriveFile = resp.json().await?;
        Ok(file)
    }

    /// List files located inside a parent folder, or root if `None`.
    pub async fn list_files(
        &self,
        parent_id: Option<&str>,
        page_token: Option<&str>,
        page_size: Option<u32>,
    ) -> Result<FileListResponse, DriveError> {
        let parent = parent_id.unwrap_or("root");
        let query = format!("'{parent}' in parents and trashed = false");
        let size_str = page_size.unwrap_or(100).to_string();

        let mut query_params = vec![
            ("q", query.as_str()),
            ("fields", "nextPageToken,files(id,name,mimeType,size,modifiedTime,createdTime,parents,trashed,webViewLink,md5Checksum,version)"),
            ("pageSize", size_str.as_str()),
            ("orderBy", "folder,name"),
        ];

        if let Some(token) = page_token {
            query_params.push(("pageToken", token));
        }

        let url = format!("{}/drive/v3/files", self.base_url);
        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let params = query_params.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&params)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let list: FileListResponse = resp.json().await?;
        Ok(list)
    }

    /// List all children of a directory, automatically handling pagination tokens.
    pub async fn list_all_children(&self, parent_id: &str) -> Result<Vec<DriveFile>, DriveError> {
        let mut all_files = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let res = self
                .list_files(Some(parent_id), page_token.as_deref(), Some(100))
                .await?;
            all_files.extend(res.files);

            if let Some(token) = res.next_page_token {
                if token.is_empty() {
                    break;
                }
                page_token = Some(token);
            } else {
                break;
            }
        }

        Ok(all_files)
    }

    /// Search files matching a Google Drive query expression.
    pub async fn search(&self, query: &str) -> Result<Vec<DriveFile>, DriveError> {
        let url = format!("{}/drive/v3/files", self.base_url);
        let resp = self
            .execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[
                            ("q", query),
                            ("fields", "files(id,name,mimeType,size,modifiedTime,createdTime,parents,trashed,webViewLink,md5Checksum,version)"),
                            ("pageSize", "50"),
                        ])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let list: FileListResponse = resp.json().await?;
        Ok(list.files)
    }

    /// Create a new folder on Google Drive.
    pub async fn create_folder(
        &self,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<DriveFile, DriveError> {
        let url = format!("{}/drive/v3/files", self.base_url);
        let parent = parent_id.unwrap_or("root");

        let body = serde_json::json!({
            "name": name,
            "mimeType": "application/vnd.google-apps.folder",
            "parents": [parent],
        });

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let body = body.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .post(&url)
                        .headers(headers)
                        .query(&[("fields", FILE_FIELDS)])
                        .json(&body)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let created: DriveFile = resp.json().await?;
        info!("Created folder: {} (id: {})", name, created.id);
        Ok(created)
    }

    /// Create a new empty file on Google Drive (metadata-only, no content).
    /// Used by the FUSE `create()` callback before writing staged content on `release()`.
    pub async fn create_empty_file(
        &self,
        name: &str,
        parent_id: &str,
        mime_type: &str,
    ) -> Result<DriveFile, DriveError> {
        let url = format!("{}/drive/v3/files", self.base_url);
        let body = serde_json::json!({
            "name": name,
            "parents": [parent_id],
            "mimeType": mime_type,
        });

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let body = body.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .post(&url)
                        .headers(headers)
                        .query(&[("fields", FILE_FIELDS)])
                        .json(&body)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let created: DriveFile = resp.json().await?;
        info!(
            "Created empty file: {} (id: {}) in parent {}",
            name, created.id, parent_id
        );
        Ok(created)
    }

    /// Rename an existing file or folder.
    pub async fn rename_file(
        &self,
        file_id: &str,
        new_name: &str,
    ) -> Result<DriveFile, DriveError> {
        let url = format!("{}/drive/v3/files/{file_id}", self.base_url);
        let body = serde_json::json!({
            "name": new_name,
        });

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let body = body.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .patch(&url)
                        .headers(headers)
                        .query(&[("fields", FILE_FIELDS)])
                        .json(&body)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let updated: DriveFile = resp.json().await?;
        info!("Renamed file {} to {}", file_id, new_name);
        Ok(updated)
    }

    /// Move a file to a new parent folder.
    pub async fn move_file(
        &self,
        file_id: &str,
        add_parent: &str,
        remove_parent: &str,
    ) -> Result<DriveFile, DriveError> {
        let url = format!("{}/drive/v3/files/{file_id}", self.base_url);

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .patch(&url)
                        .headers(headers)
                        .query(&[
                            ("addParents", add_parent),
                            ("removeParents", remove_parent),
                            ("fields", FILE_FIELDS),
                        ])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let moved: DriveFile = resp.json().await?;
        info!("Moved file {} to parent {}", file_id, add_parent);
        Ok(moved)
    }

    /// Delete a file. If `permanent` is false, moves it to trash.
    pub async fn delete_file(&self, file_id: &str, permanent: bool) -> Result<(), DriveError> {
        if permanent {
            let url = format!("{}/drive/v3/files/{file_id}", self.base_url);
            self.execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self.http.delete(&url).headers(headers).send().await?;
                    Ok(resp)
                }
            })
            .await?;
            info!("Permanently deleted file {}", file_id);
        } else {
            let url = format!("{}/drive/v3/files/{file_id}", self.base_url);
            let body = serde_json::json!({ "trashed": true });
            self.execute_request(|| {
                let url = url.clone();
                let body = body.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .patch(&url)
                        .headers(headers)
                        .json(&body)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;
            info!("Moved file {} to trash", file_id);
        }

        Ok(())
    }

    /// Upload a file using multipart upload with metadata and content.
    pub async fn upload_file(
        &self,
        name: &str,
        parent_id: Option<&str>,
        mime_type: &str,
        content: Vec<u8>,
    ) -> Result<DriveFile, DriveError> {
        let parent = parent_id.unwrap_or("root");
        let metadata = serde_json::json!({
            "name": name,
            "parents": [parent],
            "mimeType": mime_type,
        });

        let boundary = "-------314159265358979323846";
        let mut body_bytes = Vec::new();

        // Part 1: Metadata
        body_bytes.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n")
                .as_bytes(),
        );
        body_bytes.extend_from_slice(metadata.to_string().as_bytes());
        body_bytes.extend_from_slice(b"\r\n");

        // Part 2: File content
        body_bytes.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: {mime_type}\r\n\r\n").as_bytes(),
        );
        body_bytes.extend_from_slice(&content);
        body_bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let url = format!(
            "{}/drive/v3/files?uploadType=multipart",
            self.upload_base_url
        );

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let body_bytes = body_bytes.clone();
                async move {
                    let mut headers = self.auth_headers().await?;
                    headers.insert(
                        "Content-Type",
                        HeaderValue::from_str(&format!("multipart/related; boundary={boundary}"))
                            .unwrap(),
                    );
                    let resp = self
                        .http
                        .post(&url)
                        .headers(headers)
                        .query(&[("fields", FILE_FIELDS)])
                        .body(body_bytes)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let created: DriveFile = resp.json().await?;
        info!(
            "Uploaded file: {} (id: {}, size: {} bytes)",
            name,
            created.id,
            content.len()
        );
        Ok(created)
    }

    /// Update existing file content on Google Drive.
    pub async fn update_file_content(
        &self,
        file_id: &str,
        content: Vec<u8>,
        mime_type: Option<&str>,
    ) -> Result<DriveFile, DriveError> {
        let content_type = mime_type.unwrap_or("application/octet-stream");
        // Capture length before content is moved into the async closure.
        let content_len = content.len();
        let url = format!(
            "{}/drive/v3/files/{file_id}?uploadType=media",
            self.upload_base_url
        );

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let content = content.clone();
                let ct = content_type;
                async move {
                    let mut headers = self.auth_headers().await?;
                    headers.insert("Content-Type", HeaderValue::from_str(ct).unwrap());
                    let resp = self
                        .http
                        .patch(&url)
                        .headers(headers)
                        .query(&[("fields", FILE_FIELDS)])
                        .body(content)
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let updated: DriveFile = resp.json().await?;
        info!(
            "Updated file content for: {} ({} bytes uploaded)",
            file_id, content_len
        );
        Ok(updated)
    }

    /// Download entire file contents into memory.
    pub async fn download_file(&self, file_id: &str) -> Result<Vec<u8>, DriveError> {
        let url = format!("{}/drive/v3/files/{file_id}", self.base_url);
        let resp = self
            .execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[("alt", "media")])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Download a specific byte range of a file (e.g. `bytes=start-end`).
    /// Essential for lazy streaming in FUSE without downloading whole files.
    pub async fn download_file_range(
        &self,
        file_id: &str,
        start: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveError> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let end = start + length - 1;
        let range_val = format!("bytes={start}-{end}");
        let url = format!("{}/drive/v3/files/{file_id}", self.base_url);

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let range_val = range_val.clone();
                async move {
                    let mut headers = self.auth_headers().await?;
                    headers.insert(
                        RANGE,
                        HeaderValue::from_str(&range_val).map_err(|e| DriveError::Api {
                            code: 400,
                            message: format!("Range header error: {e}"),
                            reason: None,
                        })?,
                    );
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[("alt", "media")])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(DriveError::InvalidRange(start, length));
        }

        let bytes = resp.bytes().await?;
        debug!(
            "Downloaded range {} for file {} ({} bytes)",
            range_val,
            file_id,
            bytes.len()
        );
        Ok(bytes.to_vec())
    }

    /// Retrieve the start page token for the Google Drive Changes API.
    pub async fn get_start_page_token(&self) -> Result<String, DriveError> {
        let url = format!("{}/drive/v3/changes/startPageToken", self.base_url);
        let resp = self
            .execute_request(|| {
                let url = url.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[("supportsAllDrives", "true")])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let token_resp: StartPageTokenResponse = resp.json().await?;
        Ok(token_resp.start_page_token)
    }

    /// List changes from the Google Drive Changes API starting at `page_token`.
    pub async fn list_changes(
        &self,
        page_token: &str,
    ) -> Result<DriveChangesResponse, DriveError> {
        let url = format!("{}/drive/v3/changes", self.base_url);
        let fields = format!(
            "nextPageToken,newStartPageToken,changes(kind,changeType,fileId,removed,time,file({}))",
            FILE_FIELDS
        );
        let page_token = page_token.to_string();

        let resp = self
            .execute_request(|| {
                let url = url.clone();
                let fields = fields.clone();
                let pt = page_token.clone();
                async move {
                    let headers = self.auth_headers().await?;
                    let resp = self
                        .http
                        .get(&url)
                        .headers(headers)
                        .query(&[
                            ("pageToken", pt.as_str()),
                            ("fields", fields.as_str()),
                            ("pageSize", "100"),
                            ("includeItemsFromAllDrives", "true"),
                            ("supportsAllDrives", "true"),
                        ])
                        .send()
                        .await?;
                    Ok(resp)
                }
            })
            .await?;

        let changes: DriveChangesResponse = resp.json().await?;
        Ok(changes)
    }

    /// Execute a request with exponential backoff retry on HTTP 429 and 5xx errors.
    async fn execute_request<F, Fut>(&self, request_fn: F) -> Result<reqwest::Response, DriveError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, DriveError>>,
    {
        let mut retries = 0;
        let max_retries = 3;
        let mut delay = Duration::from_millis(100);

        loop {
            match request_fn().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }

                    // Rate-limited: read Retry-After, then retry or give up
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = resp
                            .headers()
                            .get("Retry-After")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());

                        if retries >= max_retries {
                            return Err(DriveError::RateLimited {
                                retry_after_secs: retry_after,
                            });
                        }
                        let backoff = retry_after.map(Duration::from_secs).unwrap_or(delay);
                        retries += 1;
                        warn!(
                            "Rate limited (429). Waiting {:?} before retry {}/{}",
                            backoff, retries, max_retries
                        );
                        tokio::time::sleep(backoff).await;
                        delay = (delay * 2).max(backoff);
                        continue;
                    }

                    // Transient server errors: retry with exponential backoff
                    if status.is_server_error() {
                        if retries >= max_retries {
                            let err_text = resp.text().await.unwrap_or_default();
                            error!(
                                "Google API server error after {} retries: {}",
                                max_retries, err_text
                            );
                            return Err(DriveError::Api {
                                code: status.as_u16(),
                                message: err_text,
                                reason: None,
                            });
                        }
                        retries += 1;
                        warn!(
                            "Server error {}. Retrying in {:?} (attempt {}/{})",
                            status, delay, retries, max_retries
                        );
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }

                    // All other non-success statuses: fail immediately
                    let err_text = resp.text().await.unwrap_or_default();
                    error!("Google API returned error {}: {}", status, err_text);
                    return Err(DriveError::Api {
                        code: status.as_u16(),
                        message: err_text,
                        reason: None,
                    });
                }
                Err(e) => {
                    if retries >= max_retries {
                        return Err(e);
                    }
                    retries += 1;
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{OAuthConfig, StoredCredentials, TokenStore};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_drive_file_classification() {
        let folder = DriveFile {
            id: "folder_1".to_string(),
            name: "Pictures".to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            size: None,
            modified_time: None,
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };
        assert!(folder.is_folder());
        assert!(!folder.is_google_doc());
        assert_eq!(folder.size_bytes(), 0);
        assert!(!folder.is_trashed());

        let doc = DriveFile {
            id: "doc_1".to_string(),
            name: "Project Plan".to_string(),
            mime_type: "application/vnd.google-apps.document".to_string(),
            size: None,
            modified_time: None,
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };
        assert!(!doc.is_folder());
        assert!(doc.is_google_doc());

        let file = DriveFile {
            id: "file_1".to_string(),
            name: "video.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            size: Some("10485760".to_string()),
            modified_time: None,
            created_time: None,
            parents: Some(vec!["root".to_string()]),
            trashed: Some(false),
            web_view_link: None,
            md5_checksum: None,
            version: None,
        };
        assert!(!file.is_folder());
        assert!(!file.is_google_doc());
        assert_eq!(file.size_bytes(), 10485760);
    }

    #[test]
    fn test_storage_quota_calculation() {
        let quota = StorageQuota {
            limit: Some("16106127360".to_string()), // 15 GB
            usage: Some("8053063680".to_string()),  // 7.5 GB
            usage_in_drive: Some("4000000000".to_string()),
            usage_in_drive_trash: Some("100000000".to_string()),
        };

        assert_eq!(quota.limit_bytes(), Some(16106127360));
        assert_eq!(quota.usage_bytes(), 8053063680);
        let pct = quota.usage_percent().expect("percentage");
        assert!((pct - 50.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_mock_drive_api_operations() {
        // Mock server simulating Google Drive API v3 endpoints
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let port = listener.local_addr().expect("port").port();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                if req.contains("GET /drive/v3/about") {
                    let body = r#"{"user":{"displayName":"Alice","emailAddress":"alice@gmail.com"},"storageQuota":{"limit":"15000000000","usage":"5000000000"}}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("GET /drive/v3/files/file_123?alt=media") {
                    if req.contains("bytes=0-3") {
                        let chunk = b"HELL";
                        let resp = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/11\r\nConnection: close\r\n\r\n");
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.write_all(chunk).await;
                    } else {
                        let full = b"HELLO WORLD";
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 11\r\nConnection: close\r\n\r\n");
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.write_all(full).await;
                    }
                } else if req.contains("GET /drive/v3/files/file_123") {
                    let body = r#"{"id":"file_123","name":"sample.txt","mimeType":"text/plain","size":"11","parents":["root"]}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("GET /drive/v3/files") {
                    let body = r#"{"files":[{"id":"f1","name":"Folder A","mimeType":"application/vnd.google-apps.folder"},{"id":"f2","name":"notes.txt","mimeType":"text/plain","size":"50"}],"nextPageToken":""}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("POST /drive/v3/files") {
                    let body = r#"{"id":"new_folder_id","name":"New Folder","mimeType":"application/vnd.google-apps.folder","parents":["root"]}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("PATCH /drive/v3/files/f1") {
                    let body = r#"{"id":"f1","name":"Renamed Folder","mimeType":"application/vnd.google-apps.folder"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("DELETE /drive/v3/files/f2") {
                    let resp = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("POST /upload/drive/v3/files") {
                    let body = r#"{"id":"uploaded_id","name":"upload.txt","mimeType":"text/plain","size":"13"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let dir = tempdir().expect("tempdir");
        let token_store = TokenStore::new(dir.path().join("creds.json"));
        let creds = StoredCredentials {
            access_token: "mock_valid_token".to_string(),
            refresh_token: "mock_refresh".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: StoredCredentials::now_epoch() + 3600,
            account_email: Some("alice@gmail.com".to_string()),
            account_name: Some("Alice".to_string()),
            client_id: None,
            client_secret: None,
        };
        token_store.save(&creds).expect("save creds");

        let oauth = Arc::new(GoogleOAuthClient::new(OAuthConfig::default(), token_store));
        let client = GoogleDriveClient::new(
            Some(format!("http://127.0.0.1:{port}")),
            Some(format!("http://127.0.0.1:{port}/upload")),
            oauth,
        );

        // 1. Test get_about
        let about = client.get_about().await.expect("about");
        assert_eq!(
            about.user.as_ref().and_then(|u| u.email_address.as_deref()),
            Some("alice@gmail.com")
        );

        // 2. Test list_files
        let list = client
            .list_files(Some("root"), None, Some(10))
            .await
            .expect("list");
        assert_eq!(list.files.len(), 2);
        assert_eq!(list.files[0].name, "Folder A");
        assert!(list.files[0].is_folder());

        // 3. Test get_metadata
        let meta = client.get_metadata("file_123").await.expect("metadata");
        assert_eq!(meta.name, "sample.txt");
        assert_eq!(meta.size_bytes(), 11);

        // 4. Test full download
        let content = client.download_file("file_123").await.expect("download");
        assert_eq!(content, b"HELLO WORLD");

        // 5. Test range download
        let range_chunk = client
            .download_file_range("file_123", 0, 4)
            .await
            .expect("range");
        assert_eq!(range_chunk, b"HELL");

        // 6. Test create_folder
        let folder = client
            .create_folder("New Folder", Some("root"))
            .await
            .expect("create folder");
        assert_eq!(folder.id, "new_folder_id");

        // 7. Test rename_file
        let renamed = client
            .rename_file("f1", "Renamed Folder")
            .await
            .expect("rename");
        assert_eq!(renamed.name, "Renamed Folder");

        // 8. Test delete_file
        client.delete_file("f2", true).await.expect("delete");

        // 9. Test upload_file
        let uploaded = client
            .upload_file(
                "upload.txt",
                Some("root"),
                "text/plain",
                b"Hello upload!".to_vec(),
            )
            .await
            .expect("upload");
        assert_eq!(uploaded.id, "uploaded_id");
    }

    #[test]
    fn test_drive_changes_response_deserialization() {
        let json_data = r#"{
            "kind": "drive#changeList",
            "nextPageToken": "token_next_123",
            "newStartPageToken": "token_start_456",
            "changes": [
                {
                    "kind": "drive#change",
                    "changeType": "file",
                    "fileId": "file_remote_1",
                    "removed": false,
                    "time": "2026-09-04T12:00:00Z",
                    "file": {
                        "id": "file_remote_1",
                        "name": "Report.docx",
                        "mimeType": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                        "size": "45056",
                        "modifiedTime": "2026-09-04T12:00:00Z",
                        "parents": ["root"]
                    }
                },
                {
                    "kind": "drive#change",
                    "changeType": "file",
                    "fileId": "file_remote_2",
                    "removed": true,
                    "time": "2026-09-04T12:01:00Z"
                }
            ]
        }"#;

        let resp: DriveChangesResponse =
            serde_json::from_str(json_data).expect("Changes response should deserialize");
        assert_eq!(resp.next_page_token.as_deref(), Some("token_next_123"));
        assert_eq!(resp.new_start_page_token.as_deref(), Some("token_start_456"));
        assert_eq!(resp.changes.len(), 2);
        assert_eq!(resp.changes[0].file_id.as_deref(), Some("file_remote_1"));
        assert!(!resp.changes[0].removed);
        assert_eq!(resp.changes[0].file.as_ref().unwrap().name, "Report.docx");
        assert!(resp.changes[1].removed);
    }
}
