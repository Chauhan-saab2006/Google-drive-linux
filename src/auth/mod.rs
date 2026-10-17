use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use url::Url;

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Configuration or system directory error: {0}")]
    Config(String),
    #[error("IO error during token storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("Network error during OAuth request: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("OAuth callback error: {0}")]
    CallbackError(String),
    #[error("OAuth state mismatch (possible CSRF attack)")]
    StateMismatch,
    #[error("Missing authorization code in OAuth callback")]
    MissingCode,
    #[error("Google API authentication error: {0}")]
    ApiError(String),
    #[error("No stored credentials found")]
    NotAuthenticated,
    #[error("Timeout waiting for OAuth browser authorization")]
    Timeout,
}

/// Represents the status of the Google user session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AuthState {
    #[default]
    Unauthenticated,
    Authenticating,
    Authenticated(AccountInfo),
    Expired,
    Error(String),
}

/// Metadata about the connected Google account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub email: String,
    pub display_name: Option<String>,
    pub storage_quota_bytes: Option<u64>,
    pub storage_used_bytes: Option<u64>,
}

/// Persistent OAuth 2.0 credentials stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_at: u64, // Unix epoch seconds
    pub account_email: Option<String>,
    pub account_name: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

impl StoredCredentials {
    /// Return current system timestamp in epoch seconds.
    pub fn now_epoch() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Returns true if the access token is already expired.
    pub fn is_expired(&self) -> bool {
        Self::now_epoch() >= self.expires_at
    }

    /// Returns true if the token is expired or within 5 minutes of expiring.
    pub fn needs_refresh(&self) -> bool {
        Self::now_epoch() + 300 >= self.expires_at
    }
}

/// PKCE (Proof Key for Code Exchange) pair according to RFC 7636.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    pub code_verifier: String,
    pub code_challenge: String,
}

impl Default for PkcePair {
    fn default() -> Self {
        Self::new()
    }
}

impl PkcePair {
    /// Generate a fresh random code verifier and its S256 code challenge.
    pub fn new() -> Self {
        let mut bytes = [0u8; 48];
        for b in &mut bytes {
            *b = fastrand::u8(..);
        }
        let code_verifier = URL_SAFE_NO_PAD.encode(bytes);
        let code_challenge = Self::challenge_from_verifier(&code_verifier);
        Self {
            code_verifier,
            code_challenge,
        }
    }

    /// Derive SHA-256 base64url challenge from a given verifier string.
    pub fn challenge_from_verifier(verifier: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        URL_SAFE_NO_PAD.encode(hash)
    }
}

/// Secure token persistence manager.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Save credentials with strict Unix 0600 permissions.
    pub fn save(&self, creds: &StoredCredentials) -> Result<(), AuthError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                let perms = fs::Permissions::from_mode(0o700);
                let _ = fs::set_permissions(parent, perms);
            }
        }

        let json = serde_json::to_string_pretty(creds)?;
        fs::write(&self.path, json)?;

        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&self.path, perms)?;
        }

        Ok(())
    }

    /// Load stored credentials from disk.
    pub fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&self.path)?;
        let creds: StoredCredentials = serde_json::from_str(&content)?;
        Ok(Some(creds))
    }

    /// Purge stored credentials upon logout.
    pub fn clear(&self) -> Result<(), AuthError> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Response payload from Google token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
}

/// User info payload from Google OAuth2 API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GoogleUserInfo {
    pub email: Option<String>,
    pub name: Option<String>,
}

/// Client configuration for Google OAuth2 endpoints.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub auth_endpoint: String,
    pub token_endpoint: String,
    pub revoke_endpoint: String,
    pub userinfo_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            auth_endpoint: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_endpoint: "https://oauth2.googleapis.com/token".to_string(),
            revoke_endpoint: "https://oauth2.googleapis.com/revoke".to_string(),
            userinfo_endpoint: "https://www.googleapis.com/oauth2/v2/userinfo".to_string(),
            client_id: String::new(),
            client_secret: None,
        }
    }
}

/// Google OAuth 2.0 manager with PKCE support and loopback callback handling.
pub struct GoogleOAuthClient {
    config: OAuthConfig,
    http: Client,
    token_store: TokenStore,
    cached_creds: std::sync::RwLock<Option<StoredCredentials>>,
}

