<div align="center">
  <img src="assets/logo.png" alt="Workbridge logo" width="180" />
  <h1>Workbridge</h1>
  <p><strong>Multi-repo coding agent orchestration in your terminal.</strong></p>
</div>

Workbridge is a terminal UI for orchestrating multi-repo development work. It
tracks work items, manages git worktrees, and drives coding agent sessions
through a Backlog -> Planning -> Implementing -> Review -> Done workflow.

<div align="center">
  <img src="assets/screenshot.png" alt="Workbridge TUI screenshot" width="800" />
</div>

## Table of Contents

- [Quick Start](#quick-start)
  - [1. Build and install Workbridge](#1-build-and-install-workbridge)
  - [2. Register the repos you want to manage](#2-register-the-repos-you-want-to-manage)
  - [3. Launch the TUI](#3-launch-the-tui)
  - [4. Start your first quick-start session](#4-start-your-first-quick-start-session)
- [How It Works](#how-it-works)
  - [Work Item Lifecycle](#work-item-lifecycle)
  - [Global assistant drawer](#global-assistant-drawer)
  - [MCP Communication](#mcp-communication)
  - [Module architecture](#module-architecture)
- [Compatibility](#compatibility)
- [Per-harness permission model](#per-harness-permission-model)
- [Further Reading](#further-reading)
- [License](#license)

## Quick Start

### 1. Build and install Workbridge

Workbridge is distributed as a Rust binary crate:

```sh
cargo install workbridge
```

For local development from a checkout, install the current workspace instead:

```sh
cargo install --path .
```

For local development without installing, `cargo run -- <args>` works the same
way as the installed `workbridge` binary.

### 2. Register the repos you want to manage

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

### 3. Launch the TUI

```sh
workbridge
```

The left panel lists work items grouped by status. Press `?` at any time to
open the settings overlay (config path, base dirs, managed/available repos,
defaults).

Before starting work, open the **Review Gate** tab in the settings overlay
(`?`, then Tab to reach the Review Gate tab) and set the "Skill (slash
command)" field. The value is passed verbatim to whichever coding agent runs
the review gate, so it can be a slash command (e.g.
`/claude-adversarial-review` for Claude Code) or plain-text guidance that any
coding agent can follow. The default is a Claude Code slash command - update
it if you are using a different coding agent.

### 4. Start your first quick-start session

Press `Ctrl+N` to begin a quick-start session. If you have exactly one managed
repo, Workbridge skips the dialog and creates a Planning work item immediately
with a placeholder title; otherwise a compact "Quick start - select repo"
dialog appears so you can pick the repo with Up/Down + Space, then Enter.

The coding agent session that spawns will ask what you want to work on, set a
real title via MCP, and walk through planning. When planning is done it records the
plan and the item is ready to advance to Implementing. See
[docs/work-items.md](docs/work-items.md) for the full lifecycle, including
the review and merge gates.

`Ctrl+B` opens the full creation dialog (title, description, repos, branch)
if you want to create a Backlog item instead of jumping straight into
planning.

## How It Works

Work items are Workbridge's central abstraction. Each one owns a branch, a
worktree, an optional GitHub issue, and an optional PR, and moves through a
linear sequence of stages driven by coding agent sessions. Two gates protect
the flow: the **review gate** (PR exists, CI is green, adversarial code
review passes the plan-vs-implementation check) and the **merge gate** (the
PR is actually merged on GitHub).

### Work Item Lifecycle

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
    Review <-->|poll strategy / retreat| Mergequeue
    Mergequeue -->|PR merged externally| Done
```

See [docs/work-items.md](docs/work-items.md) for the full stage semantics,
gate behavior, and review-request workflow.

### Global assistant drawer

Press `Ctrl+G` at any time to open the global assistant drawer. Unlike a
work item session, the global assistant has read-only access to all your
managed repos and work items, and can create new work items on your behalf
via the `workbridge_create_work_item` MCP tool. Use it to explore across
repos, ask "what is in flight right now", or kick off a Planning work item
from a freeform conversation - that last path is what the lifecycle
diagram above shows as the `Global assistant transfer -> Planning` edge.

### MCP Communication

Workbridge talks to each harness session over a per-session Unix domain
socket. The harness binary is `claude` or `codex`, picked per work item
via the `c` / `x` keys (see
[docs/harness-contract.md](docs/harness-contract.md)); the MCP tool
surface is the same for both adapters, only the spawn-side flag syntax
differs (`--mcp-config <file>` for Claude, `-c mcp_servers.<name>.*`
TOML overrides for Codex). When a session is spawned - work item
planning, implementing, review-request handling, the headless review or
rebase gate, or the global assistant drawer - Workbridge starts a small
MCP server on a fresh socket and configures the harness to spawn
`workbridge --mcp-bridge --socket <path>` as its MCP server. The bridge
subprocess pipes stdin/stdout to the socket so the harness's JSON-RPC
tool calls reach the in-process server.

Each session is handed a context blob at spawn time: a frozen snapshot
for work-item sessions, and an `Arc<Mutex<String>>` that the TUI
refreshes periodically for the global assistant. State-mutating tool
calls become `McpEvent`s on a crossbeam channel that the TUI applies on
its main thread; read-only tool calls are served directly by the MCP
server from that context (or, for `workbridge_query_log`, from the
on-disk activity log) without round-tripping through the TUI.

```mermaid
flowchart LR
    subgraph TUI["Workbridge TUI (main thread)"]
        State["Work item state<br/>(stage, plan, title, activity)"]
        Drawer["Global assistant drawer<br/>(Ctrl+G)"]
    end

    subgraph Spawn["Spawn paths (App spawn_* methods)"]
        SS["spawn_session<br/>planning / implementing /<br/>review request"]
        SR["spawn_review_gate (read_only=true)<br/>spawn_rebase_gate (read_only=false)"]
        SG["spawn_global_session"]
    end

    subgraph Sessions["Harness sessions<br/>(claude or codex)"]
        WiSession["Work item session"]
        GateSession["Headless gate session"]
        GaSession["Global assistant session"]
    end

    subgraph Mcp["MCP server (crate::mcp module)"]
        WiSock["Per-work-item socket"]
        GaSock["Global socket"]
    end

    State -- "context snapshot" --> SS
    State -- "context snapshot" --> SR
    Drawer -- "refreshable context" --> SG
    SS --> WiSession
    SR --> GateSession
    SG --> GaSession

    WiSession -- "JSON-RPC tool calls" --> WiSock
    GateSession -- "JSON-RPC tool calls" --> WiSock
    GaSession -- "JSON-RPC tool calls" --> GaSock

    WiSock -- "state-mutating tool calls<br/>become McpEvents" --> State
    GaSock -- "workbridge_create_work_item<br/>becomes an McpEvent" --> State
```

Per-session tool surface (see the `crate::mcp` module for the source of truth):

- **Interactive work-item session** and **rebase gate**: read-only
  `workbridge_get_context`, `workbridge_query_log`; mutating
  `workbridge_log_event`, `workbridge_set_activity`,
  `workbridge_delete`, `workbridge_set_status`, `workbridge_set_plan`,
  `workbridge_set_title`. The rebase gate is spawned with
  `read_only=false` and gets the same mutating set as the interactive
  session - it has to call `workbridge_log_event` to stream live
  rebase progress back to the TUI.
- **Review-request work-item session**: same read-only tools plus
  `workbridge_log_event`, `workbridge_set_activity`, and
  `workbridge_delete`; `set_status` / `set_plan` / `set_title` are
  replaced by `workbridge_approve_review` and
  `workbridge_request_changes`.
- **Review gate** (headless, `read_only=true`):
  `workbridge_get_context`, `workbridge_query_log`,
  `workbridge_get_plan`, `workbridge_report_progress`. Mutating tools
  are not exposed in `tools/list` and are rejected at `tools/call`
  even if the harness asks for them by name.
- **Global assistant session** (separate socket, separate handler):
  read-only `workbridge_list_repos`, `workbridge_list_work_items`,
  `workbridge_repo_info`; mutating `workbridge_create_work_item`,
  which spawns a new Planning work item.

### Module architecture

The workbridge binary is organized as a single crate with one module
per subsystem. The TUI runs on a single main thread that owns the
`App` aggregate; every blocking operation (git, `gh`, PTY I/O, metrics
aggregation) is spawned onto a background thread and drains back
through a crossbeam or `mpsc` channel. Host-visible APIs (clipboard,
wall-clock time, user directories) are routed through a gated
`side_effects` module so the test suite cannot touch the developer's
real environment.

```mermaid
flowchart LR
    classDef entry fill:#dbeafe,stroke:#2563eb,color:#0f172a
    classDef ui fill:#ede9fe,stroke:#7c3aed,color:#0f172a
    classDef core fill:#fef3c7,stroke:#d97706,color:#0f172a
    classDef svc fill:#dcfce7,stroke:#16a34a,color:#0f172a
    classDef bg fill:#fee2e2,stroke:#dc2626,color:#0f172a
    classDef gate fill:#f1f5f9,stroke:#475569,color:#0f172a

    Main["main<br/>binary entry + handle_cli"]:::entry
    CLI["cli::{repos, mcp,<br/>config, seed_dashboard}"]:::entry

    Salsa["salsa<br/>rat-salsa event loop"]:::ui
    App["app::App<br/>aggregate state"]:::ui
    UI["ui<br/>render functions"]:::ui
    Event["event::{keyboard,<br/>mouse, paste, layout}"]:::ui

    Assembly["assembly<br/>reassemble work items"]:::core
    CreateDialog["create_dialog<br/>CreateDialog, SetBranchDialog"]:::core
    Session["session<br/>PTY session lifecycle"]:::core
    WorkItem["work_item<br/>WorkItem types + enums"]:::core

    AgentBackend["agent_backend<br/>AgentBackend trait +<br/>claude_code / codex / opencode"]:::svc
    WorkItemBackend["work_item_backend<br/>WorkItemBackend trait +<br/>local_file / mock"]:::svc
    WorktreeService["worktree_service<br/>WorktreeService trait +<br/>git_impl"]:::svc
    GithubClient["github_client<br/>GithubClient trait +<br/>real (gh) / stub / mock"]:::svc
    Config["config<br/>Config, FileConfigProvider,<br/>loader, operations"]:::svc
    Mcp["mcp<br/>McpSocketServer, server,<br/>bridge"]:::svc

    Fetcher["fetcher<br/>per-repo poller threads"]:::bg
    Metrics["metrics<br/>dashboard aggregator"]:::bg

    SideEffects["side_effects<br/>clipboard, clock, paths<br/>(#[cfg(not(test))] gate)"]:::gate

    Main --> CLI
    Main --> Salsa
    CLI --> Config
    Salsa --> App
    Salsa --> UI
    Salsa --> Event

    Event --> App
    UI --> App
    App --> Assembly
    App --> CreateDialog
    App --> Session
    App --> WorkItem

    Assembly --> WorkItem
    Assembly --> WorkItemBackend
    App --> AgentBackend
    App --> WorkItemBackend
    App --> WorktreeService
    App --> GithubClient
    App --> Config
    App --> Mcp
    App --> Fetcher
    App --> Metrics

    Session --> AgentBackend
    Session --> Mcp

    Fetcher --> WorktreeService
    Fetcher --> GithubClient

    Metrics --> WorkItemBackend

    WorktreeService --> SideEffects
    GithubClient --> SideEffects
    WorkItemBackend --> SideEffects
    Config --> SideEffects
    Metrics --> SideEffects
```

- **Entry and CLI (blue):** `main` parses argv, dispatches to the
  appropriate `cli::*::handle_*_subcommand`, or falls through to the
  TUI path.
- **UI layer (purple):** `salsa` wires rat-salsa to the `App`
  aggregate; `ui` owns pure render functions; `event` routes
  keyboard, mouse, paste, and resize events back to `App`.
- **Core (amber):** `assembly` merges persisted records with live
  fetcher data into `WorkItem` values; `create_dialog` owns the
  creation-modal and set-branch-modal state; `session` drives PTY
  lifecycle for spawned harnesses.
- **Services (green):** every external dependency is a trait with a
  real implementation and a stub/mock. `agent_backend` isolates
  harness CLI differences; `work_item_backend` persists records;
  `worktree_service` wraps `git`; `github_client` wraps `gh`;
  `config` loads `config.toml`; `mcp` serves per-session JSON-RPC
  over a Unix socket.
- **Background workers (red):** `fetcher` runs one polling thread
  per registered repo and streams results via `mpsc`; `metrics` runs
  a single aggregator thread and streams snapshots via
  `crossbeam-channel`.
- **Side-effects gate (slate):** every clipboard / clock / user-dirs
  call routes through `side_effects::*`, which returns no-ops under
  `#[cfg(test)]` so the test suite cannot write the developer's real
  clipboard, read the system clock, or touch `$HOME`.

## Compatibility

Workbridge is harness-agnostic. Any CLI that satisfies the clauses in
[`docs/harness-contract.md`](docs/harness-contract.md) can be plugged in. Today
the shipping adapters are:

- **Claude Code** - reference adapter. Drives every workflow stage including
  the headless review gate.
- **Codex** - first-class secondary adapter. Drives every workflow stage.
  Uses a handful of CLI-level workarounds for features that Codex does not
  expose directly (see footnotes below and the per-clause notes in
  `docs/harness-contract.md`).
- **opencode** - planned. The adapter enum has a stub variant; the harness
  is not yet selectable from the picker.

Pick the harness per work item with `c` (Claude Code) / `x` (Codex) in the
work item list; the right-panel session tab title reflects the harness
actually running in the live session.

### Feature matrix

User-observable features, per harness. "Partial" means the feature works end
to end but via a different mechanism than Claude Code, and may differ in
granularity. See `docs/harness-contract.md` for the authoritative technical
contract.

| Feature                                        | Claude Code | Codex     | opencode |
| ---------------------------------------------- | :---------: | :-------: | :------: |
| Planning sessions (interactive PTY)            | Yes         | Yes       | Planned  |
| Implementing / Blocked sessions (interactive)  | Yes         | Yes       | Planned  |
| Review sessions (interactive)                  | Yes         | Yes       | Planned  |
| Review gate (headless, structured output)      | Yes         | Yes       | Planned  |
| Global assistant (`Ctrl+G`)                    | Yes         | Yes       | Planned  |
| Workbridge MCP server injection                | Yes         | Partial*  | Planned  |
| CLI-level tool allowlist                       | Yes         | Partial** | Planned  |
| Stage reminders (periodic nudges)              | Yes         | Partial***| Planned  |
| Fresh-session-per-stage invariant              | Yes         | Yes       | Planned  |

\* Claude Code accepts a single `--mcp-config <file>` JSON blob; Codex reads
its MCP servers from `~/.codex/config.toml` and is fed via per-field `-c
mcp_servers.workbridge.*=...` overrides instead. Functionally equivalent from
the user's perspective; see `docs/harness-contract.md` C4.

\** Claude Code enforces the workbridge tool allowlist via `--allowedTools`
at the CLI; Codex does not expose an equivalent flag, so tool gating relies
on the workbridge MCP server filter. Same effective result for the
read-only review gate; interactive work-item sessions see a broader tool
surface with Codex. See `docs/harness-contract.md` C5.

\*** Claude Code uses a `PostToolUse` hook to inject periodic stage
reminders; Codex has no matching hook, so the Planning reminder is embedded
in the system prompt and fires only at spawn, not on each turn. This is
strictly weaker than the hook-based delivery because it cannot re-fire after
the first turn. See `docs/harness-contract.md` C8.

## Per-harness permission model

Workbridge runs LLM coding CLIs in your terminal. Each harness has its own
permission model; workbridge's defaults differ per harness, summarised here:

| Harness     | In-CLI approval prompts | Filesystem sandbox | Network access | How it's enforced |
|-------------|-------------------------|--------------------|----------------|-------------------|
| Claude Code | Bypassed (`--dangerously-skip-permissions`) | None - Claude has no built-in sandbox | Unrestricted | workbridge MCP server allowlist (`--allowedTools`); per-stage system prompt |
| Codex       | Bypassed (`--dangerously-bypass-approvals-and-sandbox`) | None | Unrestricted | workbridge MCP server allowlist; per-server `default_tools_approval_mode = "approve"` |

Both harnesses run with full filesystem and network access - workbridge spawns
them on the same trust footing as running them yourself in a shell. The `[!]`
marker next to a session's harness name in the right-panel tab title is a
visible reminder of this.

### Why Codex doesn't use its built-in sandbox

workbridge runs each work item in a linked git worktree at
`<repo>/.worktrees/<slug>/`. Git stores that worktree's index outside the
worktree, at `<repo>/.git/worktrees/<slug>/`. Codex's default `workspace-write`
sandbox forbids writes outside the cwd, so `git commit` inside the worktree
fails: git tries to create `<repo>/.git/worktrees/<slug>/index.lock` and the
sandbox returns `Operation not permitted`.

Granting `<repo>/.git/` as a writable root does not work either - Codex's
protected-paths rule denies writes to any `.git/` directory recursively.
Granting individual subpaths (`objects/`, `refs/`, `logs/`, ...) papers over
`git commit` but still produces `packed-refs.lock` denials, blocks `git push`
(network is also off by default in workspace-write), and breaks `cargo build`
against `~/.cargo/registry/`. Rather than maintain a fragile, ever-growing
list of writable_roots that approximates "everything except `~/.ssh`",
workbridge runs Codex without the built-in sandbox.

## Further Reading

- [CONTRIBUTING.md](CONTRIBUTING.md) - coding standards, error handling, UI rules
- [RELEASING.md](RELEASING.md) - cutting a new release (cargo-release workflow)
- [docs/cli.md](docs/cli.md) - full CLI reference for every `workbridge` subcommand and flag
- [docs/repository-registry.md](docs/repository-registry.md) - repo registration and config
- [docs/work-items.md](docs/work-items.md) - work item lifecycle and stages
- [docs/UI.md](docs/UI.md) - TUI layout and interactions
- [docs/invariants.md](docs/invariants.md) - project invariants (read-only)

## License

Workbridge is released under the MIT License. See [LICENSE](LICENSE) for the
full text.

