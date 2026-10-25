//! Running tests locally. A `Runner` turns a pack + the learner's folder into
//! a list of results; reporting is separate so other languages and runners
//! (in-browser, sandbox) can reuse the same result shape.

pub mod java;
pub mod process;
pub mod python;
pub mod steps;

use std::path::{Path, PathBuf};

use anyhow::Result;

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
}

/// Runs a project's stages 1..=`up_to` (earlier stages guard against regressions).
pub struct StepsRunner {
    pub toolchain: Toolchain,
    pub run_command: String,
    pub up_to: u32,
}

impl StepsRunner {
    /// A Java project: compiles every .java file once, then runs the stages against the classes.
    pub fn java(java: java::Java, run_command: String, up_to: u32) -> Self {
        StepsRunner {
            toolchain: Toolchain::Java {
                java,
                classes: PathBuf::new(),
            },
            run_command,
            up_to,
        }
    }
}

impl Runner for StepsRunner {
    fn run(&self, pack: &Pack, work_dir: &Path) -> Result<Vec<TestResult>> {
        let build = tempfile::tempdir()?;
        let toolchain = match &self.toolchain {
            Toolchain::Java { java, .. } => {
                let sources = java::java_sources(work_dir);
                if let Some(failed) = java.compile(&sources, build.path(), work_dir)? {
                    return Ok(vec![failed]);
                }
                Toolchain::Java {
                    java: java.clone(),
                    classes: build.path().to_path_buf(),
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
