mod config;
mod lint;
mod model;
mod procs;
mod report;
mod secrets;
mod signature;

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Keep MCP servers in check across coding agent sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Audit MCP configs and running servers (read-only)
    Doctor {
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Doctor { json } => doctor(json),
    }
}

fn doctor(json: bool) -> Result<()> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let loaded = config::load_all(&home);
    let sessions = procs::sessions(&procs::snapshot(), &loaded.servers);

    let path = env::var_os("PATH");
    let mut findings = lint::lint(&loaded.servers, path.as_deref(), &home);
    findings.extend(procs::findings(&sessions));
    findings.sort_by_key(|f| f.severity);

    let report = report::Report {
        servers: loaded.servers.iter().map(model::Server::redacted).collect(),
        sessions,
        findings,
        config_errors: loaded.errors,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report::render(&report));
    }
    Ok(())
}
