//! The background process that owns one copy of each shared server.
//!
//! Stubs (`mcpherd proxy`) connect over a Unix socket and say which server they want. The
//! daemon starts that server on first use, puts every later stub for the same server on the
//! same process, and stops the server a while after its last session leaves.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Notify, mpsc};

use crate::router::{ClientId, Out, Router};
use crate::secrets;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a server keeps running after its last session leaves.
pub const SERVER_IDLE: Duration = Duration::from_secs(5 * 60);
/// How long the daemon keeps running once it has no servers.
const DAEMON_IDLE: Duration = Duration::from_secs(10 * 60);

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MCPHERD_HOME") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".mcpherd")
}

pub fn socket_path() -> PathBuf {
    dir().join("daemon.sock")
}

pub fn ensure_dir() -> Result<PathBuf> {
    let d = dir();
    fs::create_dir_all(d.join("logs")).with_context(|| format!("creating {}", d.display()))?;
    fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
    Ok(d)
}

/// What a stub sends first: which server it wants, and how to start it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// The stub's whole environment. The server is started with the environment of the first
    /// stub that asks for it.
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Share only between sessions in the same directory.
    pub project: bool,
    /// Variables from the server's `env` config. Stubs share a server only when these match.
    pub env_keys: Vec<String>,
}

