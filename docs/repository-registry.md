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

# Individual repo paths (explicit additions, always active)
repos = [
    "~/Forks/some-repo",
]

# Discovered repos opted-in for management
included_repos = [
    "~/Projects/my-app",
    "~/Work/backend",
]

[defaults]
worktree_dir = ".worktrees"
branch_issue_pattern = "^(\\d+)-"
archive_after_days = 7
```

### Fields

**base_dirs**: List of directories to auto-discover git repos under. Each
directory is scanned one level deep for subdirectories containing `.git/`.
Paths support `~` for the home directory.

**repos**: List of individual repo paths. Each must contain `.git/`. Paths
support `~` for the home directory. Explicit repos are always active.

**included_repos**: List of discovered repo paths that the user has opted
in to manage. Discovered repos not in this list are "available" but not
active.

**defaults.worktree_dir**: Directory for managed worktrees, relative to the
repo root. Defaults to `.worktrees`.

**defaults.branch_issue_pattern**: Regex for extracting issue identifiers
from branch names. The first capture group is the issue identifier.
Defaults to `^(\d+)-` (leading number).

**defaults.archive_after_days**: Number of days to keep Done work items before
auto-archiving (deleting) them. Set to 0 to disable auto-archive. Defaults
to 7.

### Atomic Writes

Config saves use atomic write (write to temp file, then rename) to prevent
data loss if the process is killed mid-write.

## CLI Commands

The authoritative CLI reference - including every `workbridge repos` and
`workbridge config` subcommand, flag, exit-code contract, and output format
- lives in [cli.md](cli.md). Keep that doc in sync when the CLI surface
changes; see the "Severity overrides" section of CLAUDE.md for the
corresponding review-policy rule.

Registry-specific behaviour notes (kept here because they belong with the
config-file format above, not the CLI surface):

- **Path canonicalization on `repos remove`**: paths are compared by
  canonical path, so `.`, a relative path, and a symlink to the same target
  all resolve to the same entry. `repos remove` clears the path from
  `repos`, `base_dirs`, and `included_repos` in a single pass.
- **Source labelling in `repos list`**: entries added with `repos add` are
  labelled `explicit`; entries found under a `base_dirs` entry are labelled
  `discovered`. Only `explicit` and opted-in `discovered` repos are
  "managed"; unmanaged discovered repos only appear with `--all`.

## Startup Behavior

On startup, WorkBridge:

1. Loads the config file. If it does not exist, uses empty defaults.
   If it exists but is malformed, shows the parse error in the TUI status
   bar and falls back to empty defaults.
2. Discovers repos by scanning each base_dir one level deep for `.git/`.
3. Passes config and discovered repos to the TUI.

## TUI Settings Overlay

Press `?` in the left panel to open the settings overlay showing:

- Config file path
- Base directories (with + available / - unavailable markers)
- Managed repos (explicit + included, with source labels)
- Available repos (discovered but not managed)
- Default settings (worktree_dir, branch_issue_pattern)

Use Tab to switch between Managed and Available lists, Enter or arrow keys
to move repos between lists. Press `?` or `Escape` to close the overlay.

The main left panel now shows work items grouped by status (Unlinked, Todo,
In Progress) rather than individual tabs. Work items are assembled from
backend records combined with GitHub PR/issue data fetched in the background.

## Unavailable Repos

A registered repo may become temporarily unavailable (external drive
unmounted, directory deleted, etc.). WorkBridge does not crash or silently
drop these repos. They appear in the settings overlay and `workbridge repos`
output with availability markers.

## Future Work

- Per-repo setting overrides
- Repo groups or tags
- Multi-backend configuration (GitHub Issue, GitHub Project backends)
