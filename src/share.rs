//! `mcpherd share` and `mcpherd unshare`: the one-time config rewrite.
//!
//! Each chosen stdio entry is pointed at `mcpherd proxy -- <original command>`. Files are
//! backed up first and replaced atomically, and every other byte of the config is kept.

use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value as Json;

use crate::config;
use crate::daemon;
use crate::lint;
use crate::model::{Agent, Scope, Server, Transport};
use crate::signature;
use crate::wrap;

pub struct Options {
    pub names: Vec<String>,
    pub exclude: Vec<String>,
    pub dry_run: bool,
    pub yes: bool,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Share { project: bool },
    Unshare,
    Skip(String),
}

/// Servers that hold per-session state (a browser, a REPL, conversation threads): two sessions
/// using one would trip over each other, so they are only shared when named explicitly.
const STATEFUL_HINTS: &[(&str, &str)] = &[
    ("playwright", "drives a browser"),
    ("puppeteer", "drives a browser"),
    ("browser", "drives a browser"),
    ("chrome", "drives a browser"),
    ("chromium", "drives a browser"),
    ("selenium", "drives a browser"),
    ("repl", "is a REPL with its own state"),
    ("codex mcp-server", "keeps conversation threads"),
];

/// Does the server need to run inside the session's directory?
fn needs_project_dir(command: &str, args: &[String]) -> bool {
    let relative = |a: &str| a == "." || a.starts_with("./") || a.starts_with("../");
    let local_script =
        |a: &str| lint::is_script(a) && !a.contains("${") && Path::new(a).is_relative();
    relative(command) || args.iter().any(|a| relative(a) || local_script(a))
}

pub fn plan(
    servers: &[Server],
    names: &[String],
    exclude: &[String],
    undo: bool,
) -> Vec<(usize, Decision)> {
    let mut out = Vec::new();
    for (i, s) in servers.iter().enumerate() {
        let Transport::Stdio { command, args, .. } = &s.transport else {
            continue;
        };
        let named = names.iter().any(|n| n == &s.name);
        if !names.is_empty() && !named {
            continue;
        }

        if undo {
            if s.shared_via.is_some() {
                out.push((i, Decision::Unshare));
            } else if named {
                out.push((i, Decision::Skip("not shared".into())));
            }
            continue;
        }

        let skip = |why: &str| Decision::Skip(why.to_string());
        let sig = signature::of(command, args).to_lowercase();
        let decision = if s.shared_via.is_some() {
            skip("already shared")
        } else if exclude.iter().any(|n| n == &s.name) {
            skip("excluded")
        } else if let Scope::Plugin(_) = s.scope {
            skip("comes from a plugin, whose files mcpherd doesn't edit")
        } else if s.project_dir().is_some_and(|d| !d.exists()) {
            skip("its project folder no longer exists")
        } else if let Some((_, what)) = STATEFUL_HINTS.iter().find(|(h, _)| sig.contains(h))
            && !named
        {
            skip(&format!(
                "{what}, so each session keeps its own (name it to share anyway)"
            ))
        } else if command.contains(char::is_whitespace) && !Path::new(command).exists() {
            // The agent runs this through a shell; the stub runs programs directly.
            skip("its command is a whole shell line; split it into command and args first")
        } else if !named && s.source.file_name().is_some_and(|f| f == ".mcp.json") {
            skip(
                "is in the project's .mcp.json, which is usually committed (name it to share anyway)",
            )
        } else {
            Decision::Share {
                project: needs_project_dir(command, args),
            }
        };
        out.push((i, decision));
    }
    out
}

/// Path of the mcpherd binary to put in configs.
fn exe_path() -> Result<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let text = exe.to_str().context("mcpherd's path is not valid UTF-8")?;
    Ok(text.to_string())
}

