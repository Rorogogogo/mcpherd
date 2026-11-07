use std::collections::HashMap;

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::lint::{self, Finding, Severity};
use crate::model::{Server, Transport};
use crate::{report, secrets, signature};

/// A snapshot of one process: everything the scan needs, decoupled from sysinfo.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub argv: Vec<String>,
    pub rss: u64,
    pub age_secs: u64,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub pid: u32,
    pub agent: &'static str,
    pub age_secs: u64,
    pub cwd: Option<String>,
    pub servers: Vec<RunningServer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningServer {
    pub pid: u32,
    /// The configured name if we could match it, otherwise a name derived from the command.
    pub name: String,
    pub command: String,
    /// Resident memory of the server and all of its children.
    pub rss_bytes: u64,
    pub processes: usize,
}

pub fn snapshot() -> Vec<Proc> {
    let Ok(mut sys) = System::new() else {
        return Vec::new();
    };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .map(|p| Proc {
            pid: p.pid().as_u32(),
            parent: p.parent().map(Pid::as_u32),
            name: p.name().to_string_lossy().into_owned(),
            argv: normalize_argv(
                p.cmd()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            ),
            rss: p.memory(),
            age_secs: p.run_time(),
            cwd: p.cwd().map(|c| c.display().to_string()),
        })
        .collect()
}

/// Programs that set their own process title (`npm exec pkg`) come back as one
/// string followed by their environment. Split the title and drop the rest.
fn normalize_argv(argv: Vec<String>) -> Vec<String> {
    match argv.first() {
        Some(title) if title.contains(' ') && !std::path::Path::new(title).exists() => {
            title.split_whitespace().map(String::from).collect()
        }
        _ => argv,
    }
}

/// Helper processes an agent starts that are never MCP servers.
fn is_ignored(p: &Proc) -> bool {
    matches!(
        p.name.as_str(),
        // `mcpherd` here is a stub: the real server runs under the daemon.
        "zsh" | "bash" | "sh" | "fish" | "caffeinate" | "mcpherd"
    )
}

/// Which agent a process is the main process of, if any.
fn agent_of(p: &Proc) -> Option<&'static str> {
    match p.name.as_str() {
        "claude" => Some("claude-code"),
        // The desktop app's main process launches its extensions' MCP servers.
        "Claude" => Some("claude-desktop"),
        "codex" if !p.argv.iter().any(|a| a == "mcp-server") => Some("codex"),
        // The daemon holds every shared server, so it reads like one more session.
        "mcpherd" if p.argv.get(1).is_some_and(|a| a == "daemon") => Some("mcpherd"),
        _ => None,
    }
}

/// Wrappers that sit between an agent and the server it launched.
fn is_wrapper(p: &Proc) -> bool {
    p.name == "disclaimer"
}

pub fn sessions(procs: &[Proc], servers: &[Server]) -> Vec<Session> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        if let Some(parent) = p.parent {
            children.entry(parent).or_default().push(p);
        }
    }
    let kids = |pid: u32| children.get(&pid).map(Vec::as_slice).unwrap_or(&[]);
    let is_session = |p: &Proc| {
        agent_of(p).is_some()
            && !p
                .parent
                .and_then(|pp| by_pid.get(&pp))
                .is_some_and(|parent| agent_of(parent).is_some())
    };

    let mut out = Vec::new();
    for p in procs.iter().filter(|p| is_session(p)) {
        let agent = agent_of(p).unwrap();
        let mut running = Vec::new();
        for child in kids(p.pid) {
            // Look through wrappers to the process they launched.
            let target = if is_wrapper(child) {
                match kids(child.pid) {
                    [only] => *only,
                    _ => continue,
                }
            } else if agent == "claude-desktop" {
                // The desktop app only launches MCP servers through wrappers.
                continue;
            } else {
                child
            };
            if agent_of(target).is_some() || is_ignored(target) {
                continue;
            }

            let matched = match_server(target, servers);
            if matched.is_none() && !target.argv.join(" ").to_lowercase().contains("mcp") {
                continue;
            }
            let (rss, count) = subtree(target.pid, target.rss, &kids);
            running.push(RunningServer {
                pid: target.pid,
                name: matched.unwrap_or_else(|| derived_name(&target.argv)),
                command: secrets::redact(&target.argv.join(" ")),
                rss_bytes: rss,
                processes: count,
            });
        }
        running.sort_by(|a, b| a.name.cmp(&b.name));
        out.push(Session {
            pid: p.pid,
            agent,
            age_secs: p.age_secs,
            cwd: p.cwd.clone(),
            servers: running,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.age_secs));
    out
}

/// A session older than this is worth a look if it is still holding MCP servers.
const OLD_SESSION_SECS: u64 = 3 * 24 * 3600;

/// Duplicates that would save less than this aren't worth flagging.
const MIN_SAVING_BYTES: u64 = 20 << 20;

