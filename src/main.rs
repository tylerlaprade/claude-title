use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "claude-title")]
#[command(version)]
#[command(about = "Show Claude Code's live state in the terminal tab title")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Install Claude Code hooks")]
    Install,
    #[command(about = "Remove Claude Code hooks")]
    Uninstall,
    #[command(hide = true)]
    Hook,
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        tty: PathBuf,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        lock: PathBuf,
        #[arg(long)]
        pid: u32,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install => claude_title::config::install_default(),
        Command::Uninstall => claude_title::config::uninstall_default(),
        Command::Hook => claude_title::hook::run(),
        Command::Daemon {
            tty,
            state,
            lock,
            pid,
        } => claude_title::daemon::run(&tty, &state, &lock, pid),
    }
}
