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
  (today: `claude`, `codex`; future candidate: `opencode`).
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
`App::finish_session_open` -> `Session::spawn` in `src/session.rs`,
which forks a `claude` process attached to a PTY slave fd. The
global assistant's interactive mode is produced by the worker
thread inside `App::spawn_global_session` (so the fork+exec runs
off the UI thread). Headless mode is produced by the review gate
worker thread, which runs the argv produced by
`ClaudeCodeBackend::build_review_gate_command` in
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

**Claude (reference)**: `Session::spawn` in `src/session.rs`
honours the `cwd` argument via `std::process::Command::current_dir`.
`App::finish_session_open` passes the worktree path for work-item
spawns. The worker thread inside
`App::spawn_global_session` passes a stable
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
(`App::finish_session_open`) and the global
assistant (`App::spawn_global_session`) go
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

**Claude (reference)**: `build_mcp_config` in `src/mcp.rs`
produces the JSON blob, and `McpSocketServer::start` in
`src/mcp.rs` starts the accept loop. Every filesystem side effect
on the spawn path runs on a background thread - the UI thread only
ever does pure-CPU precomputation plus the channel handoff. This
is enforced by `docs/UI.md` "Blocking I/O Prohibition" and is the
reason the session-open worker is structured as a single fat
closure that reads the plan, starts the MCP server, writes the
side-car files, writes the tempfile, and then sends a
`SessionOpenPlanResult` back for `poll_session_opens` to consume.

Work-item spawns write a
`/tmp/workbridge-mcp-config-<uuid>.json` tempfile from the
background worker inside `App::begin_session_open` and thread its
path into `SpawnConfig::mcp_config_path`.
`ClaudeCodeBackend::write_session_files` returns an empty list -
no side-car files are written into the worktree because
`--mcp-config` handles MCP injection entirely via the CLI flag
and writing into the worktree would pollute git state (prohibited
by the file-injection review policy rule). The tempfile path is
captured in `SessionEntry::agent_written_files` so
`AgentBackend::cleanup_session_files` can reverse it on teardown
/ `workbridge_delete`; the reverse path runs on a detached
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

**Codex**: **workaround (implemented)**. Codex reads MCP server
definitions from `~/.codex/config.toml` under `[mcp_servers.*]` and
exposes the same shape via `--config key=value` (alias `-c`)
overrides. There is no `[mcp_servers.<name>].config = "<path>"`
sub-field that reads an external JSON - verified against the live
CLI with `codex -c 'mcp_servers.workbridge.config="/tmp/fake.json"'
mcp list`, which fails with "invalid transport in
`mcp_servers.workbridge`".

`CodexBackend` in `src/agent_backend.rs` therefore emits per-field
overrides built from a structured `McpBridgeSpec` (command +
args): `-c mcp_servers.workbridge.command="<exe>"` and `-c
mcp_servers.workbridge.args=["--mcp-bridge","--socket","<sock>"]`.
The helper `CodexBackend::extend_mcp_bridge_argv` renders the TOML
values using a small `toml_quote_string` / `toml_quote_string_array`
helper so paths and prompts with special characters (quotes,
backslashes, newlines, equals signs) survive Codex's TOML parser
as literal strings.

The caller still writes the Claude-shaped JSON to
`mcp_config_path` (it is used by Claude's adapter and by the
on-disk config parity logging), but for Codex that path is
deliberately NOT referenced in argv - only the structured
`SpawnConfig::mcp_bridge` field is. Missing `mcp_bridge` (e.g.
MCP socket bind failed) causes Codex to omit the overrides
entirely, rather than falling back to `~/.codex/config.toml`
(which would silently cross-contaminate personal config with
workbridge runtime state). Pinned by the
`codex_mcp_config_injected_via_config_flag` and
`codex_mcp_bridge_none_omits_workbridge_overrides` tests; verified
live against the real CLI on 2026-04-16.

