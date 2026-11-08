//! The stub an agent starts in place of a shared server.
//!
//! It connects to the daemon (starting it if needed) and relays messages between the agent and
//! the daemon. If that fails before the session starts, it runs the real server directly
//! instead, so a broken daemon never breaks a session.
//!
//! If the shared server or the daemon goes away mid-session, the stub reconnects on its own:
//! requests that were in flight get an error, the session's handshake is replayed to the new
//! server, and the agent carries on without noticing. If the daemon can't be reached again,
//! the stub starts the server itself.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::daemon::{self, Ack, Hello, Request};

pub struct Options {
    pub name: Option<String>,
    pub project: bool,
    pub env_keys: Vec<String>,
    pub command: Vec<String>,
}

/// Reconnects allowed per minute before the stub gives up, so a server that crashes on start
/// can't spin forever.
const MAX_RECONNECTS: usize = 5;
/// How long to wait for the other side to finish after the agent closes the session.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

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

    match connect(&hello) {
        Ok((stream, reader)) => {
            let (tx, rx) = mpsc::channel();
            Stub::new(hello, tx).run(rx, stream, reader)
        }
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

fn connect(hello: &Hello) -> Result<(UnixStream, BufReader<UnixStream>)> {
    let sock = daemon::socket_path();
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            start_daemon()?;
            wait_for(&sock, Duration::from_secs(5))?
        }
    };

    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = serde_json::to_string(&Request::Hello(Box::new(hello.clone())))?;
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

enum Event {
    /// A line from the agent.
    Client(Vec<u8>),
    ClientClosed,
    /// A line from the daemon or the directly run server, tagged with its connection.
    Upstream(u64, Vec<u8>),
    UpstreamClosed(u64),
}

/// Where the session's messages go: the daemon, or the server run directly as a fallback.
enum Upstream {
    Daemon(UnixStream),
    Direct {
        child: Child,
        stdin: Option<ChildStdin>,
    },
}

impl Upstream {
    fn send(&mut self, line: &[u8]) -> io::Result<()> {
        let w: &mut dyn Write = match self {
            Upstream::Daemon(s) => s,
            Upstream::Direct {
                stdin: Some(stdin), ..
            } => stdin,
            Upstream::Direct { stdin: None, .. } => return Err(io::ErrorKind::BrokenPipe.into()),
        };
        w.write_all(line)?;
        w.flush()
    }

