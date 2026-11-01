# mcpherd

Every coding agent session starts its own copy of every local (stdio) MCP server.
Open three Claude Code sessions and a Codex window, and you are running four copies
of each server, each holding its own memory. mcpherd keeps that in check.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Rorogogogo/mcpherd/main/install.sh | sh
```

or with Cargo:

```sh
cargo install mcpherd
```

Prebuilt binaries for macOS and Linux (x86_64 and arm64) are on the
[releases page](https://github.com/Rorogogogo/mcpherd/releases).

## `mcpherd doctor`

A read-only audit of every agent on the machine: Claude Code, Codex and Cursor.
It changes nothing.

```text
$ mcpherd doctor
Running sessions (3) · 11 MCP processes · 912 MB total
  claude-code     pid 54936   up 19d    310 MB  ~/work/api
      context7                    185 MB  npm exec @upstash/context7-mcp
      postgres                     95 MB  postgres-mcp --access-mode=restricted
  ...

Findings (6)
  ✖ error  secret-in-command  Claude Code · notion (project ~/old/demo)
            token ntn_… is in the command line: it is stored in plain text and visible to other processes via `ps`
            fix: rotate it, then pass it through the server's `env` instead
  ▲ warn   duplicate-processes  context7
            3 copies running (sessions 54936, 27668, 29456), 555 MB in total; one shared copy would save about 370 MB
  • info   old-session  claude-code pid 54936
            running for 19d and holding 310 MB in MCP servers
            fix: close it if you are no longer using it
```

It reads:

- **Configs**: user and project servers, `.mcp.json` files and Claude Code plugins
  (`~/.claude.json`, `~/.codex/config.toml`, `~/.cursor/mcp.json`)
- **Running sessions**: which session started which server, and the memory each one uses

It reports:

| Code | Meaning |
|---|---|
| `secret-in-command` | A secret in a command line, where it is stored in plain text and visible via `ps` |
| `secret-in-env` | A plain-text secret in an `env` value |
| `missing-project` | A config entry for a project folder that no longer exists |
| `missing-command` / `missing-script` | A server that can't start because its program or script is gone |
| `shell-command` | A whole shell line written into `command` |
| `redundant-project-entry` / `repeated-project-entry` | The same server defined in several scopes |
| `cross-agent` | The same server configured in more than one agent |
| `duplicate-processes` | Several running copies of a server, with the memory one shared copy would save |
| `old-session` | A long-running session that still holds servers |
| `npx-wrapper` | Servers launched through `npx`, which keeps an extra wrapper process per session |

Secrets are always masked in the output. `mcpherd doctor --json` prints the same
report as JSON.

## Roadmap

1. `doctor`: read-only audit (this release)
2. `stub` + `daemon`: lazy start, idle shutdown, no leaked processes
3. `adopt` / `restore`: one-time config rewrite with a diff and a backup
4. Shared mode for stateless servers, across sessions and agents

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
