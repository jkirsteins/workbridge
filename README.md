# Workbridge

Workbridge is a terminal UI for orchestrating multi-repo development work. It
tracks work items, manages git worktrees, and drives Claude Code sessions
through a Backlog -> Planning -> Implementing -> Review -> Done workflow.

## Quick Start

### 1. Enable the git hooks

The `hooks/` directory contains git hooks that enforce code quality:

- **pre-commit** - runs `cargo fmt --check` and `cargo clippy` (lint + format)
- **pre-push** - checks for unstaged/untracked files, then runs `cargo test`

Enable them once after cloning:

```sh
git config core.hooksPath hooks
```

This is a per-repo setting.

### 2. Build and install Workbridge

Workbridge is a Rust project. Build a release binary and put it on your PATH:

```sh
cargo install --path .
```

For local development, `cargo run -- <args>` works the same way as the
installed `workbridge` binary.

### 3. Register the repos you want to manage

Workbridge does not walk your filesystem. You tell it which repos to scan,
either one at a time or by registering a base directory that gets scanned one
level deep:

```sh
workbridge repos add .                  # register the current repo
workbridge repos add ~/Projects/foo     # register a specific repo
workbridge repos add-base ~/Projects    # discover repos under ~/Projects
```

Repos added with `repos add` are always active. Repos discovered under a base
directory start unmanaged - opt them in from the TUI settings overlay (`?`)
or with an explicit `repos add`. See [docs/repository-registry.md](docs/repository-registry.md)
for the full CLI reference and config file format.

### 4. Launch the TUI

```sh
workbridge
```

The left panel lists work items grouped by status. Press `?` at any time to
open the settings overlay (config path, base dirs, managed/available repos,
defaults).

### 5. Start your first quick-start session

Press `Ctrl+N` to begin a quick-start session. If you have exactly one managed
repo, Workbridge skips the dialog and creates a Planning work item immediately
with a placeholder title; otherwise a compact "Quick start - select repo"
dialog appears so you can pick the repo with Up/Down + Space, then Enter.

The Claude session that spawns will ask what you want to work on, set a real
title via MCP, and walk through planning. When planning is done it records the
plan and the item is ready to advance to Implementing. See
[docs/work-items.md](docs/work-items.md) for the full lifecycle, including
the review and merge gates.

`Ctrl+B` opens the full creation dialog (title, description, repos, branch)
if you want to create a Backlog item instead of jumping straight into
planning.

## How It Works

Work items are Workbridge's central abstraction. Each one owns a branch, a
worktree, an optional GitHub issue, and an optional PR, and moves through a
linear sequence of stages driven by Claude Code sessions. Two gates protect
the flow: the **review gate** (PR exists, CI is green, adversarial code
review passes the plan-vs-implementation check) and the **merge gate** (the
PR is actually merged on GitHub).

```mermaid
flowchart TD
    QS["Ctrl+N<br/>quick start"] --> Planning
    CD["Ctrl+B<br/>creation dialog"] --> Backlog
    GA["Global assistant<br/>transfer"] --> Planning
    RR["Review requested<br/>on your PR"] --> Review

    Backlog --> Planning
    Planning -->|plan recorded| Implementing
    Implementing <-->|stuck / unblocked| Blocked
    Implementing -->|review gate| Review
    Blocked -->|review gate| Review
    Review -->|merge gate| Done
    Review -->|poll strategy| Mergequeue
    Mergequeue -->|PR merged externally| Done
```

See [docs/work-items.md](docs/work-items.md) for the full stage semantics,
gate behavior, and review-request workflow.

## Further Reading

- [CONTRIBUTING.md](CONTRIBUTING.md) - coding standards, error handling, UI rules
- [docs/repository-registry.md](docs/repository-registry.md) - repo registration and config
- [docs/work-items.md](docs/work-items.md) - work item lifecycle and stages
- [docs/UI.md](docs/UI.md) - TUI layout and interactions
- [docs/invariants.md](docs/invariants.md) - project invariants (read-only)

## License

Workbridge is released under the MIT License. See [LICENSE](LICENSE) for the
full text.
