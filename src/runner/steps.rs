//! Declarative project-stage tests (tests.json): run a command, talk HTTP or
//! Redis (RESP) to a server, or run a unittest file — each test in a fresh temp
//! directory.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
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
use crate::pack::UPDATE_HINT;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const REDIS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_WAIT_MS: u64 = 2000;

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
        /// Extra environment variables; "" hides a variable the learner may have set.
        #[serde(default)]
        env: BTreeMap<String, String>,
        expect: RunExpect,
    },
    Http(HttpStep),
    /// Sends every request at once, each on its own connection, and checks
    /// that they all finish within `withinMs` (proves the server is concurrent).
    Parallel {
        parallel: Vec<HttpStep>,
        #[serde(rename = "withinMs")]
        within_ms: Option<u64>,
    },
    Python {
        python: String,
    },
    /// Sends one command as a RESP array and checks the single reply.
    Redis {
        redis: Vec<String>,
        /// Named connection, so a test can interleave clients (default "main").
        conn: Option<String>,
        expect: RedisExpect,
    },
    /// Sleeps for this many milliseconds (e.g. to let a key expire).
    Wait {
        wait: u64,
    },
    /// Anything else: a step kind added after this CLI was built, or a malformed step.
    Unknown(serde_json::Map<String, Value>),
}

const STEP_KINDS: &[&str] = &["run", "http", "parallel", "python", "redis", "wait"];

/// Explains a step this CLI can't run: either it's a newer kind, or it's broken.
fn unknown_step(step: &serde_json::Map<String, Value>) -> anyhow::Error {
    if let Some(kind) = step.keys().find(|k| STEP_KINDS.contains(&k.as_str())) {
        return anyhow::anyhow!(
            "this project has a malformed `{kind}` step: {}",
            Value::Object(step.clone())
        );
    }
    let kind = step
        .keys()
        .find(|k| *k != "expect")
        .map_or("unnamed", |k| k.as_str());
    anyhow::anyhow!(
        "this project's tests use a `{kind}` step, which needs a newer prepkitty (you have {}).\n{UPDATE_HINT}",
        env!("CARGO_PKG_VERSION")
    )
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
    /// Header name -> expected value prefix, both case-insensitive, so
    /// "text/html" matches "text/html; charset=utf-8".
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct HttpStep {
    pub http: String,
    pub body: Option<Value>,
    /// Extra request headers, e.g. `Connection: close`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub expect: HttpExpect,
}

#[derive(Debug, Deserialize)]
pub struct RedisExpect {
    /// The exact raw reply, e.g. "+PONG\r\n" or "$-1\r\n".
    pub reply: String,
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
    // Fail once, up front, rather than once per test.
    let unknown = file
        .tests
        .iter()
        .flat_map(|t| &t.steps)
        .find_map(|s| match s {
            Step::Unknown(step) => Some(step),
            _ => None,
        });
    if let Some(step) = unknown {
        return Err(unknown_step(step));
    }
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
    let mut redis: HashMap<String, BufReader<TcpStream>> = HashMap::new();

    for step in &t.steps {
        match step {
            Step::Run {
                run: cmd,
                stdin,
                env,
                expect,
            } => {
                let mut command = process::shell(&cmd.replace("{run}", run));
                command.envs(env);
                let out = process::run(command, tmp.path(), stdin.as_deref(), STEP_TIMEOUT)?;
                check_run(&cmd.replace("{run}", run_display), &out, expect)?;
            }
            Step::Http(step) => {
                let server = server.as_ref().context("http steps need `server: true`")?;
                send_http(&http, server.port, step)?;
            }
            Step::Parallel {
                parallel,
                within_ms,
            } => {
                let server = server
                    .as_ref()
                    .context("parallel steps need `server: true`")?;
                let start = Instant::now();
                // A client per request, so every request gets its own connection.
                let results: Vec<Result<()>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = parallel
                        .iter()
                        .map(|step| {
                            scope.spawn(move || {
                                let client = reqwest::blocking::Client::builder()
                                    .timeout(Duration::from_secs(5))
                                    .build()?;
                                send_http(&client, server.port, step)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or_else(|_| Err(anyhow::anyhow!("request thread panicked")))
                        })
                        .collect()
                });
                for r in results {
                    r?;
                }
                let elapsed = start.elapsed();
                if let Some(limit) = within_ms {
                    if elapsed > Duration::from_millis(*limit) {
                        anyhow::bail!(
                            "{} requests sent at the same time took {}ms in total, but should finish within {limit}ms. \
                             Is your server handling one connection at a time?",
                            parallel.len(),
                            elapsed.as_millis()
                        );
                    }
                }
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
            Step::Redis {
                redis: args,
                conn,
                expect,
            } => {
                let server = server.as_ref().context("redis steps need `server: true`")?;
                let name = conn.as_deref().unwrap_or("main");
                let stream = match redis.entry(name.to_string()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let s = TcpStream::connect(("127.0.0.1", server.port))
                            .context("couldn't connect to your server")?;
                        s.set_read_timeout(Some(REDIS_TIMEOUT))?;
                        e.insert(BufReader::new(s))
                    }
                };
                let line = match conn {
                    Some(c) => format!("{} (client {c})", args.join(" ")),
                    None => args.join(" "),
                };
                stream
                    .get_mut()
                    .write_all(&encode_command(args))
                    .with_context(|| {
                        format!("{line}: couldn't send: is your server still running?")
                    })?;
                let got = read_reply(stream).map_err(|e| anyhow::anyhow!("{line}: {e}"))?;
                if got != expect.reply {
                    anyhow::bail!(
                        "{line}: expected reply {:?}\n  but got        {:?}",
                        expect.reply,
                        got
                    );
                }
            }
            Step::Wait { wait } => {
                std::thread::sleep(Duration::from_millis((*wait).min(MAX_WAIT_MS)));
            }
            Step::Unknown(step) => return Err(unknown_step(step)),
        }
    }
    Ok(())
}

/// Encodes a command the way redis-cli sends it: an array of bulk strings.
fn encode_command(args: &[String]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n{a}\r\n", a.len()).as_bytes());
    }
    out
}

