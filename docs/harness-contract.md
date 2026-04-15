# Harness Contract

## Purpose

Workbridge drives an external LLM coding harness to make progress on
work items. Today there is exactly one harness in use (`claude`), wired
directly into `src/app.rs`, `src/session.rs`, and `src/mcp.rs`. That
makes the harness surface invisible: the only way to understand what
workbridge expects from a harness is to read every call site.

This document is the single authoritative spec for what **any** LLM
harness must do to be plugged into workbridge. It is written in
harness-neutral language (the clauses say "harness", not "claude") so
that a future second or third harness can be added by satisfying the
clauses rather than by copying `claude`-specific behaviour.

The reference implementation is `claude`. A secondary, not-implemented
target (`codex`) is used as a sanity check that the contract is
harness-neutral. If a clause cannot be written without naming a vendor,
the clause is wrong.

## Scope

In scope:

- What workbridge spawns, how, and with which inputs.
- What the harness must expose to workbridge (MCP, output streams,
  exit codes).
- Lifecycle and cancellation contract between workbridge and the
  harness process.

Out of scope:

- Model selection, pricing, context windows, telemetry.
- UI details of the workbridge TUI that are not observable from the
  harness side (see `docs/UI.md`).
- Invariant 13 "fresh session per stage" is referenced but not
  restated (see `docs/invariants.md`).

## Glossary

- **Harness**: the external LLM coding CLI that workbridge spawns
  (today: `claude`; future candidates: `codex`, `opencode`).
- **Harness session**: one spawned child process running the harness
  against a single work item + stage. Session identity is owned by
  workbridge, not the harness (see C12).
- **Interactive mode**: PTY-backed, long-running, driven by user
  keystrokes forwarded through the PTY master fd.
- **Headless mode**: one-shot, no PTY, structured JSON on stdout,
  exits when done. Used today by the review gate.
- **Stage**: the workbridge workflow stage (Planning, Implementing,
  Blocked, Review). Each stage maps to a different system prompt and
  slightly different spawn args.
- **Scope**: the logical container for a session - `WorkItem`,
  `ReviewGate`, or `Global`. Scope determines cwd, MCP server mode,
  and tool allowlist (see C5).
- **Mutation policy**: whether the session is allowed to mutate work
  item state via MCP tools. `ReadOnly` is mandatory for review gates.
- **Reference payload**: a copy-pasteable example (argv, MCP config
  JSON, hook body, etc.) of what the current `claude` reference
  implementation produces. See the RP section.

## Contract Clauses

### C1 - Spawn modes

The harness MUST support two spawn modes:

1. **Interactive**: a long-running child attached to a PTY, driven by
   user keystrokes written to the PTY master, producing a
   terminal-shaped byte stream on the PTY output side.
2. **Headless**: a one-shot child with no PTY, producing a single
   structured JSON payload on stdout and then exiting.

Workbridge picks the mode at spawn time based on scope: `WorkItem` and
`Global` are interactive; `ReviewGate` is headless. A harness that
only supports one mode cannot drive the review gate or cannot drive
the main panel; both are required.

### C2 - Working directory

The child process MUST start in a specific cwd chosen by workbridge:

- `WorkItem` sessions: the worktree path of the work item (**not**
  the main repo).
- `ReviewGate` sessions: the worktree path of the work item under
  review (git commands inside the gate resolve against this).
- `Global` sessions: a stable workbridge-owned scratch directory,
  **not** the user's `$HOME` and **not** any registered repo path.
  The scratch directory must be a path that a harness is willing to
  persist workspace-level trust against (so per-invocation trust
  prompts do not fire every time the drawer opens) without requiring
  workbridge to touch any third-party tool state file. `$HOME` is
  specifically disallowed because some harnesses treat the home
  directory as a special case for trust persistence.

The harness MUST honour the cwd set on the spawn. It MUST NOT second-
guess it (e.g. by walking up looking for a repo root). Sessions that
change cwd mid-flight break both the activity log and the
worktree-to-session pairing.

Workbridge itself MUST NOT work around a harness's trust or
permissions machinery by writing to that harness's state file (e.g.
editing a dotfile to pre-mark a directory as trusted); the only
supported fixes are to pick a cwd the harness already handles
correctly, use the harness's documented configuration surface, or
accept the harness's actual behaviour. This is a review-policy
invariant (`CLAUDE.md` severity overrides, "file injection" bullet),
not a style preference.

### C3 - Permissions

Workbridge bypasses in-harness permission prompts: the tool-call
granularity is considered too fine for the user to meaningfully
consent to each one inside the embedded session. Enforcement is
instead split in two:

1. The CLI flag that the harness offers for "auto-approve
   everything" is always passed. The flag name is harness-specific;
   the clause is that the harness MUST expose a non-interactive mode
   at spawn time - a harness whose only consent path is an
   interactive prompt cannot be embedded, because a blocking
   permission prompt in a PTY pane is unreachable through the UI.
2. The real enforcement boundary is the workbridge MCP server (C4,
   C11). It decides which tools exist and whether mutations are
   permitted per session.

### C4 - MCP injection

Each session MUST get its own workbridge MCP server. Workbridge
creates a fresh Unix-domain socket under `/tmp/workbridge-mcp-<pid>-
<uuid>.sock`, starts an accept loop on it, and tells the harness to
reach that server through a **stdio-to-socket bridge**: the same
workbridge binary is re-invoked with `--mcp-bridge --socket <path>`
and relays JSON-RPC between the harness's stdio channel and the Unix
socket.