pub fn run(home: &Path, opts: &Options, undo: bool) -> Result<()> {
    let loaded = config::load_all(home);
    let servers = &loaded.servers;
    for name in &opts.names {
        if !servers.iter().any(|s| &s.name == name) {
            bail!("no MCP server named `{name}` in any config");
        }
    }
    let decisions = plan(servers, &opts.names, &opts.exclude, undo);
    let exe = exe_path()?;

    let todo: Vec<&(usize, Decision)> = decisions
        .iter()
        .filter(|(_, d)| !matches!(d, Decision::Skip(_)))
        .collect();
    println!(
        "{}",
        if undo {
            "Restoring entries to run their server directly:"
        } else {
            "Sharing one copy of each server between sessions:"
        }
    );
    for (i, d) in &decisions {
        let s = &servers[*i];
        let mark = match d {
            Decision::Share { project: true } => "share (per project folder)".to_string(),
            Decision::Share { project: false } => "share".to_string(),
            Decision::Unshare => "restore".to_string(),
            Decision::Skip(why) => format!("skip: {why}"),
        };
        println!(
            "  {:<12} {:<24} {:<30} {}",
            s.agent.label(),
            s.name,
            short(&s.scope.label(), 30),
            mark
        );
    }
    if todo.is_empty() {
        println!("\nNothing to change.");
        return Ok(());
    }

    let files: BTreeMap<&PathBuf, usize> = todo.iter().fold(BTreeMap::new(), |mut m, (i, _)| {
        *m.entry(&servers[*i].source).or_default() += 1;
        m
    });
    println!(
        "\n{} {} in {}:",
        todo.len(),
        if todo.len() == 1 { "entry" } else { "entries" },
        if files.len() == 1 {
            "1 file".to_string()
        } else {
            format!("{} files", files.len())
        }
    );
    for (file, n) in &files {
        println!("  {}  ({n})", file.display());
    }
    if !undo {
        println!("Entries will run through {exe}");
        if exe.contains("/target/debug/") || exe.contains("/target/release/") {
            println!(
                "  note: that is a build folder. Install mcpherd first (cargo install mcpherd) so \
                 the path stays valid."
            );
        }
    }

    if opts.dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if !opts.yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to apply the changes, or --dry-run to only show them");
        }
        print!("\nApply? Backups go to {}  [y/N] ", backup_dir().display());
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing was changed.");
            return Ok(());
        }
    }

    for file in files.keys() {
        let entries: Vec<(&Server, &Decision)> = todo
            .iter()
            .filter(|(i, _)| &&servers[*i].source == file)
            .map(|(i, d)| (&servers[*i], d))
            .collect();
        let backup = rewrite(file, &entries, &exe)
            .with_context(|| format!("updating {}", file.display()))?;
        println!("Updated {} (backup: {})", file.display(), backup.display());
    }

    if undo {
        println!("\nNew sessions will start their own servers again.");
    } else {
        println!(
            "\nDone. New sessions share one copy of each server; sessions that are already open \
             keep their own copies until you restart them.\nSee what's shared with `mcpherd status`, \
             undo with `mcpherd unshare`."
        );
    }
    Ok(())
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

fn backup_dir() -> PathBuf {
    daemon::dir().join("backups")
}

/// The command and args an entry should have after the change.
fn new_command(server: &Server, decision: &Decision, exe: &str) -> Option<(String, Vec<String>)> {
    let Transport::Stdio {
        command, args, env, ..
    } = &server.transport
    else {
        return None;
    };
    match decision {
        Decision::Share { project } => {
            let keys: Vec<String> = env.keys().cloned().collect();
            Some(wrap::wrap(
                exe,
                &server.name,
                *project,
                &keys,
                command,
                args,
            ))
        }
        Decision::Unshare => Some((command.clone(), args.clone())),
        Decision::Skip(_) => None,
    }
}