pub fn findings(sessions: &[Session]) -> Vec<Finding> {
    let mut out = Vec::new();

    let mut by_name: HashMap<&str, Vec<(&Session, &RunningServer)>> = HashMap::new();
    for session in sessions {
        for server in &session.servers {
            by_name
                .entry(&server.name)
                .or_default()
                .push((session, server));
        }
    }
    let mut names: Vec<_> = by_name.keys().copied().collect();
    names.sort();
    for name in names {
        let copies = &by_name[name];
        if copies.len() < 2 {
            continue;
        }
        let total: u64 = copies.iter().map(|(_, s)| s.rss_bytes).sum();
        let saved = total - total / copies.len() as u64;
        if saved < MIN_SAVING_BYTES {
            continue;
        }
        let pids: Vec<_> = copies
            .iter()
            .map(|(sess, _)| sess.pid.to_string())
            .collect();
        out.push(Finding {
            severity: Severity::Warn,
            code: "duplicate-processes",
            subject: name.to_string(),
            message: format!(
                "{} copies running (sessions {}), {} in total; one shared copy would save about {}",
                copies.len(),
                pids.join(", "),
                report::mb(total),
                report::mb(saved)
            ),
            fix: None,
        });
    }

    for session in sessions {
        let held: u64 = session.servers.iter().map(|s| s.rss_bytes).sum();
        if session.agent != "mcpherd" && session.age_secs >= OLD_SESSION_SECS && held > 0 {
            out.push(Finding {
                severity: Severity::Info,
                code: "old-session",
                subject: format!("{} pid {}", session.agent, session.pid),
                message: format!(
                    "running for {} and holding {} in MCP servers",
                    report::age(session.age_secs),
                    report::mb(held)
                ),
                fix: Some("close it if you are no longer using it".into()),
            });
        }
    }
    out
}

/// Resident memory of a process and everything under it.
pub fn tree_rss(procs: &[Proc], pid: u32) -> u64 {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        if let Some(parent) = p.parent {
            children.entry(parent).or_default().push(p);
        }
    }
    let kids = |pid: u32| children.get(&pid).map(Vec::as_slice).unwrap_or(&[]);
    let own = procs.iter().find(|p| p.pid == pid).map_or(0, |p| p.rss);
    subtree(pid, own, &kids).0
}

fn subtree<'a>(pid: u32, rss: u64, kids: &impl Fn(u32) -> &'a [&'a Proc]) -> (u64, usize) {
    let mut total = (rss, 1);
    for child in kids(pid) {
        let (r, n) = subtree(child.pid, child.rss, kids);
        total.0 += r;
        total.1 += n;
    }
    total
}

fn match_server(p: &Proc, servers: &[Server]) -> Option<String> {
    servers.iter().find_map(|s| match &s.transport {
        Transport::Stdio { command, args, .. } => {
            let found = signature::matches_process(command, args, &p.argv)
                || signature::matches_process(lint::program_of(command), args, &p.argv);
            found.then(|| s.name.clone())
        }
        _ => None,
    })
}

fn derived_name(argv: &[String]) -> String {
    let Some((head, tail)) = argv.split_first() else {
        return "?".into();
    };
    match signature::runner_package(head, tail) {
        Some((pkg, _)) => pkg.to_string(),
        None => std::path::Path::new(head)
            .file_name()
            .map_or(head.clone(), |n| n.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, Scope};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn proc(pid: u32, parent: u32, name: &str, argv: &str, rss_mb: u64) -> Proc {
        Proc {
            pid,
            parent: Some(parent),
            name: name.into(),
            argv: argv.split(' ').map(String::from).collect(),
            rss: rss_mb << 20,
            age_secs: pid as u64,
            cwd: None,
        }
    }

    #[test]
    fn splits_retitled_processes() {
        let argv = normalize_argv(vec![
            "npm exec @upstash/context7-mcp   ".into(),
            "HOME=/u".into(),
        ]);
        assert_eq!(argv, ["npm", "exec", "@upstash/context7-mcp"]);
        assert_eq!(
            normalize_argv(vec!["/bin/ls".into(), "-l".into()]),
            ["/bin/ls", "-l"]
        );
    }

    #[test]
    fn groups_servers_under_sessions() {
        let procs = vec![
            proc(100, 1, "claude", "claude", 300),
            proc(101, 100, "npm", "npm exec @upstash/context7-mcp", 100),
            proc(
                102,
                101,
                "node",
                "node /n/node_modules/.bin/context7-mcp",
                70,
            ),
            proc(103, 100, "zsh", "/bin/zsh -c mcpherd doctor", 3),
            proc(104, 100, "magic", "node /x/mcp-server.js", 40),
            // Desktop app: only wrapped children count, and wrapped CLI sessions are their own session.
            proc(200, 1, "Claude", "/Applications/Claude.app", 500),
            proc(201, 200, "Claude Helper", "helper --type=renderer", 600),
            proc(202, 200, "disclaimer", "disclaimer -- npx jj-mcp", 1),
            proc(203, 202, "npm", "npm exec jj-mcp@latest", 40),
            proc(204, 200, "disclaimer", "disclaimer -- claude", 1),
            proc(
                205,
                204,
                "claude",
                "claude --output-format stream-json",
                300,
            ),
        ];
        let servers = vec![Server {
            agent: Agent::ClaudeCode,
            scope: Scope::User,
            name: "context7".into(),
            source: PathBuf::new(),
            transport: Transport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@upstash/context7-mcp".into()],
                env: BTreeMap::new(),
                cwd: None,
            },
            shared_via: None,
        }];

        let found = sessions(&procs, &servers);
        let summary: Vec<_> = found
            .iter()
            .map(|s| {
                (
                    s.pid,
                    s.agent,
                    s.servers
                        .iter()
                        .map(|r| (r.name.as_str(), r.rss_bytes >> 20, r.processes))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (205, "claude-code", vec![]),
                (200, "claude-desktop", vec![("jj-mcp", 40, 1)]),
                (
                    100,
                    "claude-code",
                    vec![("context7", 170, 2), ("node", 40, 1)]
                ),
            ]
        );
    }
}
