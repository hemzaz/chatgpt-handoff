//! Binary entry point.
//!
//! The only responsibilities here are wiring up logging, running the CLI, and
//! turning a typed error into a readable message plus an exit code. All logs
//! and diagnostics go to stderr so that `--json` stdout stays machine-readable.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use chatgpt_handoff::cli::{self, Cli};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli);

    match cli::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error);
            ExitCode::FAILURE
        }
    }
}

/// `RUST_LOG` wins if set; otherwise the level comes from `-v` flags.
fn init_logging(cli: &Cli) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cli.log_filter()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Print the full error chain, one cause per line, to stderr.
fn report(error: &anyhow::Error) {
    eprintln!("error: {error}");
    for cause in error.chain().skip(1) {
        eprintln!("  caused by: {cause}");
    }
}
