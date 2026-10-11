use crate::auth::{GoogleOAuthClient, OAuthConfig, TokenStore};
use crate::config::AppConfig;
use crate::filesystem;
use adw::glib;
use adw::prelude::*;
use std::rc::Rc;
use std::sync::Arc;

const APP_ID: &str = "com.google_drive_linux.App";

/// Launch the GTK4 / Libadwaita desktop user interface.
pub fn run_app(config: AppConfig) -> Result<(), String> {
    let app = adw::Application::builder().application_id(APP_ID).build();

    let config_clone = config.clone();
    app.connect_activate(move |application| {
        build_ui(application, &config_clone);
    });

    // Run the application with empty arguments so CLI subcommands aren't parsed by GTK
    let empty_args: [&str; 0] = [];
    app.run_with_args(&empty_args);
    Ok(())
}

fn build_ui(app: &adw::Application, config: &AppConfig) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Google Drive Linux")
        .default_width(680)
        .default_height(580)
        .build();

    let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    // Modern Libadwaita Header Bar
    let header = adw::HeaderBar::new();
    content_box.append(&header);

    // Scrollable preferences page container
    let preferences_page = adw::PreferencesPage::new();

    // 1. Account Section
    let account_group = adw::PreferencesGroup::builder()
        .title("Google Account")
        .build();

    // Inspect stored credentials
    let (initial_account_title, initial_account_subtitle, initial_btn_label, is_logged_in) =
        match AppConfig::default_credentials_path()
            .ok()
            .map(TokenStore::new)
            .and_then(|store| store.load().ok().flatten())
        {
            Some(creds) => {
                let email = creds
                    .account_email
                    .unwrap_or_else(|| "Connected Account".to_string());
                (email, "Connected to Google Drive", "Disconnect", true)
            }
            None => (
                "No account connected".to_string(),
                "Sign in to authorize access to your Google Drive",
                "Sign in with Google",
                false,
            ),
        };

    let account_row = adw::ActionRow::builder()
        .title(&initial_account_title)
        .subtitle(initial_account_subtitle)
        .build();

    let auth_btn = gtk4::Button::builder()
        .label(initial_btn_label)
        .valign(gtk4::Align::Center)
        .build();

    if is_logged_in {
        auth_btn.add_css_class("destructive-action");
    } else {
        auth_btn.add_css_class("suggested-action");
    }

    account_row.add_suffix(&auth_btn);
    account_group.add(&account_row);
    preferences_page.add(&account_group);

    // 2. Status & Sync Section
    let status_group = adw::PreferencesGroup::builder()
        .title("Filesystem Status")
        .build();

    let is_mounted = filesystem::is_path_mounted(&config.mount_point);

    let status_subtitle = if is_mounted {
        format!("Mounted at {}", config.mount_point.display())
    } else if is_logged_in {
        "Connected (Unmounted)".to_string()
    } else {
        "Offline (Sign in required)".to_string()
    };

    let status_row = adw::ActionRow::builder()
        .title("Service State")
        .subtitle(&status_subtitle)
        .build();
    status_group.add(&status_row);
    preferences_page.add(&status_group);

    // 3. Virtual Disk / Mount Section
    let mount_group = adw::PreferencesGroup::builder()
        .title("Virtual Disk")
        .build();

    let mount_point_str = config.mount_point.to_string_lossy().to_string();
    let mount_row_subtitle = if is_mounted {
        format!("{mount_point_str} (Mounted)")
    } else {
        format!("{mount_point_str} (Unmounted)")
    };

    let mount_row = adw::ActionRow::builder()
        .title("Mount Location")
        .subtitle(&mount_row_subtitle)
        .build();

    let mount_btn = gtk4::Button::builder()
        .label(if is_mounted { "Unmount" } else { "Mount" })
        .valign(gtk4::Align::Center)
        .build();

    if is_mounted {
        mount_btn.add_css_class("destructive-action");
    } else {
        mount_btn.add_css_class("suggested-action");
    }

    mount_row.add_suffix(&mount_btn);
    mount_group.add(&mount_row);
    preferences_page.add(&mount_group);

    // 4. Settings Section
    let settings_group = adw::PreferencesGroup::builder().title("Settings").build();

    let auto_start_row = adw::SwitchRow::builder()
        .title("Start automatically")
        .subtitle("Launch Google Drive Linux background service when logging in")
        .active(config.auto_start)
        .build();

    let auto_mount_row = adw::SwitchRow::builder()
        .title("Mount automatically")
        .subtitle("Automatically mount ~/GoogleDrive upon application launch")
        .active(config.auto_mount)
        .build();

    settings_group.add(&auto_start_row);
    settings_group.add(&auto_mount_row);
    preferences_page.add(&settings_group);

    // Wire auth button click
    let config_ref = Rc::new(config.clone());
    let config_auth_ref = config_ref.clone();
    let account_row_clone = account_row.clone();
    let auth_btn_clone = auth_btn.clone();
    let status_row_clone = status_row.clone();

    auth_btn.connect_clicked(move |_btn| {
        let store_opt = AppConfig::default_credentials_path()
            .ok()
            .map(TokenStore::new);
        if let Some(store) = store_opt {
            if let Ok(Some(_)) = store.load() {
                // Logout action
                let oauth_cfg = OAuthConfig::default();
                let client = GoogleOAuthClient::new(oauth_cfg, store);
                let acc_row = account_row_clone.clone();
                let btn = auth_btn_clone.clone();
                let s_row = status_row_clone.clone();

                glib::spawn_future_local(async move {
                    let _ = client.logout().await;
                    acc_row.set_title("No account connected");
                    acc_row.set_subtitle("Sign in to authorize access to your Google Drive");
                    btn.set_label("Sign in with Google");
                    btn.remove_css_class("destructive-action");
                    btn.add_css_class("suggested-action");
                    s_row.set_subtitle("Offline (Sign in required)");
                });
            } else {
                // Login action
                let cid = config_auth_ref
                    .client_id
                    .clone()
                    .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
                    .unwrap_or_default();
                let csecret = config_auth_ref
                    .client_secret
                    .clone()
                    .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok());

                if cid.trim().is_empty() {
                    account_row_clone.set_subtitle("Client ID required. See docs/GOOGLE_OAUTH.md");
                    return;
                }

                let oauth_cfg = OAuthConfig {
                    client_id: cid,
                    client_secret: csecret,
                    ..OAuthConfig::default()
                };
                let client = GoogleOAuthClient::new(oauth_cfg, store);
                let acc_row = account_row_clone.clone();
                let btn = auth_btn_clone.clone();
                let s_row = status_row_clone.clone();

                glib::spawn_future_local(async move {
                    btn.set_sensitive(false);
                    btn.set_label("Authenticating...");
                    match client.start_browser_flow(true).await {
                        Ok(creds) => {
                            let email = creds
                                .account_email
                                .unwrap_or_else(|| "Connected Account".to_string());
                            acc_row.set_title(&email);
                            acc_row.set_subtitle("Connected to Google Drive");
                            btn.set_label("Disconnect");
                            btn.remove_css_class("suggested-action");
                            btn.add_css_class("destructive-action");
                            s_row.set_subtitle("Connected");
                        }
                        Err(e) => {
                            acc_row.set_subtitle(&format!("Authentication error: {e}"));
                            btn.set_label("Sign in with Google");
                        }
                    }
                    btn.set_sensitive(true);
                });
            }
        }
    });

    let mount_row_clone = mount_row.clone();
    let config_mount_ref = config_ref.clone();

    mount_btn.connect_clicked(move |btn| {
        let mount_path = config_mount_ref.mount_point.clone();
        let is_now_mounted = filesystem::is_path_mounted(&mount_path);

        if is_now_mounted {
            let _ = filesystem::unmount_path(&mount_path);
            btn.set_label("Mount");
            btn.remove_css_class("destructive-action");
            btn.add_css_class("suggested-action");
            mount_row_clone.set_subtitle(&format!("{} (Unmounted)", mount_path.display()));
        } else {
            let store_opt = AppConfig::default_credentials_path()
                .ok()
                .map(TokenStore::new);
            let has_creds = store_opt
                .as_ref()
                .and_then(|s| s.load().ok().flatten())
                .is_some();
            if !has_creds {
                mount_row_clone.set_subtitle("Sign in with Google first");
                return;
            }

            if let Some(store) = store_opt {
                let oauth = Arc::new(GoogleOAuthClient::new(OAuthConfig::default(), store));
                let drive_client =
                    Arc::new(crate::drive::GoogleDriveClient::new(None, None, oauth));
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let cache = crate::cache::open_default_cache(config_mount_ref.cache_size_mb)
                        .ok()
                        .map(|c| Arc::new(std::sync::Mutex::new(c)));
                    match filesystem::mount_filesystem(&mount_path, drive_client, handle, cache) {
                        Ok(active_mount) => {
                            std::mem::forget(active_mount);
                            btn.set_label("Unmount");
                            btn.remove_css_class("suggested-action");
                            btn.add_css_class("destructive-action");
                            mount_row_clone
                                .set_subtitle(&format!("{} (Mounted)", mount_path.display()));
                        }
                        Err(e) => {
                            mount_row_clone.set_subtitle(&format!("Mount failed: {e}"));
                        }
                    }
                }
            }
        }
    });

    content_box.append(&preferences_page);
    window.set_content(Some(&content_box));
    window.present();
}
