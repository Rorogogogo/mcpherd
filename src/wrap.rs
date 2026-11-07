//! The one-line change `mcpherd share` makes to a config entry, and how to undo it.
//!
//! ```text
//! before:  "command": "npx", "args": ["-y", "@upstash/context7-mcp"]
//! after:   "command": "/path/to/mcpherd",
//!          "args": ["proxy", "--name", "context7", "--", "npx", "-y", "@upstash/context7-mcp"]
//! ```
//!
//! Everything after `--` is the original command, untouched, so undoing it is exact. The
//! entry's `env` is left where it is; only the names of its keys are added, never values.

use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Wrapped {
    /// Path of the mcpherd binary the entry runs.
    pub wrapper: String,
    pub name: Option<String>,
    pub project: bool,
    pub env_keys: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
}

fn is_mcpherd(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .is_some_and(|n| n == "mcpherd")
}

/// If this entry is a shared server, what it really runs.
pub fn unwrap(command: &str, args: &[String]) -> Option<Wrapped> {
    if !is_mcpherd(command) || args.first().map(String::as_str) != Some("proxy") {
        return None;
    }
    let split = args.iter().position(|a| a == "--")?;
    let (real, real_args) = args[split + 1..].split_first()?;
    let mut out = Wrapped {
        wrapper: command.to_string(),
        name: None,
        project: false,
        env_keys: Vec::new(),
        command: real.clone(),
        args: real_args.to_vec(),
    };
    let mut flags = args[1..split].iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--name" => out.name = flags.next().cloned(),
            "--project" => out.project = true,
            "--env" => {
                out.env_keys = flags
                    .next()
                    .map(|v| v.split(',').map(String::from).collect())
                    .unwrap_or_default();
            }
            _ => {}
        }
    }
    Some(out)
}

/// The command and args that run `command args` through the daemon.
pub fn wrap(
    exe: &str,
    name: &str,
    project: bool,
    env_keys: &[String],
    command: &str,
    args: &[String],
) -> (String, Vec<String>) {
    let mut out = vec!["proxy".to_string(), "--name".into(), name.into()];
    if project {
        out.push("--project".into());
    }
    if !env_keys.is_empty() {
        out.push("--env".into());
        out.push(env_keys.join(","));
    }
    out.push("--".into());
    out.push(command.into());
    out.extend(args.iter().cloned());
    (exe.to_string(), out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn round_trips() {
        let keys = v(&["API_KEY", "REGION"]);
        let (cmd, args) = wrap(
            "/u/.cargo/bin/mcpherd",
            "jj",
            true,
            &keys,
            "npx",
            &v(&["-y", "jj", "--", "x"]),
        );
        assert_eq!(cmd, "/u/.cargo/bin/mcpherd");
        let back = unwrap(&cmd, &args).unwrap();
        assert_eq!(back.command, "npx");
        assert_eq!(back.args, v(&["-y", "jj", "--", "x"]));
        assert_eq!(back.name.as_deref(), Some("jj"));
        assert!(back.project);
        assert_eq!(back.env_keys, keys);
    }

    #[test]
    fn ignores_other_commands() {
        assert!(unwrap("npx", &v(&["-y", "proxy"])).is_none());
        assert!(unwrap("mcpherd", &v(&["doctor"])).is_none());
        assert!(unwrap("mcpherd", &v(&["proxy", "--name", "x"])).is_none());
    }
}
