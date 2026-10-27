//! Running tests locally. A `Runner` turns a pack + the learner's folder into
//! a list of results; reporting is separate so other languages and runners
//! (in-browser, sandbox) can reuse the same result shape.

pub mod java;
pub mod process;
pub mod python;
pub mod steps;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use console::style;

use crate::api::TestResult;
use crate::pack::Pack;

pub trait Runner {
    fn run(&self, pack: &Pack, work_dir: &Path) -> Result<Vec<TestResult>>;
}

/// Runs an exercise's unittest tests.
pub struct UnittestRunner {
    pub python: python::Python,
}

impl Runner for UnittestRunner {
    fn run(&self, pack: &Pack, work_dir: &Path) -> Result<Vec<TestResult>> {
        let timeout = pack.manifest.timeout_seconds.unwrap_or(10);
        self.python
            .run_unittest(&pack.runner(), &pack.dir.join("tests"), work_dir, timeout)
    }
}

/// Runs a Java exercise's `*Test.java` classes.
pub struct JavaTestRunner {
    pub java: java::Java,
}

impl Runner for JavaTestRunner {
    fn run(&self, pack: &Pack, work_dir: &Path) -> Result<Vec<TestResult>> {
        let timeout = pack.manifest.timeout_seconds.unwrap_or(10);
        self.java.run_tests(
            &pack.dir.join(java::RUNNER_FILE),
            &pack.dir.join("tests"),
            work_dir,
            timeout,
        )
    }
}

/// The language tools a project's stages run with.
#[derive(Clone, Debug)]
pub enum Toolchain {
    Python(python::Python),
    /// `classes` holds the learner's compiled code; `{run}` puts it on the classpath.
    Java {
        java: java::Java,
        classes: PathBuf,
    },
    /// The learner's own `build` compiled the project (any-framework projects), so
    /// `{run}` is used as-is.
    Custom,
}

/// How long a learner's `build` command may take (a first Maven build downloads a lot).
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);

/// Runs a project's stages 1..=`up_to` (earlier stages guard against regressions).
pub struct StepsRunner {
    pub toolchain: Toolchain,
    pub run_command: String,
    /// Run once in the project folder before any stage (any-framework projects).
    pub build: Option<String>,
    pub launch: steps::Launch,
    pub up_to: u32,
}

impl StepsRunner {
    /// Runs the learner's `build` command; a failure comes back as a failed "build your project" result.
    fn build(&self, build: &str, work_dir: &Path) -> Result<Option<TestResult>> {
        eprintln!("{} {build}", style("Building:").dim());
        let mut cmd = process::script(build);
        self.launch.apply_path(&mut cmd);
        let out = process::run(cmd, work_dir, None, BUILD_TIMEOUT)?;
        if out.status == Some(0) {
            return Ok(None);
        }
        let why = if out.timed_out {
            format!(
                "`{build}` didn't finish within {}s",
                BUILD_TIMEOUT.as_secs()
            )
        } else {
            format!(
                "`{build}` failed (exit code {})",
                out.status.map_or("none".into(), |c| c.to_string())
            )
        };
        let output = python::tail(&format!("{}{}", out.stdout, out.stderr), 1500);
        Ok(Some(TestResult {
            name: "build your project".into(),
            passed: false,
            message: Some(format!(
                "{why}:\n{output}\nFix it, or change `build` in prepkitty.toml."
            )),
        }))
    }
}

impl Runner for StepsRunner {
    fn run(&self, pack: &Pack, work_dir: &Path) -> Result<Vec<TestResult>> {
        if let Some(build) = &self.build {
            if let Some(failed) = self.build(build, work_dir)? {
                return Ok(vec![failed]);
            }
        }
        let classes = tempfile::tempdir()?;
        let toolchain = match &self.toolchain {
            Toolchain::Java { java, .. } => {
                let sources = java::java_sources(work_dir);
                if let Some(failed) = java.compile(&sources, classes.path(), work_dir)? {
                    return Ok(vec![failed]);
                }
                Toolchain::Java {
                    java: java.clone(),
                    classes: classes.path().to_path_buf(),
                }
            }
            other => other.clone(),
        };
        let mut all = Vec::new();
        for stage in pack
            .manifest
            .stages
            .iter()
            .filter(|s| s.stage <= self.up_to)
        {
            let stage_dir = pack.dir.join(&stage.dir);
            let results = steps::run_stage(
                &stage_dir,
                work_dir,
                &self.run_command,
                &toolchain,
                &pack.runner(),
                &self.launch,
            )?;
            let label = if stage.stage == self.up_to {
                String::new()
            } else {
                format!(" (stage {})", stage.stage)
            };
            all.extend(results.into_iter().map(|mut r| {
                r.name = format!("{}{label}", r.name);
                r
            }));
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::Manifest;

    #[test]
    fn a_failing_build_is_one_failed_result() {
        if cfg!(windows) {
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let manifest: Manifest = serde_json::from_str(
            r#"{"kind":"project","id":"p","title":"P","language":"java","packHash":"x","run":"java -jar app.jar","anyFramework":true,"stages":[]}"#,
        )
        .unwrap();
        let pack = Pack {
            dir: project.path().to_path_buf(),
            manifest,
        };
        let runner = StepsRunner {
            toolchain: Toolchain::Custom,
            run_command: "java -jar app.jar".into(),
            build: Some("echo boom >&2; exit 3".into()),
            launch: steps::Launch::default(),
            up_to: 1,
        };
        let results = runner.run(&pack, project.path()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "build your project");
        let msg = results[0].message.clone().unwrap();
        assert!(msg.contains("exit code 3") && msg.contains("boom"), "{msg}");
    }
}
