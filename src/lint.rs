use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{Agent, Scope, Server, Transport};
use crate::{secrets, signature};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warn,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    /// Where it applies, e.g. "Claude Code · notion (project /x)".
    pub subject: String,
    pub message: String,
    pub fix: Option<String>,
}

pub(crate) fn subject(s: &Server) -> String {
    format!("{} · {} ({})", s.agent.label(), s.name, s.scope.label())
}

pub fn lint(servers: &[Server], path_var: Option<&OsStr>, home: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for server in servers {
        lint_server(server, path_var, home, &mut out);
    }
    lint_duplicates(servers, &mut out);
    lint_npx(servers, &mut out);
    out.sort_by_key(|f| f.severity);
    out
}

fn lint_server(server: &Server, path_var: Option<&OsStr>, home: &Path, out: &mut Vec<Finding>) {
    let Transport::Stdio {
        command,
        args,
        env,
        cwd,
    } = &server.transport
    else {
        lint_missing_project(server, out);
        return;
    };

    let line = std::iter::once(command.as_str())
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    for hit in secrets::scan(&line) {
        out.push(Finding {
            severity: Severity::Error,
            code: "secret-in-command",
            subject: subject(server),
            message: format!(
                "{} {} is in the command line: it is stored in plain text and visible to other processes via `ps`",
                hit.kind,
                secrets::mask(&hit.value)
            ),
            fix: Some("rotate it, then pass it through the server's `env` instead".into()),
        });
    }

    for (key, value) in env {
        if secrets::is_secret_name(key) && !secrets::is_reference(value) && value.len() >= 8 {
            out.push(Finding {
                severity: Severity::Info,
                code: "secret-in-env",
                subject: subject(server),
                message: format!(
                    "{key} = {} is stored in plain text in {}",
                    secrets::mask(value),
                    server.source.display()
                ),
                fix: Some("use a reference such as ${VAR} if the agent supports it".into()),
            });
        }
    }

    if lint_missing_project(server, out) {
        return;
    }

    if let Some(wrapper) = &server.shared_via
        && !Path::new(wrapper).exists()
    {
        out.push(Finding {
            severity: Severity::Error,
            code: "missing-mcpherd",
            subject: subject(server),
            message: format!(
                "this entry is shared through {wrapper}, which no longer exists, so it can't start"
            ),
            fix: Some("reinstall mcpherd, or restore the entry with `mcpherd unshare`".into()),
        });
    }

    if command.contains("${") {
        return;
    }
    let base_dir = cwd
        .clone()
        .or_else(|| server.project_dir().cloned())
        .unwrap_or_else(|| home.to_path_buf());

    // A path may contain spaces, so only treat the field as a shell line
    // (`FOO=bar npx thing`) when it doesn't resolve as a whole.
    let program = if resolve(command, &base_dir, path_var).is_some() {
        command.as_str()
    } else {
        program_of(command)
    };
    if program != command {
        out.push(Finding {
            severity: Severity::Warn,
            code: "shell-command",
            subject: subject(server),
            message: format!(
                "command is a whole shell line (`{}`)",
                secrets::redact(command)
            ),
            fix: Some(
                "put the program in `command`, its arguments in `args` and variables in `env`"
                    .into(),
            ),
        });
    }
    if resolve(program, &base_dir, path_var).is_none() {
        out.push(Finding {
            severity: Severity::Error,
            code: "missing-command",
            subject: subject(server),
            message: format!("`{program}` was not found"),
            fix: Some("install it, fix the path, or remove the entry".into()),
        });
        return;
    }

    let is_interpreter = matches!(
        Path::new(program).file_name().and_then(|n| n.to_str()),
        Some("node" | "bun" | "deno" | "tsx" | "python" | "python3" | "uv")
    );
    if is_interpreter
        && let Some(script) = args.iter().find(|a| is_script(a))
        && !script.contains("${")
        && !base_dir.join(script).exists()
    {
        out.push(Finding {
            severity: Severity::Error,
            code: "missing-script",
            subject: subject(server),
            message: format!("script {script} does not exist"),
            fix: Some("fix the path or remove the entry".into()),
        });
    }
}