### C5 - Tool allowlist by spawn type

**Claude (reference)**: `ClaudeCodeBackend::build_command` in
`src/agent_backend.rs` passes `--allowedTools` with a comma-joined
list from the `WORK_ITEM_ALLOWED_TOOLS` constant - the 15
workbridge MCP tools shared between work-item and global-assistant
profiles. Both spawn sites (`App::finish_session_open` via
`App::build_agent_cmd`, and the global worker spawned from
`App::spawn_global_session`) hand the same constant to
`SpawnConfig::allowed_tools`. The review gate uses
`build_review_gate_command` instead, which does NOT pass
`--allowedTools`; it relies entirely on the MCP server exposing
only the 4 read-only tools (see `src/mcp.rs` `tools/list` handling
and the `read_only_mode_exposes_only_read_tools` test in
`src/mcp.rs`).

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

**Claude (reference)**: `stage_system_prompt` in `src/app.rs`
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
loops on `libc::read` against a dup'd master
fd and calls `vt100::Parser::process` on every chunk. The UI thread
locks the parser and renders its screen (`App::render_*` paths).
Headless capture lives in the review gate worker - the review gate
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

**Claude (reference)**: `Session::kill` in `src/session.rs`
implements the SIGTERM -> 50ms grace -> SIGKILL escalation against
the child's process group via `libc::killpg`. `Session::force_kill`
in `src/session.rs` is the SIGKILL-immediately path used in
`Drop`. `Session::is_alive` uses
`Child::try_wait`. `Drop for Session`
force-kills and joins the reader thread; slave-PTY close on child
exit gives the reader its EOF. Work-item session teardown goes
through `App::delete_work_item_by_id`, which
takes ownership of `SessionEntry::agent_written_files` and hands
the list to `App::spawn_agent_file_cleanup`. That helper spawns a
detached background thread that calls
`AgentBackend::cleanup_session_files` off the UI thread (see
`docs/UI.md` "Blocking I/O Prohibition" - `std::fs::remove_file`
blocks on the filesystem and must never run on the event loop), so
the `--mcp-config` tempfile and any future backend's side-car
files are reversed when the work item is deleted without freezing
the TUI on a slow or wedged filesystem. The global-assistant teardown adds one extra layer on top
of `Session::kill`: `App::teardown_global_session`
kills the child, drops the `SessionEntry` (which
joins the reader via `Drop`), drops the MCP server, routes the
temp MCP config file removal through `App::spawn_agent_file_cleanup`
(off the UI thread), cancels any in-flight
`GlobalSessionOpenPending` worker by dropping the receiver, and
drains any buffered keystrokes - symmetric with the work-item
cleanup path so new global-assistant state cannot leak across
opens.

**Codex (secondary, not implemented)**: **supported**. The
lifecycle contract is a POSIX process-group protocol, not a
harness-specific one. As long as Codex does not install a SIGTERM
handler that swallows the signal (it does not, as of the public CLI
behaviour), the existing `Session` struct handles it unchanged.

### C11 - Read-only sessions

**Claude (reference)**: The review gate passes `read_only: true` to
`McpSocketServer::start`. The server in
`src/mcp.rs` stores the flag into `SessionMcpConfig` and threads
it through `handle_message`, which filters `tools/list` and rejects
mutating `tools/call`. The unit tests
`read_only_mode_exposes_only_read_tools` and
`read_only_mode_rejects_mutating_tool_calls` in `src/mcp.rs`
pin the contract.

**Codex (secondary, not implemented)**: **supported**. Read-only
enforcement is entirely inside the workbridge MCP server, which is
harness-agnostic. A Codex adapter just sets the same flag.

### C12 - Session identity

