use std::fmt::Write as _;

use serde::Serialize;

use crate::lint::{Finding, Severity};
use crate::model::Server;
use crate::procs::Session;

#[derive(Serialize)]
pub struct Report {
    pub servers: Vec<Server>,
    pub sessions: Vec<Session>,
    pub findings: Vec<Finding>,
    pub config_errors: Vec<String>,
}

pub fn mb(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

pub fn age(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{cut}…")
}

pub fn render(r: &Report) -> String {
    let mut out = String::new();
    let w = &mut out;

    let _ = writeln!(w, "Configured MCP servers ({})", r.servers.len());
    for s in &r.servers {
        let _ = writeln!(
            w,
            "  {:<12} {:<24} {:<34} {}",
            s.agent.label(),
            shorten(&s.name, 24),
            shorten(&s.scope.label(), 34),
            if s.shared_via.is_some() {
                format!("[shared] {}", shorten(&s.launch_line(), 51))
            } else {
                shorten(&s.launch_line(), 60)
            }
        );
    }

    let procs: usize = r
        .sessions
        .iter()
        .flat_map(|s| &s.servers)
        .map(|s| s.processes)
        .sum();
    let total: u64 = r
        .sessions
        .iter()
        .flat_map(|s| &s.servers)
        .map(|s| s.rss_bytes)
        .sum();
    let _ = writeln!(
        w,
        "\nRunning sessions ({}) · {} MCP processes · {} total",
        r.sessions.len(),
        procs,
        mb(total)
    );
    for s in &r.sessions {
        let held: u64 = s.servers.iter().map(|x| x.rss_bytes).sum();
        let _ = writeln!(
            w,
            "  {:<15} pid {:<7} up {:<5} {:>7}  {}",
            s.agent,
            s.pid,
            age(s.age_secs),
            mb(held),
            s.cwd.as_deref().map(|c| shorten(c, 50)).unwrap_or_default()
        );
        for x in &s.servers {
            let _ = writeln!(
                w,
                "      {:<26} {:>7}  {}",
                shorten(&x.name, 26),
                mb(x.rss_bytes),
                shorten(&x.command, 60)
            );
        }
    }

    if !r.config_errors.is_empty() {
        let _ = writeln!(w, "\nUnreadable config files");
        for e in &r.config_errors {
            let _ = writeln!(w, "  {e}");
        }
    }

    let _ = writeln!(w, "\nFindings ({})", r.findings.len());
    if r.findings.is_empty() {
        let _ = writeln!(w, "  none, all good");
    }
    for f in &r.findings {
        let mark = match f.severity {
            Severity::Error => "✖ error",
            Severity::Warn => "▲ warn ",
            Severity::Info => "• info ",
        };
        let _ = writeln!(w, "  {mark}  {}  {}", f.code, f.subject);
        let _ = writeln!(w, "            {}", f.message);
        if let Some(fix) = &f.fix {
            let _ = writeln!(w, "            fix: {fix}");
        }
    }

    let _ = writeln!(
        w,
        "\nMemory is resident size per process tree; shared pages are counted more than once."
    );
    out
}
