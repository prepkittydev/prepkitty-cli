//! Finding a suitable Python and running the bundled unittest runner.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Deserialize;

use super::process;
use crate::api::TestResult;

const MARKER: &str = "__PREPKITTY_RESULTS__";
pub const MIN_MINOR: u32 = 10;

#[derive(Clone, Debug)]
pub struct Python {
    pub bin: String,
}

impl Python {
    /// First of python3 / python / py that is Python 3.10+.
    pub fn find() -> Result<Python> {
        let candidates: &[&str] = if cfg!(windows) {
            &["py", "python", "python3"]
        } else {
            &["python3", "python"]
        };
        let mut found_old = None;
        for bin in candidates {
            let Ok(out) = Command::new(bin)
                .args(["-c", "import sys; print('%d.%d.%d' % sys.version_info[:3])"])
                .output()
            else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
            let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
            if major == 3 && minor >= MIN_MINOR {
                return Ok(Python {
                    bin: bin.to_string(),
                });
            }
            found_old = Some(version);
        }
        match found_old {
            Some(v) => bail!("PrepKitty needs Python 3.{MIN_MINOR}+, but found Python {v}. Install a newer Python from python.org."),
            None => bail!("Couldn't find Python. Install Python 3.{MIN_MINOR}+ from python.org and make sure `python3` is on your PATH."),
        }
    }

    /// The interpreter of `<project>/.venv`, created and filled with `requirements`
    /// on first use (and again whenever they change). Uses uv when available.
    pub fn with_requirements(&self, project_dir: &Path, requirements: &[String]) -> Result<Python> {
        if requirements.is_empty() {
            return Ok(self.clone());
        }
        self.venv(
            project_dir,
            requirements,
            &requirements.join("\n"),
            &requirements.join(", "),
        )
    }

    /// Like `with_requirements`, but for the learner's own `requirements.txt`
    /// (any-framework projects). Without one, returns `self` unchanged.
    pub fn with_requirements_file(&self, project_dir: &Path) -> Result<Python> {
        let file = project_dir.join("requirements.txt");
        let Ok(contents) = std::fs::read_to_string(&file) else {
            return Ok(self.clone());
        };
        if contents.lines().all(|l| {
            let l = l.trim();
            l.is_empty() || l.starts_with('#')
        }) {
            return Ok(self.clone());
        }
        let args = ["-r".to_string(), file.to_string_lossy().into_owned()];
        self.venv(project_dir, &args, &contents, "requirements.txt")
    }

    /// Creates `<project>/.venv` and pip-installs `install_args` unless the venv's
    /// marker already says `marker` was installed.
    fn venv(
        &self,
        project_dir: &Path,
        install_args: &[String],
        marker_value: &str,
        what: &str,
    ) -> Result<Python> {
        let venv_python = venv_python(project_dir);
        let marker = project_dir.join(".venv").join(".prepkitty-requirements");
        if venv_python.is_file()
            && std::fs::read_to_string(&marker).is_ok_and(|m| m == marker_value)
        {
            return Ok(Python {
                bin: venv_python.to_string_lossy().into_owned(),
            });
        }

        eprintln!("Installing this project's packages into .venv (first run only): {what}");
        let venv = project_dir.join(".venv");
        let uv = Command::new("uv")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        let ok = if uv {
            // Give uv an absolute interpreter path: a bare `python3` is read as a version
            // request and may resolve to a newer uv-managed Python the pins have no wheels for.
            let exe = Command::new(&self.bin)
                .args(["-c", "import sys; print(sys.executable)"])
                .output()?;
            let exe = String::from_utf8_lossy(&exe.stdout).trim().to_string();
            Command::new("uv")
                .args(["venv", "--quiet", "--allow-existing", "--python", &exe])
                .arg(&venv)
                .status()?
                .success()
                && Command::new("uv")
                    .args(["pip", "install", "--quiet", "--python"])
                    .arg(&venv_python)
                    .args(install_args)
                    .status()?
                    .success()
        } else {
            (venv_python.is_file()
                || Command::new(&self.bin)
                    .args(["-m", "venv"])
                    .arg(&venv)
                    .status()?
                    .success())
                && Command::new(&venv_python)
                    .args([
                        "-m",
                        "pip",
                        "install",
                        "--quiet",
                        "--disable-pip-version-check",
                    ])
                    .args(install_args)
                    .status()?
                    .success()
        };
        if !ok {
            bail!(
                "Couldn't install the project's packages. Check your internet connection, or install them yourself:\n  {} -m pip install {}",
                venv_python.display(),
                install_args.join(" ")
            );
        }
        std::fs::write(&marker, marker_value)?;
        Ok(Python {
            bin: venv_python.to_string_lossy().into_owned(),
        })
    }

    /// Runs `runner` against `tests` (dir or file) with `solution` on sys.path.
    pub fn run_unittest(
        &self,
        runner: &Path,
        tests: &Path,
        solution: &Path,
        timeout_secs: u64,
    ) -> Result<Vec<TestResult>> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(runner)
            .arg("--tests")
            .arg(tests)
            .arg("--solution")
            .arg(solution);
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        let out = process::run(cmd, solution, None, Duration::from_secs(timeout_secs))?;
        if out.timed_out {
            return Ok(vec![TestResult {
                name: "finish within the time limit".into(),
                passed: false,
                message: Some(format!(
                    "Your code took longer than {timeout_secs}s. Look for an infinite loop or a slow (nested loop) approach."
                )),
            }]);
        }
        parse_report(&out.stdout).ok_or(()).or_else(|_| {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout
            } else {
                out.stderr
            };
            Ok(vec![TestResult {
                name: "run the tests".into(),
                passed: false,
                message: Some(tail(&detail, 1500)),
            }])
        })
    }
}

#[derive(Deserialize)]
struct Report {
    results: Vec<TestResult>,
}

pub fn parse_report(stdout: &str) -> Option<Vec<TestResult>> {
    let start = stdout.find(MARKER)? + MARKER.len();
    let end = stdout[start..].find(MARKER)? + start;
    serde_json::from_str::<Report>(&stdout[start..end])
        .ok()
        .map(|r| r.results)
}

fn venv_python(project_dir: &Path) -> PathBuf {
    let venv = project_dir.join(".venv");
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// The project's `.venv` scripts folder (where `uvicorn`, `flask`... live), if it has one.
pub fn venv_bin(project_dir: &Path) -> Option<PathBuf> {
    let python = venv_python(project_dir);
    if python.is_file() {
        python.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

pub fn tail(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &s[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_marked_report() {
        let out = "noise\n__PREPKITTY_RESULTS__{\"passed\":true,\"total\":1,\"results\":[{\"name\":\"a\",\"passed\":true}]}__PREPKITTY_RESULTS__\n";
        let r = parse_report(out).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].passed);
    }

    #[test]
    fn missing_marker_is_none() {
        assert!(parse_report("Traceback ...").is_none());
    }

    #[test]
    fn tail_respects_char_boundaries() {
        let s = "é".repeat(100);
        let t = tail(&s, 11);
        assert!(t.starts_with('…'));
    }
}
