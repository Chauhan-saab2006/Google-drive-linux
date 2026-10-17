use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

pub mod auth;
pub mod cache;
pub mod config;
pub mod daemon;
pub mod drive;
pub mod filesystem;
pub mod gui;
pub mod sync;

use config::AppConfig;

#[derive(Parser, Debug)]
#[command(
    name = "google-drive-linux",
    author = "Google Drive Linux Contributors",
    version,
    about = "A FUSE-based Google Drive client and desktop manager for Linux",
    long_about = "Google Drive Linux mounts your Google Drive as a virtual filesystem, allowing native desktop file access with lazy on-demand streaming and offline caching."
)]
struct Cli {
    /// Run graphical desktop interface (default if no subcommand is given)
    #[arg(long)]
    gui: bool,

    /// Specify custom configuration file path
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Set logging level (trace, debug, info, warn, error)
    #[arg(short, long, value_name = "LEVEL")]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum Commands {
    /// Authenticate with Google account via browser OAuth2 flow
    Login {
        /// Google OAuth 2.0 Client ID override
        #[arg(long)]
        client_id: Option<String>,
        /// Google OAuth 2.0 Client Secret override
        #[arg(long)]
        client_secret: Option<String>,
        /// Print authorization URL without automatically launching desktop browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke credentials and log out of Google account
    Logout,
    /// Mount Google Drive to the configured mount point
    Mount {
        /// Optional mount directory override
        #[arg(short, long, visible_alias = "mountpoint")]
        target: Option<PathBuf>,
    },
    /// Unmount Google Drive
    Unmount {
        /// Optional mount directory override
        #[arg(short, long, visible_alias = "mountpoint")]
        target: Option<PathBuf>,
    },
    /// Display current account, authentication, mount, and sync status
    Status,
    /// List files and folders in Google Drive
    List {
        /// Parent folder ID (defaults to "root")
        #[arg(short, long)]
        parent: Option<String>,
    },
    /// View or update application configuration
    Settings {
        /// Print active configuration in TOML format
        #[arg(long)]
        show: bool,
    },
    /// Run background daemon service
    Daemon,
    /// Synchronize pending offline writes and poll remote changes
    Sync,
}

fn init_logging(level_name: &str) {
    let level = match level_name.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_target(false)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load configuration
    let mut config = if let Some(ref path) = cli.config {
        AppConfig::load_from_path(path)?
    } else {
        AppConfig::load().unwrap_or_default()
    };

    if let Some(ref lvl) = cli.log_level {
        config.log_level = lvl.clone();
    }

    init_logging(&config.log_level);

    match cli.command {
        Some(Commands::Login {
            client_id,
            client_secret,
            no_browser,
        }) => {
            let cid = client_id
                .or(config.client_id.clone())
                .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
                .unwrap_or_default();
            let csecret = client_secret
                .or(config.client_secret.clone())
                .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok());

            if cid.trim().is_empty() {
                eprintln!("Error: Google Client ID is required to initiate OAuth flow.");
                eprintln!("\nPlease provide your Google Cloud OAuth Client ID by:");
                eprintln!("  1. Passing --client-id <YOUR_CLIENT_ID>");
                eprintln!(
                    "  2. Setting client_id = \"...\" in ~/.config/google-drive-linux/config.toml"
                );
                eprintln!("  3. Setting the GOOGLE_CLIENT_ID environment variable\n");
                eprintln!("Documentation: see docs/GOOGLE_OAUTH.md for setup instructions.");
                std::process::exit(1);
            }

            let creds_path =
                AppConfig::default_credentials_path().map_err(|e| anyhow::anyhow!(e))?;
            let token_store = auth::TokenStore::new(creds_path);
            let oauth_cfg = auth::OAuthConfig {
                client_id: cid,
                client_secret: csecret,
                ..auth::OAuthConfig::default()
            };
            let oauth_client = auth::GoogleOAuthClient::new(oauth_cfg, token_store);

            println!("Starting Google authorization flow...");
            match oauth_client.start_browser_flow(!no_browser).await {
                Ok(creds) => {
                    println!("\nAuthentication successful!");
                    if let Some(ref email) = creds.account_email {
                        println!("Google Account: {email}");
                    }
                    if let Some(ref name) = creds.account_name {
                        println!("Account Name:   {name}");
                    }
                    println!(
                        "Stored in:      {:?}",
                        AppConfig::default_credentials_path().ok()
                    );
                }
                Err(e) => {
                    eprintln!("\nAuthentication failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Logout) => {
            let creds_path =
                AppConfig::default_credentials_path().map_err(|e| anyhow::anyhow!(e))?;
            let token_store = auth::TokenStore::new(creds_path);
            let oauth_cfg = auth::OAuthConfig::default();
            let oauth_client = auth::GoogleOAuthClient::new(oauth_cfg, token_store);

            info!("Revoking credentials and logging out...");
            oauth_client.logout().await?;
            println!("Logged out successfully. Saved credentials have been cleared.");
        }
        Some(Commands::Mount { target }) => {
            let mount_path = target.unwrap_or(config.mount_point.clone());

            if filesystem::is_path_mounted(&mount_path) {
                println!("Google Drive is already mounted at: {:?}", mount_path);
                return Ok(());
            }

            let creds_path =
                AppConfig::default_credentials_path().map_err(|e| anyhow::anyhow!(e))?;
            let token_store = auth::TokenStore::new(creds_path);
            let creds = match token_store.load() {
                Ok(Some(c)) if !c.is_expired() || !c.refresh_token.is_empty() => c,
                _ => {
                    eprintln!("Error: Authentication required before mounting Google Drive.");
                    eprintln!("\nPlease sign in with your Google account:");
                    eprintln!("  google-drive-linux login\n");
                    std::process::exit(1);
                }
            };

            let oauth_cfg = auth::OAuthConfig {
                client_id: config
                    .client_id
                    .clone()
                    .or_else(|| creds.client_id.clone())
                    .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
                    .unwrap_or_default(),
                client_secret: config
                    .client_secret
                    .clone()
                    .or_else(|| creds.client_secret.clone())
                    .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok()),
                ..auth::OAuthConfig::default()
            };
            let oauth = std::sync::Arc::new(auth::GoogleOAuthClient::new(
                oauth_cfg,
                token_store,
            ));
            let drive_client =
                std::sync::Arc::new(drive::GoogleDriveClient::new(None, None, oauth));

            // Open metadata + chunk cache
            let meta_cache = match cache::open_default_cache(config.cache_size_mb) {
                Ok(c) => {
                    info!("Metadata cache opened (max {} MB)", config.cache_size_mb);
                    Some(std::sync::Arc::new(std::sync::Mutex::new(c)))
                }
                Err(e) => {
                    eprintln!(
                        "Warning: failed to open metadata cache: {e}. Running without cache."
                    );
                    None
                }
            };

            println!("Mounting Google Drive at {:?}...", mount_path);
            let active_mount = filesystem::mount_filesystem(
                &mount_path,
                drive_client,
                tokio::runtime::Handle::current(),
                meta_cache,
            )
            .map_err(|e| anyhow::anyhow!(e))?;

            println!("\n✓ Google Drive successfully mounted at: {:?}", mount_path);
            if let Some(email) = creds.account_email {
                println!("  Connected Account: {}", email);
            }
            println!(
                "\nYou can now read, write, create, rename, and delete files using your file manager or terminal:"
            );
            println!("  nautilus {:?} &", mount_path);
            println!("  ls -la {:?}", mount_path);
            println!("  cp /etc/hosts {:?}/hosts_test.txt", mount_path);
            println!("\nPress Ctrl+C to unmount safely, or run 'google-drive-linux unmount' in another terminal.\n");

            // Wait for Ctrl+C to unmount gracefully
            tokio::signal::ctrl_c().await?;
            println!("\nReceived shutdown signal. Unmounting FUSE filesystem...");
            active_mount.unmount().map_err(|e| anyhow::anyhow!(e))?;
            println!("✓ Google Drive safely unmounted.");
        }
        Some(Commands::Unmount { target }) => {
            let mount_path = target.unwrap_or(config.mount_point);
            if !filesystem::is_path_mounted(&mount_path) {
                println!("Google Drive is not mounted at {:?}", mount_path);
                return Ok(());
            }

            info!("Unmounting Google Drive from {:?}", mount_path);
            filesystem::unmount_path(&mount_path).map_err(|e| anyhow::anyhow!(e))?;
            println!(
                "✓ Google Drive successfully unmounted from {:?}",
                mount_path
            );
        }
        Some(Commands::Status) => {
            let creds_path =
                AppConfig::default_credentials_path().map_err(|e| anyhow::anyhow!(e))?;
            let token_store = auth::TokenStore::new(creds_path);
            let state = match token_store.load() {
                Ok(Some(creds)) => {
                    let now = auth::StoredCredentials::now_epoch();
                    let (auth_str, expires_str) = if creds.is_expired() {
                        ("Expired", "Expired".to_string())
                    } else {
                        let remaining_mins = (creds.expires_at.saturating_sub(now)) / 60;
                        ("Connected", format!("{remaining_mins} minutes remaining"))
                    };
                    let email = creds
                        .account_email
                        .unwrap_or_else(|| "Connected Account".to_string());
                    (email, auth_str, expires_str)
                }
                Ok(None) => (
                    "<not connected>".to_string(),
                    "Disconnected",
                    "N/A".to_string(),
                ),
                Err(_) => (
                    "<error reading credentials>".to_string(),
                    "Error",
                    "N/A".to_string(),
                ),
            };

            println!("Google Account: {}", state.0);
            println!("Authentication: {}", state.1);
            if state.1 == "Connected" {
                println!("Token Expiry:   {}", state.2);

                // Fetch storage quota info
                let oauth = std::sync::Arc::new(auth::GoogleOAuthClient::new(
                    auth::OAuthConfig::default(),
                    token_store,
                ));
                let drive_client = drive::GoogleDriveClient::new(None, None, oauth);
                if let Ok(about) = drive_client.get_about().await {
                    if let Some(quota) = about.storage_quota {
                        let used_gb = quota.usage_bytes() as f64 / (1024.0 * 1024.0 * 1024.0);
                        if let Some(limit_bytes) = quota.limit_bytes() {
                            let limit_gb = limit_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                            let pct = quota.usage_percent().unwrap_or(0.0);
                            println!("Storage Quota:  {used_gb:.2} GB / {limit_gb:.2} GB ({pct:.1}% used)");
                        } else {
                            println!("Storage Quota:  {used_gb:.2} GB used (Unlimited)");
                        }
                    }
                }
            }

            let is_mounted = filesystem::is_path_mounted(&config.mount_point);
            println!("Mount:          {}", config.mount_point.display());
            println!(
                "Status:         {}",
                if is_mounted { "Mounted" } else { "Unmounted" }
            );
            println!("Network:        Online");
        }
        Some(Commands::List { parent }) => {
            let creds_path =
                AppConfig::default_credentials_path().map_err(|e| anyhow::anyhow!(e))?;
            let token_store = auth::TokenStore::new(creds_path);
            let oauth = std::sync::Arc::new(auth::GoogleOAuthClient::new(
                auth::OAuthConfig::default(),
                token_store,
            ));
            let drive_client = drive::GoogleDriveClient::new(None, None, oauth);

            let parent_id = parent.as_deref().unwrap_or("root");
            println!("Listing files in Google Drive folder: '{parent_id}'...");

            match drive_client
                .list_files(Some(parent_id), None, Some(50))
                .await
            {
                Ok(resp) => {
                    println!("\nFound {} items:", resp.files.len());
                    for file in resp.files {
                        let kind = if file.is_folder() { "[DIR]" } else { "[FILE]" };
                        let size_str = if file.is_folder() {
                            "-".to_string()
                        } else {
                            format!("{} B", file.size_bytes())
                        };
                        println!(
                            "  {kind:<7} {:<36} {:<12} (ID: {})",
                            file.name, size_str, file.id
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Failed to list files: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Settings { show }) => {
            if show {
                let serialized = toml::to_string_pretty(&config)?;
                println!("{serialized}");
            } else {
                println!(
                    "Configuration file path: {:?}",
                    AppConfig::default_config_path().ok()
                );
                println!("Mount point: {:?}", config.mount_point);
                println!("Cache directory: {:?}", config.cache_dir);
                println!("Auto-mount: {}", config.auto_mount);
                println!("Auto-start: {}", config.auto_start);
            }
        }
        Some(Commands::Daemon) => {
            info!("Starting background daemon service...");
            let daemon = daemon::DaemonService::new(config);
            daemon.run().await.map_err(|e| anyhow::anyhow!(e))?;
        }
        Some(Commands::Sync) => {
            let creds_path =
                AppConfig::default_credentials_path().map_err(|e| anyhow::anyhow!(e))?;
            let token_store = auth::TokenStore::new(creds_path);
            let oauth = std::sync::Arc::new(auth::GoogleOAuthClient::new(
                auth::OAuthConfig::default(),
                token_store,
            ));
            let drive_client =
                std::sync::Arc::new(drive::GoogleDriveClient::new(None, None, oauth));
            let meta_cache = cache::open_default_cache(config.cache_size_mb)
                .ok()
                .map(|c| std::sync::Arc::new(std::sync::Mutex::new(c)));
            let queue_path = dirs::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join("google-drive-linux")
                .join("offline_queue.json");
            let offline_queue = std::sync::Arc::new(sync::OfflineQueue::new(queue_path));
            let sync_engine = sync::SyncEngine::new(
                drive_client,
                meta_cache,
                offline_queue,
                config.sync_interval_secs,
            );

            println!("Synchronizing with Google Drive...");
            let flushed = sync_engine
                .flush_offline_queue()
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("Flushed {flushed} offline write(s).");
            let polled = sync_engine
                .poll_remote_changes()
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            println!("Polled {polled} remote change(s).");
            println!("Sync complete.");
        }
        None => {
            // Launch GUI by default
            info!("Launching Google Drive Linux desktop interface...");
            gui::run_app(config).map_err(|e| anyhow::anyhow!(e))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing_status() {
        let cli = Cli::parse_from(["google-drive-linux", "status"]);
        assert_eq!(cli.command, Some(Commands::Status));
    }

    #[test]
    fn test_cli_parsing_mount() {
        let cli = Cli::parse_from(["google-drive-linux", "mount", "--target", "/tmp/mnt"]);
        assert_eq!(
            cli.command,
            Some(Commands::Mount {
                target: Some(PathBuf::from("/tmp/mnt")),
            })
        );
    }

    #[test]
    fn test_cli_parsing_default_gui() {
        let cli = Cli::parse_from(["google-drive-linux"]);
        assert_eq!(cli.command, None);
    }

    #[test]
    fn test_cli_parsing_login() {
        let cli = Cli::parse_from([
            "google-drive-linux",
            "login",
            "--client-id",
            "test_id",
            "--no-browser",
        ]);
        assert_eq!(
            cli.command,
            Some(Commands::Login {
                client_id: Some("test_id".to_string()),
                client_secret: None,
                no_browser: true,
            })
        );
    }

    #[test]
    fn test_cli_parsing_list() {
        let cli = Cli::parse_from(["google-drive-linux", "list", "--parent", "folder_xyz"]);
        assert_eq!(
            cli.command,
            Some(Commands::List {
                parent: Some("folder_xyz".to_string()),
            })
        );
    }

    #[test]
    fn test_cli_parsing_sync() {
        let cli = Cli::parse_from(["google-drive-linux", "sync"]);
        assert_eq!(cli.command, Some(Commands::Sync));
    }

    #[test]
    fn test_cli_parsing_daemon() {
        let cli = Cli::parse_from(["google-drive-linux", "daemon"]);
        assert_eq!(cli.command, Some(Commands::Daemon));
    }
}
