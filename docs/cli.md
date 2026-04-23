# CLI Reference

This document is the authoritative reference for every `workbridge` subcommand,
flag, and user-facing argument. Any change to the CLI surface (new subcommand,
new flag, removed flag, changed semantics, new supported `config set` key,
changed exit-code or stdout/stderr contract) must land together with an update
to this file. See CLAUDE.md "Review Policy" for the corresponding severity
override.

The source of truth for behavior is the `handle_cli` entry point and
its per-subcommand delegates in the `cli` module. This doc mirrors that
dispatch tree.

## Overview

| Command                                  | Purpose                                                          |
|------------------------------------------|------------------------------------------------------------------|
| `workbridge`                             | Launch the TUI                                                   |
| `workbridge repos [list\|add\|add-base\|remove]` | Manage the repository registry                             |
| `workbridge mcp [add\|remove\|list\|import]`     | Manage per-repo MCP server entries                         |
| `workbridge config [set]`                | Show the config path and contents, or set a supported key        |
| `workbridge seed-dashboard <dir>`        | Dev tool: populate a work-items dir with synthetic metrics data  |
| `workbridge --mcp-bridge --socket <path>` | Internal: run as an MCP stdio<->Unix-socket bridge              |

All subcommands return exit code `0` on success and `1` on error. Errors are
written to stderr; normal output is written to stdout.

## `workbridge` (no subcommand)

```sh
workbridge
```

Launches the TUI. Reads the config file (see `workbridge config`) and the
work-item backend data dir. Config or backend load failures do not prevent
the TUI from starting; they are surfaced in the status bar.

If the `gh` CLI is not on `PATH`, the TUI still launches but PR creation and
merge features are disabled, with a warning in the status bar.

## `workbridge repos`

Manage which repositories Workbridge scans and manages. Full context and
config file format: [repository-registry.md](repository-registry.md).

### `workbridge repos` / `workbridge repos list`

```sh
workbridge repos                    # same as `repos list`
workbridge repos list               # list managed repos only
workbridge repos list --all         # list every known repo, including unmanaged
```

Prints a table with columns `PATH`, `SOURCE` (`explicit` or `discovered`),
and `AVAILABLE` (`yes` if the git directory is currently present). With
`--all`, unmanaged discovered repos are shown with a `[unmanaged]` marker.

### `workbridge repos add <path>`

```sh
workbridge repos add .
workbridge repos add ~/Projects/foo
```

Registers a single repository. Explicitly-added repos are always active -
they cannot be turned off from the TUI settings overlay without removing
them from the config.

### `workbridge repos add-base <path>`

```sh
workbridge repos add-base ~/Projects
```

Registers a base directory. Workbridge scans it one level deep for
directories containing a `.git` entry. Discovered repos start as
**unmanaged** - they show up in the settings overlay but are not actively
scanned until the user opts them in (overlay or `repos add`).

### `workbridge repos remove <path>`

```sh
workbridge repos remove ~/Projects/foo
```

Removes a path from the config entirely. Works for both individually-added
repos and base directories. If the path is not in the config, prints a
message and exits `0`.

## `workbridge mcp`

Manage MCP server entries that Workbridge installs into a repo's Claude Code
configuration when a session is spawned there.

### `workbridge mcp add <repo-path> <name> [flags]`

```sh
# stdio server
workbridge mcp add ~/Projects/foo my-server --command /path/to/bin --args -v --port 8080

# HTTP server
workbridge mcp add ~/Projects/foo my-server --url https://example.com/mcp
```

Flags:

- `--command <cmd>` - executable to launch for a stdio server. Required
  unless `--url` is given.
- `--args <arg>...` - zero or more arguments to pass to the command. Reads
  tokens until the next `--flag`.
- `--env KEY=VALUE...` - zero or more `KEY=VALUE` pairs. Reads tokens until
  the next `--flag`. Invalid entries (no `=`) exit `1`.
- `--url <url>` - HTTP endpoint for an HTTP server. Mutually exclusive with
  `--command` in practice; providing `--url` switches the server type to
  `http`.

### `workbridge mcp remove <repo-path> <name>`

```sh
workbridge mcp remove ~/Projects/foo my-server
```

Removes a named MCP server entry for the given repo. Exits `0` with a
message if the entry does not exist.

### `workbridge mcp list [<repo-path>]`

```sh
workbridge mcp list                     # every repo
workbridge mcp list ~/Projects/foo      # only entries for this repo
```

Prints entries grouped by repo. For stdio servers, shows the command and
args. For HTTP servers, shows the URL.

### `workbridge mcp import <repo-path> <json-file>`

```sh
workbridge mcp import ~/Projects/foo mcp-servers.json
```

Imports every entry from a JSON file of the form:

```json
{
  "mcpServers": {
    "my-server": {
      "type": "stdio",
      "command": "/path/to/bin",
      "args": ["-v"],
      "env": { "FOO": "bar" }
    }
  }
}
```

`type` defaults to `stdio` when absent. Existing entries for the same repo
are replaced by the imported set.

## `workbridge config`

### `workbridge config`

```sh
workbridge config
```

Prints the resolved config file path, followed by the current file contents
(or `(no config file yet)` if it has not been written). Useful for
copy-pasting the path into an editor.

### `workbridge config set <key> <value>`

```sh
workbridge config set global-assistant-harness claude-code
```

Supported keys:

- `global-assistant-harness` - picks the harness used for the global
  assistant drawer. Valid values are whatever
  `agent_backend::AgentBackendKind::from_str` accepts. Invalid values exit
  `1` without mutating the file.

Missing key or value exits `1` with a usage line. Unknown keys exit `1`
with a list of supported keys.

## `workbridge seed-dashboard <work-items-dir>`

```sh
workbridge seed-dashboard /tmp/workbridge-seed/work-items
```

Dev tool. Populates the given directory with synthetic work items so the
metrics Dashboard can be visually verified end-to-end. Intended to be run
against an isolated `HOME` override - see [metrics.md](metrics.md) for the
recommended tmux harness flow. Not meant for end-user use.

## `workbridge --mcp-bridge --socket <path>`

```sh
workbridge --mcp-bridge --socket /tmp/workbridge-mcp.sock
```

Internal mode. Pipes stdin/stdout to/from a Unix domain socket, so a
spawned harness session (any supported coding-CLI adapter, currently
Claude Code or Codex) can reach Workbridge's in-process MCP server through
a stdio MCP client. Not intended to be run manually; Workbridge spawns
itself with these flags when wiring MCP into a session.

## Related Documentation

- [repository-registry.md](repository-registry.md) - config file format,
  registry semantics, and TUI settings overlay interaction
- [metrics.md](metrics.md) - dashboard seeding harness
- [harness-contract.md](harness-contract.md) - what a harness adapter must
  provide (relevant to `config set global-assistant-harness`)
