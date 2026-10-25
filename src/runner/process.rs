//! Spawning learner programs with stdin, captured output and a hard timeout.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Builds a shell command. On Unix we `exec` so killing the shell kills the program.
pub fn shell(command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("exec {command}")]);
        c
    }
}

pub fn run(mut cmd: Command, cwd: &Path, stdin: Option<&str>, timeout: Duration) -> Result<Output> {
    cmd.current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("couldn't start the program")?;

    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_string();
        thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    let out_reader = reader(child.stdout.take());
    let err_reader = reader(child.stderr.take());

    let (status, timed_out) = wait(&mut child, timeout)?;
    Ok(Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        timed_out,
    })
}

fn reader<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).replace("\r\n", "\n")
    })
}

pub fn wait(child: &mut Child, timeout: Duration) -> Result<(Option<i32>, bool)> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), false));
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((None, true));
        }
        thread::sleep(Duration::from_millis(15));
    }
}
