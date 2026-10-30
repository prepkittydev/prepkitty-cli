//! Test packs: downloaded once per (item, hash) into the cache dir and extracted.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde::Deserialize;

use crate::api::{Api, Item};
use crate::config;

/// The newest pack format this CLI understands (`schema` in manifest.json).
/// 3 added lesson checkpoints (kind "lesson", run like exercises).
pub const SUPPORTED_SCHEMA: u32 = 3;

pub const UPDATE_HINT: &str = "Update it with:\n  macOS / Linux: curl -fsSL https://prepkitty.dev/install.sh | sh\n  Windows: powershell -ExecutionPolicy Bypass -c \"irm https://prepkitty.dev/install.ps1 | iex\"";

#[derive(Debug, Deserialize, Clone)]
pub struct PackStage {
    pub stage: u32,
    pub title: String,
    pub dir: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default)]
    pub schema: u32,
    pub kind: String,
    pub id: String,
    pub title: String,
    pub language: String,
    pub pack_hash: String,
    // exercise and lesson checkpoint
    pub entry: Option<String>,
    pub timeout_seconds: Option<u64>,
    // project
    pub run: Option<String>,
    /// Pip requirements; when present tests run in the project's own `.venv`.
    #[serde(default)]
    pub requirements: Vec<String>,
    /// The learner picks the framework: `run` and `build` come from their prepkitty.toml.
    #[serde(default)]
    pub any_framework: bool,
    /// How long a project's server may take to start listening (default 10s).
    pub start_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub stages: Vec<PackStage>,
}

pub struct Pack {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Manifest {
    /// Projects are staged, own their git repo and use the steps runner;
    /// exercises and lesson checkpoints share the language workspace and unit tests.
    pub fn is_project(&self) -> bool {
        self.kind == "project"
    }
}

impl Pack {
    pub fn open(dir: &Path) -> Result<Pack> {
        let raw = fs::read_to_string(dir.join("manifest.json"))
            .with_context(|| format!("no manifest.json in {}", dir.display()))?;
        let manifest: Manifest = serde_json::from_str(&raw)?;
        if manifest.schema > SUPPORTED_SCHEMA {
            bail!(
                "{} needs a newer prepkitty (you have {}).\n{UPDATE_HINT}",
                manifest.title,
                env!("CARGO_PKG_VERSION")
            );
        }
        Ok(Pack {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    pub fn runner(&self) -> PathBuf {
        self.dir.join("_prepkitty_runner.py")
    }
}

fn cache_path(id: &str, hash: &str) -> Result<PathBuf> {
    Ok(config::cache_dir()?
        .join("packs")
        .join(id)
        .join(&hash[..hash.len().min(16)]))
}

/// Returns the extracted pack for `item`, downloading it if needed.
pub fn ensure(api: &Api, item: &Item) -> Result<Pack> {
    let dir = cache_path(&item.id, &item.pack_hash)?;
    if dir.join("manifest.json").exists() {
        return Pack::open(&dir);
    }
    let bytes = api.download(&item.pack_url)?;
    let tmp = dir.with_extension("partial");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    extract(&bytes, &tmp)?;
    let _ = fs::remove_dir_all(&dir);
    fs::rename(&tmp, &dir)?;
    let pack = Pack::open(&dir)?;
    if pack.manifest.pack_hash != item.pack_hash {
        bail!("downloaded pack doesn't match the expected version; try again in a moment");
    }
    Ok(pack)
}

/// Most recently cached pack for an item, for working offline.
pub fn cached(id: &str) -> Result<Option<Pack>> {
    let root = config::cache_dir()?.join("packs").join(id);
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(None);
    };
    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("manifest.json").exists())
        .collect();
    dirs.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    match dirs.last() {
        Some(e) => Ok(Some(Pack::open(&e.path())?)),
        None => Ok(None),
    }
}

fn extract(bytes: &[u8], into: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Refuse anything that would escape the target directory.
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("refusing unsafe path in pack: {}", path.display());
        }
        entry.unpack_in(into)?;
    }
    Ok(())
}

/// Copies files from `src` into `dest` without overwriting anything that exists.
pub fn copy_missing(src: &Path, dest: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    if !src.exists() {
        return Ok(written);
    }
    for entry in walk(src)? {
        let rel = entry.strip_prefix(src)?;
        let target = dest.join(rel);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&entry, &target)?;
        written.push(rel.to_path_buf());
    }
    Ok(written)
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(walk(&path)?);
        } else {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("manifest.json"), json).unwrap();
        dir
    }

    #[test]
    fn lesson_checkpoints_run_like_exercises() {
        let dir = write_manifest(
            r#"{"schema":3,"kind":"lesson","id":"py-loops","title":"Loops","language":"python","packHash":"x","entry":"solution.py","timeoutSeconds":10}"#,
        );
        let pack = Pack::open(dir.path()).unwrap();
        assert!(!pack.manifest.is_project());
        assert_eq!(pack.manifest.entry.as_deref(), Some("solution.py"));
    }

    #[test]
    fn newer_schemas_ask_to_update() {
        let dir = write_manifest(
            r#"{"schema":4,"kind":"lesson","id":"x","title":"Future","language":"python","packHash":"x"}"#,
        );
        let Err(err) = Pack::open(dir.path()) else {
            panic!("schema 4 opened");
        };
        let err = err.to_string();
        assert!(err.contains("needs a newer prepkitty"), "{err}");
    }
}
