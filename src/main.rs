mod config;
mod daemon;
mod lint;
mod mock;
mod model;
mod procs;
mod proxy;
mod report;
mod router;
mod secrets;
mod share;
mod signature;
mod wrap;

use std::env;
use std::path::PathBuf;
use std::time::Duration;

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
    /// Run one copy of each server for all sessions, instead of one copy per session
    Share {
        /// Only these servers (default: every server that is safe to share)
        names: Vec<String>,
        /// Leave these servers alone, now and in later runs (naming one shares it again)
        #[arg(long, value_name = "NAME")]
        exclude: Vec<String>,
        /// Show what would change without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Undo `share`: entries run their server directly again
    Unshare {
        /// Only these servers (default: every shared server)
        names: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Show the shared servers and the sessions using them
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop the daemon and every shared server (sessions reconnect on next start)
    Stop,
    /// Run a server through the daemon (this is what shared config entries call)
    #[command(hide = true)]
    Proxy {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        project: bool,
        #[arg(long, value_delimiter = ',')]
        env: Vec<String>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Run the daemon in the foreground
    #[command(hide = true)]
    Daemon {
        /// Seconds a server keeps running after its last session leaves
        #[arg(long, default_value_t = daemon::SERVER_IDLE.as_secs())]
        idle_secs: u64,
    },
    #[command(hide = true)]
    MockServer,
}

fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Doctor { json } => doctor(json),
        Command::Share {
            names,
            exclude,
            dry_run,
            yes,
        } => share::run(
            &home()?,
            &share::Options {
                names,
                exclude,
                dry_run,
                yes,
            },
            false,
        ),
        Command::Unshare {
            names,
            dry_run,
            yes,
        } => share::run(
            &home()?,
            &share::Options {
                names,
                exclude: Vec::new(),
                dry_run,
                yes,
            },
            true,
        ),
        Command::Status { json } => status(json),
        Command::Stop => stop(),
        Command::Proxy {
            name,
            project,
            env,
            command,
        } => proxy::run(proxy::Options {
            name,
            project,
            env_keys: env,
            command,
        }),
        Command::Daemon { idle_secs } => {
            // Tests shorten the idle time for daemons that stubs start on their own.
            let idle = env::var("MCPHERD_IDLE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(idle_secs);
            daemon::run(Duration::from_secs(idle))
        }
        Command::MockServer => {
            mock::run();
            Ok(())
        }
    }
}

fn doctor(json: bool) -> Result<()> {
    let home = home()?;
    let loaded = config::load_all(&home);
    let sessions = procs::sessions(&procs::snapshot(), &loaded.servers);

    let path = env::var_os("PATH");
    let mut findings = lint::lint(&loaded.servers, path.as_deref(), &home);
    findings.extend(share::findings(
        &loaded.servers,
        &share::remembered_excludes(),
    ));
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

fn status(json: bool) -> Result<()> {
    let reply = match daemon::call(&daemon::Request::Status) {
        Ok(r) => r,
        Err(_) => {
            if json {
                println!("null");
            } else {
                println!(
                    "The mcpherd daemon is not running. It starts on its own when a session \
                     opens a shared server."
                );
            }
            return Ok(());
        }
    };
    let status: daemon::Status = serde_json::from_str(&reply).context("unexpected reply")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    let procs = procs::snapshot();
    let rss: Vec<u64> = status
        .servers
        .iter()
        .map(|s| procs::tree_rss(&procs, s.pid))
        .collect();
    let sessions: usize = status.servers.iter().map(|s| s.sessions).sum();
    println!(
        "mcpherd daemon {} · pid {} · up {} · {} shared {} · {} {} · {}",
        status.version,
        status.pid,
        report::age(status.age_secs),
        status.servers.len(),
        if status.servers.len() == 1 {
            "server"
        } else {
            "servers"
        },
        sessions,
        if sessions == 1 {
            "connection"
        } else {
            "connections"
        },
        report::mb(rss.iter().sum())
    );
    for (s, rss) in status.servers.iter().zip(&rss) {
        let who = match s.sessions {
            0 => "idle".to_string(),
            1 => "1 session".to_string(),
            n => format!("{n} sessions"),
        };
        println!(
            "  {:<24} pid {:<7} {:<11} {:>7}  {}",
            s.name,
            s.pid,
            who,
            report::mb(*rss),
            s.command
        );
    }
    if status.servers.is_empty() {
        println!("  no servers running right now");
    }
    Ok(())
}

fn stop() -> Result<()> {
    match daemon::call(&daemon::Request::Stop) {
        Ok(_) => {
            // Wait for the daemon to let go of its socket so a restart starts clean.
            let sock = daemon::socket_path();
            for _ in 0..140 {
                if !sock.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            println!(
                "Stopped the mcpherd daemon and its servers. Sessions that are still open \
                 reconnect to a fresh one the next time they use a shared server."
            );
        }
        Err(_) => println!("The mcpherd daemon is not running."),
    }
    Ok(())
}
