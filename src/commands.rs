//! The `prepkitty` subcommands.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use console::style;

use crate::api::{Api, Item, Submission, Summary, TestResult, TokenResponse};
use crate::config::{self, Credentials};
use crate::git;
use crate::kitty::{self, Mood};
use crate::pack::{self, Pack};
use crate::runner::{
    java::Java, python::Python, JavaTestRunner, Runner, StepsRunner, Toolchain, UnittestRunner,
};
use crate::workspace::{self, ItemConfig};

/// Ignores build output and tool caches for every supported language.
const GITIGNORE: &str = "__pycache__/\n*.pyc\n.venv/\n*.class\nout/\ntarget/\n";

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn cwd() -> Result<PathBuf> {
    env::current_dir().context("can't read the current directory")
}

fn display_path(p: &Path) -> String {
    let base = cwd().unwrap_or_default();
    p.strip_prefix(&base)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

fn mood_line(s: &Summary) -> String {
    match &s.kitty {
        Some(k) => format!(
            "{} the {} is {}.",
            k.name,
            k.stage.replace("codecat", "code cat"),
            describe_mood(&k.mood)
        ),
        None => "Finish onboarding on the website to adopt your kitty.".into(),
    }
}

fn describe_mood(m: &str) -> &'static str {
    match m {
        "happy" => "happy and full",
        "content" => "nibbling away",
        "hungry" => "hungry for a lesson",
        "sleepy" => "sleepy. It's been a few days",
        "sad" => "missing you",
        _ => "curious",
    }
}

fn goal_line(s: &Summary) -> String {
    format!(
        "Level {} ({} XP to next)   streak {} day{}   treats {}   today {}/{} XP",
        s.level,
        s.xp_to_next,
        s.streak,
        if s.streak == 1 { "" } else { "s" },
        s.treats,
        s.xp_today,
        s.daily_goal
    )
}

// ---------------------------------------------------------------- login

pub fn login(api_arg: Option<String>) -> Result<ExitCode> {
    let mut api = Api::new(None)?;
    // A fresh login ignores the server saved by an earlier one (e.g. a local dev server).
    api.base = api_arg
        .map(|a| a.trim_end_matches('/').to_string())
        .unwrap_or_else(|| config::api_base(None));

    let dc = api.device_code().context("couldn't start sign-in")?;
    let code = if dc.user_code.len() == 8 {
        format!("{}-{}", &dc.user_code[..4], &dc.user_code[4..])
    } else {
        dc.user_code.clone()
    };
    let url = dc.verification_uri_complete.clone().unwrap_or_else(|| {
        let sep = if dc.verification_uri.contains('?') {
            '&'
        } else {
            '?'
        };
        format!("{}{sep}user_code={}", dc.verification_uri, dc.user_code)
    });
    let url = api.url(&url);

    kitty::say(
        Mood::Content,
        &[
            format!("Your code: {}", style(&code).bold().yellow()),
            format!("Approve it at {}", style(&url).underlined()),
            "Waiting for you to approve in the browser…".into(),
        ],
    );
    let _ = open::that(&url);

    let mut interval = dc.interval.unwrap_or(5).max(1);
    let deadline = Instant::now() + Duration::from_secs(dc.expires_in);
    let token = loop {
        if Instant::now() > deadline {
            bail!("The code expired. Run `prepkitty login` again.");
        }
        thread::sleep(Duration::from_secs(interval));
        match api.device_token(&dc.device_code)? {
            TokenResponse::Token { access_token } => break access_token,
            TokenResponse::Error {
                error,
                error_description,
            } => match error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => interval += 5,
                "access_denied" => bail!("Sign-in was cancelled in the browser."),
                "expired_token" => bail!("The code expired. Run `prepkitty login` again."),
                _ => bail!("Sign-in failed: {}", error_description.unwrap_or(error)),
            },
        }
    };

    let mut creds = Credentials {
        api: api.base.clone(),
        token,
        name: None,
    };
    let me = Api::new(Some(&creds))?.me()?;
    creds.name = Some(me.name.clone());
    config::save_credentials(&creds)?;
    println!();
    kitty::say(
        Mood::Celebrating,
        &[
            format!("Signed in as {}!", style(&me.name).bold()),
            mood_line(&me),
            "Next: `prepkitty next` shows what to work on.".into(),
        ],
    );
    Ok(ExitCode::SUCCESS)
}

