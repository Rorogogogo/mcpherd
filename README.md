# mcphub

Every coding agent session starts its own copy of every local (stdio) MCP server.
Open three Claude Code sessions and a Codex window, and you are running four copies
of each server. mcphub keeps that in check.

## `mcphub doctor`

Read-only audit of every agent on the machine (Claude Code, Codex, Cursor):

- **Configs**: user, project, `.mcp.json` and plugin servers
- **Running servers**: which session started which server, and how much memory each uses
- **Findings**:
  - secrets in command lines (also visible via `ps`) or in plain-text `env` values
  - entries for project folders that no longer exist
  - commands or scripts that can't be found
  - shell lines written into `command`
  - duplicate definitions across scopes and agents
  - duplicate running copies, with the memory sharing would save
  - long-running sessions that still hold servers
  - servers launched through `npx`, which keeps an extra wrapper process per session

```sh
mcphub doctor          # human-readable report
mcphub doctor --json   # machine-readable, secrets masked
```

Secrets are always masked in output.

## Roadmap

1. `doctor`: read-only audit (this release)
2. `stub` + `daemon`: lazy start, idle shutdown, no leaked processes
3. `adopt` / `restore`: one-time config rewrite with a diff and a backup
4. Shared mode for stateless servers, across sessions and agents
