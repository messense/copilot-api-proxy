//! Configuration and file path management.

use crate::error::Error;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// Get the path to the GitHub token file
pub fn token_path() -> PathBuf {
    dirs::home_dir()
        .expect("No home directory")
        .join(".local/share/copilot-api-proxy/github_token")
}

/// Load GitHub token from environment or file
pub fn load_github_token() -> Result<String, Error> {
    std::env::var("GITHUB_TOKEN").or_else(|_| {
        let path = token_path();
        fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::Config(format!("Token not found at {:?}", path)))
    })
}

/// Ensure token directory exists with secure permissions
pub fn ensure_token_dir() -> Result<(), Error> {
    if let Some(parent) = token_path().parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write token with secure permissions (0600)
pub fn write_token(path: &PathBuf, token: &str) -> Result<(), Error> {
    fs::write(path, token)?;
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    Ok(())
}

/// Load VSCode device ID from the system-specific path, or generate one.
pub fn load_vscode_device_id() -> String {
    let path = vscode_device_id_path();
    match fs::read_to_string(&path) {
        Ok(id) => {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist — generate and persist below
        }
        Err(_) => {
            // Other read error — return ephemeral ID
            return uuid::Uuid::new_v4().to_string();
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, &id).is_err() {
        tracing::warn!("Failed to persist device ID to {:?}", path);
    }
    id
}

/// Load VSCode machine ID from the system-specific path, or generate one.
/// This is sent as the `vscode-machineid` header to the Copilot API.
pub fn load_vscode_machine_id() -> String {
    let path = vscode_machine_id_path();
    match fs::read_to_string(&path) {
        Ok(id) => {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return uuid::Uuid::new_v4().to_string();
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, &id).is_err() {
        tracing::warn!("Failed to persist machine ID to {:?}", path);
    }
    id
}

fn vscode_machine_id_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .expect("No home directory")
            .join("Library/Application Support/Microsoft/DeveloperTools/machineid")
    } else {
        dirs::cache_dir()
            .expect("No cache directory")
            .join("Microsoft/DeveloperTools/machineid")
    }
}

fn vscode_device_id_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .expect("No home directory")
            .join("Library/Application Support/Microsoft/DeveloperTools/deviceid")
    } else {
        dirs::cache_dir()
            .expect("No cache directory")
            .join("Microsoft/DeveloperTools/deviceid")
    }
}