**Claude (reference)**: Sessions are stored in `App::sessions` keyed
by `(WorkItemId, WorkItemStatus)` and inserted inside
`finish_session_open` on the UI thread.
Stage transitions orphan old entries, which are killed by the
periodic liveness sweep. The poll handler in `poll_review_gate`
explicitly kills the current session and
respawns when a gate rejects or errors. The global assistant
drawer uses a simpler identity rule: exactly one live session at
a time, torn down on every drawer close and re-spawned fresh on
every drawer open via `App::toggle_global_drawer` calling
`teardown_global_session` and `spawn_global_session`;
see also `docs/UI.md` "Global assistant
drawer session lifetime".

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

**Codex**: **supported (implemented)**. `CodexBackend` delivers the
full per-session payload (system prompt, MCP server definition,
write-access flag) exclusively through `--config key=value`
overrides on the CLI argv. No harness-specific environment
variable is set on the child, and `write_session_files` returns
an empty list (no side-car writes). Specifically:
- system prompt -> `-c instructions="<prompt>"`
- MCP bridge command -> `-c mcp_servers.workbridge.command="<exe>"`
- MCP bridge args -> `-c mcp_servers.workbridge.args=[...]`
- permissions -> `--full-auto` (interactive work-capable +
  rebase gate) or omitted (review gate)

No `CODEX_MCP_CONFIG`, `CODEX_CONFIG_FILE`, `CODEX_HOME`, or any
other env var is touched. The only filesystem writes workbridge
performs for a Codex session are under `std::env::temp_dir()`
(the MCP config JSON, used by the on-disk parity path and by
future harnesses; Codex itself does not read it).

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
`src/agent_backend.rs`, called via `App::build_agent_cmd`
from `App::finish_session_open`. Cwd: the work
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
`std::process::Command::new(agent_backend.command_name())` inside
the review gate's `std::thread::spawn` worker closure.
Cwd: inherited (unspecified). The review gate
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

Source: `build_mcp_config` in `src/mcp.rs`. For work-item
sessions, `extra_servers` (user-configured per-repo entries) are
inserted first; the workbridge server is appended last so it wins
on name collision. The socket path is produced by
`socket_path_for_session` in `src/mcp.rs`.

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

| File          | Line   | Mode        | Scope       | Thread     | Cwd                                       |
|---------------|--------|-------------|-------------|------------|-------------------------------------------|
| `src/app.rs`  | 5962   | Interactive | WorkItem    | Background | Work-item worktree                        |
| `src/app.rs`  | 10435  | Headless RO | ReviewGate  | Background | inherited                                 |
| `src/app.rs`  | 10914  | Headless RW | RebaseGate  | Background | Work-item worktree                        |
| `src/app.rs`  | 12145  | Interactive | Global      | Background | `$TMPDIR/workbridge-global-assistant-cwd` |

The "Thread" column records which thread actually calls
`Session::spawn` / `std::process::Command::output()`. All four
sites are fully off the UI thread. Every blocking operation on
every spawn path - the backend's `write_session_files` call, the
`--mcp-config` tempfile, the global assistant's scratch
`create_dir_all`, the review gate's temporary `--mcp-config`
file, and the `Session::spawn` fork+exec itself - runs on a
background worker thread per `docs/UI.md` "Blocking I/O
Prohibition". The work-item path uses a two-phase pipeline:
Phase 1 (`App::begin_session_open`, drained by
`poll_session_opens` which hands the prepared
`SessionOpenPlanResult` to `finish_session_open`) does plan read,
MCP socket bind, side-car file writes, and temp config write;
Phase 2 (`finish_session_open` spawns a thread, drained by
`poll_session_spawns`) does the `Session::spawn` fork+exec after
the UI thread builds the command (pure CPU). The global worker is
`App::spawn_global_session` (drained by
`poll_global_session_open`). The review gate worker is
`App::spawn_review_gate` (its own closure).