impl Hello {
    /// Stubs with the same key share one process.
    pub fn key(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.command.hash(&mut h);
        self.args.hash(&mut h);
        let mut keys = self.env_keys.clone();
        keys.sort();
        for k in &keys {
            k.hash(&mut h);
            self.env.get(k).hash(&mut h);
        }
        if self.project {
            self.cwd.hash(&mut h);
        }
        h.finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello(Box<Hello>),
    Status,
    Stop,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub age_secs: u64,
    pub servers: Vec<ServerStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerStatus {
    pub name: String,
    pub pid: u32,
    pub sessions: usize,
    pub age_secs: u64,
    pub command: String,
    pub project: Option<PathBuf>,
}

struct Info {
    name: String,
    command: String,
    project: Option<PathBuf>,
    pid: AtomicU32,
    sessions: AtomicUsize,
    started: Instant,
}

enum Event {
    Join(ClientId, mpsc::UnboundedSender<String>),
    FromClient(ClientId, String),
    Leave(ClientId),
    FromServer(String),
    ServerExited,
    Shutdown,
}

struct Entry {
    generation: u64,
    tx: mpsc::UnboundedSender<Event>,
    info: Arc<Info>,
}

struct Shared {
    servers: Mutex<HashMap<u64, Entry>>,
    next_id: AtomicU64,
    started: Instant,
    stop: Notify,
    server_idle: Duration,
}

/// Run the daemon in the foreground. Returns at once if another daemon is running.
pub fn run(server_idle: Duration) -> Result<()> {
    let d = ensure_dir()?;
    let lock = File::create(d.join("daemon.lock"))?;
    // SAFETY: flock on a file descriptor we own; the lock is held until the process exits.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(serve(server_idle));
    let _ = fs::remove_file(socket_path());
    drop(lock);
    result
}

async fn serve(server_idle: Duration) -> Result<()> {
    let sock = socket_path();
    let _ = fs::remove_file(&sock);
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    log(&format!(
        "daemon {VERSION} started, pid {}",
        std::process::id()
    ));

    let shared = Arc::new(Shared {
        servers: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        started: Instant::now(),
        stop: Notify::new(),
        server_idle,
    });
    let mut empty_since = Some(Instant::now());
    let mut tick = tokio::time::interval(Duration::from_secs(15));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(connection(shared.clone(), stream));
                }
            }
            _ = shared.stop.notified() => break,
            _ = tick.tick() => {
                let empty = shared.servers.lock().unwrap().is_empty();
                match (empty, empty_since) {
                    (false, _) => empty_since = None,
                    (true, None) => empty_since = Some(Instant::now()),
                    (true, Some(t)) if t.elapsed() >= DAEMON_IDLE => break,
                    _ => {}
                }
            }
        }
    }

    log("daemon stopping");
    let _ = fs::remove_file(&sock);
    for entry in shared.servers.lock().unwrap().values() {
        let _ = entry.tx.send(Event::Shutdown);
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    while !shared.servers.lock().unwrap().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn log(line: &str) {
    eprintln!("[mcpherd] {line}");
}

async fn write_line(w: &mut (impl AsyncWriteExt + Unpin), value: &impl Serialize) -> Result<()> {
    let mut text = serde_json::to_string(value)?;
    text.push('\n');
    w.write_all(text.as_bytes()).await?;
    Ok(())
}

async fn connection(shared: Arc<Shared>, stream: UnixStream) {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let Ok(Some(first)) = lines.next_line().await else {
        return;
    };
    let hello = match serde_json::from_str::<Request>(&first) {
        Ok(Request::Status) => {
            let _ = write_line(&mut wr, &status(&shared)).await;
            return;
        }
        Ok(Request::Stop) => {
            let _ = write_line(
                &mut wr,
                &Ack {
                    ok: true,
                    ..Ack::default()
                },
            )
            .await;
            shared.stop.notify_one();
            return;
        }
        Ok(Request::Hello(h)) => h,
        Err(e) => {
            let ack = Ack {
                error: Some(format!("bad request: {e}")),
                ..Ack::default()
            };
            let _ = write_line(&mut wr, &ack).await;
            return;
        }
    };

    if hello.version != VERSION {
        let ack = Ack {
            error: Some(format!(
                "daemon is version {VERSION}, stub is {}; run `mcpherd stop` to restart it",
                hello.version
            )),
            ..Ack::default()
        };
        let _ = write_line(&mut wr, &ack).await;
        return;
    }

    let client = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (to_client, mut outbox) = mpsc::unbounded_channel::<String>();
    let joined = join(&shared, &hello, client, to_client);
    let (server, pid) = match joined {
        Ok(j) => j,
        Err(e) => {
            let ack = Ack {
                error: Some(format!("{e:#}")),
                ..Ack::default()
            };
            let _ = write_line(&mut wr, &ack).await;
            return;
        }
    };

    if write_line(
        &mut wr,
        &Ack {
            ok: true,
            pid: Some(pid),
            ..Ack::default()
        },
    )
    .await
    .is_err()
    {
        let _ = server.send(Event::Leave(client));
        return;
    }

    tokio::spawn(async move {
        while let Some(mut line) = outbox.recv().await {
            line.push('\n');
            if wr.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if server.send(Event::FromClient(client, line)).is_err() {
            return;
        }
    }
    let _ = server.send(Event::Leave(client));
}

/// Attach a client to the server it asked for, starting the server if needed.
fn join(
    shared: &Arc<Shared>,
    hello: &Hello,
    client: ClientId,
    to_client: mpsc::UnboundedSender<String>,
) -> Result<(mpsc::UnboundedSender<Event>, u32)> {
    let mut servers = shared.servers.lock().unwrap();
    let key = hello.key();
    if let std::collections::hash_map::Entry::Vacant(slot) = servers.entry(key) {
        slot.insert(start_server(shared, key, hello)?);
    }
    let entry = &servers[&key];
    // Sent while holding the lock, so an idle server can't shut down in between.
    let _ = entry.tx.send(Event::Join(client, to_client));
    Ok((entry.tx.clone(), entry.info.pid.load(Ordering::Relaxed)))
}

fn status(shared: &Shared) -> Status {
    let servers = shared.servers.lock().unwrap();
    let mut list: Vec<ServerStatus> = servers
        .values()
        .map(|e| ServerStatus {
            name: e.info.name.clone(),
            pid: e.info.pid.load(Ordering::Relaxed),
            sessions: e.info.sessions.load(Ordering::Relaxed),
            age_secs: e.info.started.elapsed().as_secs(),
            command: e.info.command.clone(),
            project: e.info.project.clone(),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Status {
        pid: std::process::id(),
        version: VERSION.into(),
        age_secs: shared.started.elapsed().as_secs(),
        servers: list,
    }
}

fn log_file(name: &str) -> Stdio {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir().join("logs").join(format!("{safe}.log"));
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    }
}

fn start_server(shared: &Arc<Shared>, key: u64, hello: &Hello) -> Result<Entry> {
    let mut cmd = Command::new(&hello.command);
    cmd.args(&hello.args)
        .env_clear()
        .envs(&hello.env)
        .current_dir(&hello.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log_file(&hello.name))
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", hello.command))?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;

    let mut line = hello.command.clone();
    for a in &hello.args {
        line.push(' ');
        line.push_str(a);
    }
    let info = Arc::new(Info {
        name: hello.name.clone(),
        command: secrets::redact(&line),
        project: hello.project.then(|| hello.cwd.clone()),
        pid: AtomicU32::new(pid),
        sessions: AtomicUsize::new(0),
        started: Instant::now(),
    });
    log(&format!("started {} (pid {pid})", hello.name));

    let (tx, rx) = mpsc::unbounded_channel();
    let from_server = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if from_server.send(Event::FromServer(line)).is_err() {
                return;
            }
        }
        let _ = from_server.send(Event::ServerExited);
    });

    let generation = shared.next_id.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(run_server(
        shared.clone(),
        key,
        generation,
        rx,
        child,
        stdin,
        info.clone(),
    ));
    Ok(Entry {
        generation,
        tx,
        info,
    })
}

fn deliver(
    out: Vec<Out>,
    clients: &HashMap<ClientId, mpsc::UnboundedSender<String>>,
    to_server: &mpsc::UnboundedSender<String>,
) {
    for o in out {
        match o {
            Out::Server(v) => {
                let _ = to_server.send(v.to_string());
            }
            Out::Client(c, v) => {
                if let Some(tx) = clients.get(&c) {
                    let _ = tx.send(v.to_string());
                }
            }
        }
    }
}

async fn run_server(
    shared: Arc<Shared>,
    key: u64,
    generation: u64,
    mut rx: mpsc::UnboundedReceiver<Event>,
    mut child: Child,
    mut stdin: ChildStdin,
    info: Arc<Info>,
) {
    let (to_server, mut server_inbox) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = server_inbox.recv().await {
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
        // Dropping stdin closes it, which tells a well-behaved server to exit.
    });

    let mut router = Router::new();
    let mut clients: HashMap<ClientId, mpsc::UnboundedSender<String>> = HashMap::new();
    let mut idle_since = Some(tokio::time::Instant::now());

    loop {
        let deadline = idle_since.map(|t| t + shared.server_idle);
        let event = tokio::select! {
            ev = rx.recv() => ev,
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                let mut servers = shared.servers.lock().unwrap();
                if !rx.is_empty() {
                    continue;
                }
                if servers.get(&key).is_some_and(|e| e.generation == generation) {
                    servers.remove(&key);
                }
                log(&format!("stopping {}: no sessions left", info.name));
                break;
            }
        };
        match event {
            None | Some(Event::Shutdown) => break,
            Some(Event::Join(c, tx)) => {
                router.join(c);
                clients.insert(c, tx);
                idle_since = None;
            }
            Some(Event::Leave(c)) => {
                deliver(router.leave(c), &clients, &to_server);
                clients.remove(&c);
                if clients.is_empty() {
                    idle_since = Some(tokio::time::Instant::now());
                }
            }
            Some(Event::FromClient(c, line)) => match serde_json::from_str(&line) {
                Ok(v) => deliver(router.on_client(c, v), &clients, &to_server),
                Err(_) => log(&format!(
                    "{}: dropped a non-JSON line from a client",
                    info.name
                )),
            },
            Some(Event::FromServer(line)) => match serde_json::from_str(&line) {
                Ok(v) => deliver(router.on_server(v), &clients, &to_server),
                Err(_) => {
                    if !line.trim().is_empty() {
                        log(&format!("{}: dropped a non-JSON line on stdout", info.name));
                    }
                }
            },
            Some(Event::ServerExited) => {
                log(&format!("{} exited", info.name));
                break;
            }
        }
        info.sessions.store(clients.len(), Ordering::Relaxed);
    }

    {
        let mut servers = shared.servers.lock().unwrap();
        if servers
            .get(&key)
            .is_some_and(|e| e.generation == generation)
        {
            servers.remove(&key);
        }
    }
    // Closing the client channels ends their connections, so their stubs exit and the agent
    // sees the server go away, exactly as if it had run the server itself.
    drop(clients);
    drop(to_server);
    rx.close();

    let pid = child.id();
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        if let Some(pid) = pid {
            // SAFETY: signalling the process group we created for this server.
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        }
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    if let Some(pid) = pid {
        // Clean up anything the server left behind in its group (npx → node, for example).
        // SAFETY: as above; ESRCH if the group is already gone.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }
}

/// Send one request to a running daemon and read one reply line.
pub fn call(request: &Request) -> Result<String> {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path())
        .context("the mcpherd daemon is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    std::io::BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}
