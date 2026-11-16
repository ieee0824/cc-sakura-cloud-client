//! Platform-native absolute paths for private client state.
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;
pub fn home() -> Result<PathBuf> {
    #[cfg(windows)]
    let value = std::env::var_os("HOME")
        .filter(|value| PathBuf::from(value).is_absolute())
        .or_else(|| std::env::var_os("USERPROFILE"));
    #[cfg(not(windows))]
    let value = std::env::var_os("HOME");
    let path =
        PathBuf::from(value.context("home directory is not configured; supply explicit paths")?);
    ensure!(path.is_absolute(), "home directory must be absolute");
    Ok(path)
}
pub fn state_directory() -> Result<PathBuf> {
    let base = if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(value)
    } else {
        #[cfg(windows)]
        {
            match std::env::var_os("LOCALAPPDATA") {
                Some(path) => PathBuf::from(path),
                None => home()?.join("AppData/Local"),
            }
        }
        #[cfg(not(windows))]
        {
            home()?.join(".local/state")
        }
    };
    ensure!(
        base.is_absolute(),
        "client state directory must be absolute"
    );
    Ok(base.join("cc-sakura"))
}

pub fn default_outbox() -> Result<PathBuf> {
    let native = state_directory()?.join("outbox.sqlite3");
    #[cfg(windows)]
    if std::env::var_os("XDG_STATE_HOME").is_none() {
        let legacy = home()?.join(".local/state/cc-sakura/outbox.sqlite3");
        if legacy.exists() {
            return Ok(legacy);
        }
    }
    Ok(native)
}