The harness MUST support MCP servers that are declared as
`{"command": ..., "args": [...]}` (stdio transport). Servers that can
only be configured globally (once per user) are unacceptable - each
session needs its own socket so workbridge can scope tool-call
routing and read-only enforcement per session.

### C5 - Tool allowlist by spawn type

The harness MUST let workbridge restrict the set of tools that the
session can call. Workbridge declares three allowlist profiles:

- **Work-item profile** (Planning / Implementing / Blocked / Review):
  the full write-capable set of `workbridge_*` MCP tools plus the
  harness's built-in tools (file ops, shell, search).
- **Review-gate profile**: the same built-ins but only the read-only
  `workbridge_*` tools (see C11). Mutations are hidden from
  `tools/list` and rejected at `tools/call` by the server even if
  somehow requested.
- **Global profile**: the same write-capable `workbridge_*` set as
  the work-item profile, scoped to the user's `$HOME`.

Enforcement happens on both sides: the harness CLI receives the
allowlist, and the MCP server independently filters tools. Either
side alone is insufficient - an allowlist-only harness with no MCP
filter can still call mutations through direct socket access; an
MCP-filter-only setup can still leak non-workbridge tools that the
harness knows about natively.

### C6 - System prompt injection per stage

Every work-item spawn MUST pass a stage-specific system prompt built
by workbridge from the current plan, situation summary, and (when
applicable) a rework reason or review-gate findings. The review gate
passes its own adversarial review system prompt. The global assistant
passes a repo-listing system prompt.

The harness MUST accept a system prompt as a CLI-level input (not via
an interactive "/" command after startup), because spawn is the only
control surface workbridge has before the session is handed to the
user.

### C7 - Auto-start prompt

