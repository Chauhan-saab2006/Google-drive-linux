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

    // 5. Google Cloud OAuth Credentials Section
    let oauth_group = adw::PreferencesGroup::builder()
        .title("Google Cloud OAuth Credentials")
        .description("Enter your Client ID and Client Secret from Google Cloud Console")
        .build();

    let initial_client_id = config
        .client_id
        .clone()
        .or_else(|| {
            AppConfig::default_credentials_path()
                .ok()
                .map(TokenStore::new)
                .and_then(|s| s.load().ok().flatten())
                .and_then(|c| c.client_id)
        })
        .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
        .unwrap_or_default();

    let initial_client_secret = config
        .client_secret
        .clone()
        .or_else(|| {
            AppConfig::default_credentials_path()
                .ok()
                .map(TokenStore::new)
                .and_then(|s| s.load().ok().flatten())
                .and_then(|c| c.client_secret)
        })
        .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok())
        .unwrap_or_default();

    let client_id_row = adw::EntryRow::builder()
        .title("OAuth Client ID")
        .build();
    if !initial_client_id.is_empty() {
        client_id_row.set_text(&initial_client_id);
    }

    let client_secret_row = adw::PasswordEntryRow::builder()
        .title("OAuth Client Secret (Optional)")
        .build();
    if !initial_client_secret.is_empty() {
        client_secret_row.set_text(&initial_client_secret);
    }

    let save_creds_row = adw::ActionRow::builder()
        .title("Save Credentials")
        .subtitle("Save keys to ~/.config/google-drive-linux/config.toml")
        .build();

    let save_creds_btn = gtk4::Button::builder()
        .label("Save")
        .valign(gtk4::Align::Center)
        .build();
    save_creds_btn.add_css_class("suggested-action");
    save_creds_row.add_suffix(&save_creds_btn);

    oauth_group.add(&client_id_row);
    oauth_group.add(&client_secret_row);
    oauth_group.add(&save_creds_row);
    preferences_page.add(&oauth_group);

    // Shared mutable configuration state
    let config_ref = Rc::new(std::cell::RefCell::new(config.clone()));
    let config_auth_ref = config_ref.clone();
    let account_row_clone = account_row.clone();
    let auth_btn_clone = auth_btn.clone();
    let status_row_clone = status_row.clone();
    let client_id_row_for_auth = client_id_row.clone();
    let client_secret_row_for_auth = client_secret_row.clone();

    // Wire toggle saving
    let config_toggle1 = config_ref.clone();
    auto_start_row.connect_active_notify(move |row| {
        let is_active = row.is_active();
        let mut conf = config_toggle1.borrow_mut();
        conf.auto_start = is_active;
        let _ = conf.save();
    });

    let config_toggle2 = config_ref.clone();
    auto_mount_row.connect_active_notify(move |row| {
        let is_active = row.is_active();
        let mut conf = config_toggle2.borrow_mut();
        conf.auto_mount = is_active;
        let _ = conf.save();
    });

    // Wire save credentials button
    let client_id_entry_clone = client_id_row.clone();
    let client_secret_entry_clone = client_secret_row.clone();
    let save_creds_row_clone = save_creds_row.clone();
    let config_save_ref = config_ref.clone();
    let account_row_for_save = account_row.clone();

    save_creds_btn.connect_clicked(move |btn| {
        let cid = client_id_entry_clone.text().trim().to_string();
        let csecret = client_secret_entry_clone.text().trim().to_string();

        let mut conf = config_save_ref.borrow_mut();
        conf.client_id = if cid.is_empty() { None } else { Some(cid.clone()) };
        conf.client_secret = if csecret.is_empty() { None } else { Some(csecret) };

        match conf.save() {
            Ok(()) => {
                save_creds_row_clone.set_subtitle("✓ Credentials saved to config.toml");
                btn.set_label("Saved");
                if !cid.is_empty() {
                    account_row_for_save.set_subtitle("Credentials configured. Click 'Sign in with Google' above.");
                }
                let b = btn.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                    b.set_label("Save");
                });
            }
            Err(e) => {
                save_creds_row_clone.set_subtitle(&format!("Error saving config: {e}"));
            }
        }
    });

    // Wire auth button click
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
                // Login action: check client_id_row directly as well
                let cid_from_entry = client_id_row_for_auth.text().trim().to_string();
                let csecret_from_entry = client_secret_row_for_auth.text().trim().to_string();

                let (cid, csecret) = {
                    let mut conf = config_auth_ref.borrow_mut();
                    if !cid_from_entry.is_empty() {
                        conf.client_id = Some(cid_from_entry.clone());
                    }
                    if !csecret_from_entry.is_empty() {
                        conf.client_secret = Some(csecret_from_entry.clone());
                    }
                    let _ = conf.save();

                    let cid = conf
                        .client_id
                        .clone()
                        .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
                        .unwrap_or_default();
                    let csecret = conf
                        .client_secret
                        .clone()
                        .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok());
                    (cid, csecret)
                };

                if cid.trim().is_empty() {
                    account_row_clone.set_subtitle("Please enter your Client ID in the Google Cloud Credentials section below");
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
        let (mount_path, cache_size_mb) = {
            let conf = config_mount_ref.borrow();
            (conf.mount_point.clone(), conf.cache_size_mb)
        };
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
                let (cid, csecret) = {
                    let conf = config_mount_ref.borrow();
                    let creds_opt = store.load().ok().flatten();
                    let cid = conf
                        .client_id
                        .clone()
                        .or_else(|| creds_opt.as_ref().and_then(|c| c.client_id.clone()))
                        .or_else(|| std::env::var("GOOGLE_CLIENT_ID").ok())
                        .unwrap_or_default();
                    let csecret = conf
                        .client_secret
                        .clone()
                        .or_else(|| creds_opt.as_ref().and_then(|c| c.client_secret.clone()))
                        .or_else(|| std::env::var("GOOGLE_CLIENT_SECRET").ok());
                    (cid, csecret)
                };

                let oauth_cfg = OAuthConfig {
                    client_id: cid,
                    client_secret: csecret,
                    ..OAuthConfig::default()
                };
                let oauth = Arc::new(GoogleOAuthClient::new(oauth_cfg, store));
                let drive_client =
                    Arc::new(crate::drive::GoogleDriveClient::new(None, None, oauth));
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let cache = crate::cache::open_default_cache(cache_size_mb)
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
