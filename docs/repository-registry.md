# Repository Registry

WorkBridge operates across multiple repositories. The user registers
repositories via CLI, and WorkBridge discovers them on startup.

Two registration modes are supported:

- **Individual repos**: explicitly registered paths containing `.git/`
- **Base directories**: parent directories scanned one level deep for git repos

## Why a Registry

Git worktrees cannot be discovered globally. `git worktree list` only works
inside a repository. There is no system-level index of "all git repos on
this machine."

WorkBridge needs to know which repositories to scan. Rather than walking the
entire filesystem (slow, noisy, privacy-invasive), the user registers repos
or base directories explicitly.

## Configuration File

TOML format at a platform-specific path determined by the `directories` crate:

- macOS: `~/Library/Application Support/workbridge/config.toml`
- Linux: `~/.config/workbridge/config.toml`

Run `workbridge config` to see the exact path on your system.

### Format

```toml
# Directories to scan one level deep for git repos
base_dirs = [
    "~/Projects",
    "~/Work",
]

# Individual repo paths (explicit additions)
repos = [
    "~/Forks/some-repo",
]

[defaults]
worktree_dir = ".worktrees"
branch_issue_pattern = "^(\\d+)-"
```

### Fields

**base_dirs**: List of directories to auto-discover git repos under. Each
directory is scanned one level deep for subdirectories containing `.git/`.
Paths support `~` for the home directory.

**repos**: List of individual repo paths. Each must contain `.git/`. Paths
support `~` for the home directory.

**defaults.worktree_dir**: Directory for managed worktrees, relative to the
repo root. Defaults to `.worktrees`.

**defaults.branch_issue_pattern**: Regex for extracting issue identifiers
from branch names. The first capture group is the issue identifier.
Defaults to `^(\d+)-` (leading number).

### Atomic Writes

Config saves use atomic write (write to temp file, then rename) to prevent
data loss if the process is killed mid-write.

## CLI Commands

```
workbridge                          # launch TUI
workbridge config                   # print config file path and contents
workbridge repos list               # list all repos (explicit + discovered)
workbridge repos add <path>         # add individual repo
workbridge repos add-base <path>    # add base directory for auto-discovery
workbridge repos remove <path>      # remove from config
```

### workbridge repos add

Adds an individual repo. The path must contain `.git/`.

```
workbridge repos add .                    # register current directory
workbridge repos add ~/Projects/backend   # register a specific repo
```

### workbridge repos add-base

Adds a base directory. WorkBridge scans it one level deep for git repos
on startup.

```
workbridge repos add-base ~/Projects      # discovers repos under ~/Projects
```

### workbridge repos remove

Removes a path from both `repos` and `base_dirs`. Compares by canonical
path to handle symlinks and relative paths.

### workbridge repos list

Lists all repos (explicit + discovered) with their source and availability.
This is the default when running `workbridge repos` with no subcommand.

```
PATH                                                         SOURCE       AVAILABLE
--------------------------------------------------------------------------------
~/Projects/workbridge                                        explicit     yes
~/Projects/backend-api                                       discovered   yes
~/Projects/old-project                                       discovered   no
```

### workbridge config

Prints the config file path and its contents (or "(no config file yet)" if
no config exists).

## Startup Behavior

On startup, WorkBridge:

1. Loads the config file. If it does not exist, uses empty defaults.
   If it exists but is malformed, shows the parse error in the TUI status
   bar and falls back to empty defaults.
2. Discovers repos by scanning each base_dir one level deep for `.git/`.
3. Passes config and discovered repos to the TUI.

## TUI Settings Overlay

Press `?` in the left panel to open a read-only settings overlay showing:

- Config file path
- Base directories (with + available / - unavailable markers)
- Explicit repos (with availability markers)
- Discovered repos
- Default settings

Press `?` or `Escape` to close the overlay.

## Unavailable Repos

A registered repo may become temporarily unavailable (external drive
unmounted, directory deleted, etc.). WorkBridge does not crash or silently
drop these repos. They appear in the settings overlay and `workbridge repos`
output with availability markers.

## Future Work

- GitHub remote detection (parse origin URL for API calls)
- Worktree discovery within registered repos
- Per-repo setting overrides
- Repo groups or tags
