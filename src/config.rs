//! Credentials and paths. The token lives in the user's config dir with 0600 permissions.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// API used when nothing else is configured. Override with `PREPKITTY_API` or `login --api`.
pub const DEFAULT_API: &str = match option_env!("PREPKITTY_DEFAULT_API") {
    Some(v) => v,
    None => "https://prepkitty.dev",
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Credentials {
    pub api: String,
    pub token: String,
    pub name: Option<String>,
}

fn dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("dev", "prepkitty", "prepkitty")
        .context("could not determine a home directory")
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(dirs()?.config_dir().join("credentials.toml"))
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(dirs()?.cache_dir().to_path_buf())
}

pub fn load_credentials() -> Result<Option<Credentials>> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(toml::from_str(&raw).context(
        "credentials file is corrupt; run `prepkitty login`",
    )?))
}

pub fn save_credentials(creds: &Credentials) -> Result<()> {
    let path = credentials_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, toml::to_string(creds)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn delete_credentials() -> Result<bool> {
    let path = credentials_path()?;
    if path.exists() {
        fs::remove_file(path)?;
        return Ok(true);
    }
    Ok(false)
}

/// API base URL: env var, then the one we logged in to, then the default.
pub fn api_base(creds: Option<&Credentials>) -> String {
    std::env::var("PREPKITTY_API")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| creds.map(|c| c.api.clone()))
        .unwrap_or_else(|| DEFAULT_API.to_string())
        .trim_end_matches('/')
        .to_string()
}
