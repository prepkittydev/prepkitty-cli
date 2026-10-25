//! `prepkitty`: start, test and submit PrepKitty exercises and projects locally.

mod api;
mod commands;
mod config;
mod git;
mod kitty;
mod pack;
mod runner;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use console::style;

#[derive(Parser)]
#[command(
    name = "prepkitty",
    version,
    about = "Learn to code with your own kitty. Tests run on your machine."
)]
struct Cli {
    /// Hide the ASCII kitty.
    #[arg(long, global = true, env = "PREPKITTY_QUIET", value_parser = clap::builder::FalseyValueParser::new())]
    no_kitty: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect this computer to your PrepKitty account.
    Login {
        /// API to sign in to (defaults to PREPKITTY_API or the hosted site).
        #[arg(long)]
        api: Option<String>,
        /// Sign in again even if already signed in, without asking.
        #[arg(long)]
        force: bool,
    },
    /// Sign out on this computer and end the session.
    Logout,
    /// Show who you're signed in as.
    Whoami,
    /// Download an exercise or project and set up a folder for it.
    Start {
        /// Item id, e.g. py-ex-greeting or py-proj-pywc.
        id: String,
        /// Where to create it (default: ./prepkitty-<language> for exercises, ./<id> for projects).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Run the tests for the exercise or project in this folder.
    Test {
        /// Projects: run up to this stage instead of your current one.
        #[arg(long)]
        stage: Option<u32>,
    },
    /// Run the tests and, if they pass, record your progress.
    Submit,
    /// Your kitty, streak and today's goal.
    Status,
    /// What to work on next.
    Next,
    /// Open the current item (or your dashboard) in the browser.
    Open,
    /// Tools for content authors.
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
}

#[derive(Subcommand)]
enum DevCommand {
    /// Run an extracted pack's tests against a folder (used by content CI).
    RunPack {
        #[arg(long)]
        pack: PathBuf,
        #[arg(long)]
        dir: PathBuf,
        /// Projects: highest stage to run (default: all).
        #[arg(long)]
        stage: Option<u32>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    kitty::set_quiet(cli.no_kitty);
    let result = match cli.command {
        Command::Login { api, force } => commands::login(api, force),
        Command::Logout => commands::logout(),
        Command::Whoami => commands::whoami(),
        Command::Start { id, dir } => commands::start(&id, dir),
        Command::Test { stage } => commands::test(stage),
        Command::Submit => commands::submit(),
        Command::Status => commands::status(),
        Command::Next => commands::next(),
        Command::Open => commands::open(),
        Command::Dev {
            command: DevCommand::RunPack { pack, dir, stage },
        } => commands::dev_run_pack(&pack, &dir, stage),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", style("error:").red().bold());
            ExitCode::from(2)
        }
    }
}