For interactive spawns in `Planning`, `Implementing`, and
`Review` (when there are pending review-gate findings), workbridge
MUST pass a literal initial user message (e.g. "Explain who you are
and start working."). This lets the session do useful work before the
user types anything, and it is the only mechanism that guarantees the
harness actually calls its own tools (which in turn exercises the
MCP path).

Headless spawns always have an initial prompt (the review gate
prompt). Interactive `Blocked` spawns do **not** auto-start - the
session is waiting for user input by definition.

### C8 - Stage reminders

Workbridge MUST be able to remind the session about stage-specific
obligations. The only obligation with teeth today is that the
`Planning` stage must call `workbridge_set_plan` before ending; an
interactive session that closes its plan without calling that tool
will not advance the stage.

The delivery mechanism for the reminder is deliberately unspecified:
it MAY be a hook the harness fires on a built-in tool call, an
injected system-prompt fragment, a background nudge, or anything
else. What is specified is that **the reminder exists and is
triggered from workbridge-side configuration, not from hand-written
prompt text**, so that adding a new harness is a matter of choosing
the delivery mechanism, not re-deriving the business rule.

### C9 - Output capture

Interactive sessions: workbridge reads the PTY master fd in a
dedicated reader thread and feeds every byte to a shared `vt100`
parser behind an `Arc<Mutex<_>>`. The UI thread only ever locks the
parser and renders its screen - it never reads fds directly. Any
harness that requires workbridge to consume a non-bytestream channel
(structured events, frames, etc.) in interactive mode cannot be
embedded without rewriting `src/session.rs`.

Headless sessions: workbridge reads stdout to completion via
`Command::output()`, then parses the payload as a single JSON
document. The harness MUST support a mode that emits one final
machine-readable document on stdout and uses a non-zero exit status
for failure.

### C10 - Lifecycle and cancellation

The harness process MUST be well-behaved under the following
lifecycle protocol:

1. Liveness is polled by workbridge via `waitpid(WNOHANG)` (`Child::
   try_wait`) on each background tick.
2. Graceful shutdown is SIGTERM delivered to the **process group**
   (`killpg`), with a ~50ms grace window before escalation to
   SIGKILL.
3. Drop of the `Session` struct force-kills (SIGKILL to the process
   group) and joins the reader thread. The slave PTY closing on
   child exit terminates the reader naturally; no fd manipulation
   from the UI thread is required.

The harness MUST therefore: run in its own process group, not install
signal handlers that swallow SIGTERM, not spawn grandchildren that
survive SIGKILL on the group leader, and not leave the PTY in a state
where the reader thread cannot observe EOF.

### C11 - Read-only sessions

Some sessions MUST NOT mutate work item state. The review gate is the
canonical case: the gate is an opinion, not a driver, and it runs
concurrently with a live Implementing session that is the only legal
source of state changes for that work item.

Read-only enforcement happens in the MCP server, not in the harness
CLI, because the CLI-level allowlist is a hint, not a guarantee. The
server MUST:

- Hide mutating tools from `tools/list`.
- Reject mutating tool calls at `tools/call` even when called by
  name (not just "missing from list").

A harness that is unable to present a reduced tool set and still
behave sensibly (e.g. by panicking when an expected tool is missing)
is unsafe for the review gate.

### C12 - Session identity

Session identity is owned by workbridge, not by the harness. Every
session is keyed by `(WorkItemId, WorkItemStatus)` in the registry
(`App::sessions`); a stage transition changes the key and orphans
any previous session, which is then killed on the next liveness
check. This is how `docs/invariants.md` invariant 13 (fresh session
per stage) is enforced mechanically.

Harnesses that maintain their own persistent "session id" and try to
resume conversations across workbridge restarts are fine to do so
internally but MUST NOT prevent workbridge from spawning a fresh
child for every stage transition. Any cross-stage state leakage from
the harness (for example, a chat history file that is auto-loaded
by default) MUST be defeatable at spawn time.

The same rule applies to `Global` sessions: every open of the global
assistant drawer MUST be a completely fresh harness process with no
inherited conversation, scrollback, or PTY state from any previous
opening. Closing the drawer MUST tear the session down (kill, drop
the MCP server, delete the per-session MCP config file, drop any
buffered keystrokes); the teardown is symmetric with the work-item
session teardown so new state added later cannot leak across opens.

### C13 - No env leakage

Workbridge MUST NOT set harness-specific environment variables on
the child process at spawn time. The current code path does not set
any env vars beyond what the OS inherits; a new adapter MUST keep
that property so that configuration stays inside CLI args and
MCP-config JSON files, where it is visible, auditable, and per-
session.

The reason is twofold: env vars are inherited by grandchildren
(where they can leak credentials into user-spawned subshells), and
they are invisible to the review gate, which cannot tell from a live
process whether a variable was set by workbridge or by the user.

## Implementation Map

Each subsection cites the reference implementation in `src/` and
gives a one-paragraph assessment for the Codex secondary target. The
Codex column is marked **supported** (clause is satisfiable with
documented CLI flags), **workaround** (clause needs a shim or
non-obvious config), or **not supported** (clause cannot be met with
the current Codex CLI surface).

### C1 - Spawn modes

**Claude (reference)**: Interactive mode is produced by
`App::finish_session_open` -> `Session::spawn` in `src/session.rs:57`,
which forks a `claude` process attached to a PTY slave fd. The
global assistant's interactive mode is produced by the worker
thread inside `App::spawn_global_session` (so the fork+exec runs
off the UI thread). Headless mode is produced by the review gate
worker thread at `src/app.rs:8000`, which runs the argv produced
by `ClaudeCodeBackend::build_review_gate_command` in
`src/agent_backend.rs` (yielding `claude --print --output-format
json --json-schema ...`) via `std::process::Command::output()`. The
backend is selected through the `Arc<dyn AgentBackend>` stored on
`App::agent_backend`; the spawn sites call the trait methods and
never reference the `claude` binary name directly except via
`AgentBackend::command_name`.

**Codex (secondary, not implemented)**: **supported**. Interactive
corresponds to plain `codex`; headless corresponds to `codex exec
--json` (non-interactive mode with a newline-delimited event
stream). The review gate would need a final-message extractor
because Codex's JSON stream is a series of events rather than a
single structured document, but that is parsing glue, not a clause
violation.

### C2 - Working directory

**Claude (reference)**: `Session::spawn` at `src/session.rs:57`
honours the `cwd` argument via `std::process::Command::current_dir`.
`App::finish_session_open` passes the worktree path for work-item
spawns at `src/app.rs:4198`. The worker thread inside
`App::spawn_global_session` at `src/app.rs:8572` passes a stable
workbridge-owned scratch directory
(`$TMPDIR/workbridge-global-assistant-cwd`, created idempotently
by `std::fs::create_dir_all` on the same worker thread just before
the spawn - never on the UI thread). The scratch path
is used instead of `$HOME` because Claude Code's workspace trust
dialog persists per non-home directory but NOT for `$HOME`, so
spawning in home would re-prompt the user on every Ctrl+G; spawning
in a stable non-home directory lets the harness's own trust
persistence handle the first-prompt case without workbridge ever
reading or writing `~/.claude.json`. The review gate runs `git diff`
inside the worktree on a background thread (not in the harness
child) but the harness child for `claude --print` is spawned with
the default cwd because the gate only needs MCP access to fetch the
plan.

**Codex (secondary, not implemented)**: **supported**. Codex accepts
a `--cd <path>` flag as well as inheriting the parent's cwd; either
works. No clause violation.

### C3 - Permissions

**Claude (reference)**: `ClaudeCodeBackend::build_command` in
`src/agent_backend.rs` pushes `--dangerously-skip-permissions` into
argv for every write-capable spawn; both work-item sessions
(`App::finish_session_open` at `src/app.rs:4159`) and the global
assistant (`App::spawn_global_session` at `src/app.rs:8430`) go
through the same method. The review gate uses
`ClaudeCodeBackend::build_review_gate_command` instead, which does
NOT pass the bypass because `claude --print` is non-interactive and
never prompts. Interactive read-only sessions (`SpawnConfig::
read_only = true`, no caller today) also skip the bypass flag; see
the `claude_interactive_argv_read_only_skips_permission_flags` test
in `src/agent_backend.rs`.

**Codex (secondary, not implemented)**: **supported**. Codex has
`--full-auto` and `--ask-for-approval never` for the same role.
Either flag satisfies C3 as long as it is passed on every spawn; no
clause violation.

### C4 - MCP injection

**Claude (reference)**: `build_mcp_config` in `src/mcp.rs:1378`
produces the JSON blob, and `McpSocketServer::start` at
`src/mcp.rs:80` starts the accept loop. Every filesystem side effect
on the spawn path runs on a background thread - the UI thread only
ever does pure-CPU precomputation plus the channel handoff. This
is enforced by `docs/UI.md` "Blocking I/O Prohibition" and is the
reason the session-open worker is structured as a single fat
closure that reads the plan, starts the MCP server, writes the
side-car files, writes the tempfile, and then sends a
`SessionOpenPlanResult` back for `poll_session_opens` to consume.

Work-item spawns call
`ClaudeCodeBackend::write_session_files` **from the background
worker inside `App::begin_session_open`** to write `.mcp.json` into
the worktree root (for Claude Code's project discovery). The
worker also writes a separate `/tmp/workbridge-mcp-config-<uuid>.json`
tempfile and threads its path into `SpawnConfig::mcp_config_path`.
Both paths are captured in `SessionEntry::agent_written_files` so
`AgentBackend::cleanup_session_files` can reverse them on teardown
/ `workbridge_delete`; the reverse path also runs on a detached
background thread via `App::spawn_agent_file_cleanup`. The backend
appends `--mcp-config <tempfile>` in its own argv order
(`ClaudeCodeBackend::build_command` places it AFTER the auto-start
positional - see RP1 and the `claude_interactive_argv_for_planning`
test).

The review gate writes its own tempfile inside its own
`std::thread::spawn` closure in `App::spawn_review_gate` (all three
spawn sites have always been background for the review gate) and
passes it via `ClaudeCodeBackend::build_review_gate_command`.

The global assistant uses the same two-phase pattern: the UI
thread in `App::spawn_global_session` precomputes the system
prompt and spawns a background worker that runs
`McpSocketServer::start_global`, the temp-config `std::fs::write`,
the `std::fs::create_dir_all` on the scratch cwd, AND
`Session::spawn` itself, then hands the `McpSocketServer`,
`Session`, and config path back through
`GlobalSessionOpenPending::rx` for `poll_global_session_open` to
drain into `App::global_session` / `App::global_mcp_server` /
`App::global_mcp_config_path`. No step on this path writes to
disk, binds a socket, or spawns a subprocess from the UI thread.

The bridge process is the same workbridge binary re-invoked with
`--mcp-bridge --socket <path>` (see `build_mcp_config`).

**Codex (secondary, not implemented)**: **workaround**. Codex reads
MCP server definitions from `~/.codex/config.toml` under
`[mcp_servers.*]`. There is no per-invocation `--mcp-config` flag
equivalent. A Codex adapter would have to either write a temporary
`config.toml` and point Codex at it via its config-override flag, or
use `--config mcp_servers.workbridge=...` overrides. Both are shim-
level work; the clause itself (per-session MCP injection with
stdio transport) is still achievable.

### C5 - Tool allowlist by spawn type

**Claude (reference)**: `ClaudeCodeBackend::build_command` in
`src/agent_backend.rs` passes `--allowedTools` with a comma-joined
list from the `WORK_ITEM_ALLOWED_TOOLS` constant - the 15
workbridge MCP tools shared between work-item and global-assistant
profiles. Both spawn sites (`App::finish_session_open` at
`src/app.rs:4159` via `App::build_agent_cmd` at `src/app.rs:4249`,
and the global worker spawned from `App::spawn_global_session` at
`src/app.rs:8430`) hand the same constant to
`SpawnConfig::allowed_tools`. The review gate uses
`build_review_gate_command` instead, which does NOT pass
`--allowedTools`; it relies entirely on the MCP server exposing
only the 4 read-only tools (see `src/mcp.rs` `tools/list` handling
and the `read_only_mode_exposes_only_read_tools` test at
`src/mcp.rs:1506`).

**Codex (secondary, not implemented)**: **workaround**. Codex does
not expose a fine-grained MCP tool allowlist at the CLI level; its
closest concepts are `--sandbox` (filesystem/network policy) and
`--approval-policy`. A Codex adapter would have to rely on the
workbridge MCP server filter alone for the review-gate case
(which is already the current behaviour for `claude --print`) and
either accept the broader tool surface for work-item sessions or
preprocess requests in the bridge. Not a clause violation because
the MCP-server filter is the real enforcement; the CLI allowlist is
defence in depth.

### C6 - System prompt injection per stage

**Claude (reference)**: `stage_system_prompt` at `src/app.rs:4187`
builds the prompt by rendering a per-stage template
(`planning` / `planning_retroactive` / `planning_quickstart` /
`implementing_with_plan` / `implementing_rework` /
`implementing_no_plan` / `blocked` / `review` /
`review_with_findings`) from `src/prompts.rs`. The rendered string is
threaded into `SpawnConfig::system_prompt`, and
`ClaudeCodeBackend::build_command` in `src/agent_backend.rs` pushes
`--system-prompt <string>` into argv. The review gate renders the
`review_gate` template and passes it into
`ReviewGateSpawnConfig::system_prompt`, which
`ClaudeCodeBackend::build_review_gate_command` forwards via the
same flag.

**Codex (secondary, not implemented)**: **workaround**. Codex does
not have a dedicated `--system-prompt` flag. The harness-neutral
escape hatch is to prepend the stage prompt as an initial user
message (via stdin or the positional prompt argument). This is
observably different from a true system-prompt because the model
may treat it as lower priority, but the clause (per-stage prompt
injection at spawn time) is still met.

### C7 - Auto-start prompt

**Claude (reference)**: The auto-start message for a given stage is
resolved by `App::auto_start_message_for_stage` in `src/app.rs`,
which renders `auto_start_default` or `auto_start_review` from
`prompts/stage_prompts.json` depending on whether the session is a
normal work-item open or a Review with pending gate findings. The
rendered literal is passed through `SpawnConfig::auto_start_message`
and `ClaudeCodeBackend::build_command` appends it as the positional
argument **before** `--mcp-config` so Claude Code does not mistake
it for a config file path - the ordering is locked in by the
`claude_interactive_argv_for_planning` test in
`src/agent_backend.rs`. Blocked sessions and Review sessions
without gate findings receive `auto_start_message: None` and the
backend appends nothing.

**Codex (secondary, not implemented)**: **supported**. Codex accepts
an initial prompt as a positional argument in interactive mode and
as the `-p` / stdin payload in `codex exec`. No clause violation.

### C8 - Stage reminders

**Claude (reference)**: Planning sessions get a second-layer
reminder via `--settings`, installed by
`ClaudeCodeBackend::planning_reminder_argv` in
`src/agent_backend.rs`. The hook JSON lives in the
`ClaudeCodeBackend::PLANNING_REMINDER_JSON` constant in the same
file (moved out of the inline string literal that used to sit in
`build_claude_cmd`); the constant installs a `PostToolUse` hook on
`TodoWrite` that greps the tool payload for `workbridge_set_plan`
and, if missing, writes a reminder to stderr so Claude sees it on
the next turn. Non-Planning stages use only the system-prompt-
embedded reminder from the templates in `src/prompts.rs`.

**Codex (secondary, not implemented)**: **workaround**. Codex does
not have a hook system matching Claude Code's `PostToolUse`
matcher. The fallback is to embed the reminder into the system
prompt (or the initial user message under C6) and rely on the
model to comply. C8 is explicit that the delivery mechanism is
unspecified, so this is a valid adapter choice, but it is strictly
weaker than the hook-based reminder because it cannot re-fire after
the first turn.

### C9 - Output capture

**Claude (reference)**: Interactive capture lives in
`src/session.rs` - the reader thread in `Session::spawn`
(`src/session.rs:159`) loops on `libc::read` against a dup'd master
fd and calls `vt100::Parser::process` on every chunk. The UI thread
locks the parser and renders its screen (`App::render_*` paths).
Headless capture lives around `src/app.rs:7848` - the review gate
consumes stdout via `Command::output()` and hands the bytes to
`ClaudeCodeBackend::parse_review_gate_stdout` in
`src/agent_backend.rs`, which parses the top-level JSON envelope
and reaches into `envelope["structured_output"]` for the fields.
Moving the parsing into the backend lets a second harness (e.g.
Codex `exec --json`) do its own event-stream extraction before
returning a `ReviewGateVerdict`.

**Codex (secondary, not implemented)**: **supported**. Interactive
mode produces a byte stream on the PTY exactly like any other CLI.
For headless, `codex exec --json` emits a stream of events rather
than one final document; an adapter would keep only the last
`agent_message` event (or equivalent). The PTY path is unchanged.
No clause violation.

### C10 - Lifecycle and cancellation

**Claude (reference)**: `Session::kill` at `src/session.rs:320`
implements the SIGTERM -> 50ms grace -> SIGKILL escalation against
the child's process group via `libc::killpg`. `Session::force_kill`
at `src/session.rs:304` is the SIGKILL-immediately path used in
`Drop`. `Session::is_alive` at `src/session.rs:245` uses
`Child::try_wait`. `Drop for Session` at `src/session.rs:347`
force-kills and joins the reader thread; slave-PTY close on child
exit gives the reader its EOF. Work-item session teardown goes
through `App::delete_work_item_by_id` at `src/app.rs:2259`, which
takes ownership of `SessionEntry::agent_written_files` and hands
the list to `App::spawn_agent_file_cleanup`. That helper spawns a
detached background thread that calls
`AgentBackend::cleanup_session_files` off the UI thread (see
`docs/UI.md` "Blocking I/O Prohibition" - `std::fs::remove_file`
blocks on the filesystem and must never run on the event loop), so
Claude's `.mcp.json` and any future backend's side-car files are
reversed when the work item is deleted without freezing the TUI
on a slow or wedged filesystem. The global-assistant teardown adds one extra layer on top
of `Session::kill`: `App::teardown_global_session` at
`src/app.rs:8224` kills the child, drops the `SessionEntry` (which
joins the reader via `Drop`), drops the MCP server, removes the
temp MCP config file, and drains any buffered keystrokes - symmetric
with the work-item cleanup path so new global-assistant state cannot
leak across opens.

**Codex (secondary, not implemented)**: **supported**. The
lifecycle contract is a POSIX process-group protocol, not a
harness-specific one. As long as Codex does not install a SIGTERM
handler that swallows the signal (it does not, as of the public CLI
behaviour), the existing `Session` struct handles it unchanged.

### C11 - Read-only sessions

**Claude (reference)**: The review gate passes `read_only: true` to
`McpSocketServer::start` at `src/app.rs:7789`. The server at
`src/mcp.rs:80` stores the flag into `SessionMcpConfig` and threads
it through `handle_message`, which filters `tools/list` (see
`src/mcp.rs` around line 439) and rejects mutating `tools/call`
(line 608). The unit tests
`read_only_mode_exposes_only_read_tools` and
`read_only_mode_rejects_mutating_tool_calls` in `src/mcp.rs:1506`
pin the contract.

**Codex (secondary, not implemented)**: **supported**. Read-only
enforcement is entirely inside the workbridge MCP server, which is
harness-agnostic. A Codex adapter just sets the same flag.

### C12 - Session identity

**Claude (reference)**: Sessions are stored in `App::sessions` keyed
by `(WorkItemId, WorkItemStatus)` and inserted at
`src/app.rs:3984`. Stage transitions orphan old entries, which are
killed by the periodic liveness sweep. The poll handler in
`poll_review_gate` at `src/app.rs:7910` explicitly kills the
current session and respawns when a gate rejects or errors. The
global assistant drawer uses a simpler identity rule: exactly one
live session at a time, torn down on every drawer close and
re-spawned fresh on every drawer open via
`App::toggle_global_drawer` at `src/app.rs:8192` calling
`teardown_global_session` (`src/app.rs:8224`) and
`spawn_global_session` (`src/app.rs:8245`); see also `docs/UI.md`
"Global assistant drawer session lifetime".

**Codex (secondary, not implemented)**: **supported**. Identity is
owned by workbridge; the harness only needs to exit when signalled.
A Codex adapter that uses Codex's own session-resume feature MUST
defeat it at spawn time so workbridge's fresh-session invariant is
not bypassed.

### C13 - No env leakage

**Claude (reference)**: Neither `ClaudeCodeBackend::build_command`
and `::build_review_gate_command`, `App::finish_session_open`,
`App::spawn_global_session`, nor the review gate background thread
sets any harness-specific environment variable on the child. The
child inherits the parent environment (so the user's `$PATH`,
`$HOME`, etc. are visible) but workbridge adds nothing.

**Codex (secondary, not implemented)**: **supported**. A Codex
adapter that needs to point at a per-session MCP config would
either use a CLI flag (preferred) or write to a config file (see
C4). Setting an env var like `CODEX_MCP_CONFIG` would violate C13
and MUST be avoided.

## Reference Payloads (Claude)

These payloads are what the current `claude` reference
implementation produces. They are here so a reader who knows Rust
but not workbridge can reproduce the three current spawn sites from
this document alone.

### RP1 - Interactive work-item argv

```text
claude
  --dangerously-skip-permissions
  --allowedTools mcp__workbridge__workbridge_get_context,mcp__workbridge__workbridge_query_log,mcp__workbridge__workbridge_get_plan,mcp__workbridge__workbridge_report_progress,mcp__workbridge__workbridge_log_event,mcp__workbridge__workbridge_set_activity,mcp__workbridge__workbridge_approve_review,mcp__workbridge__workbridge_request_changes,mcp__workbridge__workbridge_set_status,mcp__workbridge__workbridge_set_plan,mcp__workbridge__workbridge_set_title,mcp__workbridge__workbridge_set_description,mcp__workbridge__workbridge_list_repos,mcp__workbridge__workbridge_list_work_items,mcp__workbridge__workbridge_repo_info
  [--settings '<RP4 hook JSON, Planning only>']
  --system-prompt '<stage system prompt from stage_system_prompt()>'
  'Explain who you are and start working.'
  --mcp-config /tmp/workbridge-mcp-config-<uuid>.json
```

Source: `ClaudeCodeBackend::build_command` in
`src/agent_backend.rs`, called via `App::build_agent_cmd` at
`src/app.rs:4030` from `App::finish_session_open`. Cwd: the work
item's worktree path. The positional prompt MUST precede
`--mcp-config`; see the regression test
`claude_interactive_argv_for_planning` in the `tests` module at
the bottom of `src/agent_backend.rs`.

### RP2 - Headless review-gate argv

```text
claude
  --print
  -p '<review skill prompt from config.defaults.review_skill>'
  --system-prompt '<review_gate template with default_branch, branch, repo_path vars>'
  --output-format json
  --json-schema '{"type":"object","properties":{"approved":{"type":"boolean"},"detail":{"type":"string"}},"required":["approved","detail"]}'
  --mcp-config /tmp/workbridge-rg-mcp-<uuid>.json
```

Source: argv built by
`ClaudeCodeBackend::build_review_gate_command` in
`src/agent_backend.rs` and handed to
`std::process::Command::new(agent_backend.command_name())` at
`src/app.rs:7848`. Cwd: inherited (unspecified). The review gate
does NOT pass `--dangerously-skip-permissions` because `--print`
is non-interactive and never prompts. The review gate does NOT
pass `--allowedTools`; it relies on the read-only MCP server to
hide mutating tools.

### RP3 - MCP config JSON

```json
{
  "mcpServers": {
    "workbridge": {
      "command": "<absolute path to the workbridge binary>",
      "args": [
        "--mcp-bridge",
        "--socket",
        "/tmp/workbridge-mcp-<pid>-<uuid>.sock"
      ]
    }
  }
}
```

Source: `build_mcp_config` at `src/mcp.rs:1378`. For work-item
sessions, `extra_servers` (user-configured per-repo entries) are
inserted first; the workbridge server is appended last so it wins
on name collision. The socket path is produced by
`socket_path_for_session` at `src/mcp.rs:1424`.

### RP4 - Planning `--settings` hook payload

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "TodoWrite",
        "hooks": [
          {
            "type": "command",
            "command": "bash -c 'cat | grep -q workbridge_set_plan || echo \"REMINDER: Your plan MUST include a step to call workbridge_set_plan MCP tool to persist the plan. Add this as the FIRST step.\" >&2; true'"
          }
        ]
      }
    ]
  }
}
```

Source: `ClaudeCodeBackend::PLANNING_REMINDER_JSON` constant in
`src/agent_backend.rs`, installed into argv by
`ClaudeCodeBackend::planning_reminder_argv` when the stage is
`Planning`. Passed as the argument to `--settings` on Planning
spawns only. The harness fires the command after every `TodoWrite`
tool call; the command greps stdin (the tool payload) for
`workbridge_set_plan` and, if missing, emits a stderr reminder the
model sees on its next turn.

### RP5 - Review gate JSON envelope

The review gate parses the top-level JSON document emitted by
`claude --print --output-format json` and reaches into a nested
`structured_output` field:

```json
{
  "structured_output": {
    "approved": false,
    "detail": "why not approved, or explanation when approved"
  }
}
```

Source: `ClaudeCodeBackend::parse_review_gate_stdout` in
`src/agent_backend.rs`. The harness MUST produce an envelope whose
structured body conforms to the `--json-schema` payload in RP2;
`parse_review_gate_stdout` uses `.as_bool()` and `.as_str()` with
safe defaults, so absence of either field is interpreted as "not
approved, empty detail". A backend whose headless output shape
differs (e.g. Codex `exec --json` emits an event stream) does its
own extraction inside its `parse_review_gate_stdout` implementation
before returning the same `ReviewGateVerdict` struct.

## Trait Implementation

The provider-agnostic interface described by C1-C13 is implemented in
`src/agent_backend.rs`. `ClaudeCodeBackend` is the reference adapter;
a test-only `CodexBackend` stub in the same file proves the trait
shape fits a second harness without editing any spawn site. The
trait and config structs live in this one file so the entire
harness-specific knowledge surface is grep-able and self-contained.

The trait surface (abridged, doc comments stripped; see the file for
the full signatures and per-method clause mappings):

```rust
pub enum AgentBackendKind { ClaudeCode, /* #[cfg(test)] Codex */ }

