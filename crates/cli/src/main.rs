//! Command line for controlling damper.

mod commands;
mod doctor;
mod machine;
mod render;

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use damper_core::units::parse_duration;

/// Inspect and control the damper build broker.
#[derive(Debug, Parser)]
#[command(name = "damper", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the limit, the pressure inputs and the open sessions.
    Status {
        /// Print the daemon's status as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Show the daemon's event log.
    Log {
        /// Print the events as one JSON document.
        #[arg(long)]
        json: bool,
        /// Only events from the last duration, such as 90s, 10m or 1h.
        #[arg(long, value_parser = parse_since)]
        since: Option<Duration>,
        /// Only events of one session.
        #[arg(long)]
        session: Option<String>,
    },
    /// Explain what happened to one session.
    Explain {
        /// The session id, as shown by `damper status`.
        session: String,
        /// Print the events as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Configuration commands.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check that the install is wired up correctly.
    Doctor,
    /// Remove the disabled flag, so builds go through damper again.
    Enable,
    /// Set the disabled flag, so builds bypass damper.
    Disable,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the effective configuration and any warnings.
    Check,
}

fn parse_since(text: &str) -> Result<Duration, String> {
    parse_duration(text).map_err(|e| format!("{e} (use a number and s, m or h, such as 10m)"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let code = commands::run(&cli.command, &mut out, &mut err);
    // Nothing useful can be done if flushing the last output fails.
    let _ = out.flush();
    ExitCode::from(code)
}