/// Returns true if the server belongs to a project folder that is gone.
fn lint_missing_project(server: &Server, out: &mut Vec<Finding>) -> bool {
    if let Some(dir) = server.project_dir()
        && !dir.exists()
    {
        out.push(Finding {
            severity: Severity::Warn,
            code: "missing-project",
            subject: subject(server),
            message: format!("project folder {} no longer exists", dir.display()),
            fix: Some("remove this project's entry from the config".into()),
        });
        return true;
    }
    false
}

/// The executable a shell-line `command` field runs, skipping leading `VAR=value` words.
pub fn program_of(command: &str) -> &str {
    command
        .split_whitespace()
        .find(|t| !t.contains('='))
        .unwrap_or(command)
}

pub fn is_script(arg: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts", ".py"]
        .iter()
        .any(|ext| arg.ends_with(ext))
}

fn resolve(program: &str, base_dir: &Path, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if program.contains('/') {
        let p = base_dir.join(program);
        return p.exists().then_some(p);
    }
    env::split_paths(path_var?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

fn lint_duplicates(servers: &[Server], out: &mut Vec<Finding>) {
    let mut by_sig: BTreeMap<String, Vec<&Server>> = BTreeMap::new();
    for s in servers {
        if let Transport::Stdio { command, args, .. } = &s.transport {
            by_sig
                .entry(signature::of(command, args))
                .or_default()
                .push(s);
        }
    }

    for (sig, group) in &by_sig {
        let name = &group[0].name;

        let claude: Vec<_> = group
            .iter()
            .filter(|s| s.agent == Agent::ClaudeCode)
            .collect();
        let has_user = claude.iter().any(|s| s.scope == Scope::User);
        let projects: Vec<String> = claude
            .iter()
            .filter_map(|s| {
                s.project_dir()
                    .filter(|d| d.exists())
                    .map(|d| d.display().to_string())
            })
            .collect();
        if has_user && !projects.is_empty() {
            out.push(Finding {
                severity: Severity::Info,
                code: "redundant-project-entry",
                subject: format!("Claude Code · {name}"),
                message: format!(
                    "already defined at user scope, and again in {} project(s): {}",
                    projects.len(),
                    projects.join(", ")
                ),
                fix: Some("the project copies can be removed".into()),
            });
        } else if !has_user && projects.len() > 1 {
            out.push(Finding {
                severity: Severity::Info,
                code: "repeated-project-entry",
                subject: format!("Claude Code · {name}"),
                message: format!(
                    "defined separately in {} projects: {}",
                    projects.len(),
                    projects.join(", ")
                ),
                fix: Some("define it once at user scope instead".into()),
            });
        }

        let mut agents: Vec<Agent> = group.iter().map(|s| s.agent).collect();
        agents.sort();
        agents.dedup();
        if agents.len() > 1 {
            let labels: Vec<_> = agents.iter().map(|a| a.label()).collect();
            out.push(Finding {
                severity: Severity::Info,
                code: "cross-agent",
                subject: name.clone(),
                message: format!(
                    "`{}` is configured identically in {}",
                    secrets::redact(sig),
                    labels.join(" and ")
                ),
                fix: None,
            });
        }
    }
}

fn lint_npx(servers: &[Server], out: &mut Vec<Finding>) {
    let mut names: Vec<&str> = servers
        .iter()
        .filter(|s| match &s.transport {
            Transport::Stdio { command, .. } => Path::new(program_of(command))
                .file_name()
                .is_some_and(|n| n == "npx"),
            _ => false,
        })
        .map(|s| s.name.as_str())
        .collect();
    names.sort();
    names.dedup();
    if !names.is_empty() {
        out.push(Finding {
            severity: Severity::Info,
            code: "npx-wrapper",
            subject: names.join(", "),
            message:
                "launched with npx, which keeps an extra `npm exec` process alive in every session"
                    .into(),
            fix: Some("install the package globally and point `command` at its binary".into()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn stdio(agent: Agent, scope: Scope, name: &str, command: &str, args: &[&str]) -> Server {
        Server {
            agent,
            scope,
            name: name.into(),
            source: PathBuf::from("/cfg"),
            transport: Transport::Stdio {
                command: command.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: BTreeMap::new(),
                cwd: None,
            },
            shared_via: None,
        }
    }

    /// A PATH holding a fake `npx`, so results don't depend on the machine.
    fn fake_path(dir: &Path) -> std::ffi::OsString {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("npx"), "").unwrap();
        bin.into_os_string()
    }

    fn codes(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn flags_secrets_shell_lines_and_missing_projects() {
        let home = tempfile::tempdir().unwrap();
        let gone = Scope::Project(home.path().join("deleted"));
        let here = Scope::Project(home.path().to_path_buf());
        let servers = vec![
            stdio(
                Agent::ClaudeCode,
                here,
                "notion",
                "NOTION_TOKEN=ntn_FAKEFAKEFAKEFAKEFAKE00 npx notion-mcp",
                &[],
            ),
            stdio(Agent::ClaudeCode, gone, "old", "node", &["./missing.js"]),
        ];
        let path = fake_path(home.path());
        let found = lint(&servers, Some(&path), home.path());
        assert_eq!(
            codes(&found),
            [
                "secret-in-command",
                "shell-command",
                "missing-project",
                "npx-wrapper"
            ]
        );

        // A dead project still leaks its secrets.
        let dead = vec![stdio(
            Agent::ClaudeCode,
            Scope::Project(home.path().join("gone")),
            "n",
            "API_TOKEN=notreal12345 npx x",
            &[],
        )];
        assert_eq!(
            codes(&lint(&dead, Some(&path), home.path())),
            ["secret-in-command", "missing-project", "npx-wrapper"]
        );
        assert!(
            !found[0].message.contains("FAKEFAKE"),
            "secret must be masked"
        );
    }

    #[test]
    fn flags_missing_commands_and_scripts() {
        let home = tempfile::tempdir().unwrap();
        let servers = vec![
            stdio(
                Agent::Codex,
                Scope::User,
                "a",
                "definitely-not-installed-xyz",
                &[],
            ),
            stdio(Agent::Codex, Scope::User, "b", "/bin/sh", &[]),
            stdio(
                Agent::Codex,
                Scope::User,
                "c",
                "/bin/sh",
                &["./not-a-script-runner.js"],
            ),
            stdio(Agent::Codex, Scope::User, "d", "npx", &["-y", "pkg"]),
        ];
        let path = fake_path(home.path());
        let found = lint(&servers, Some(&path), home.path());
        assert_eq!(codes(&found), ["missing-command", "npx-wrapper"]);

        let spaced = home.path().join("Personal Project/bin/server");
        std::fs::create_dir_all(spaced.parent().unwrap()).unwrap();
        std::fs::write(&spaced, "").unwrap();
        let servers = vec![stdio(
            Agent::Codex,
            Scope::User,
            "e",
            spaced.to_str().unwrap(),
            &["mcp"],
        )];
        assert!(lint(&servers, Some(&path), home.path()).is_empty());
    }

    #[test]
    fn groups_duplicates() {
        let home = tempfile::tempdir().unwrap();
        let p = Scope::Project(home.path().to_path_buf());
        let servers = vec![
            stdio(
                Agent::ClaudeCode,
                Scope::User,
                "context7",
                "npx",
                &["-y", "@upstash/context7-mcp"],
            ),
            stdio(
                Agent::ClaudeCode,
                p,
                "context7",
                "npx",
                &["-y", "@upstash/context7-mcp@latest"],
            ),
            stdio(
                Agent::Codex,
                Scope::User,
                "c7",
                "npx",
                &["@upstash/context7-mcp"],
            ),
        ];
        let path = fake_path(home.path());
        let found = lint(&servers, Some(&path), home.path());
        let mut got = codes(&found);
        got.sort();
        assert_eq!(
            got,
            ["cross-agent", "npx-wrapper", "redundant-project-entry"]
        );
    }
}