/// Apply the changes to one file. Returns the backup's path.
fn rewrite(path: &Path, entries: &[(&Server, &Decision)], exe: &str) -> Result<PathBuf> {
    // Write through symlinks (dotfile managers) instead of replacing them.
    let real = fs::canonicalize(path)?;
    let original = fs::read_to_string(&real)?;

    let updated = if real.extension().is_some_and(|e| e == "toml") {
        rewrite_toml(&original, entries, exe)?
    } else {
        rewrite_json(&original, entries, exe)?
    };

    let dir = backup_dir();
    fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let flat = real
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "_");
    let backup = dir.join(format!("{stamp}-{flat}"));
    fs::write(&backup, &original)?;

    let tmp = real.with_extension("mcpherd-tmp");
    fs::write(&tmp, updated)?;
    fs::set_permissions(&tmp, fs::metadata(&real)?.permissions())?;
    fs::rename(&tmp, &real)?;
    Ok(backup)
}

fn rewrite_json(original: &str, entries: &[(&Server, &Decision)], exe: &str) -> Result<String> {
    let mut root: Json = serde_json::from_str(original)?;
    for (server, decision) in entries {
        let Some((command, args)) = new_command(server, decision, exe) else {
            continue;
        };
        let file_name = server.source.file_name().and_then(|n| n.to_str());
        let entry = match (&server.scope, server.agent) {
            (Scope::Project(dir), Agent::ClaudeCode) if file_name == Some(".claude.json") => root
                .get_mut("projects")
                .and_then(|p| p.get_mut(dir.to_str()?))
                .and_then(|p| p.get_mut("mcpServers")),
            _ => root.get_mut("mcpServers"),
        }
        .and_then(|m| m.get_mut(&server.name))
        .with_context(|| format!("entry `{}` not found", server.name))?;
        entry["command"] = Json::from(command);
        entry["args"] = Json::from(args);
    }
    let mut text = serde_json::to_string_pretty(&root)?;
    if original.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn rewrite_toml(original: &str, entries: &[(&Server, &Decision)], exe: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = original.parse()?;
    for (server, decision) in entries {
        let Some((command, args)) = new_command(server, decision, exe) else {
            continue;
        };
        let entry = doc
            .get_mut("mcp_servers")
            .and_then(|t| t.get_mut(&server.name))
            .with_context(|| format!("entry `{}` not found", server.name))?;
        entry["command"] = toml_edit::value(command);
        let mut array = toml_edit::Array::new();
        for a in args {
            array.push(a);
        }
        entry["args"] = toml_edit::value(array);
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn plans_sensible_defaults() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let gone = h.join("deleted-project");
        write(
            &h.join(".claude.json"),
            &serde_json::json!({
                "mcpServers": {
                    "context7": {"command": "npx", "args": ["-y", "@upstash/context7-mcp"]},
                    "pw": {"command": "npx", "args": ["@playwright/mcp@latest"]},
                    "local": {"command": "node", "args": ["./server.js"]},
                    "shell": {"command": "FOO=1 npx thing"},
                    "remote": {"type": "http", "url": "https://x"}
                },
                "projects": {gone.to_str().unwrap(): {"mcpServers": {"old": {"command": "x"}}}}
            })
            .to_string(),
        );
        let servers = config::load_all(h).servers;
        let names = |d: &[(usize, Decision)]| -> Vec<(String, String)> {
            d.iter()
                .map(|(i, d)| (servers[*i].name.clone(), format!("{d:?}")))
                .collect()
        };

        let d = plan(&servers, &[], &[], false);
        assert_eq!(
            names(&d),
            [
                ("context7".into(), "Share { project: false }".into()),
                ("pw".into(), format!("{:?}", d[1].1)),
                ("local".into(), "Share { project: true }".into()),
                ("shell".into(), format!("{:?}", d[3].1)),
                ("old".into(), format!("{:?}", d[4].1)),
            ]
        );
        assert!(matches!(&d[1].1, Decision::Skip(w) if w.contains("browser")));
        assert!(matches!(&d[3].1, Decision::Skip(w) if w.contains("shell line")));
        assert!(matches!(&d[4].1, Decision::Skip(w) if w.contains("no longer exists")));

        // Naming a browser server shares it anyway.
        let d = plan(&servers, &["pw".into()], &[], false);
        assert_eq!(d, [(1, Decision::Share { project: false })]);
    }

    #[test]
    fn rewrites_and_restores_every_format() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let project = h.join("app");
        fs::create_dir_all(&project).unwrap();
        let claude = serde_json::json!({
            "numStartups": 7,
            "mcpServers": {"context7": {"type": "stdio", "command": "npx", "args": ["-y", "@upstash/context7-mcp"], "env": {}}},
            "projects": {project.to_str().unwrap(): {"allowedTools": [], "mcpServers": {"db": {"command": "postgres-mcp", "args": [], "env": {"DATABASE_URI": "postgres://localhost/x"}}}}},
            "zLast": true
        });
        let claude_text = serde_json::to_string_pretty(&claude).unwrap() + "\n";
        write(&h.join(".claude.json"), &claude_text);
        let codex_text = "# my codex config\nmodel = \"o3\"\n\n[mcp_servers.jj]\ncommand = \"npx\"\nargs = [\"-y\", \"jj\"]\n\n[mcp_servers.jj.env]\nAPI = \"https://x\"\n";
        write(&h.join(".codex/config.toml"), codex_text);
        write(
            &h.join(".cursor/mcp.json"),
            r#"{"mcpServers": {"c": {"command": "uvx", "args": ["c-mcp"]}}}"#,
        );

        let servers = config::load_all(h).servers;
        let decisions = plan(&servers, &[], &[], false);
        assert_eq!(decisions.len(), 4);
        for file in [".claude.json", ".codex/config.toml", ".cursor/mcp.json"] {
            let path = h.join(file);
            let entries: Vec<_> = decisions
                .iter()
                .filter(|(i, _)| servers[*i].source == path)
                .map(|(i, d)| (&servers[*i], d))
                .collect();
            let real = fs::canonicalize(&path).unwrap();
            let text = fs::read_to_string(&real).unwrap();
            let new = if file.ends_with(".toml") {
                rewrite_toml(&text, &entries, "/bin/mcpherd").unwrap()
            } else {
                rewrite_json(&text, &entries, "/bin/mcpherd").unwrap()
            };
            fs::write(&real, new).unwrap();
        }

        // Every entry now goes through the stub, and loads back as the real server.
        let shared = config::load_all(h).servers;
        assert!(
            shared
                .iter()
                .all(|s| s.shared_via.as_deref() == Some("/bin/mcpherd"))
        );
        let db = shared.iter().find(|s| s.name == "db").unwrap();
        match &db.transport {
            Transport::Stdio { command, env, .. } => {
                assert_eq!(command, "postgres-mcp");
                assert_eq!(env["DATABASE_URI"], "postgres://localhost/x");
            }
            _ => unreachable!(),
        }
        let raw = fs::read_to_string(h.join(".claude.json")).unwrap();
        assert!(raw.contains("\"--env\",\n") && raw.contains("\"DATABASE_URI\",\n"));
        assert!(!raw.contains("postgres://localhost/x\",\n        \"--"));
        let codex = fs::read_to_string(h.join(".codex/config.toml")).unwrap();
        assert!(codex.starts_with("# my codex config\n"));

        // Undo restores the original files exactly.
        let decisions = plan(&shared, &[], &[], true);
        for file in [".claude.json", ".codex/config.toml"] {
            let path = h.join(file);
            let entries: Vec<_> = decisions
                .iter()
                .filter(|(i, _)| shared[*i].source == path)
                .map(|(i, d)| (&shared[*i], d))
                .collect();
            let text = fs::read_to_string(&path).unwrap();
            let new = if file.ends_with(".toml") {
                rewrite_toml(&text, &entries, "/bin/mcpherd").unwrap()
            } else {
                rewrite_json(&text, &entries, "/bin/mcpherd").unwrap()
            };
            fs::write(&path, new).unwrap();
        }
        assert_eq!(
            fs::read_to_string(h.join(".claude.json")).unwrap(),
            claude_text
        );
        assert_eq!(
            fs::read_to_string(h.join(".codex/config.toml")).unwrap(),
            codex_text
        );
    }
}
