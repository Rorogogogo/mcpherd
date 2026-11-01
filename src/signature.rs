//! Identify "the same server" across scopes, agents and running processes.

use std::path::Path;

fn basename(s: &str) -> &str {
    Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(s)
}

/// `@scope/pkg@1.2` → `@scope/pkg`, `pkg@latest` → `pkg`.
fn strip_version(pkg: &str) -> &str {
    let search_from = if pkg.starts_with('@') { 1 } else { 0 };
    match pkg[search_from..].find('@') {
        Some(i) => &pkg[..search_from + i],
        None => pkg,
    }
}

/// If the command is a package runner (`npx`, `npm exec`, `uvx`, …), the package it runs
/// and the arguments after it.
pub fn runner_package<'a>(command: &str, args: &'a [String]) -> Option<(&'a str, &'a [String])> {
    let base = basename(command);
    let rest = match base {
        "npx" | "bunx" | "pnpx" | "uvx" => args,
        "npm" | "pnpm" | "bun" | "yarn"
            if matches!(args.first().map(String::as_str), Some("exec" | "dlx" | "x")) =>
        {
            &args[1..]
        }
        _ => return None,
    };
    let i = rest.iter().position(|a| !a.starts_with('-'))?;
    Some((strip_version(&rest[i]), &rest[i + 1..]))
}

/// A normalized description of what a stdio server runs. Two entries with the same
/// signature start the same program the same way.
pub fn of(command: &str, args: &[String]) -> String {
    match runner_package(command, args) {
        Some((pkg, rest)) => join(pkg, rest),
        None => join(basename(command), args),
    }
}

fn join(head: &str, rest: &[String]) -> String {
    let mut out = head.to_string();
    for a in rest {
        out.push(' ');
        out.push_str(a);
    }
    out
}

/// Does a running process (by argv) look like it was started from this config entry?
pub fn matches_process(command: &str, args: &[String], argv: &[String]) -> bool {
    if argv.is_empty() {
        return false;
    }
    if let Some((pkg, _)) = runner_package(command, args) {
        let (head, tail) = argv.split_first().unwrap();
        if let Some((running, _)) = runner_package(head, tail) {
            return running == pkg;
        }
        // The real node process behind npx: …/node_modules/.bin/<name>
        let bin = basename(pkg);
        return argv
            .iter()
            .any(|a| a.contains("node_modules") && basename(a) == bin);
    }
    let want = basename(command);
    let launched = argv
        .iter()
        .take(2)
        .any(|a| a == command || basename(a) == want);
    launched && args.iter().all(|a| argv.contains(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn runners_collapse_to_package() {
        assert_eq!(
            of("npx", &v(&["-y", "@upstash/context7-mcp"])),
            "@upstash/context7-mcp"
        );
        assert_eq!(
            of("/opt/homebrew/bin/npx", &v(&["--yes", "jj-plugin@latest"])),
            "jj-plugin"
        );
        assert_eq!(of("npm", &v(&["exec", "brainctl", "mcp"])), "brainctl mcp");
        assert_eq!(of("node", &v(&["/a/cli.js", "mcp"])), "node /a/cli.js mcp");
    }

    #[test]
    fn matches_running_processes() {
        let npx_args = v(&["-y", "@upstash/context7-mcp"]);
        assert!(matches_process(
            "npx",
            &npx_args,
            &v(&["npm", "exec", "@upstash/context7-mcp"])
        ));
        assert!(matches_process(
            "npx",
            &npx_args,
            &v(&["node", "/u/.npm/_npx/abc/node_modules/.bin/context7-mcp"])
        ));
        assert!(!matches_process(
            "npx",
            &npx_args,
            &v(&["npm", "exec", "other-mcp"])
        ));

        let pg_args = v(&["--access-mode=restricted"]);
        let python = v(&[
            "/opt/Python",
            "/u/.local/bin/postgres-mcp",
            "--access-mode=restricted",
        ]);
        assert!(matches_process("postgres-mcp", &pg_args, &python));
        assert!(!matches_process(
            "postgres-mcp",
            &v(&["--access-mode=unrestricted"]),
            &python
        ));
    }
}