/// Reads exactly one RESP reply and returns its raw text (nested arrays included).
fn read_reply(r: &mut impl BufRead) -> Result<String> {
    let mut raw = Vec::new();
    read_value(r, &mut raw)?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn read_value(r: &mut impl BufRead, raw: &mut Vec<u8>) -> Result<()> {
    let start = raw.len();
    let n = r.read_until(b'\n', raw).map_err(io_hint)?;
    if n == 0 {
        anyhow::bail!("your server closed the connection without replying");
    }
    let line = String::from_utf8_lossy(&raw[start..]).into_owned();
    let Some(head) = line.strip_suffix("\r\n") else {
        anyhow::bail!("reply lines must end with \\r\\n, got {line:?}");
    };
    let count = || {
        head[1..]
            .parse::<i64>()
            .with_context(|| format!("bad length in {line:?}"))
    };
    match head.as_bytes().first() {
        Some(b'+' | b'-' | b':') => Ok(()),
        Some(b'$') => {
            let len = count()?;
            if len >= 0 {
                let mut buf = vec![0; len as usize + 2];
                r.read_exact(&mut buf).map_err(io_hint)?;
                raw.extend_from_slice(&buf);
            }
            Ok(())
        }
        Some(b'*') => {
            for _ in 0..count()?.max(0) {
                read_value(r, raw)?;
            }
            Ok(())
        }
        _ => anyhow::bail!("{line:?} isn't a RESP reply (it should start with + - : $ or *)"),
    }
}

fn io_hint(e: std::io::Error) -> anyhow::Error {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => anyhow::anyhow!(
            "no complete reply within {}s. Does every reply end with \\r\\n? \
             Can your server serve a second client while another is connected?",
            REDIS_TIMEOUT.as_secs()
        ),
        ErrorKind::UnexpectedEof => anyhow::anyhow!("your server closed the connection mid-reply"),
        _ => e.into(),
    }
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

fn send_http(client: &reqwest::blocking::Client, port: u16, step: &HttpStep) -> Result<()> {
    let line = &step.http;
    let (method, path) = line.split_once(' ').unwrap_or(("GET", line));
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut req = client.request(method.parse()?, &url);
    req = match &step.body {
        Some(Value::String(s)) => req.header("Content-Type", "text/plain").body(s.clone()),
        Some(v) => req.json(v),
        None => req,
    };
    for (name, value) in &step.headers {
        req = req.header(name, value);
    }
    let resp = req
        .send()
        .with_context(|| format!("{line} failed: is your server still running?"))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().unwrap_or_default();
    check_http(line, status, &text, &step.expect)?;
    check_headers(line, &headers, &step.expect.headers)
}

fn check_headers(
    line: &str,
    got: &reqwest::header::HeaderMap,
    want: &BTreeMap<String, String>,
) -> Result<()> {
    for (name, prefix) in want {
        let Some(value) = got.get(name.as_str()) else {
            anyhow::bail!("{line}: expected a {name} header, but the response has none");
        };
        let value = String::from_utf8_lossy(value.as_bytes());
        if !value
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
        {
            anyhow::bail!(
                "{line}: expected header {name}: {prefix}\n  but got         {name}: {value}"
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
            {"run":"{run}","env":{"TOKEN":""},"expect":{}},
            {"http":"GET /health","headers":{"Connection":"close"},"expect":{"status":200,"headers":{"Content-Type":"text/html"}}},
            {"parallel":[{"http":"GET /slow","expect":{}}],"withinMs":1500},
            {"python":"test_x.py"},
            {"redis":["SET","k","v"],"conn":"b","expect":{"reply":"+OK\r\n"}},
            {"wait":200}]}]}"#;
        let f: StageFile = serde_json::from_str(raw).unwrap();
        assert_eq!(f.tests[0].steps.len(), 7);
        assert!(matches!(f.tests[0].steps[0], Step::Run { .. }));
        assert!(matches!(&f.tests[0].steps[1], Step::Run { env, .. } if env["TOKEN"].is_empty()));
        assert!(matches!(&f.tests[0].steps[2], Step::Http(h) if h.expect.headers.len() == 1));
        assert!(matches!(
            &f.tests[0].steps[3],
            Step::Parallel { parallel, within_ms: Some(1500) } if parallel.len() == 1
        ));
        assert!(matches!(f.tests[0].steps[4], Step::Python { .. }));
        assert!(matches!(f.tests[0].steps[5], Step::Redis { .. }));
        assert!(matches!(f.tests[0].steps[6], Step::Wait { wait: 200 }));
    }

    /// Answers every connection with one canned HTTP response.
    fn canned_server(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let _ = reader.get_mut().write_all(response.as_bytes());
            }
        });
        port
    }

    #[test]
    fn http_step_checks_header_prefixes() {
        let port = canned_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/HTML; charset=utf-8\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
        );
        let client = reqwest::blocking::Client::new();
        let step = |want: &str| HttpStep {
            http: "GET /".into(),
            body: None,
            headers: BTreeMap::new(),
            expect: HttpExpect {
                headers: [("content-type".to_string(), want.to_string())].into(),
                ..Default::default()
            },
        };
        assert!(send_http(&client, port, &step("text/html")).is_ok());
        let err = send_http(&client, port, &step("text/css"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("expected header content-type: text/css"),
            "{err}"
        );
    }

    #[test]
    fn unknown_steps_ask_for_an_update() {
        let raw = r#"{"tests":[{"name":"t","steps":[
            {"teleport":"GET /","expect":{}},
            {"run":"{run}"}]}]}"#;
        let f: StageFile = serde_json::from_str(raw).unwrap();
        let [Step::Unknown(newer), Step::Unknown(broken)] = &f.tests[0].steps[..] else {
            panic!("expected two unknown steps, got {:?}", f.tests[0].steps);
        };
        let msg = unknown_step(newer).to_string();
        assert!(
            msg.contains("`teleport` step") && msg.contains("newer prepkitty"),
            "{msg}"
        );
        assert!(msg.contains("install.sh"), "{msg}");
        let msg = unknown_step(broken).to_string();
        assert!(msg.contains("malformed `run` step"), "{msg}");
    }

    #[test]
    fn encodes_commands_as_bulk_arrays() {
        let cmd = encode_command(&["ECHO".into(), "hey".into()]);
        assert_eq!(cmd, b"*2\r\n$4\r\nECHO\r\n$3\r\nhey\r\n");
    }

    #[test]
    fn reads_one_resp_reply_at_a_time() {
        let mut r = BufReader::new(
            &b"+PONG\r\n$-1\r\n$5\r\nhi\r\no\r\n*2\r\n:1\r\n*1\r\n$1\r\nx\r\n-ERR no\r\n"[..],
        );
        assert_eq!(read_reply(&mut r).unwrap(), "+PONG\r\n");
        assert_eq!(read_reply(&mut r).unwrap(), "$-1\r\n");
        assert_eq!(read_reply(&mut r).unwrap(), "$5\r\nhi\r\no\r\n");
        assert_eq!(read_reply(&mut r).unwrap(), "*2\r\n:1\r\n*1\r\n$1\r\nx\r\n");
        assert_eq!(read_reply(&mut r).unwrap(), "-ERR no\r\n");
        assert!(read_reply(&mut r)
            .unwrap_err()
            .to_string()
            .contains("closed"));
    }

    #[test]
    fn rejects_replies_without_crlf() {
        let mut r = BufReader::new(&b"+PONG\n"[..]);
        assert!(read_reply(&mut r)
            .unwrap_err()
            .to_string()
            .contains("\\r\\n"));
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