pub struct SpawnConfig<'a> {
    pub stage: WorkItemStatus,
    pub system_prompt: Option<&'a str>,
    pub mcp_config_path: Option<&'a Path>,
    pub allowed_tools: &'a [&'a str],
    pub auto_start_message: Option<&'a str>,
    pub read_only: bool,
}

pub struct ReviewGateSpawnConfig<'a> {
    pub system_prompt: &'a str,
    pub initial_prompt: &'a str,
    pub json_schema: &'a str,
    pub mcp_config_path: &'a Path,
}

pub struct ReviewGateVerdict { pub approved: bool, pub detail: String }

pub trait AgentBackend: Send + Sync {
    fn kind(&self) -> AgentBackendKind;                              // logging / parity
    fn command_name(&self) -> &'static str;                          // C1
    fn build_command(&self, cfg: &SpawnConfig) -> Vec<String>;       // C1/C3/C5/C6/C7/C8/C11
    fn build_review_gate_command(&self, cfg: &ReviewGateSpawnConfig) // C1 headless / C11
        -> Vec<String>;
    fn parse_review_gate_stdout(&self, stdout: &str) -> ReviewGateVerdict; // C9
    fn write_session_files(&self, cwd: &Path, mcp_config_json: &str)  // C4 side-car
        -> io::Result<Vec<PathBuf>>;
    fn cleanup_session_files(&self, paths: &[PathBuf]);              // C4 reverse
}
```

The three spawn sites consume this trait via `App::agent_backend:
Arc<dyn AgentBackend>`:

- `App::finish_session_open` builds an interactive work-item spawn
  via `App::build_agent_cmd` (thin wrapper) -> `build_command`.
- The review-gate thread in `App::run_review_gate` (spawned from
  `review_gates` handling) clones `agent_backend` and calls
  `build_review_gate_command` + `parse_review_gate_stdout`.
- `App::spawn_global_session` calls `build_command` directly with
  `stage: Implementing` and `auto_start_message: None`.

The checklist for provider-agnosticism is enforced by the review
policy rule in `CLAUDE.md` ("Code that touches harness invocation...
must update `docs/harness-contract.md`") and the shape stub test
`codex_shape_compiles` in `src/agent_backend.rs::tests`, which
forces the trait to stay harness-neutral by exercising a second
implementation on every `cargo test` run.

## Known Spawn Sites

These are the only places in `src/` that launch an LLM harness child
process today. Any new spawn site MUST update this table **and**
update the Implementation Map section above.

| File          | Line  | Mode        | Scope      | Thread     | Cwd                                       |
|---------------|-------|-------------|------------|------------|-------------------------------------------|
| `src/app.rs`  | 4198  | Interactive | WorkItem   | UI thread  | Work-item worktree                        |
| `src/app.rs`  | 8000  | Headless    | ReviewGate | Background | inherited                                 |
| `src/app.rs`  | 8572  | Interactive | Global     | Background | `$TMPDIR/workbridge-global-assistant-cwd` |

The "Thread" column records which thread actually calls
`Session::spawn` / `std::process::Command::output()`. Two of the
three sites are fully off the UI thread; the work-item site still
does its `Session::spawn` on the UI thread for historical reasons
(the pre-refactor code did the same, and the fork+exec is typically
sub-millisecond). Every **filesystem** side effect on every spawn
path - the backend's `write_session_files` call, the
`--mcp-config` tempfile, the global assistant's scratch
`create_dir_all`, and the review gate's temporary `--mcp-config`
file - runs on a background worker thread per `docs/UI.md`
"Blocking I/O Prohibition". The work-item worker is
`App::begin_session_open` at `src/app.rs:3906` (drained by
`poll_session_opens` at 4095, which hands the prepared
`SessionOpenPlanResult` to `finish_session_open` at 4159). The
global worker is `App::spawn_global_session` at `src/app.rs:8430`
(drained by `poll_global_session_open`). The review gate worker is
`App::spawn_review_gate` at `src/app.rs:7636` (its own closure at
`src/app.rs:7711`).

All three sites go through `src/session.rs:57` (`Session::spawn`) for
the interactive path or `std::process::Command::output()` directly
for the headless path; argv is built by
`ClaudeCodeBackend::build_command` / `::build_review_gate_command` in
`src/agent_backend.rs` via `self.agent_backend` - no spawn site
constructs a Claude-specific argv inline. `App::build_agent_cmd` at
`src/app.rs:4249` is the thin wrapper the work-item and global
spawn sites call. Global assistant teardown lives at
`src/app.rs:8383` (`App::teardown_global_session`); see C10 and C12
for why each drawer open spawns a fresh session and each close
fully tears it down. Teardown drops any in-flight
`GlobalSessionOpenPending` entry (so a drawer-close mid-preparation
cancels the worker cleanly) and routes the global `--mcp-config`
tempfile removal through `App::spawn_agent_file_cleanup` so no
`std::fs::remove_file` runs on the event loop.

Work-item session teardown goes through
`App::delete_work_item_by_id` at `src/app.rs:2343`, which hands the
list of written side-car files back to the backend via
`AgentBackend::cleanup_session_files` - routed off the UI thread
via `App::spawn_agent_file_cleanup`.

## Change Log

This doc is the authoritative harness contract spec. When a spawn
site changes, when a clause is added or relaxed, or when a new
harness adapter is introduced, add a dated bullet here.

- 2026-04-15: Initial spec. Captures the three current `claude` spawn
  sites, 13 clauses, reference payloads RP1-RP5, and the Codex
  secondary-target sanity check. No code changes; `CLAUDE.md`
  severity overrides and review guidelines updated in the same
  change.
- 2026-04-15: Rebase audit against `master` (PR #84, "Global
  assistant: spawn fresh every Ctrl+G, use scratch cwd"). Updated
  C2 (Global cwd: `$HOME` -> `$TMPDIR/workbridge-global-assistant-
  cwd`), added the file-injection prohibition cross-reference,
  extended C10 with `App::teardown_global_session`, extended C12
  with the fresh-every-open Global behaviour and the
  `toggle_global_drawer` -> `teardown_global_session` /
  `spawn_global_session` cycle, and refreshed every `src/app.rs`
  line citation to match the post-rebase tree (build_claude_cmd
  3870 -> 3967, review-gate Command::new 7500 -> 7784,
  spawn_global_session 7884 -> 8201, etc.). The Known Spawn Sites
  table now reflects the new line numbers and the new Global cwd.
- 2026-04-15: Trait implementation landed. The "Target Trait
  Sketch" section (illustrative `trait Harness` sketch) was
  replaced with a pointer to `src/agent_backend.rs`, where
  `AgentBackend`, `ClaudeCodeBackend`, and a `#[cfg(test)]`
  `CodexBackend` now live. Every spawn site
  (`App::finish_session_open`, the review-gate thread inside
  `App::run_review_gate`, `App::spawn_global_session`) now builds
  argv via the trait instead of inline `"claude"` literals. Moved
  the planning-stage `PostToolUse` hook JSON from an inline string
  in `build_claude_cmd` to
  `ClaudeCodeBackend::PLANNING_REMINDER_JSON`; moved the two
  auto-start user prompts from inline strings to new
  `auto_start_default` / `auto_start_review` keys in
  `prompts/stage_prompts.json`. `claude_working` renamed to
  `agent_working`; `build_claude_cmd` renamed to `build_agent_cmd`
  (now a thin `self.agent_backend.build_command` wrapper).
  Refreshed every Implementation Map citation (C1..C13) and the
  Known Spawn Sites table line numbers
  (work-item 3931 -> 3979, review-gate 7784 -> 7848, global 8313 ->
  8344, teardown 8185 -> 8224, spawn_global 8201 -> 8245).
  `CodexBackend` is not wired; a shape-verification test
  `codex_shape_compiles` asserts the trait fits a second harness
  with `--full-auto`, `--config`, and no `PostToolUse` equivalent.