All three sites go through `Session::spawn` in `src/session.rs` for
the interactive path or `std::process::Command::output()` directly
for the headless path; argv is built by
`ClaudeCodeBackend::build_command` / `::build_review_gate_command` in
`src/agent_backend.rs` via `self.agent_backend` - no spawn site
constructs a Claude-specific argv inline. `App::build_agent_cmd`
is the thin wrapper the work-item and global
spawn sites call. Global assistant teardown lives in
`App::teardown_global_session`; see C10 and C12
for why each drawer open spawns a fresh session and each close
fully tears it down. Teardown drops any in-flight
`GlobalSessionOpenPending` entry (so a drawer-close mid-preparation
cancels the worker cleanly) and routes the global `--mcp-config`
tempfile removal through `App::spawn_agent_file_cleanup` so no
`std::fs::remove_file` runs on the event loop.

Work-item session teardown goes through
`App::delete_work_item_by_id`, which hands the
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
  `finish_session_open` is now pure-CPU; `Session::spawn` was
  later moved off the UI thread in a follow-up (see the
  2026-04-16 changelog entry).
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
- 2026-04-15: Session-spawn worker cancellation + citation refresh
  (same PR, follow-up to adversarial review rounds 2 and 3). Both
  `SessionOpenPending` and `GlobalSessionOpenPending` gained a
  shared `cancelled: Arc<AtomicBool>` flag that workers load via
  `Ordering::Acquire` before every `std::fs::write`,
  `McpSocketServer::start*`, and `std::fs::create_dir_all`. Every
  abort site (`cleanup_session_state_for`, the `Disconnected` arm
  of `poll_session_opens`, the stage-transition respawn site,
  `teardown_global_session`, and `cleanup_all_mcp`) sets the flag
  via `Ordering::Release` before scheduling
  `spawn_agent_file_cleanup` so the worker bails out on the
  remaining blocking operations. A new `cancel_session_open_entry`
  helper owns the cancellation path for the work-item flow;
  `drop_session_open_entry` stays as the no-cleanup normal-drain
  variant used after `poll_session_opens` has an `Ok(result)`.
  The work-item `mcp_config_path` is now committed on the UI
  thread (mirroring the R1 global fix) so every abort site knows
  which tempfile to clean up. `cleanup_all_mcp` drains three
  sources on shutdown: `global_mcp_config_path`, in-flight
  `global_session_open_pending`, and in-flight `session_open_rx`
  entries. `check_liveness` on global child death now also routes
  `global_mcp_config_path` through `spawn_agent_file_cleanup`.
  Refreshed every `src/app.rs:NNNN` and `src/mcp.rs:NNNN` citation
  in this doc against the post-commit tree; the Known Spawn Sites
  table now points at 4361 / 8166 / 8807 (the actual three spawn
  call sites). Citations are brittle to future code edits - if
  round-4 code changes shift lines again, a follow-up sweep is
  required.
- 2026-04-15: Async-spawn UX + cleanup completeness (same PR,
  follow-up to a Codex review pass on top of round 4). Two P2
  regressions the round-4 reviewer missed and Codex caught:
  - `flush_pty_buffers` now gates the global / active / terminal
    PTY flush on the corresponding session being live AND having
    an attached PTY handle. Without this gate, the keystrokes a
    user typed in the ~one-tick window between `Ctrl+G` opening
    the drawer and `poll_global_session_open` installing the
    session were silently lost: the `take()` in `flush_pty_buffers`
    cleared the buffer before `send_bytes_to_global` discovered
    there was no session to write to. Now buffered bytes stay
    parked until the worker installs the session, then flush in
    one batch on the next tick. Same gate applies symmetrically
    to the active work-item pane and the terminal pane. Two
    regression tests
    (`flush_pty_buffers_preserves_global_bytes_when_no_session`
    and `flush_pty_buffers_drains_global_bytes_once_session_alive`)
    pin the contract.
  - `SessionOpenPending` gained `committed_files: Arc<Mutex<Vec<PathBuf>>>`.
    The work-item session-open worker pushes each successfully
    written side-car file into this shared list immediately after
    the write. On cancellation, `cancel_session_open_entry` and
    `cleanup_all_mcp` drain the mutex and feed the entries into
    `spawn_agent_file_cleanup` alongside the UI-thread-committed
    `mcp_config_path`. Without this list, a cancellation race
    (worker writes a side-car file, main thread drops the receiver
    before the worker can `tx.send` the `written_files` Vec) would
    orphan the file until the next delete swept the directory.
    Regression test
    `cancel_session_open_entry_cleans_committed_side_car_files`
    pins the cleanup behaviour. Note: `ClaudeCodeBackend` currently
    returns no side-car files (MCP injection uses `--mcp-config`
    only), so the `committed_files` machinery is exercised only by
    future backends that need file-based config; the cancellation
    test still validates the plumbing with a synthetic file list.
