//! Real processes end to end: agents → stubs → daemon → one mock server.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_mcpherd");

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(home: &Path, extra_env: &[(&str, &str)]) -> Session {
        let mut cmd = Command::new(BIN);
        cmd.args(["proxy", "--name", "mock", "--", BIN, "mock-server"])
            .env("MCPHERD_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Session {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, msg: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "session closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    /// Run the handshake and return the server's answer.
    fn initialize(&mut self) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}));
        let reply = self.recv();
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        reply
    }

    /// Call the mock's tool and return (server pid, calls handled so far).
    fn whoami(&mut self, id: u64) -> (String, u64) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "whoami", "arguments": {}, "_meta": {"progressToken": id}}}));
        let progress = self.recv();
        assert_eq!(progress["method"], "notifications/progress");
        assert_eq!(progress["params"]["progressToken"], json!(id));
        let reply = self.recv();
        assert_eq!(reply["id"], json!(id));
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let (pid, calls) = text.split_once(' ').unwrap();
        (
            pid.trim_start_matches("pid=").to_string(),
            calls.trim_start_matches("calls=").parse().unwrap(),
        )
    }

    fn close(mut self) {
        drop(self.stdin.take());
        wait_exit(&mut self.child);
    }
}

fn wait_exit(child: &mut Child) {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "process did not exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn mcpherd(home: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .env("MCPHERD_HOME", home)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn sessions_share_one_server() {
    // Unix socket paths are short-lived and length-limited, so stay under /tmp.
    let home = tempfile::Builder::new()
        .prefix("mh")
        .tempdir_in("/tmp")
        .unwrap();
    let h = home.path();

    let mut a = Session::start(h, &[]);
    let mut b = Session::start(h, &[]);
    let init_a = a.initialize();
    let init_b = b.initialize();
    assert_eq!(init_a["result"]["serverInfo"]["name"], "mock");
    assert_eq!(init_a["result"], init_b["result"]);

    // Both sessions use id 1, and both reach the same process, which counts both calls.
    let (pid_a, calls_a) = a.whoami(1);
    let (pid_b, calls_b) = b.whoami(1);
    assert_eq!(pid_a, pid_b);
    assert_eq!((calls_a, calls_b), (1, 2));

    let status: Value = serde_json::from_str(&mcpherd(h, &["status", "--json"])).unwrap();
    assert_eq!(status["servers"].as_array().unwrap().len(), 1);
    assert_eq!(status["servers"][0]["sessions"], 2);
    assert_eq!(status["servers"][0]["pid"].to_string(), pid_a);

    // One session leaving doesn't affect the other.
    a.close();
    let (pid, calls) = b.whoami(2);
    assert_eq!((pid, calls), (pid_a.clone(), 3));

    // A session that opts out runs its own copy.
    let mut direct = Session::start(h, &[("MCPHERD_DIRECT", "1")]);
    direct.initialize();
    let (direct_pid, _) = direct.whoami(1);
    assert_ne!(direct_pid, pid_a);
    direct.close();

    // Stopping the daemon ends the remaining session, like a server exiting would.
    assert!(mcpherd(h, &["stop"]).contains("Stopped"));
    wait_exit(&mut b.child);
    assert!(mcpherd(h, &["status"]).contains("not running"));
}

#[test]
fn idle_servers_stop_on_their_own() {
    let home = tempfile::Builder::new()
        .prefix("mh")
        .tempdir_in("/tmp")
        .unwrap();
    let h = home.path();

    let mut a = Session::start(h, &[("MCPHERD_IDLE_SECS", "1")]);
    a.initialize();
    let (pid, _) = a.whoami(1);
    a.close();

    let start = Instant::now();
    loop {
        let status: Value = serde_json::from_str(&mcpherd(h, &["status", "--json"])).unwrap();
        if status["servers"].as_array().is_some_and(|s| s.is_empty()) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "server never stopped"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The server process itself is gone, not just forgotten.
    let alive = Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "server pid {pid} still running");

    // The next session starts a fresh copy.
    let mut b = Session::start(h, &[]);
    b.initialize();
    let (fresh, calls) = b.whoami(1);
    assert_ne!(fresh, pid);
    assert_eq!(calls, 1);
    b.close();
    mcpherd(h, &["stop"]);
}
