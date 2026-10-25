//! Declarative project-stage tests (tests.json): run a command, talk HTTP to a
//! server, or run a unittest file — each test in a fresh temp directory.

use std::collections::BTreeMap;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::Value;

use super::process;
use super::python::tail;
use super::Toolchain;
use crate::api::TestResult;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
pub struct StageFile {
    pub tests: Vec<StageTest>,
}

#[derive(Debug, Deserialize)]
pub struct StageTest {
    pub name: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub server: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Run {
        run: String,
        stdin: Option<String>,
        expect: RunExpect,
    },
    Http {
        http: String,
        body: Option<Value>,
        expect: HttpExpect,
    },
    Python {
        python: String,
    },
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunExpect {
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stdout_contains: Option<Vec<String>>,
    pub stdout_regex: Option<String>,
    pub stderr_contains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpExpect {
    pub status: Option<u16>,
    pub json: Option<Value>,
    pub body_contains: Option<Vec<String>>,
}

/// Expands `{run}` so it works from any cwd: `python3` becomes the detected
/// interpreter, `java` gets the compiled classes on its classpath, and file
/// arguments that exist in the project become absolute.
pub fn expand_run(run: &str, project_dir: &Path, toolchain: &Toolchain) -> String {
    run.split_whitespace()
        .enumerate()
        .map(|(i, tok)| {
            match toolchain {
                Toolchain::Python(python) if i == 0 && (tok == "python3" || tok == "python") => {
                    return python.bin.clone();
                }
                Toolchain::Java { java, classes } if i == 0 && tok == "java" => {
                    return format!(
                        "{} -cp {}",
                        quote(&java.java),
                        quote(&classes.to_string_lossy())
                    );
                }
                _ => {}
            }
            let candidate = project_dir.join(tok);
            if candidate.is_file() {
                return quote(&candidate.to_string_lossy());
            }
            tok.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote(s: &str) -> String {
    if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn run_stage(
    stage_dir: &Path,
    project_dir: &Path,
    run_cmd: &str,
    toolchain: &Toolchain,
    runner: &Path,
) -> Result<Vec<TestResult>> {
    let raw =
        fs::read_to_string(stage_dir.join("tests.json")).context("stage has no tests.json")?;
    let file: StageFile = serde_json::from_str(&raw).context("couldn't read this stage's tests")?;
    let run = expand_run(run_cmd, project_dir, toolchain);
    Ok(file
        .tests
        .iter()
        .map(|t| {
            match run_test(
                t,
                stage_dir,
                project_dir,
                (&run, run_cmd),
                toolchain,
                runner,
            ) {
                Ok(()) => TestResult {
                    name: t.name.clone(),
                    passed: true,
                    message: None,
                },
                Err(e) => TestResult {
                    name: t.name.clone(),
                    passed: false,
                    message: Some(format!("{e:#}")),
                },
            }
        })
        .collect())
}

struct Server {
    child: Child,
    port: u16,
    // Keeps the stderr log file alive for the server's lifetime.
    _log: tempfile::NamedTempFile,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `run` is (expanded command to execute, original command to show the learner).
fn run_test(
    t: &StageTest,
    stage_dir: &Path,
    project_dir: &Path,
    (run, run_display): (&str, &str),
    toolchain: &Toolchain,
    runner: &Path,
) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    for (name, content) in &t.files {
        let path = tmp.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
    }

    let server = if t.server {
        Some(start_server(run, tmp.path())?)
    } else {
        None
    };
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    for step in &t.steps {
        match step {
            Step::Run {
                run: cmd,
                stdin,
                expect,
            } => {
                let out = process::run(
                    process::shell(&cmd.replace("{run}", run)),
                    tmp.path(),
                    stdin.as_deref(),
                    STEP_TIMEOUT,
                )?;
                check_run(&cmd.replace("{run}", run_display), &out, expect)?;
            }
            Step::Http {
                http: line,
                body,
                expect,
            } => {
                let server = server.as_ref().context("http steps need `server: true`")?;
                let (method, path) = line.split_once(' ').unwrap_or(("GET", line));
                let url = format!("http://127.0.0.1:{}{}", server.port, path);
                let mut req = http.request(method.parse()?, &url);
                req = match body {
                    Some(Value::String(s)) => {
                        req.header("Content-Type", "text/plain").body(s.clone())
                    }
                    Some(v) => req.json(v),
                    None => req,
                };
                let resp = req
                    .send()
                    .with_context(|| format!("{line} failed: is your server still running?"))?;
                let status = resp.status().as_u16();
                let text = resp.text().unwrap_or_default();
                check_http(line, status, &text, expect)?;
            }
            Step::Python { python: file } => {
                let Toolchain::Python(python) = toolchain else {
                    anyhow::bail!("`python` steps only work in Python projects");
                };
                let results = python.run_unittest(
                    runner,
                    &stage_dir.join(file),
                    project_dir,
                    STEP_TIMEOUT.as_secs(),
                )?;
                if let Some(bad) = results.iter().find(|r| !r.passed) {
                    anyhow::bail!("{}: {}", bad.name, bad.message.clone().unwrap_or_default());
                }
            }
        }
    }
    Ok(())
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn start_server(run: &str, cwd: &Path) -> Result<Server> {
    let port = free_port()?;
    // Server stderr goes to a file so a chatty server can never block on a full pipe.
    let log = tempfile::NamedTempFile::new()?;
    let mut cmd = process::shell(run);
    cmd.current_dir(cwd)
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.reopen()?));
    let mut child = cmd.spawn().context("couldn't start your server")?;
    let start = Instant::now();
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(Server {
                child,
                port,
                _log: log,
            });
        }
        if let Some(status) = child.try_wait()? {
            let err = fs::read_to_string(log.path()).unwrap_or_default();
            anyhow::bail!(
                "your server exited (code {:?}) before listening on $PORT:\n{}",
                status.code(),
                tail(&err, 1200)
            );
        }
        if start.elapsed() > SERVER_START_TIMEOUT {
            let _ = child.kill();
            anyhow::bail!("your server didn't start listening on port $PORT ({port}) within 10s. Read the port from the PORT environment variable.");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn show(s: &str) -> String {
    if s.is_empty() {
        "(nothing)".into()
    } else {
        format!("{s:?}")
    }
}

fn check_run(cmd: &str, out: &process::Output, e: &RunExpect) -> Result<()> {
    if out.timed_out {
        anyhow::bail!("`{cmd}` didn't finish within {}s", STEP_TIMEOUT.as_secs());
    }
    let ctx = || {
        let mut s = format!("\n  ran: {cmd}");
        if !out.stderr.trim().is_empty() {
            s.push_str(&format!("\n  stderr: {}", tail(&out.stderr, 600)));
        }
        s
    };
    if let Some(code) = e.exit_code {
        if out.status != Some(code) {
            anyhow::bail!(
                "expected exit code {code}, got {}{}",
                out.status.map_or("none".into(), |c| c.to_string()),
                ctx()
            );
        }
    }
    if let Some(want) = &e.stdout {
        if out.stdout.trim_end() != want.replace("\r\n", "\n").trim_end() {
            anyhow::bail!(
                "expected output {}\n  but got        {}{}",
                show(want.trim_end()),
                show(out.stdout.trim_end()),
                ctx()
            );
        }
    }
    for needle in e.stdout_contains.iter().flatten() {
        if !out.stdout.contains(needle) {
            anyhow::bail!(
                "expected output to contain {needle:?}, got {}{}",
                show(out.stdout.trim_end()),
                ctx()
            );
        }
    }
    if let Some(pattern) = &e.stdout_regex {
        let re = RegexBuilder::new(pattern).multi_line(true).build()?;
        if !re.is_match(&out.stdout) {
            anyhow::bail!(
                "expected output to match /{pattern}/, got {}{}",
                show(out.stdout.trim_end()),
                ctx()
            );
        }
    }
    for needle in e.stderr_contains.iter().flatten() {
        if !out.stderr.contains(needle) {
            anyhow::bail!(
                "expected stderr to contain {needle:?}, got {}\n  ran: {cmd}",
                show(out.stderr.trim_end())
            );
        }
    }
    Ok(())
}

fn check_http(line: &str, status: u16, body: &str, e: &HttpExpect) -> Result<()> {
    if let Some(want) = e.status {
        if status != want {
            anyhow::bail!(
                "{line}: expected status {want}, got {status}\n  body: {}",
                show(&tail(body, 400))
            );
        }
    }
    if let Some(want) = &e.json {
        let got: Value = serde_json::from_str(body).with_context(|| {
            format!(
                "{line}: response isn't valid JSON: {}",
                show(&tail(body, 300))
            )
        })?;
        if !json_subset(want, &got) {
            anyhow::bail!("{line}: expected JSON including {want}\n  but got {got}");
        }
    }
    for needle in e.body_contains.iter().flatten() {
        if !body.contains(needle) {
            anyhow::bail!("{line}: expected body to contain {needle:?}");
        }
    }
    Ok(())
}

/// Objects match if every expected key matches; arrays must match element-wise.
pub fn json_subset(want: &Value, got: &Value) -> bool {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => w
            .iter()
            .all(|(k, v)| g.get(k).is_some_and(|gv| json_subset(v, gv))),
        (Value::Array(w), Value::Array(g)) => {
            w.len() == g.len() && w.iter().zip(g).all(|(a, b)| json_subset(a, b))
        }
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => want == got,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::runner::java::Java;
    use crate::runner::python::Python;

    #[test]
    fn json_subset_matches_nested_objects() {
        assert!(json_subset(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
        assert!(json_subset(
            &json!({"v": {"x": [1, 2]}}),
            &json!({"v": {"x": [1, 2], "y": 0}})
        ));
        assert!(!json_subset(&json!({"a": 1}), &json!({"a": 2})));
        assert!(!json_subset(&json!([1, 2]), &json!([1, 2, 3])));
        assert!(json_subset(&json!(1), &json!(1.0)));
    }

    #[test]
    fn parses_all_step_kinds() {
        let raw = r#"{"tests":[{"name":"t","files":{"a.txt":"x"},"server":true,"steps":[
            {"run":"{run} -c a.txt","expect":{"exitCode":0,"stdout":"1 a.txt\n"}},
            {"http":"GET /health","expect":{"status":200,"json":{"status":"ok"}}},
            {"python":"test_x.py"}]}]}"#;
        let f: StageFile = serde_json::from_str(raw).unwrap();
        assert_eq!(f.tests[0].steps.len(), 3);
        assert!(matches!(f.tests[0].steps[0], Step::Run { .. }));
        assert!(matches!(f.tests[0].steps[1], Step::Http { .. }));
        assert!(matches!(f.tests[0].steps[2], Step::Python { .. }));
    }

    #[test]
    fn expands_run_with_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pywc.py"), "").unwrap();
        let py = Toolchain::Python(Python {
            bin: "python3.12".into(),
        });
        let out = expand_run("python3 pywc.py --flag", dir.path(), &py);
        assert!(out.starts_with("python3.12 "));
        assert!(out.contains(&dir.path().join("pywc.py").to_string_lossy().to_string()));
        assert!(out.ends_with("--flag"));
    }

    #[test]
    fn expands_java_run_with_classpath() {
        let dir = tempfile::tempdir().unwrap();
        let java = Toolchain::Java {
            java: Java {
                javac: "javac".into(),
                java: "java".into(),
            },
            classes: dir.path().join("classes"),
        };
        let out = expand_run("java Main --port 1", dir.path(), &java);
        assert!(out.contains(" -cp "));
        assert!(out.contains("classes"));
        assert!(out.ends_with("Main --port 1"));
    }

    #[test]
    fn run_step_checks_stdout_and_exit_code() {
        let out = process::run(
            process::shell("echo hi"),
            Path::new("."),
            None,
            STEP_TIMEOUT,
        )
        .unwrap();
        let ok = RunExpect {
            exit_code: Some(0),
            stdout: Some("hi\n".into()),
            ..Default::default()
        };
        assert!(check_run("echo hi", &out, &ok).is_ok());
        let bad = RunExpect {
            stdout: Some("bye".into()),
            ..Default::default()
        };
        let err = check_run("echo hi", &out, &bad).unwrap_err().to_string();
        assert!(err.contains("expected output"));
    }

    #[test]
    fn run_step_times_out() {
        if cfg!(windows) {
            return;
        }
        let out = process::run(
            process::shell("sleep 5"),
            Path::new("."),
            None,
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(out.timed_out);
    }
}