impl GoogleOAuthClient {
    pub fn new(mut config: OAuthConfig, token_store: TokenStore) -> Self {
        let initial_creds = token_store.load().ok().flatten();
        if config.client_id.trim().is_empty() {
            if let Some(ref c) = initial_creds {
                if let Some(ref cid) = c.client_id {
                    config.client_id = cid.clone();
                }
                if config.client_secret.is_none() && c.client_secret.is_some() {
                    config.client_secret = c.client_secret.clone();
                }
            }
        }
        Self {
            config,
            http: Client::new(),
            token_store,
            cached_creds: std::sync::RwLock::new(initial_creds),
        }
    }

    pub fn token_store(&self) -> &TokenStore {
        &self.token_store
    }

    /// Return current authentication state.
    pub fn current_state(&self) -> AuthState {
        let creds_opt = if let Ok(guard) = self.cached_creds.read() {
            guard
                .clone()
                .or_else(|| self.token_store.load().ok().flatten())
        } else {
            self.token_store.load().ok().flatten()
        };

        match creds_opt {
            Some(creds) => {
                if creds.is_expired() && creds.refresh_token.is_empty() {
                    AuthState::Expired
                } else {
                    AuthState::Authenticated(AccountInfo {
                        email: creds
                            .account_email
                            .unwrap_or_else(|| "Google User".to_string()),
                        display_name: creds.account_name,
                        storage_quota_bytes: None,
                        storage_used_bytes: None,
                    })
                }
            }
            None => AuthState::Unauthenticated,
        }
    }

    /// Retrieve a valid access token, automatically refreshing if expired or expiring soon.
    pub async fn get_valid_access_token(&self) -> Result<String, AuthError> {
        // Fast path: check in-memory cache without hitting disk
        if let Ok(guard) = self.cached_creds.read() {
            if let Some(ref creds) = *guard {
                if !creds.needs_refresh() {
                    return Ok(creds.access_token.clone());
                }
            }
        }

        // Slow path: load or refresh from token store
        let mut creds = self
            .token_store
            .load()?
            .ok_or(AuthError::NotAuthenticated)?;

        if creds.needs_refresh() {
            info!("Access token expired or expiring soon; refreshing...");
            self.refresh_access_token(&mut creds).await?;
            self.token_store.save(&creds)?;
        }

        let token = creds.access_token.clone();
        if let Ok(mut guard) = self.cached_creds.write() {
            *guard = Some(creds);
        }

        Ok(token)
    }

