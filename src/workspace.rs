//! Finding which exercise/project the learner is working on, from the cwd.
//!
//! Every exercise folder and project root has a `prepkitty.toml`. Exercises
//! live together in a `prepkitty-<language>/` workspace (one git repo);
//! each project is its own git repo.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const ITEM_FILE: &str = "prepkitty.toml";
pub const WORKSPACE_MARKER: &str = ".prepkitty-workspace";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ItemConfig {
    pub id: String,
    pub kind: String,
    pub language: String,
    /// Projects: command that starts the learner's program.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Any-framework projects: command run once in the project folder before the
    /// tests, e.g. `./mvnw -q -DskipTests package`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Projects: the stage the learner is working on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<u32>,
}

pub struct Context {
    pub dir: PathBuf,
    pub config: ItemConfig,
}

impl Context {
    pub fn save(&self) -> Result<()> {
        write_config(&self.dir, &self.config)
    }
}

pub fn write_config(dir: &Path, config: &ItemConfig) -> Result<()> {
    let header = "# Managed by prepkitty. Safe to commit.\n";
    fs::write(
        dir.join(ITEM_FILE),
        format!("{header}{}", toml::to_string(config)?),
    )?;
    Ok(())
}

/// The prepkitty.toml directly in `dir`, if there is a readable one.
pub fn read_config(dir: &Path) -> Option<ItemConfig> {
    toml::from_str(&fs::read_to_string(dir.join(ITEM_FILE)).ok()?).ok()
}

/// Walks up from `start` to the nearest folder with a prepkitty.toml.
pub fn find(start: &Path) -> Result<Context> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let file = d.join(ITEM_FILE);
        if file.exists() {
            let config: ItemConfig = toml::from_str(&fs::read_to_string(&file)?)?;
            return Ok(Context {
                dir: d.to_path_buf(),
                config,
            });
        }
        if d.join(WORKSPACE_MARKER).exists() {
            break;
        }
        dir = d.parent();
    }
    bail!(
        "This folder isn't a PrepKitty exercise or project.\n  cd into one, or run `prepkitty start <id>` to begin a new one."
    )
}

/// The exercises workspace containing `start`, if any.
pub fn find_workspace(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join(WORKSPACE_MARKER).exists())
        .map(Path::to_path_buf)
}
