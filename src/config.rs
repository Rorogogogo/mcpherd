use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::model::{Agent, Scope, Server, Transport};

#[derive(Debug, Default)]
pub struct Loaded {
    pub servers: Vec<Server>,
    /// Config files that exist but could not be parsed.
    pub errors: Vec<String>,
}

pub fn load_all(home: &Path) -> Loaded {
    let mut loaded = Loaded::default();
    load_claude(home, &mut loaded);
    load_codex(home, &mut loaded);
    load_cursor(home, &mut loaded);
    loaded
}

fn read_json(path: &Path, loaded: &mut Loaded) -> Option<Json> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            loaded.errors.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

fn load_claude(home: &Path, loaded: &mut Loaded) {
    let main = home.join(".claude.json");
    if let Some(root) = read_json(&main, loaded) {
        push_json_servers(
            &root["mcpServers"],
            Agent::ClaudeCode,
            Scope::User,
            &main,
            loaded,
        );

        if let Some(projects) = root["projects"].as_object() {
            for (dir, project) in projects {
                let dir = PathBuf::from(dir);
                let scope = Scope::Project(dir.clone());
                push_json_servers(
                    &project["mcpServers"],
                    Agent::ClaudeCode,
                    scope.clone(),
                    &main,
                    loaded,
                );

                let shared = dir.join(".mcp.json");
                if let Some(file) = read_json(&shared, loaded) {
                    push_json_servers(
                        &file["mcpServers"],
                        Agent::ClaudeCode,
                        scope,
                        &shared,
                        loaded,
                    );
                }
            }
        }
    }

    let installed = home.join(".claude/plugins/installed_plugins.json");
    if let Some(root) = read_json(&installed, loaded)
        && let Some(plugins) = root["plugins"].as_object()
    {
        for (name, installs) in plugins {
            for install in installs.as_array().into_iter().flatten() {
                let Some(dir) = install["installPath"].as_str() else {
                    continue;
                };
                let path = Path::new(dir).join(".mcp.json");
                let Some(file) = read_json(&path, loaded) else {
                    continue;
                };
                // Plugins use either {"mcpServers": {...}} or a bare map of servers.
                let map = if file.get("mcpServers").is_some() {
                    &file["mcpServers"]
                } else {
                    &file
                };
                push_json_servers(
                    map,
                    Agent::ClaudeCode,
                    Scope::Plugin(name.clone()),
                    &path,
                    loaded,
                );
            }
        }
    }
}

fn load_cursor(home: &Path, loaded: &mut Loaded) {
    let path = home.join(".cursor/mcp.json");
    if let Some(root) = read_json(&path, loaded) {
        push_json_servers(
            &root["mcpServers"],
            Agent::Cursor,
            Scope::User,
            &path,
            loaded,
        );
    }
}

