# 🐑 mcpherd

> Your AI coding agents are quietly running five copies of every MCP server. mcpherd finds them and brings them back in line.

[![Crates.io](https://img.shields.io/crates/v/mcpherd.svg)](https://crates.io/crates/mcpherd)
[![CI](https://img.shields.io/github/actions/workflow/status/Rorogogogo/mcpherd/ci.yml?branch=main&label=CI)](https://github.com/Rorogogogo/mcpherd/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/mcpherd.svg)](#-license)
[![Stars](https://img.shields.io/github/stars/Rorogogogo/mcpherd?style=flat)](https://github.com/Rorogogogo/mcpherd/stargazers)

---

## 🤔 Why?

Every time you open a new Claude Code, Codex or Cursor session, it starts its **own copy** of
every local MCP server you have configured. Nothing is shared.

```text
Claude Code session 1 ──▶ context7, postgres, jobs-tool, ...
Claude Code session 2 ──▶ context7, postgres, jobs-tool, ...
Codex window          ──▶ context7, postgres, ...
```

Three sessions means three of everything. A heavy Node-based server can use 100–250 MB, so this
easily adds up to a gigabyte or more. Sessions you forgot about keep their servers running for
days. And your config files slowly collect copy-pasted entries, dead projects and API keys sitting
in plain text.

**mcpherd shows you all of it in one command.** Later releases will fix it too.

---

## ✨ Features

- 🔍 **One view of every agent.** Reads Claude Code, Codex and Cursor configs, including project-level entries, `.mcp.json` files and Claude Code plugins.
- 🧠 **See what's actually running.** Lists each live session, the servers it started and how much memory each one uses.
- 👯 **Spot duplicates.** Shows the same server running in several sessions, and how much memory one shared copy would save.
- 🔑 **Catch leaked secrets.** Finds API keys in command lines, where any process can read them with `ps`, and plain-text keys in `env`.
- 🪦 **Find dead config.** Flags entries for deleted project folders and commands or scripts that no longer exist.
- 🕰️ **Find forgotten sessions.** Points out sessions that have run for days and still hold servers.
- 🙈 **Safe to share.** Secrets are always masked, and it never changes anything.
- ⚡ **Tiny and fast.** A single ~2 MB native binary with no runtime to install.

---

## 📸 Demo

```text
$ mcpherd doctor

Running sessions (3) · 11 MCP processes · 912 MB total
  claude-code     pid 54936   up 19d    310 MB  ~/work/api
      context7                    185 MB  npm exec @upstash/context7-mcp
      postgres                     95 MB  postgres-mcp --access-mode=restricted
  claude-code     pid 27668   up 1h     420 MB  ~/work/web
      ...

Findings (6)
  ✖ error  secret-in-command  Claude Code · notion (project ~/old/demo)
            token ntn_… is in the command line: it is stored in plain text and visible to other processes via `ps`
            fix: rotate it, then pass it through the server's `env` instead
  ▲ warn   duplicate-processes  context7
            3 copies running (sessions 54936, 27668, 29456), 555 MB in total; one shared copy would save about 370 MB
  ▲ warn   missing-project  Claude Code · local-server (project ~/tmp/scratch)
            project folder ~/tmp/scratch no longer exists
            fix: remove this project's entry from the config
  • info   old-session  claude-code pid 54936
            running for 19d and holding 310 MB in MCP servers
            fix: close it if you are no longer using it
```

<!-- A terminal recording would go here. -->

---

## 📦 Installation

**One-line install** (macOS and Linux):

```sh
curl -fsSL https://raw.githubusercontent.com/Rorogogogo/mcpherd/main/install.sh | sh
```

This downloads the right binary for your machine, checks its SHA-256 checksum and puts it in
`~/.local/bin`. Set `MCPHERD_INSTALL_DIR` to install somewhere else.

**With Cargo:**

```sh
cargo install mcpherd
```

**Manual download:** grab a binary from the
[releases page](https://github.com/Rorogogogo/mcpherd/releases). Builds are available for macOS
(Apple Silicon and Intel) and Linux (x86_64 and arm64).

---

## 🚀 Quick Start

```sh
mcpherd doctor
```

That's it. It finishes in a fraction of a second and only reads your configs and process list. Nothing is changed.

---

## 📖 Usage

### Commands

| Command | What it does |
|---|---|
| `mcpherd doctor` | Prints a readable report of your configured servers, running sessions and findings |
| `mcpherd doctor --json` | Prints the same report as JSON, for scripts or other tools |
| `mcpherd --version` | Prints the version |

### Reading the report

The report has three parts:

1. **Configured MCP servers.** Every server each agent knows about, and where it's defined (user, project or plugin).
2. **Running sessions.** Each live agent session, with the servers it started and their memory.
3. **Findings.** What's worth your attention, most serious first: `✖ error`, then `▲ warn`, then `• info`.

### What it checks

| Finding | What it means | What to do |
|---|---|---|
| `secret-in-command` | An API key is in a command line, stored in plain text and readable via `ps` | Rotate the key, then move it into `env` |
| `secret-in-env` | A key is stored in plain text in a config file | Use a `${VAR}` reference if your agent supports it |
| `missing-project` | The entry belongs to a project folder that was deleted | Remove the entry |
| `missing-command` | The server's program can't be found, so it will never start | Install it, fix the path or remove the entry |
| `missing-script` | The script the server runs (`.js`, `.py`, …) doesn't exist | Fix the path or remove the entry |
| `shell-command` | A whole shell line was put in `command` (e.g. `FOO=1 npx thing`) | Split it into `command`, `args` and `env` |
| `duplicate-processes` | Several copies of the same server are running | Close unused sessions; sharing is coming in a later release |
| `old-session` | A session has run for days and still holds servers | Close it if you're done with it |
| `redundant-project-entry` | A project repeats a server that's already defined for your user | Remove the project copy |
| `repeated-project-entry` | Several projects each define the same server | Define it once at user level |
| `cross-agent` | The same server is set up in more than one agent | Nothing yet. It's a candidate for sharing later |
| `npx-wrapper` | The server starts through `npx`, which keeps an extra process alive in every session | Install the package globally and point `command` at it |

### Where it looks

| Agent | Files |
|---|---|
| Claude Code | `~/.claude.json` (user and project servers), each project's `.mcp.json`, installed plugins |
| Codex | `~/.codex/config.toml` |
| Cursor | `~/.cursor/mcp.json` |

> **Note:** memory is measured as resident size per process tree. Memory shared between processes
> is counted more than once, so treat the numbers as a good estimate rather than an exact figure.

---

## 🏗️ Architecture

mcpherd is a single Rust binary with no runtime dependencies.

```text
src/
├── main.rs        CLI entry point
├── config.rs      reads Claude Code, Codex and Cursor configs
├── procs.rs       finds agent sessions and the servers they started
├── signature.rs   works out when two entries or processes are "the same server"
├── lint.rs        config checks (secrets, dead entries, duplicates)
├── secrets.rs     secret detection and masking
└── report.rs      text and JSON output
```

### Roadmap

`doctor` finds the problem. The next releases fix it, without asking users to do anything extra:

1. ✅ **`doctor`**: a read-only audit (available now)
2. 🔜 **Stub + daemon**: servers start only when first used, stop when idle and never get left behind
3. 🔜 **`adopt` / `restore`**: a one-time automatic config rewrite, with a diff and a backup, that can be undone
4. 🔜 **Sharing**: one copy of each stateless server for all your sessions, even across Claude Code, Codex and Cursor

```text
Today                                With mcpherd sharing
session 1 ──▶ context7               session 1 ──┐
session 2 ──▶ context7               session 2 ──┼──▶ mcpherd ──▶ context7 (one copy)
session 3 ──▶ context7               session 3 ──┘
```

Servers that keep per-session state, such as one that drives a browser, will keep running one copy
per session, so sessions never interfere with each other.

---

## 🤝 Contributing

Issues and pull requests are welcome! If `doctor` misses a problem or reports something wrong on
your machine, please [open an issue](https://github.com/Rorogogogo/mcpherd/issues) with the
relevant part of `mcpherd doctor --json`. Secrets are already masked, but check it before posting.

To work on mcpherd locally:

```sh
git clone https://github.com/Rorogogogo/mcpherd.git
cd mcpherd
cargo test
cargo run -- doctor
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings` and the tests on macOS and Linux.

---

## 📄 License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