pub fn logout() -> Result<ExitCode> {
    if config::delete_credentials()? {
        println!("Signed out. Your kitty will wait for you.");
    } else {
        println!("You weren't signed in.");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn whoami() -> Result<ExitCode> {
    let api = Api::authed()?;
    let me = api.me()?;
    let handle = me
        .username
        .as_ref()
        .map(|u| format!(" (@{u})"))
        .unwrap_or_default();
    println!("{}{} on {}", style(&me.name).bold(), handle, api.base);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- start

pub fn start(id: &str, dir: Option<PathBuf>) -> Result<ExitCode> {
    if !git::available() {
        bail!("PrepKitty uses git to track your work. Install git from https://git-scm.com and try again.");
    }
    let api = Api::authed()?;
    let item = api.item(id, true)?;
    let pack = pack::ensure(&api, &item)?;
    let m = &pack.manifest;
    let here = cwd()?;

    let target = if m.kind == "exercise" {
        let ws = match workspace::find_workspace(&here) {
            Some(ws) => ws,
            None => {
                let ws = dir
                    .clone()
                    .unwrap_or_else(|| here.join(format!("prepkitty-{}", m.language)));
                create_workspace(&ws, &m.language)?;
                ws
            }
        };
        ws.join(&m.id)
    } else {
        dir.unwrap_or_else(|| here.join(&m.id))
    };

    let already = target.join(workspace::ITEM_FILE).exists();
    fs::create_dir_all(&target)?;
    pack::copy_missing(&pack.dir.join("starter"), &target)?;
    if !target.join("README.md").exists() {
        fs::copy(pack.dir.join("README.md"), target.join("README.md"))?;
    }
    if !already {
        let config = ItemConfig {
            id: m.id.clone(),
            kind: m.kind.clone(),
            language: m.language.clone(),
            run: m.run.clone(),
            stage: if m.kind == "project" {
                Some(item.current_stage.unwrap_or(1))
            } else {
                None
            },
        };
        workspace::write_config(&target, &config)?;
    }
    if m.kind == "project" {
        if !target.join(".gitignore").exists() {
            fs::write(target.join(".gitignore"), GITIGNORE)?;
        }
        if !git::is_repo(&target) {
            git::init(&target)?;
        }
    }
    if !already
        && git::dirty_files(&target)
            .map(|d| !d.is_empty())
            .unwrap_or(false)
    {
        git::commit_all(&target, &format!("Start {}", m.id))?;
    }

    let rel = display_path(&target);
    let entry = m
        .entry
        .clone()
        .unwrap_or_else(|| "the starter files".into());
    let first = if m.kind == "project" {
        format!(
            "Read README.md, then stage {}.",
            item.current_stage.unwrap_or(1)
        )
    } else {
        format!("Open {entry} and read README.md.")
    };
    kitty::say(
        Mood::Happy,
        &[
            format!(
                "{} {}",
                if already { "Welcome back to" } else { "Ready:" },
                style(&m.title).bold()
            ),
            format!("cd {}", style(&rel).cyan()),
            format!("{first} Then run `prepkitty test`."),
        ]
        .into_iter()
        .chain((!m.requirements.is_empty()).then(|| {
            "Your first test run installs this project's packages into .venv.".to_string()
        }))
        .collect::<Vec<_>>(),
    );
    Ok(ExitCode::SUCCESS)
}

fn create_workspace(ws: &Path, language: &str) -> Result<()> {
    fs::create_dir_all(ws)?;
    fs::write(
        ws.join(workspace::WORKSPACE_MARKER),
        "PrepKitty exercises workspace\n",
    )?;
    if !ws.join("README.md").exists() {
        fs::write(
            ws.join("README.md"),
            format!("# PrepKitty {language} exercises\n\nOne folder per exercise. Commit as you go, then run `prepkitty submit` inside a folder.\n"),
        )?;
    }
    if !ws.join(".gitignore").exists() {
        fs::write(ws.join(".gitignore"), GITIGNORE)?;
    }
    if !git::is_repo(ws) {
        git::init(ws)?;
    }
    git::commit_all(ws, "Start PrepKitty workspace")?;
    Ok(())
}

// ---------------------------------------------------------------- test / submit

/// Resolves the item for the cwd and its pack, falling back to the cache offline.
fn load(ctx: &workspace::Context) -> Result<(Option<Item>, Option<Api>, Pack)> {
    let api = Api::authed().ok();
    if let Some(api) = api {
        match api.item(&ctx.config.id, false) {
            Ok(item) => {
                let pack = pack::ensure(&api, &item)?;
                return Ok((Some(item), Some(api), pack));
            }
            Err(e) => {
                if let Some(pack) = pack::cached(&ctx.config.id)? {
                    eprintln!("{} {e:#}. Using cached tests.", style("warning:").yellow());
                    return Ok((None, Some(api), pack));
                }
                return Err(e);
            }
        }
    }
    match pack::cached(&ctx.config.id)? {
        Some(pack) => Ok((None, None, pack)),
        None => bail!("You're not signed in. Run `prepkitty login` first."),
    }
}

fn target_stage(
    ctx: &workspace::Context,
    item: Option<&Item>,
    pack: &Pack,
    explicit: Option<u32>,
) -> u32 {
    let last = pack
        .manifest
        .stages
        .iter()
        .map(|s| s.stage)
        .max()
        .unwrap_or(1);
    explicit
        .or_else(|| {
            item.and_then(|i| {
                if i.completed {
                    Some(last)
                } else {
                    i.current_stage
                }
            })
        })
        .or(ctx.config.stage)
        .unwrap_or(1)
        .clamp(1, last)
}

fn run_tests(pack: &Pack, dir: &Path, stage: u32) -> Result<Vec<TestResult>> {
    let is_java = pack.manifest.language == "java";
    if pack.manifest.kind == "project" {
        let run = pack
            .manifest
            .run
            .clone()
            .context("project pack has no run command")?;
        if is_java {
            return StepsRunner::java(Java::find()?, run, stage).run(pack, dir);
        }
        let python = Python::find()?.with_requirements(dir, &pack.manifest.requirements)?;
        StepsRunner {
            toolchain: Toolchain::Python(python),
            run_command: run,
            up_to: stage,
        }
        .run(pack, dir)
    } else if is_java {
        JavaTestRunner {
            java: Java::find()?,
        }
        .run(pack, dir)
    } else {
        UnittestRunner {
            python: Python::find()?,
        }
        .run(pack, dir)
    }
}

fn header(pack: &Pack, stage: u32) {
    let m = &pack.manifest;
    if m.kind == "project" {
        let title = m
            .stages
            .iter()
            .find(|s| s.stage == stage)
            .map(|s| s.title.as_str())
            .unwrap_or("");
        println!(
            "{} {} (stage {} of {}: {})",
            style("Testing").bold(),
            m.title,
            stage,
            m.stages.len(),
            title
        );
    } else {
        println!("{} {}", style("Testing").bold(), m.title);
    }
}

/// Prints results; returns whether everything passed.
pub fn report(results: &[TestResult]) -> bool {
    for r in results {
        if r.passed {
            println!("  {} {}", style("✓").green().bold(), r.name);
        } else {
            println!("  {} {}", style("✗").red().bold(), style(&r.name).red());
            if let Some(msg) = &r.message {
                for line in msg.lines().take(14) {
                    println!("      {}", style(line).dim());
                }
            }
        }
    }
    let passed = results.iter().filter(|r| r.passed).count();
    let all = !results.is_empty() && passed == results.len();
    println!();
    println!(
        "  {}",
        if all {
            style(format!("{passed}/{} tests passed", results.len()))
                .green()
                .bold()
        } else {
            style(format!("{passed}/{} tests passed", results.len()))
                .red()
                .bold()
        }
    );
    println!();
    all
}

pub fn test(stage: Option<u32>) -> Result<ExitCode> {
    let ctx = workspace::find(&cwd()?)?;
    let (item, _, pack) = load(&ctx)?;
    let stage = target_stage(&ctx, item.as_ref(), &pack, stage);
    header(&pack, stage);
    let results = run_tests(&pack, &ctx.dir, stage)?;
    let ok = report(&results);
    if ok {
        kitty::say(
            Mood::Happy,
            &[
                style("All tests passed!").green().bold().to_string(),
                "Commit your work, then run `prepkitty submit`.".into(),
                String::new(),
            ],
        );
        Ok(ExitCode::SUCCESS)
    } else {
        kitty::say(
            Mood::Thinking,
            &[
                "Not quite yet.".into(),
                "Read the first failing test's message: it says what to fix.".into(),
                String::new(),
            ],
        );
        Ok(ExitCode::from(1))
    }
}

pub fn submit() -> Result<ExitCode> {
    let ctx = workspace::find(&cwd()?)?;
    let api = Api::authed()?;
    let item = api.item(&ctx.config.id, false)?;
    let pack = pack::ensure(&api, &item)?;

    if pack.manifest.kind == "project" && item.completed {
        kitty::say(
            Mood::Happy,
            &[
                "You've already completed this project!".into(),
                "Try `prepkitty next`.".into(),
            ],
        );
        return Ok(ExitCode::SUCCESS);
    }
    if !git::is_repo(&ctx.dir) {
        bail!("This folder isn't a git repository. Run `git init`, commit your work, then submit.");
    }
    let dirty = git::dirty_files(&ctx.dir)?;
    if !dirty.is_empty() {
        let list = dirty
            .iter()
            .take(8)
            .map(|f| format!("    {f}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "You have uncommitted changes:\n{list}\n  Commit them first:\n    git add -A && git commit -m \"Work on {}\"",
            ctx.config.id
        );
    }
    let sha = git::head_sha(&ctx.dir)?;

    let stage = if pack.manifest.kind == "project" {
        target_stage(&ctx, Some(&item), &pack, None)
    } else {
        0
    };
    header(&pack, stage.max(1));
    let results = run_tests(&pack, &ctx.dir, stage.max(1))?;
    let passed = report(&results);

    let resp = api.submit(&Submission {
        item_id: &item.id,
        stage,
        pack_hash: &item.pack_hash,
        commit_sha: &sha,
        cli_version: VERSION,
        passed,
        results: &results,
    })?;
    let o = &resp.outcome;

    if !o.passed {
        kitty::say(
            Mood::Thinking,
            &[
                "Submission recorded, but some tests are failing.".into(),
                "Fix them, commit, and submit again.".into(),
                String::new(),
            ],
        );
        return Ok(ExitCode::from(1));
    }

    let mut lines = Vec::new();
    match &o.reward {
        Some(r) => {
            lines.push(format!(
                "{} +{} XP  +{} treats",
                style(if o.project_complete == Some(true) {
                    "Project complete!"
                } else {
                    "Verified!"
                })
                .green()
                .bold(),
                r.xp,
                r.treats
            ));
            if r.level_after > r.level_before {
                lines.push(format!("Level up! You're now level {}.", r.level_after));
            }
            if r.daily_goal_reached {
                lines.push("Daily goal reached. Your kitty's bowl is full!".into());
            } else if r.streak_extended && r.streak > 1 {
                lines.push(format!("{}-day streak!", r.streak));
            }
        }
        None if o.first_completion => lines.push(style("Verified!").green().bold().to_string()),
        None => lines.push("Verified again. You already earned the XP for this one.".into()),
    }
    for b in &o.badges {
        lines.push(format!("New badge: {} {}", b.emoji, b.name));
    }
    for i in &o.items {
        lines.push(format!("Unlocked for your kitty: {}", i.name));
    }
    if let Some(next) = &o.next_stage {
        lines.push(format!(
            "Next up: stage {}, {}. Run `prepkitty test`.",
            next.stage, next.title
        ));
        let mut ctx = ctx;
        ctx.config.stage = Some(next.stage);
        ctx.save()?;
        if git::dirty_files(&ctx.dir)
            .map(|d| !d.is_empty())
            .unwrap_or(false)
        {
            git::commit_all(&ctx.dir, &format!("Advance to stage {}", next.stage))?;
        }
    }
    lines.push(goal_line(&resp.summary));
    kitty::say(Mood::Celebrating, &lines);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- status / next / open

pub fn status() -> Result<ExitCode> {
    let me = Api::authed()?.me()?;
    let mood = me
        .kitty
        .as_ref()
        .map(|k| Mood::from_api(&k.mood))
        .unwrap_or(Mood::Content);
    kitty::say(
        mood,
        &[
            format!("Hi {}!", style(&me.name).bold()),
            mood_line(&me),
            goal_line(&me),
        ],
    );
    if !me.onboarded {
        println!("\n  Finish setting up your plan on the website first.");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn next() -> Result<ExitCode> {
    let api = Api::authed()?;
    let next = api.next()?;
    if next.items.is_empty() {
        println!(
            "Nothing queued. Open Explore to pick something: {}/explore",
            api.base
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", style("Up next").bold());
    for i in &next.items {
        let how = match i.kind.as_str() {
            "exercise" | "project" => format!("prepkitty start {}", i.id),
            _ => i.web_url.clone(),
        };
        println!(
            "  {} {}  {}",
            style(format!("[{}]", i.kind)).dim(),
            i.title,
            style(format!("+{} XP", i.xp)).yellow()
        );
        println!("      {}", style(how).cyan());
    }
    Ok(ExitCode::SUCCESS)
}

pub fn open() -> Result<ExitCode> {
    let api = Api::authed()?;
    let url = match workspace::find(&cwd()?) {
        Ok(ctx) => api.item(&ctx.config.id, false)?.web_url,
        Err(_) => format!("{}/dashboard", api.base),
    };
    println!("Opening {url}");
    let _ = open::that(&url);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- dev

pub fn dev_run_pack(pack_dir: &Path, dir: &Path, stage: Option<u32>) -> Result<ExitCode> {
    let pack = Pack::open(pack_dir)?;
    let last = pack
        .manifest
        .stages
        .iter()
        .map(|s| s.stage)
        .max()
        .unwrap_or(1);
    let stage = stage.unwrap_or(last);
    header(&pack, stage);
    let dir = fs::canonicalize(dir)?;
    let ok = report(&run_tests(&pack, &dir, stage)?);
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
