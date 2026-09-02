//! Filesystem locations and URL normalization: where the note database, the
//! device key and the import/export folders live on each platform.

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::env;
use std::path::PathBuf;

pub(crate) const DATABASE_FILE: &str = "enkr_notes.sqlite3";
pub(crate) const DEVICE_KEY_FILE: &str = "enkr_device.key";
/// `localStorage` key holding the device identity on wasm32 — the browser
/// counterpart of `DEVICE_KEY_FILE` (see `sync/identity.rs`).
#[cfg(target_arch = "wasm32")]
pub(crate) const DEVICE_KEY_STORAGE_KEY: &str = "enkr_device_key";
pub(crate) const APP_DIR_NAME: &str = "enkr";
pub(crate) const IMPORT_DIR: &str = "enkr_import";
pub(crate) const EXPORT_DIR: &str = "enkr_export";

pub(crate) fn default_database_path() -> PathBuf {
    platform_config_dir().join(APP_DIR_NAME).join(DATABASE_FILE)
}

pub(crate) fn default_device_key_path() -> PathBuf {
    platform_config_dir()
        .join(APP_DIR_NAME)
        .join(DEVICE_KEY_FILE)
}

/// Accept full ws:// URLs or bare host:port (expanded to `ws://host/ws`).
pub(crate) fn normalize_server_url(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if input.starts_with("ws://") || input.starts_with("wss://") {
        Some(input.to_string())
    } else {
        Some(format!("ws://{input}/ws"))
    }
}

pub(crate) fn default_import_path() -> PathBuf {
    PathBuf::from(IMPORT_DIR)
}

pub(crate) fn default_export_path() -> PathBuf {
    PathBuf::from(EXPORT_DIR)
}

/// A space name derived from a folder path: its final component, or a sensible
/// fallback for odd paths (root, `..`, empty).
pub(crate) fn folder_display_name(root: &std::path::Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Imported".to_string())
}

#[cfg(target_os = "linux")]
pub(crate) fn platform_config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(target_os = "macos")]
pub(crate) fn platform_config_dir() -> PathBuf {
    env::var_os("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(target_os = "windows")]
pub(crate) fn platform_config_dir() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn platform_config_dir() -> PathBuf {
    PathBuf::from(".")
}