    /// Tell the other side the session is over.
    fn close(&mut self) {
        match self {
            Upstream::Daemon(s) => {
                let _ = s.shutdown(Shutdown::Write);
            }
            Upstream::Direct { stdin, .. } => *stdin = None,
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if let Upstream::Direct { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Read lines until EOF, sending each one as an event. (Plain reads on purpose: on Linux
/// `io::copy` turns pipe/socket pairs into `splice` calls, which can hold data back.)
fn read_lines(
    mut from: impl BufRead + Send + 'static,
    tx: Sender<Event>,
    wrap: impl Fn(Vec<u8>) -> Event + Send + 'static,
    end: Event,
) {
    std::thread::spawn(move || {
        loop {
            let mut line = Vec::new();
            match from.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(wrap(line)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(end);
    });
}

/// Each JSON-RPC message in a line (a line may hold a batch).
fn messages(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

fn is_request(m: &Value) -> bool {
    m.get("method").is_some() && m.get("id").is_some_and(|id| !id.is_null())
}

fn is_response(m: &Value) -> bool {
    m.get("method").is_none() && m.get("id").is_some_and(|id| !id.is_null())
}

fn line_of(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(v).unwrap_or_default();
    out.push(b'\n');
    out
}

struct Stub {
    hello: Hello,
    tx: Sender<Event>,
    upstream: Option<Upstream>,
    /// Which connection the current upstream is; events from older ones are ignored.
    generation: u64,
    /// The agent's `initialize` request, replayed to a new server after a reconnect.
    init: Option<Value>,
    init_answered: bool,
    /// Ids (as JSON text) of the agent's requests that haven't been answered yet.
    pending: HashSet<String>,
    /// While a replayed `initialize` is in flight: its id. The agent's lines wait meanwhile.
    replaying: Option<String>,
    held: Vec<Vec<u8>>,
    reconnects: VecDeque<Instant>,
    client_closed: bool,
}

impl Stub {
    fn new(hello: Hello, tx: Sender<Event>) -> Self {
        Stub {
            hello,
            tx,
            upstream: None,
            generation: 0,
            init: None,
            init_answered: false,
            pending: HashSet::new(),
            replaying: None,
            held: Vec::new(),
            reconnects: VecDeque::new(),
            client_closed: false,
        }
    }

    fn run(mut self, rx: Receiver<Event>, stream: UnixStream, reader: BufReader<UnixStream>) -> ! {
        read_lines(
            BufReader::new(io::stdin()),
            self.tx.clone(),
            Event::Client,
            Event::ClientClosed,
        );
        self.attach_daemon(stream, reader);

        loop {
            let event = if self.client_closed {
                match rx.recv_timeout(CLOSE_GRACE) {
                    Ok(e) => e,
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => self.exit(0),
                }
            } else {
                match rx.recv() {
                    Ok(e) => e,
                    Err(_) => self.exit(0),
                }
            };
            match event {
                Event::Client(line) => self.on_client(line),
                Event::ClientClosed => {
                    self.client_closed = true;
                    match &mut self.upstream {
                        Some(up) => up.close(),
                        None => self.exit(0),
                    }
                }
                Event::Upstream(g, line) if g == self.generation => self.on_upstream(line),
                Event::UpstreamClosed(g) if g == self.generation => {
                    if self.client_closed {
                        self.exit(0);
                    }
                    self.lost();
                }
                Event::Upstream(..) | Event::UpstreamClosed(_) => {}
            }
        }
    }

    fn exit(&mut self, code: i32) -> ! {
        // Dropping the upstream stops a directly run server.
        self.upstream = None;
        std::process::exit(code);
    }

    fn attach_daemon(&mut self, stream: UnixStream, reader: BufReader<UnixStream>) {
        self.generation += 1;
        let g = self.generation;
        read_lines(
            reader,
            self.tx.clone(),
            move |l| Event::Upstream(g, l),
            Event::UpstreamClosed(g),
        );
        self.upstream = Some(Upstream::Daemon(stream));
    }

    fn attach_direct(&mut self) -> Result<()> {
        let mut child = Command::new(&self.hello.command)
            .args(&self.hello.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run {}", self.hello.command))?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stdin = child.stdin.take();
        self.generation += 1;
        let g = self.generation;
        read_lines(
            BufReader::new(stdout),
            self.tx.clone(),
            move |l| Event::Upstream(g, l),
            Event::UpstreamClosed(g),
        );
        self.upstream = Some(Upstream::Direct { child, stdin });
        Ok(())
    }

    fn on_client(&mut self, line: Vec<u8>) {
        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
            for m in messages(&v) {
                if is_request(m) {
                    self.pending.insert(m["id"].to_string());
                    if m["method"] == "initialize" {
                        self.init = Some(m.clone());
                        self.init_answered = false;
                    }
                }
            }
        }
        if self.upstream.is_none() {
            // The server went away while the session was quiet; reconnect now that it's needed.
            self.held.push(line);
            self.reconnect();
            return;
        }
        if self.replaying.is_some() {
            self.held.push(line);
            return;
        }
        self.send(&line);
    }

    fn on_upstream(&mut self, line: Vec<u8>) {
        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
            let msgs = messages(&v);
            if let Some(ours) = &self.replaying
                && msgs.len() == 1
                && is_response(msgs[0])
                && &msgs[0]["id"].to_string() == ours
            {
                self.replaying = None;
                self.send(&line_of(
                    &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                ));
                for held in std::mem::take(&mut self.held) {
                    self.send(&held);
                }
                return;
            }
            for m in msgs {
                if is_response(m) {
                    let id = m["id"].to_string();
                    self.pending.remove(&id);
                    if self.init.as_ref().is_some_and(|i| i["id"] == m["id"]) {
                        self.init_answered = true;
                    }
                }
            }
        }
        let mut out = io::stdout().lock();
        if out.write_all(&line).and_then(|_| out.flush()).is_err() {
            self.exit(0);
        }
    }

    fn send(&mut self, line: &[u8]) {
        let failed = match &mut self.upstream {
            Some(up) => up.send(line).is_err(),
            None => true,
        };
        if failed {
            // The reader will see the close too; deal with it once, here.
            self.lost();
        }
    }

    /// The daemon or the server went away.
    fn lost(&mut self) {
        self.upstream = None;
        self.generation += 1;
        self.replaying = None;
        self.held.clear();

        // Requests that were in flight can't be answered any more. The handshake is the
        // exception: if the agent is still waiting for it, it's sent again below.
        let waiting_init = self
            .init
            .as_ref()
            .filter(|_| !self.init_answered)
            .map(|i| i["id"].to_string());
        let failed: Vec<String> = self
            .pending
            .iter()
            .filter(|id| Some(*id) != waiting_init.as_ref())
            .cloned()
            .collect();
        let mut out = io::stdout().lock();
        for id in &failed {
            self.pending.remove(id);
            let id: Value = serde_json::from_str(id).unwrap_or(Value::Null);
            let err = json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32603,
                "message": "the shared MCP server restarted while this request was running; please try again"
            }});
            let _ = out.write_all(&line_of(&err));
        }
        let _ = out.flush();
        drop(out);

        // Reconnect right away only if the agent is blocked on the handshake; otherwise wait
        // until the session sends something, so `mcpherd stop` doesn't bring everything back.
        if waiting_init.is_some() {
            self.reconnect();
        }
    }

    fn reconnect(&mut self) {
        let now = Instant::now();
        while self
            .reconnects
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            self.reconnects.pop_front();
        }
        if self.reconnects.len() >= MAX_RECONNECTS {
            eprintln!(
                "mcpherd: {} keeps going away; giving up after {MAX_RECONNECTS} restarts in a minute",
                self.hello.name
            );
            self.exit(1);
        }
        self.reconnects.push_back(now);

        let mut attached = false;
        for delay in [0, 200, 800] {
            std::thread::sleep(Duration::from_millis(delay));
            if let Ok((stream, reader)) = connect(&self.hello) {
                self.attach_daemon(stream, reader);
                attached = true;
                break;
            }
        }
        if !attached {
            eprintln!(
                "mcpherd: can't reach the daemon again; running {} directly",
                self.hello.command
            );
            if let Err(e) = self.attach_direct() {
                eprintln!("mcpherd: {e:#}");
                self.exit(1);
            }
        }
        eprintln!("mcpherd: reconnected {}", self.hello.name);

        let held = std::mem::take(&mut self.held);
        match self.init.clone() {
            // The agent never got its handshake answer: send its own request again.
            Some(init) if !self.init_answered => {
                self.send(&line_of(&init));
                for line in held {
                    self.send(&line);
                }
            }
            // A fresh server needs the handshake before anything else.
            Some(mut init) => {
                let id = format!("mcpherd-replay-{}", self.generation);
                init["id"] = json!(id);
                self.replaying = Some(json!(id).to_string());
                self.held = held;
                self.send(&line_of(&init));
            }
            None => {
                for line in held {
                    self.send(&line);
                }
            }
        }
    }
}
