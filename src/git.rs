//! Thin wrappers around the `git` command.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(git_raw(dir, args)?.trim().to_string())
}

/// Like `git`, but keeps leading whitespace (porcelain output is column-based).
fn git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("`git` isn't installed or isn't on your PATH")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn is_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s == "true")
        .unwrap_or(false)
}

pub fn init(dir: &Path) -> Result<()> {
    git(dir, &["init", "-q"])?;
    Ok(())
}

/// Stages and commits everything in `dir` (used for the starter commit).
pub fn commit_all(dir: &Path, message: &str) -> Result<()> {
    git(dir, &["add", "-A", "."])?;
    // Fall back to a local identity so the first commit never fails on a fresh machine.
    let has_identity = git(dir, &["config", "user.email"]).is_ok();
    if has_identity {
        git(dir, &["commit", "-q", "-m", message])?;
    } else {
        git(
            dir,
            &[
                "-c",
                "user.name=PrepKitty Learner",
                "-c",
                "user.email=learner@prepkitty.local",
                "commit",
                "-q",
                "-m",
                message,
            ],
        )?;
    }
    Ok(())
}

/// Uncommitted changes inside `dir` (paths relative to the repo).
pub fn dirty_files(dir: &Path) -> Result<Vec<String>> {
    let out = git_raw(dir, &["status", "--porcelain", "--", "."])?;
    Ok(out
        .lines()
        .map(|l| l.get(3..).unwrap_or(l).to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

pub fn head_sha(dir: &Path) -> Result<String> {
    git(dir, &["rev-parse", "HEAD"]).context("no commits yet; commit your work first")
}