- 2026-04-15: Blocking-I/O compliance pass for every spawn path
  (same PR, follow-up to adversarial review rounds 1 and 2). Every
  `std::fs::*` call and every `McpSocketServer::start*` call on
  the session-open paths now runs on a background worker thread;
  no filesystem I/O is left on the UI thread on any spawn or
  teardown path. Work-item sessions: `App::begin_session_open`
  now pre-computes MCP context on the UI thread and hands a fat
  worker the plan read, `McpSocketServer::start`,
  `AgentBackend::write_session_files`, and the
  `std::fs::write` on the temp `--mcp-config` file; the worker's
  `SessionOpenPlanResult` grew `server` / `written_files` /
  `mcp_config_path` / `server_error` / `mcp_config_error` fields
  which `poll_session_opens` drains on the next tick.
  `finish_session_open` is now pure-CPU plus `Session::spawn`
  (which stays on the UI thread for the work-item path because
  it was already there pre-refactor and fork+exec is sub-ms).
  `start_mcp_for_session` was removed since its only caller is
  gone. Global assistant: `App::spawn_global_session` now runs
  `McpSocketServer::start_global`, both `std::fs::write` calls,
  `std::fs::create_dir_all`, AND `Session::spawn` itself on a
  background worker; `App::poll_global_session_open` drains the
  `GlobalSessionOpenPending::rx` and moves handles into the
  durable `global_session` / `global_mcp_server` /
  `global_mcp_config_path` fields. `teardown_global_session` now
  cancels any in-flight preparation and routes the temp config
  file removal through `App::spawn_agent_file_cleanup`.
  `cleanup_all_mcp` also routes its file removal through
  `spawn_agent_file_cleanup` now. `docs/UI.md` updated to
  describe the two-phase pattern for both spawn sites.