- 2026-04-16: Remove worktree .mcp.json injection + move
  Session::spawn off UI thread (same PR, follow-up to Codex review).
  `ClaudeCodeBackend::write_session_files` now returns an empty
  list - the `--mcp-config` flag is sufficient and writing
  `.mcp.json` into the user's worktree pollutes git state
  (prohibited by the file-injection review policy rule). The
  `AgentBackend::write_session_files` trait doc now explicitly
  prohibits writing into the worktree; backends MUST use temp
  directories for any config files they need to write.
  `cleanup_session_state_for` now drains `agent_written_files`
  from live session entries so natural session death (detected by
  `check_liveness`) and orphan removal clean up the `--mcp-config`
  tempfile instead of leaking it. The orphan removal loop in
  `check_liveness` also drains `agent_written_files` before
  dropping removed entries.
  Work-item `Session::spawn` moved off the UI thread: the
  `finish_session_open` path now uses a two-phase pipeline where
  Phase 1 (`begin_session_open` worker, drained by
  `poll_session_opens`) handles plan read, MCP socket bind,
  side-car file writes, and temp config write, and Phase 2
  (`finish_session_open` spawns a thread, drained by
  `poll_session_spawns`) handles the `Session::spawn` fork+exec.
  All three spawn sites (work-item, review-gate, global) are now
  fully off the UI thread. Known Spawn Sites table updated.
