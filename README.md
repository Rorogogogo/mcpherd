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

**mcpherd shows you all of it in one command, and fixes the duplicates with one more:** `mcpherd share` makes every session use one copy of each server.

---

## ✨ Features

- 🔍 **One view of every agent.** Reads Claude Code, Codex and Cursor configs, including project-level entries, `.mcp.json` files and Claude Code plugins.
- 🧠 **See what's actually running.** Lists each live session, the servers it started and how much memory each one uses.
- 👯 **Spot duplicates.** Shows the same server running in several sessions, and how much memory one shared copy would save.
- 🔑 **Catch leaked secrets.** Finds API keys in command lines, where any process can read them with `ps`, and plain-text keys in `env`.
- 🪦 **Find dead config.** Flags entries for deleted project folders and commands or scripts that no longer exist.
- 🕰️ **Find forgotten sessions.** Points out sessions that have run for days and still hold servers.
- 🐑 **One copy for every session.** `mcpherd share` routes your servers through a small daemon, so ten sessions use one copy instead of ten. It works with any stdio server, including ones you only have as a binary.
- ↩️ **Easy to undo.** `share` backs up every file it touches and `mcpherd unshare` puts each entry back exactly as it was.
- 🙈 **Safe by default.** Secrets are always masked, `doctor` never changes anything, and `share` leaves servers with per-session state alone.
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

If it shows duplicate servers, share them:

```sh
mcpherd share --dry-run   # see the plan
mcpherd share             # apply it (asks first, backs everything up)
```

New sessions now share one copy of each server. Sessions that are already open keep their own
copies until you restart them.

---

## 📖 Usage

### Commands

| Command | What it does |
|---|---|
| `mcpherd doctor` | Prints a readable report of your configured servers, running sessions and findings |
| `mcpherd doctor --json` | Prints the same report as JSON, for scripts or other tools |
| `mcpherd share` | Rewrites your configs so all sessions share one copy of each server. Asks first and backs up each file |
| `mcpherd share context7 postgres` | Shares only the servers you name |
| `mcpherd share --exclude jobs` | Shares everything that is safe to share except `jobs`, and remembers to leave `jobs` alone next time |
| `mcpherd share --dry-run` | Shows what would change without changing anything |
| `mcpherd unshare` | Puts every shared entry back the way it was (or name the ones to restore) |
| `mcpherd status` | Lists the shared servers, how many sessions use each one, and their memory |
| `mcpherd stop` | Stops the daemon and its servers. Open sessions reconnect to a fresh one the next time they use a server |
| `mcpherd --version` | Prints the version |

### How sharing works

`mcpherd share` makes one small change to each entry: the command becomes `mcpherd proxy`, with
the original command kept after `--`.

```text
before  "command": "npx", "args": ["-y", "@upstash/context7-mcp"]
after   "command": "/Users/you/.cargo/bin/mcpherd",
        "args": ["proxy", "--name", "context7", "--", "npx", "-y", "@upstash/context7-mcp"]
```

When a session starts that entry, the stub connects to the mcpherd daemon (starting it if
needed). The daemon runs **one** copy of the server and routes every session's messages to it:

- request ids, progress updates and cancellations are rewritten, so sessions never see each
  other's traffic;
- the server's handshake runs once, and later sessions get the same answer instantly;
- a server stops 5 minutes after its last session closes, so nothing is left running;
- if the daemon can't be reached, the stub runs the server directly, so a session never breaks.
  Set `MCPHERD_DIRECT=1` to always do that;
- if a shared server or the daemon goes away mid-session, each stub reconnects on its own the
  next time its session needs the server. A call that was running at that moment returns an
  error; the session's handshake is replayed to the new server, so the agent carries on as if
  nothing happened. If the daemon can't come back, the stub runs the server itself.

Sessions share a server only when they would start it the same way: same command, same
arguments and the same values for the variables in its `env`. Two Claude Code sessions, or a
Claude Code and a Codex session, with the same setup share one copy.

**What `share` leaves alone by default:**

| Skipped | Why |
|---|---|
| Servers that drive a browser, REPLs, `codex mcp-server` | They keep per-session state, so sessions would interfere |
| Entries in a project's `.mcp.json` | That file is usually committed, and would get your local path |
| Plugin servers | Their files belong to the plugin and are replaced on update |
| Remote (HTTP) servers and entries for deleted projects | Nothing to share |

Name a server explicitly (`mcpherd share playwright`) to share it anyway. Servers that read
relative paths are shared only between sessions in the same folder.

**Adding servers later.** `share` changes your configs once; it doesn't watch them. A server you
add afterwards runs one copy per session until you run `mcpherd share` again. Already shared
entries are skipped, and `mcpherd doctor` lists the ones that aren't shared yet (`not-shared`).
Servers you excluded are remembered in `~/.mcpherd/config.toml`, so a plain `mcpherd share`
keeps leaving them alone; name one to share it after all.

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
| `duplicate-processes` | Several copies of the same server are running | Run `mcpherd share` |
| `old-session` | A session has run for days and still holds servers | Close it if you're done with it |
| `redundant-project-entry` | A project repeats a server that's already defined for your user | Remove the project copy |
| `repeated-project-entry` | Several projects each define the same server | Define it once at user level |
| `cross-agent` | The same server is set up in more than one agent | `mcpherd share` lets both agents use one copy |
| `npx-wrapper` | The server starts through `npx`, which keeps an extra process alive in every session | Install the package globally and point `command` at it |
| `missing-mcpherd` | A shared entry points at an mcpherd binary that no longer exists | Reinstall mcpherd or run `mcpherd unshare` |
| `not-shared` | You use sharing, but this server still starts a copy in every session (usually added later) | Run `mcpherd share NAME`, or `--exclude NAME` to keep it per session |

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
├── report.rs      text and JSON output
├── share.rs       the config rewrite behind `share` / `unshare`
├── wrap.rs        the stub command line, and how to undo it
├── proxy.rs       the stub each session starts, and its reconnects
├── daemon.rs      runs one copy of each server and the sessions attached to it
└── router.rs      rewrites JSON-RPC ids, tokens and the handshake between sessions
```

### Roadmap

1. ✅ **`doctor`**: a read-only audit
2. ✅ **`share` / `unshare`**: one copy of each server for all sessions, through a stub and a daemon, with a one-time config rewrite that can be undone
3. 🔜 **Claude Desktop configs** and plugin servers
4. 🔜 **Per-session state** for servers that need it, so browser servers can be shared safely too

```text
Before                               After mcpherd share
session 1 ──▶ context7               session 1 ──┐
session 2 ──▶ context7               session 2 ──┼──▶ mcpherd ──▶ context7 (one copy)
session 3 ──▶ context7               session 3 ──┘
```

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