    /// Execute the full OAuth 2.0 PKCE browser authorization flow with loopback listener.
    pub async fn start_browser_flow(
        &self,
        open_browser: bool,
    ) -> Result<StoredCredentials, AuthError> {
        if self.config.client_id.trim().is_empty() {
            return Err(AuthError::Config(
                "Google Client ID is not configured. Please set client_id in config.toml or via GOOGLE_CLIENT_ID environment variable.".to_string(),
            ));
        }

        // Bind loopback TCP listener on random available port
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let pkce = PkcePair::new();
        let mut state_bytes = [0u8; 16];
        for b in &mut state_bytes {
            *b = fastrand::u8(..);
        }
        let state = URL_SAFE_NO_PAD.encode(state_bytes);

        // Construct authorization URL
        let mut auth_url =
            Url::parse(&self.config.auth_endpoint).map_err(|e| AuthError::Config(e.to_string()))?;
        auth_url
            .query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair(
                "scope",
                "https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile",
            )
            .append_pair("code_challenge", &pkce.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");

        let auth_url_str = auth_url.to_string();
        info!("OAuth authorization URL generated.");

        if open_browser {
            // Attempt to launch desktop browser via xdg-open
            let _ = std::process::Command::new("xdg-open")
                .arg(&auth_url_str)
                .spawn();
        }

        println!("\nAuthorize Google Drive Linux by visiting:\n\n{auth_url_str}\n");
        println!("Waiting for browser authorization callback...");

        // Wait for incoming HTTP request with timeout (120 seconds)
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(120), listener.accept())
                .await
                .map_err(|_| AuthError::Timeout)??;

        let mut buffer = [0u8; 2048];
        let bytes_read = socket.read(&mut buffer).await?;
        let request_str = String::from_utf8_lossy(&buffer[..bytes_read]);

        // Parse HTTP request line e.g. "GET /callback?code=...&state=... HTTP/1.1"
        let first_line = request_str.lines().next().unwrap_or_default();
        let query_part = first_line.split_whitespace().nth(1).unwrap_or_default();

        let parsed_url = Url::parse(&format!("http://localhost{query_part}"))
            .map_err(|e| AuthError::CallbackError(e.to_string()))?;

        let mut received_code = None;
        let mut received_state = None;

        for (k, v) in parsed_url.query_pairs() {
            if k == "code" {
                received_code = Some(v.to_string());
            } else if k == "state" {
                received_state = Some(v.to_string());
            }
        }

        // Send HTTP response to the browser
        let html_body = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Google Drive Linux — Connected</title>
    <style>
        body { font-family: system-ui, -apple-system, sans-serif; text-align: center; padding: 60px 20px; background: #f8f9fa; color: #202124; }
        .card { max-width: 480px; margin: 0 auto; background: white; border-radius: 12px; padding: 40px; box-shadow: 0 4px 12px rgba(0,0,0,0.08); }
        h1 { color: #1a73e8; font-size: 24px; margin-bottom: 12px; }
        p { color: #5f6368; font-size: 16px; line-height: 1.5; }
    </style>
</head>
<body>
    <div class="card">
        <h1>Authorization Successful!</h1>
        <p>Google Drive Linux is now connected to your Google account.<br>You may safely close this browser window and return to the application.</p>
    </div>
</body>
</html>"#;

        let http_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html_body.len(),
            html_body
        );

        let _ = socket.write_all(http_response.as_bytes()).await;
        let _ = socket.flush().await;

        let code = received_code.ok_or(AuthError::MissingCode)?;
        let state_val = received_state.unwrap_or_default();

        if state_val != state {
            return Err(AuthError::StateMismatch);
        }

        // Exchange authorization code for tokens
        info!("Exchanging authorization code with Google token endpoint...");
        let creds = self
            .exchange_code_for_tokens(&code, &pkce.code_verifier, &redirect_uri)
            .await?;

        // Save credentials to secure disk store
        self.token_store.save(&creds)?;
        if let Ok(mut guard) = self.cached_creds.write() {
            *guard = Some(creds.clone());
        }
        info!("OAuth credentials saved successfully.");

        Ok(creds)
    }

    /// Exchange authorization code with token endpoint.
    pub async fn exchange_code_for_tokens(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<StoredCredentials, AuthError> {
        let mut params = vec![
            ("code", code),
            ("client_id", &self.config.client_id),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", code_verifier),
        ];

        let secret_ref;
        if let Some(ref sec) = self.config.client_secret {
            secret_ref = sec.as_str();
            params.push(("client_secret", secret_ref));
        }

        let resp = self
            .http
            .post(&self.config.token_endpoint)
            .form(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            error!("Google token exchange failed: {}", err_text);
            return Err(AuthError::ApiError(format!(
                "Token exchange failed: {err_text}"
            )));
        }

        let token_data: TokenResponse = resp.json().await?;
        let expires_at = StoredCredentials::now_epoch() + token_data.expires_in;

        // Fetch user information to capture email and name
        let user_info = self.fetch_user_info(&token_data.access_token).await.ok();

        let creds = StoredCredentials {
            access_token: token_data.access_token,
            refresh_token: token_data.refresh_token.unwrap_or_default(),
            token_type: token_data.token_type,
            expires_at,
            account_email: user_info.as_ref().and_then(|u| u.email.clone()),
            account_name: user_info.as_ref().and_then(|u| u.name.clone()),
            client_id: Some(self.config.client_id.clone()),
            client_secret: self.config.client_secret.clone(),
        };

        Ok(creds)
    }

    /// Fetch profile user info using bearer access token.
    pub async fn fetch_user_info(&self, access_token: &str) -> Result<GoogleUserInfo, AuthError> {
        let resp = self
            .http
            .get(&self.config.userinfo_endpoint)
            .bearer_auth(access_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(AuthError::ApiError(
                "Failed to fetch Google user profile".to_string(),
            ));
        }

        let info: GoogleUserInfo = resp.json().await?;
        Ok(info)
    }

    /// Refresh access token using stored refresh token.
    pub async fn refresh_access_token(
        &self,
        creds: &mut StoredCredentials,
    ) -> Result<(), AuthError> {
        if creds.refresh_token.is_empty() {
            return Err(AuthError::ApiError(
                "No refresh token available to refresh session".to_string(),
            ));
        }

        let cid = if !self.config.client_id.trim().is_empty() {
            self.config.client_id.as_str()
        } else if let Some(ref c_id) = creds.client_id {
            c_id.as_str()
        } else {
            ""
        };

        if cid.trim().is_empty() {
            return Err(AuthError::Config(
                "Google Client ID is missing. Cannot refresh access token without client_id."
                    .to_string(),
            ));
        }

        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", creds.refresh_token.as_str()),
            ("client_id", cid),
        ];

        let secret_ref = self
            .config
            .client_secret
            .as_deref()
            .or(creds.client_secret.as_deref());

        if let Some(sec) = secret_ref {
            params.push(("client_secret", sec));
        }

        let resp = self
            .http
            .post(&self.config.token_endpoint)
            .form(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            warn!("Token refresh request rejected: {}", err_text);
            return Err(AuthError::ApiError(format!(
                "Token refresh rejected: {err_text}"
            )));
        }

        let token_data: TokenResponse = resp.json().await?;
        creds.access_token = token_data.access_token;
        creds.expires_at = StoredCredentials::now_epoch() + token_data.expires_in;
        if let Some(new_rt) = token_data.refresh_token {
            creds.refresh_token = new_rt;
        }
        if creds.client_id.is_none() && !cid.is_empty() {
            creds.client_id = Some(cid.to_string());
        }
        if creds.client_secret.is_none() && secret_ref.is_some() {
            creds.client_secret = secret_ref.map(str::to_string);
        }

        Ok(())
    }

    /// Revoke token remotely with Google revocation endpoint and clear local storage.
    pub async fn logout(&self) -> Result<(), AuthError> {
        if let Ok(Some(creds)) = self.token_store.load() {
            let token_to_revoke = if !creds.refresh_token.is_empty() {
                &creds.refresh_token
            } else {
                &creds.access_token
            };

            let _ = self
                .http
                .post(&self.config.revoke_endpoint)
                .query(&[("token", token_to_revoke)])
                .send()
                .await;
        }

        self.token_store.clear()?;
        if let Ok(mut guard) = self.cached_creds.write() {
            *guard = None;
        }
        info!("Logged out and cleared stored credentials.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_pkce_generation() {
        let pkce = PkcePair::new();
        assert!(pkce.code_verifier.len() >= 43);
        assert!(pkce.code_verifier.len() <= 128);
        assert!(!pkce.code_challenge.is_empty());

        // Verify deterministic challenge generation
        let challenge2 = PkcePair::challenge_from_verifier(&pkce.code_verifier);
        assert_eq!(pkce.code_challenge, challenge2);
    }

    #[test]
    fn test_pkce_known_vector() {
        // RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let challenge = PkcePair::challenge_from_verifier(verifier);
        assert_eq!(challenge, expected_challenge);
    }

    #[test]
    fn test_token_expiration_logic() {
        let now = StoredCredentials::now_epoch();
        let creds_active = StoredCredentials {
            access_token: "active_token".to_string(),
            refresh_token: "refresh".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: now + 3600,
            account_email: Some("user@gmail.com".to_string()),
            account_name: Some("User Name".to_string()),
            client_id: None,
            client_secret: None,
        };
        assert!(!creds_active.is_expired());
        assert!(!creds_active.needs_refresh());

        let creds_soon = StoredCredentials {
            expires_at: now + 200, // within 300s window
            ..creds_active.clone()
        };
        assert!(!creds_soon.is_expired());
        assert!(creds_soon.needs_refresh());

        let creds_expired = StoredCredentials {
            expires_at: now - 10,
            ..creds_active
        };
        assert!(creds_expired.is_expired());
        assert!(creds_expired.needs_refresh());
    }

    #[test]
    fn test_token_store_save_load_permissions() {
        let dir = tempdir().expect("tempdir");
        let token_file = dir.path().join("sub/credentials.json");
        let store = TokenStore::new(token_file.clone());

        let creds = StoredCredentials {
            access_token: "tok_123".to_string(),
            refresh_token: "ref_123".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: 1800000000,
            account_email: Some("test@example.com".to_string()),
            account_name: Some("Test User".to_string()),
            client_id: Some("client1".to_string()),
            client_secret: None,
        };

        store.save(&creds).expect("save should succeed");

        #[cfg(unix)]
        {
            let meta = fs::metadata(&token_file).expect("metadata");
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "Token file permissions must be 0600");
        }

        let loaded = store.load().expect("load").expect("must exist");
        assert_eq!(creds, loaded);

        store.clear().expect("clear");
        assert!(!token_file.exists());
        assert_eq!(store.load().expect("load"), None);
    }

    #[tokio::test]
    async fn test_mock_token_exchange_and_refresh() {
        // Spin up a mock HTTP server to simulate Google OAuth endpoints
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let mock_port = listener.local_addr().expect("port").port();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                if req.contains("grant_type=authorization_code") {
                    let body = r#"{"access_token":"mock_access_token_1","refresh_token":"mock_refresh_token_1","expires_in":3600,"token_type":"Bearer"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("grant_type=refresh_token") {
                    let body = r#"{"access_token":"mock_access_token_refreshed","expires_in":3600,"token_type":"Bearer"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("GET /userinfo") {
                    let body = r#"{"email":"mockuser@gmail.com","name":"Mock User"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if req.contains("POST /revoke") {
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let dir = tempdir().expect("tempdir");
        let store = TokenStore::new(dir.path().join("creds.json"));

        let config = OAuthConfig {
            auth_endpoint: format!("http://127.0.0.1:{mock_port}/auth"),
            token_endpoint: format!("http://127.0.0.1:{mock_port}/token"),
            revoke_endpoint: format!("http://127.0.0.1:{mock_port}/revoke"),
            userinfo_endpoint: format!("http://127.0.0.1:{mock_port}/userinfo"),
            client_id: "test_client_id".to_string(),
            client_secret: None,
        };

        let client = GoogleOAuthClient::new(config, store);

        // 1. Test code exchange
        let mut creds = client
            .exchange_code_for_tokens("test_code", "test_verifier", "http://127.0.0.1/callback")
            .await
            .expect("exchange should succeed");

        assert_eq!(creds.access_token, "mock_access_token_1");
        assert_eq!(creds.refresh_token, "mock_refresh_token_1");
        assert_eq!(creds.account_email.as_deref(), Some("mockuser@gmail.com"));

        // 2. Test token refresh
        client
            .refresh_access_token(&mut creds)
            .await
            .expect("refresh should succeed");
        assert_eq!(creds.access_token, "mock_access_token_refreshed");

        // 3. Test logout & revocation
        client.token_store().save(&creds).expect("save");
        client.logout().await.expect("logout should succeed");
        assert_eq!(client.token_store().load().expect("load"), None);
    }

    #[tokio::test]
    async fn test_token_refresh_uses_stored_client_id_when_config_empty() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let mock_port = listener.local_addr().expect("port").port();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                if req.contains("grant_type=refresh_token")
                    && req.contains("client_id=stored_client_xyz")
                {
                    let body = r#"{"access_token":"refreshed_from_stored_id","expires_in":3600,"token_type":"Bearer"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let dir = tempdir().expect("tempdir");
        let store = TokenStore::new(dir.path().join("creds.json"));

        let mut stored_creds = StoredCredentials {
            access_token: "expired_token".to_string(),
            refresh_token: "mock_refresh_token".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: 0,
            account_email: Some("test@gmail.com".to_string()),
            account_name: None,
            client_id: Some("stored_client_xyz".to_string()),
            client_secret: None,
        };
        store.save(&stored_creds).expect("save creds");

        // Pass OAuthConfig with EMPTY client_id
        let config = OAuthConfig {
            token_endpoint: format!("http://127.0.0.1:{mock_port}/token"),
            client_id: String::new(),
            ..OAuthConfig::default()
        };

        let client = GoogleOAuthClient::new(config, store);
        client
            .refresh_access_token(&mut stored_creds)
            .await
            .expect("should refresh using stored client_id");

        assert_eq!(stored_creds.access_token, "refreshed_from_stored_id");
    }
}
