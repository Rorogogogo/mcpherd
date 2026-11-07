use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::secrets;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    Cursor,
}

impl Agent {
    pub fn label(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Cursor => "Cursor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum Scope {
    User,
    Project(PathBuf),
    Plugin(String),
}

impl Scope {
    pub fn label(&self) -> String {
        match self {
            Scope::User => "user".into(),
            Scope::Project(p) => format!("project {}", p.display()),
            Scope::Plugin(name) => format!("plugin {name}"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Remote {
        kind: String,
        url: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Server {
    pub agent: Agent,
    pub scope: Scope,
    pub name: String,
    pub source: PathBuf,
    #[serde(flatten)]
    pub transport: Transport,
    /// Set when `mcpherd share` routed this entry through the daemon: the mcpherd binary the
    /// entry runs. `transport` always describes the real server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_via: Option<String>,
}

impl Server {
    /// One-line description of how the server is launched, with secrets masked.
    pub fn launch_line(&self) -> String {
        match &self.transport {
            Transport::Stdio { command, args, .. } => {
                let mut line = command.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                secrets::redact(&line)
            }
            Transport::Remote { kind, url } => format!("{kind} {}", secrets::redact(url)),
        }
    }

    /// A copy that is safe to print or serialize.
    pub fn redacted(&self) -> Server {
        let mut out = self.clone();
        match &mut out.transport {
            Transport::Stdio {
                command, args, env, ..
            } => {
                *command = secrets::redact(command);
                for arg in args.iter_mut() {
                    *arg = secrets::redact(arg);
                }
                for (key, value) in env.iter_mut() {
                    if secrets::is_secret_name(key) && !secrets::is_reference(value) {
                        *value = secrets::mask(value);
                    } else {
                        *value = secrets::redact(value);
                    }
                }
            }
            Transport::Remote { url, .. } => *url = secrets::redact(url),
        }
        out
    }

    pub fn project_dir(&self) -> Option<&PathBuf> {
        match &self.scope {
            Scope::Project(p) => Some(p),
            _ => None,
        }
    }
}
