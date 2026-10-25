//! Test packs: downloaded once per (item, hash) into the cache dir and extracted.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde::Deserialize;

use crate::api::{Api, Item};
use crate::config;

#[derive(Debug, Deserialize, Clone)]
pub struct PackStage {
    pub stage: u32,
    pub title: String,
    pub dir: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub language: String,
    pub pack_hash: String,
    // exercise
    pub entry: Option<String>,
    pub timeout_seconds: Option<u64>,
    // project
    pub run: Option<String>,
    /// Pip requirements; when present tests run in the project's own `.venv`.
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default)]
    pub stages: Vec<PackStage>,
}

pub struct Pack {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Pack {
    pub fn open(dir: &Path) -> Result<Pack> {
        let raw = fs::read_to_string(dir.join("manifest.json"))
            .with_context(|| format!("no manifest.json in {}", dir.display()))?;
        Ok(Pack {
            dir: dir.to_path_buf(),
            manifest: serde_json::from_str(&raw)?,
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
