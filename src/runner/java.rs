//! Finding a JDK, compiling the learner's code and running the bundled Java runner.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Result};

use super::process;
use super::python::{parse_report, tail};
use crate::api::TestResult;

pub const MIN_VERSION: u32 = 17;
pub const RUNNER_FILE: &str = "PrepKittyRunner.java";
const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct Java {
    pub javac: String,
    pub java: String,
}

impl Java {
    /// The JDK in $JAVA_HOME, else the first `javac` on PATH, if it is 17+.
    pub fn find() -> Result<Java> {
        let exe = |name: &str| {
            if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_string()
            }
        };
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("JAVA_HOME") {
            let bin = PathBuf::from(home).join("bin");
            candidates.push((
                bin.join(exe("javac")).to_string_lossy().into_owned(),
                bin.join(exe("java")).to_string_lossy().into_owned(),
            ));
        }
        candidates.push(("javac".into(), "java".into()));

        let mut found_old = None;
        for (javac, java) in candidates {
            let Ok(out) = Command::new(&javac).arg("-version").output() else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            // `javac 21.0.2` (stdout on modern JDKs, stderr on old ones).
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let version = text.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
            let major = match parts.next().unwrap_or(0) {
                1 => parts.next().unwrap_or(0), // 1.8 → 8
                m => m,
            };
            if major >= MIN_VERSION {
                return Ok(Java { javac, java });
            }
            found_old = Some(version);
        }
        match found_old {
            Some(v) => bail!("PrepKitty needs Java {MIN_VERSION}+, but found Java {v}. Install a newer JDK (e.g. from adoptium.net) or point JAVA_HOME at one."),
            None => bail!("Couldn't find a Java compiler. Install a JDK {MIN_VERSION}+ (e.g. from adoptium.net) and make sure `javac` is on your PATH or JAVA_HOME is set."),
        }
    }

    /// Compiles `sources` into `out`. Errors come back as a failed "compile your code" result.
    pub fn compile(
        &self,
        sources: &[PathBuf],
        out: &Path,
        cwd: &Path,
    ) -> Result<Option<TestResult>> {
        if sources.is_empty() {
            return Ok(Some(TestResult {
                name: "compile your code".into(),
                passed: false,
                message: Some("No .java files found.".into()),
            }));
        }
        let mut cmd = Command::new(&self.javac);
        cmd.args(["-g", "-encoding", "UTF-8", "-nowarn", "-d"])
            .arg(out)
            .args(sources);
        let res = process::run(cmd, cwd, None, COMPILE_TIMEOUT)?;
        if res.status == Some(0) {
            return Ok(None);
        }
        let prefix = format!("{}{}", cwd.display(), std::path::MAIN_SEPARATOR);
        let detail = format!("{}{}", res.stdout, res.stderr).replace(&prefix, "");
        Ok(Some(TestResult {
            name: "compile your code".into(),
            passed: false,
            message: Some(tail(&detail, 1500)),
        }))
    }

    /// Compiles the learner's code with the tests and runner, then runs every `*Test.java`.
    pub fn run_tests(
        &self,
        runner: &Path,
        tests: &Path,
        solution: &Path,
        timeout_secs: u64,
    ) -> Result<Vec<TestResult>> {
        let classes = tempfile::tempdir()?;
        let mut sources = java_sources(solution);
        sources.extend(java_sources(tests));
        sources.push(runner.to_path_buf());
        if let Some(failed) = self.compile(&sources, classes.path(), solution)? {
            return Ok(vec![failed]);
        }

        let mut cmd = Command::new(&self.java);
        cmd.arg("-cp")
            .arg(classes.path())
            .arg("-Dfile.encoding=UTF-8")
            .arg("PrepKittyRunner")
            .arg("--tests")
            .arg(tests);
        let out = process::run(cmd, solution, None, Duration::from_secs(timeout_secs))?;
        if out.timed_out {
            return Ok(vec![TestResult {
                name: "finish within the time limit".into(),
                passed: false,
                message: Some(format!(
                    "Your code took longer than {timeout_secs}s. Look for an infinite loop, a deadlock or a slow (nested loop) approach."
                )),
            }]);
        }
        Ok(parse_report(&out.stdout).unwrap_or_else(|| {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout
            } else {
                out.stderr
            };
            vec![TestResult {
                name: "run the tests".into(),
                passed: false,
                message: Some(tail(&detail, 1500)),
            }]
        }))
    }
}

/// Every `.java` file under `dir`, skipping hidden and build folders.
pub fn java_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if dir.is_file() {
        out.push(dir.to_path_buf());
        return out;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "target" || name == "build" || name == "out" {
            continue;
        }
        if path.is_dir() {
            out.extend(java_sources(&path));
        } else if name.ends_with(".java") {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sources_recursively_and_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("Main.java"), "").unwrap();
        std::fs::write(dir.path().join("pkg/Util.java"), "").unwrap();
        std::fs::write(dir.path().join(".git/X.java"), "").unwrap();
        std::fs::write(dir.path().join("notes.md"), "").unwrap();
        let found = java_sources(dir.path());
        assert_eq!(found.len(), 2);
    }
}
