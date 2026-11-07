//! The stub an agent starts in place of a shared server.
//!
//! It connects to the daemon (starting it if needed) and then just copies bytes between the
//! agent and the daemon. If anything goes wrong before the first byte is exchanged, it runs
//! the real server directly instead, so a broken daemon never breaks a session.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::daemon::{self, Ack, Hello, Request};

pub struct Options {
    pub name: Option<String>,
    pub project: bool,
    pub env_keys: Vec<String>,
    pub command: Vec<String>,
}

pub fn run(opts: Options) -> Result<()> {
    let (command, args) = opts
        .command
        .split_first()
        .context("no server command given after `--`")?;

    if std::env::var_os("MCPHERD_DIRECT").is_some() {
        return run_directly(command, args);
    }

    let hello = Hello {
        version: daemon::VERSION.into(),
        name: opts.name.clone().unwrap_or_else(|| command.clone()),
        command: command.clone(),
        args: args.to_vec(),
        env: std::env::vars().collect::<BTreeMap<_, _>>(),
        cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
        project: opts.project,
        env_keys: opts.env_keys,
    };

    match connect(hello) {
        Ok((stream, reader)) => pipe(stream, reader),
        Err(e) => {
            eprintln!("mcpherd: {e:#}; running {command} directly");
            run_directly(command, args)
        }
    }
}

fn run_directly(command: &str, args: &[String]) -> Result<()> {
    let err = Command::new(command).args(args).exec();
    bail!("could not run {command}: {err}")
}

fn connect(hello: Hello) -> Result<(UnixStream, BufReader<UnixStream>)> {
    let sock = daemon::socket_path();
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            start_daemon()?;
            wait_for(&sock, Duration::from_secs(5))?
        }
    };

    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = serde_json::to_string(&Request::Hello(Box::new(hello)))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    let ack: Ack = serde_json::from_str(&reply).context("no answer from the daemon")?;
    if !ack.ok {
        bail!("{}", ack.error.unwrap_or_else(|| "refused".into()));
    }
    stream.set_read_timeout(None)?;
    reader.get_ref().set_read_timeout(None)?;
    Ok((stream, reader))
}

fn wait_for(sock: &std::path::Path, limit: Duration) -> Result<UnixStream> {
    let start = Instant::now();
    loop {
        match UnixStream::connect(sock) {
            Ok(s) => return Ok(s),
            Err(e) if start.elapsed() >= limit => {
                return Err(e).context("the mcpherd daemon did not start");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Start the daemon in the background, detached from this session.
pub fn start_daemon() -> Result<()> {
    let dir = daemon::ensure_dir()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; it moves the daemon out of the agent's session so
    // closing the agent (or its terminal) doesn't take the daemon down with it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the mcpherd daemon")?;
    Ok(())
}

fn pipe(stream: UnixStream, mut from_daemon: BufReader<UnixStream>) -> Result<()> {
    let mut to_daemon = stream;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_daemon);
        let _ = to_daemon.shutdown(Shutdown::Write);
    });
    let mut out = io::stdout().lock();
    let _ = io::copy(&mut from_daemon, &mut out);
    let _ = out.flush();
    // The stdin thread may still be blocked on a read; we're done either way.
    std::process::exit(0);
}