- 2026-04-16: Codex adapter + per-work-item harness selection +
  first-run Ctrl+G modal. `AgentBackendKind::Codex` promoted out
  of `#[cfg(test)]` and `CodexBackend` is now a real adapter
  satisfying C1..C13 with these workarounds, all pinned by unit
  tests in `src/agent_backend.rs::tests::codex_*`:
  - C1: `codex` (interactive) / `codex exec --json` (headless).
  - C2: PTY sets cwd (same mechanism as Claude); `--cd` flag is
    available but not used.
  - C3: `--full-auto` is the permission-bypass flag; omitted for
    read-only spawns per parity with Claude's
    `--dangerously-skip-permissions` convention.
  - C4: MCP injection via per-field `--config
    mcp_servers.workbridge.command="<exe>"` plus
    `--config mcp_servers.workbridge.args=[...]`. Per-repo extras
    from `Config::mcp_servers_for_repo` are forwarded as additional
    `mcp_servers.<name>.{command,args}` pairs (one set per entry,
    threaded through `SpawnConfig::extra_bridges` /
    `ReviewGateSpawnConfig::extra_bridges`). HTTP-transport entries
    are filtered out at the spawn site because Codex's TOML schema
    has no `mcp_servers.<name>.url` field. No
    `~/.codex/config.toml` mutation (file-injection rule).
  - C5: no CLI allowlist; enforced at the MCP server layer
    (same mechanism as Claude's review gate).
  - C6: `--config instructions=<prompt>` (Codex has no
    `--system-prompt`).
  - C7: auto-start prompt as the last positional argument.
  - C8: **workaround** - Codex has no `PostToolUse` hook; the
    Planning reminder is embedded in the system prompt. Strictly
    weaker than Claude's hook because it cannot re-fire on
    subsequent turns.
  - C9 / C10 / C11 / C12: unchanged from the shared
    infrastructure (PTY / `run_cancellable` / MCP filter /
    fresh-per-open).
  - C13: no env vars, no `$HOME` writes.
  `OpenCodeBackend` added as future-work scaffolding only:
  `build_command` returns just `["opencode"]`,
  `build_review_gate_command` / `build_headless_rw_command` return
  empty argv, and `parse_review_gate_stdout` returns a diagnostic
  "not yet implemented" verdict. The backend is NOT user-
  selectable: `AgentBackendKind::OpenCode` is excluded from
  `AgentBackendKind::all()`, rejected by `AgentBackendKind::from_str`
  so `workbridge config set global-assistant-harness opencode`
  fails, and is not bound to any keystroke. The enum variant and
  the `backend_for_kind` arm are kept so a future real adapter can
  land without reintroducing the type at the same time. The `o`
  keybinding is reserved for "open PR in browser" (its pre-
  existing meaning); there is no harness picker on `o`.
  Per-work-item selection: new `App::harness_choice:
  HashMap<WorkItemId, AgentBackendKind>` stores the user's pick
  from c (Claude) / x (Codex). Spawn sites
  (`finish_session_open`, `spawn_review_gate`,
  `spawn_rebase_gate`) look up the choice via
  `App::backend_for_work_item`. Review and rebase gates abort
  with a surfaced error when the choice is missing - "abort
  rather than default to claude", per the plan. Enter on a
  work-item row without a prior c/x press is now a no-op with
  a hint toast (breaking keybinding change from the v1 scope).
  Double-press `k` within 1.5s ends the session (SIGTERM / 50ms
  / SIGKILL via the shared `Drop for Session` path).
  Global assistant: `spawn_global_session` resolves its backend
  from `config.defaults.global_assistant_harness` rather than
  the App singleton; when the field is unset, Ctrl+G opens a
  first-run modal (`FirstRunGlobalHarnessModal`) that lists
  harnesses on PATH and persists the pick to `config.toml` on
  selection. New CLI: `workbridge config set
  global-assistant-harness <name>` sets the same field non-
  interactively via `apply_config_set` (split into a testable
  core + `ConfigSetOutcome` enum so unit tests can assert
  branch-taken without shelling out).
  New dep: `which = "6"` for lazy PATH scans via
  `agent_backend::is_available`. Known Spawn Sites table
  unchanged (the three sites still exist at the same call
  sites; the trait-object used is now per-work-item rather than
  singleton).
- 2026-04-16: Codex MCP injection fix + silent-fallback removal.
  C4: the previous Codex shape `-c mcp_servers.workbridge.config=<path>`
  is syntactically accepted by Codex but rejected at configuration
  load time with "invalid transport in `mcp_servers.workbridge`"
  (verified against the live `codex` CLI). Replaced with per-field
  overrides built from a new `McpBridgeSpec` (command + args):
  `-c mcp_servers.workbridge.command="<exe>"` and
  `-c mcp_servers.workbridge.args=[...]`. RP1c / RP2c / RP2bc
  updated. `SpawnConfig` and `ReviewGateSpawnConfig` grew a
  `mcp_bridge` field so the structured spec flows from the
  session-open worker (where `std::env::current_exe` already runs)
  to the backend without round-tripping through JSON. Claude
  ignores it and continues to consume `--mcp-config <path>`. New
  test `codex_mcp_bridge_none_omits_workbridge_overrides` pins
  the "degrade, do not fall back to `~/.codex/config.toml`"
  contract. F-2: the `finish_session_open` and `begin_session_open`
  paths previously resolved the per-work-item backend via
  `backend_for_work_item(id).unwrap_or_else(|| self.agent_backend)`
  which silently ran Claude even when the user had picked Codex
  and restarted. CLAUDE.md grew an `[ABSOLUTE]` rule forbidding
  that; the fallback was removed at both sites and at the global-
  assistant path. Missing `harness_choice` now produces a user-
  visible toast ("Cannot open session: no harness chosen...") and
  aborts the spawn, matching `spawn_review_gate` / `spawn_rebase_gate`.
  Regression test `stage_transition_without_harness_choice_surfaces_error`.
  `finish_session_open` signature collapsed from 8 positional
  arguments to a single `SessionOpenPlanResult` (clippy
  too-many-arguments threshold). Known Spawn Sites table line
  numbers refreshed: 5792 -> 5915 (work-item interactive),
  9990 -> 10337 (review gate), 10397 -> 10779 (rebase gate),
  11608 -> 12018 (global assistant).
- 2026-04-16 (round 2): Codex rebase-gate argv placement fix +
  per-repo MCP server parity. RP2bc previously emitted
  `exec --json --full-auto --ask-for-approval never ...`, which
  the `codex` CLI rejects with `error: unexpected argument
  '--ask-for-approval' found` (verified live). The flag is parsed
  as a TOP-LEVEL `codex` flag and MUST come BEFORE the `exec`
  subcommand; `--full-auto` is an `exec`-subcommand flag and stays
  inside `exec`. `build_headless_rw_command` rearranged accordingly;
  `codex_headless_rw_includes_full_auto_and_approval_never` updated
  to assert the new placement; RP2bc updated. The interactive path
  (`build_command`) and review-gate path (`build_review_gate_command`)
  do not use `--ask-for-approval` and were unaffected. Per-repo
  MCP servers from `Config::mcp_servers_for_repo` were silently
  dropped for Codex sessions in round 1 (Claude consumed them via
  `--mcp-config <file>`, but Codex's argv only emitted the workbridge
  primary). `McpBridgeSpec` grew a `name` field and
  `SpawnConfig` / `ReviewGateSpawnConfig` grew an
  `extra_bridges: &[McpBridgeSpec]` field. The work-item, review-
  gate, and rebase-gate spawn sites now resolve per-repo MCP servers
  on the UI thread, filter out HTTP-transport entries (Codex has no
  `mcp_servers.<name>.url` schema), and forward the rest. RP1c /
  RP2c / RP2bc updated with `[--config mcp_servers.<extra>.*]`
  placeholders. Regression test
  `codex_mcp_bridge_extras_emit_per_key_overrides`. Function arg
  count of `build_agent_cmd_with` collapsed via a new
  `McpInjection<'a>` bundle struct (config_path + primary_bridge +
  extra_bridges) to stay under the clippy threshold.

## Reference Payloads (Codex)

These are the per-harness equivalents of RP1 / RP2 / RP2b for
Codex. They are the argv that `CodexBackend::build_command` /
`::build_review_gate_command` / `::build_headless_rw_command`
produce for a typical Planning / review-gate / rebase-gate spawn.
Pinned by the `codex_*` tests in `src/agent_backend.rs`.

### RP1c - Codex interactive work-item argv

```
codex
  --full-auto
  --config mcp_servers.workbridge.command="<workbridge exe path>"
  --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<socket path>"]
  [--config mcp_servers.<extra>.command="..."  ]   # zero or more extras
  [--config mcp_servers.<extra>.args=[...]      ]
  --config instructions="<stage system prompt>"
  <auto-start user prompt (if any)>
```

Differences from Claude's RP1:
- `--full-auto` instead of `--dangerously-skip-permissions`.
- Per-field MCP overrides instead of `--mcp-config <path>`: Codex's
  TOML schema for `mcp_servers.<name>` requires `command` (string)
  and `args` (array of strings) directly. There is no `.config=<path>`
  sub-field that reads an external JSON - verified by running
  `codex -c 'mcp_servers.workbridge.config="/tmp/fake.json"' mcp list`,
  which fails with "invalid transport in `mcp_servers.workbridge`".
- `--config instructions="<prompt>"` instead of `--system-prompt
  <prompt>`; value is TOML-quoted so prompts containing quotes,
  newlines, or equals signs survive the TOML parser.
- No `--allowedTools` flag (allowlist enforced at MCP server).
- No `--settings` flag for the planning hook (C8 workaround:
  embed reminder in system prompt).
- Auto-start is the trailing positional; ordering relative to
  the `--config` flags does NOT matter (unlike Claude where the
  positional must precede `--mcp-config`).

Implementation note: the `mcp_config_path` field on `SpawnConfig`
is still populated by the caller (it is consumed by Claude and by
the on-disk config parity logging), but it is intentionally NOT
referenced in Codex's argv - only the structured
`SpawnConfig::mcp_bridge` field is. Callers that fail to start the
MCP server (e.g. socket bind error) pass `mcp_bridge: None`; Codex
then omits the `mcp_servers.workbridge.*` overrides entirely rather
than falling back to `~/.codex/config.toml`, which would
silently cross-contaminate the user's personal config.

### RP2c - Codex headless review-gate argv

```
exec
  --json
  --config instructions="<review gate system prompt>"
  --config mcp_servers.workbridge.command="<workbridge exe path>"
  --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<socket path>"]
  [--config mcp_servers.<extra>.command="..."  ]   # zero or more extras
  [--config mcp_servers.<extra>.args=[...]      ]
  <review skill prompt (e.g. /claude-adversarial-review)>
```

The first positional is `exec` (not `--print`) - Codex's headless
mode is a separate subcommand. `--json` switches the event stream
to newline-delimited JSON;
`CodexBackend::parse_review_gate_stdout` keeps only the last
`agent_message` event's `content` field and parses it as the
verdict envelope body. The workbridge MCP bridge is registered via
the per-field `mcp_servers.workbridge.command` / `.args` overrides
(same rationale as RP1c).

### RP2bc - Codex headless rebase-gate argv

```
--ask-for-approval never
exec
  --json
  --full-auto
  --config mcp_servers.workbridge.command="<workbridge exe path>"
  --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<socket path>"]
  [--config mcp_servers.<extra>.command="..."  ]   # zero or more extras
  [--config mcp_servers.<extra>.args=[...]      ]
  <rebase instruction prompt>
```

`--ask-for-approval` is a TOP-LEVEL `codex` flag and MUST come BEFORE
the `exec` subcommand. Verified live on 2026-04-16 by running each
shape against the installed `codex` CLI:

- `codex --ask-for-approval never exec --json --full-auto
  --skip-git-repo-check "echo hi"` -> valid event stream.
- `codex exec --json --full-auto --ask-for-approval never "echo hi"`
  -> `error: unexpected argument '--ask-for-approval' found`.

This pinning lives in `codex_headless_rw_includes_full_auto_and_approval_never`
in `src/agent_backend.rs`.

`--full-auto` (write access) IS an `exec` subcommand flag and stays
inside `exec`. The combination parallels Claude's
`--dangerously-skip-permissions`, which already implies "no approval
prompts" so Claude needs no separate flag.

Each `mcp_servers.<extra>.*` pair is rendered for one entry of
`SpawnConfig::extra_bridges` / `ReviewGateSpawnConfig::extra_bridges`
in addition to the workbridge primary. The list is populated from
`Config::mcp_servers_for_repo` at the spawn site (see
`begin_session_open` and `spawn_rebase_gate` / `spawn_review_gate`),
mirroring the `extra_servers` slice that the Claude side already
threads into `crate::mcp::build_mcp_config`. HTTP-transport entries
are filtered out because Codex's `mcp_servers.<name>` schema requires
command + args (no `url` sub-field). Missing extras render as zero
overrides; the workbridge primary is unaffected.