fn load_codex(home: &Path, loaded: &mut Loaded) {
    let path = home.join(".codex/config.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let root: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            loaded.errors.push(format!("{}: {e}", path.display()));
            return;
        }
    };
    let Some(servers) = root.get("mcp_servers").and_then(|v| v.as_table()) else {
        return;
    };

    for (name, entry) in servers {
        if entry.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let str_of = |key: &str| entry.get(key).and_then(|v| v.as_str()).map(String::from);
        let transport = if let Some(url) = str_of("url") {
            Transport::Remote {
                kind: "http".into(),
                url,
            }
        } else if let Some(command) = str_of("command") {
            let args = entry
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let env = entry
                .get("env")
                .and_then(|v| v.as_table())
                .map(|t| {
                    t.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Transport::Stdio {
                command,
                args,
                env,
                cwd: str_of("cwd").map(PathBuf::from),
            }
        } else {
            continue;
        };
        loaded.servers.push(Server {
            agent: Agent::Codex,
            scope: Scope::User,
            name: name.clone(),
            source: path.clone(),
            transport,
        });
    }
}

fn push_json_servers(map: &Json, agent: Agent, scope: Scope, source: &Path, loaded: &mut Loaded) {
    let Some(map) = map.as_object() else { return };
    for (name, entry) in map {
        if let Some(transport) = parse_json_entry(entry) {
            loaded.servers.push(Server {
                agent,
                scope: scope.clone(),
                name: name.clone(),
                source: source.to_path_buf(),
                transport,
            });
        }
    }
}

fn parse_json_entry(entry: &Json) -> Option<Transport> {
    if entry["disabled"].as_bool() == Some(true) {
        return None;
    }
    let url = entry["url"].as_str();
    let kind = entry["type"]
        .as_str()
        .unwrap_or(if url.is_some() { "http" } else { "stdio" });

    if kind != "stdio" {
        return Some(Transport::Remote {
            kind: kind.to_string(),
            url: url?.to_string(),
        });
    }

    let command = entry["command"].as_str()?.to_string();
    let args = entry["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let env: BTreeMap<String, String> = entry["env"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let cwd = entry["cwd"].as_str().map(PathBuf::from);
    Some(Transport::Stdio {
        command,
        args,
        env,
        cwd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn loads_every_agent_and_scope() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let project = h.join("work/app");

        write(
            &h.join(".claude.json"),
            &serde_json::json!({
                "mcpServers": {
                    "context7": {"type": "stdio", "command": "npx", "args": ["-y", "@upstash/context7-mcp"]},
                    "linear": {"type": "http", "url": "https://mcp.linear.app/mcp"}
                },
                "projects": {
                    project.to_str().unwrap(): {"mcpServers": {"db": {"command": "postgres-mcp"}}}
                }
            })
            .to_string(),
        );
        write(
            &project.join(".mcp.json"),
            r#"{"mcpServers": {"shared": {"command": "node", "args": ["s.js"]}}}"#,
        );
        let plugin_dir = h.join(".claude/plugins/cache/vercel/1.0");
        write(
            &h.join(".claude/plugins/installed_plugins.json"),
            &serde_json::json!({"plugins": {"vercel@x": [{"installPath": plugin_dir}]}})
                .to_string(),
        );
        write(
            &plugin_dir.join(".mcp.json"),
            r#"{"vercel": {"type": "http", "url": "https://mcp.vercel.com"}}"#,
        );
        write(
            &h.join(".codex/config.toml"),
            "[mcp_servers.off]\ncommand = \"x\"\nenabled = false\n[mcp_servers.jj]\ncommand = \"npx\"\nargs = [\"-y\", \"jj\"]\n[mcp_servers.jj.env]\nAPI_URL = \"https://x\"\n",
        );
        write(
            &h.join(".cursor/mcp.json"),
            r#"{"mcpServers": {"c": {"url": "https://c"}}}"#,
        );

        let loaded = load_all(h);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

        let names: Vec<_> = loaded
            .servers
            .iter()
            .map(|s| (s.agent, s.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (Agent::ClaudeCode, "context7"),
                (Agent::ClaudeCode, "linear"),
                (Agent::ClaudeCode, "db"),
                (Agent::ClaudeCode, "shared"),
                (Agent::ClaudeCode, "vercel"),
                (Agent::Codex, "jj"),
                (Agent::Cursor, "c"),
            ]
        );
        assert_eq!(loaded.servers[2].scope, Scope::Project(project.clone()));
        assert_eq!(loaded.servers[4].scope, Scope::Plugin("vercel@x".into()));
        match &loaded.servers[5].transport {
            Transport::Stdio { env, .. } => assert_eq!(env["API_URL"], "https://x"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn reports_unparseable_files() {
        let home = tempfile::tempdir().unwrap();
        write(&home.path().join(".cursor/mcp.json"), "{ not json");
        let loaded = load_all(home.path());
        assert_eq!(loaded.errors.len(), 1);
    }
}
