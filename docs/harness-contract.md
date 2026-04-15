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
`App::spawn_session` -> `Session::spawn` in `src/session.rs:57`,
which forks a `claude` process attached to a PTY slave fd. Headless
mode is produced by the review gate at `src/app.rs:8423` and the
rebase gate at `src/app.rs:8696`, which run `claude --print
--output-format json --json-schema ...` via
`std::process::Command::output()`. The rebase gate also passes
`--dangerously-skip-permissions` because `claude --print` runs
non-interactively and any pre-flight tool the harness wants to
execute (`git rebase`, `git add`, etc.) must succeed without an
interactive consent prompt; the review gate omits the flag because
its read-only MCP server forbids the only mutations that would
otherwise need consent.

**Codex (secondary, not implemented)**: **supported**. Interactive
corresponds to plain `codex`; headless corresponds to `codex exec
--json` (non-interactive mode with a newline-delimited event
stream). The review and rebase gates would each need a final-message
extractor because Codex's JSON stream is a series of events rather
than a single structured document, but that is parsing glue, not a
clause violation.

### C2 - Working directory

**Claude (reference)**: `Session::spawn` at `src/session.rs:57`
honours the `cwd` argument via `std::process::Command::current_dir`.
`App::finish_session_open` passes the worktree path for work-item
spawns at `src/app.rs:4636`. `spawn_global_session` at
`src/app.rs:9384` passes a stable workbridge-owned scratch directory
(`$TMPDIR/workbridge-global-assistant-cwd`, created idempotently by
`std::fs::create_dir_all` just before the spawn). The scratch path
is used instead of `$HOME` because Claude Code's workspace trust
dialog persists per non-home directory but NOT for `$HOME`, so
spawning in home would re-prompt the user on every Ctrl+G; spawning
in a stable non-home directory lets the harness's own trust
persistence handle the first-prompt case without workbridge ever
reading or writing `~/.claude.json`. The review gate runs `git diff`
inside the worktree on a background thread (not in the harness
child) but the harness child for `claude --print` is spawned with
the default cwd because the gate only needs MCP access to fetch the
plan. The rebase gate at `src/app.rs:8696` is the opposite case:
the harness child needs to actually run `git rebase` against the
worktree, so its `Command::new("claude").current_dir(&worktree_path)`
sets the cwd to the work-item's worktree path explicitly. The cwd
is captured by destructuring a `RebaseTarget` produced by
`App::selected_rebase_target` so the spawn site cannot drift from
the work item the user pressed `m` on. The `worktree_path` field
on `RebaseTarget` is intentionally the per-worktree directory, not
the registered repo root: each git worktree has its own HEAD, so a
rebase launched from the repo root would silently target whatever
the main checkout has checked out (almost always `main` itself, so
the rebase no-ops; or, if a different branch is checked out in the
main checkout, it rewrites that unrelated branch). The same
`worktree_path` is also used for the in-thread `git fetch origin
<base>` and `git merge-base --is-ancestor` verification calls so
every git context the gate touches lives inside the worktree.

**Codex (secondary, not implemented)**: **supported**. Codex accepts
a `--cd <path>` flag as well as inheriting the parent's cwd; either
works. No clause violation.

### C3 - Permissions

**Claude (reference)**: `build_claude_cmd` at `src/app.rs:4672` and
`spawn_global_session` at `src/app.rs:9272` both push
`--dangerously-skip-permissions` into argv unconditionally. The
review gate at `src/app.rs:8423` does not need it because
`claude --print` is non-interactive and the read-only MCP server
forbids the only mutations a permission prompt would normally
guard. The rebase gate at `src/app.rs:8696` DOES pass
`--dangerously-skip-permissions` because its job is to run the
write-side `git rebase` / `git add` / `git rebase --continue`
sequence in the worktree; an interactive consent prompt in
`claude --print` mode is unreachable, so without the flag the
harness child would block on the first tool call and never exit.

**Codex (secondary, not implemented)**: **supported**. Codex has
`--full-auto` and `--ask-for-approval never` for the same role.
Either flag satisfies C3 as long as it is passed on every spawn; no
clause violation.

### C4 - MCP injection

**Claude (reference)**: `build_mcp_config` in `src/mcp.rs:1382`
produces the JSON blob, and `McpSocketServer::start` at
`src/mcp.rs:80` starts the accept loop. All four spawn sites
deliver the MCP config exclusively via `--mcp-config <tempfile>`
under `std::env::temp_dir()` (workbridge-owned): work-item spawns
at `src/app.rs:4624` (see `finish_session_open`), the review gate
at `src/app.rs:8434`, the rebase gate at `src/app.rs:8706`, and the
global assistant at `src/app.rs:9272`. No spawn site drops
`.mcp.json` or any other harness-state file into the worktree -
doing so would violate the "file injection" invariant
cross-referenced in C2 (CLAUDE.md severity overrides). The bridge
process is the same workbridge binary re-invoked with
`--mcp-bridge --socket <path>` (see `build_mcp_config`). The
rebase gate's MCP server is started with `read_only: false`
because the harness must call `workbridge_set_status` and
`workbridge_log_event` to persist the rebase outcome; the rebase
gate's `Sender<McpEvent>` is a private channel owned by the
spawning thread (NOT `App::mcp_tx`) so the gate's progress events
do not pollute the main TUI dispatch loop. The thread translates
incoming `McpEvent::ReviewGateProgress` and
`McpEvent::LogEvent { event_type: "rebase_progress", .. }` calls
into `RebaseGateMessage::Progress` updates, which the right-pane
takeover in `src/ui.rs` renders into the spinner panel.

**Codex (secondary, not implemented)**: **workaround**. Codex reads
MCP server definitions from `~/.codex/config.toml` under
`[mcp_servers.*]`. There is no per-invocation `--mcp-config` flag
equivalent. A Codex adapter would have to either write a temporary
`config.toml` and point Codex at it via its config-override flag, or
use `--config mcp_servers.workbridge=...` overrides. Both are shim-
level work; the clause itself (per-session MCP injection with
stdio transport) is still achievable.

### C5 - Tool allowlist by spawn type

**Claude (reference)**: `build_claude_cmd` at `src/app.rs:4672`
passes `--allowedTools` with a comma-separated list of the 15
workbridge MCP tools for work-item profiles.
`spawn_global_session` at `src/app.rs:9272` uses the same list.
The review gate does not pass `--allowedTools`; it relies entirely
on the MCP server exposing only the 4 read-only tools (see
`src/mcp.rs` `tools/list` handling and the
`read_only_mode_exposes_only_read_tools` test at `src/mcp.rs:1510`).
The rebase gate at `src/app.rs:8696` also does not pass
`--allowedTools`: with `read_only: false` on its MCP server, the
harness has access to the full work-item tool set, but in practice
the rebase prompt only asks for `workbridge_log_event` (progress)
and `workbridge_set_status` (final result) plus the harness's own
shell tool to run `git rebase`. The "no allowlist" choice keeps the
spawn site uniform with the review gate; the prompt's instructions
are the upper bound on which tools actually get called.

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

**Claude (reference)**: `stage_system_prompt` at `src/app.rs:4308`
builds the prompt by rendering a per-stage template
(`planning` / `planning_retroactive` / `planning_quickstart` /
`implementing_with_plan` / `implementing_rework` /
`implementing_no_plan` / `blocked` / `review` /
`review_with_findings`) from `src/prompts.rs`. The result is passed
via `--system-prompt <string>` in `build_claude_cmd` at
`src/app.rs:4672`. The review gate renders the `review_gate`
template and passes it with the same flag at `src/app.rs:8423`.
The rebase gate does NOT pass a separate `--system-prompt`; the
prompt is delivered as the positional `-p` payload (an inline
string built in `spawn_rebase_gate`) because the rebase task has no
template variables to expand and no per-user customisation surface
- the prompt enumerates the rebase steps verbatim, the JSON output
shape, and the "do not push" prohibition, and never changes from
spawn to spawn. The clause is satisfied because the harness still
sees a per-spawn task definition before any user input could
arrive; whether it lands in the system slot or the initial-user
slot is the harness adapter's choice.

**Codex (secondary, not implemented)**: **workaround**. Codex does
not have a dedicated `--system-prompt` flag. The harness-neutral
escape hatch is to prepend the stage prompt as an initial user
message (via stdin or the positional prompt argument). This is
observably different from a true system-prompt because the model
may treat it as lower priority, but the clause (per-stage prompt
injection at spawn time) is still met.

### C7 - Auto-start prompt

**Claude (reference)**: `build_claude_cmd` at `src/app.rs:4672`
appends a literal positional prompt ("Explain who you are and start
working." for Planning/Implementing, a review-gate-findings
presentation prompt for Review) when `auto_start` is true. It is
placed **before** `--mcp-config` because Claude Code otherwise
treats it as an additional config file path. The headless gates
have their own initial prompts: the review gate passes the review
skill as `-p <prompt>`, and the rebase gate passes its inlined
rebase-task prompt as `-p <prompt>` at `src/app.rs:8696`. The
clause is met for headless spawns the same way it is met for
interactive spawns: the harness child has work to do before any
human input could possibly arrive.

**Codex (secondary, not implemented)**: **supported**. Codex accepts
an initial prompt as a positional argument in interactive mode and
as the `-p` / stdin payload in `codex exec`. No clause violation.

### C8 - Stage reminders

**Claude (reference)**: Planning sessions get a second-layer
reminder via `--settings`, passed at `src/app.rs:4672` with a JSON
blob that installs a `PostToolUse` hook on `TodoWrite`. The hook
greps the tool payload for `workbridge_set_plan`; if missing, it
writes a reminder to stderr so Claude sees it on the next turn.
Non-Planning stages use only the system-prompt-embedded reminder
from the templates in `src/prompts.rs`. The headless gates do not
need stage reminders: the review gate's only obligation is "emit
JSON envelope", which the `--json-schema` enforces, and the rebase
gate's only obligations are "do not push" and "emit JSON envelope",
both stated verbatim in the inline rebase prompt. There is no
multi-turn invariant a hook would need to re-fire for.

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
(`src/session.rs:164`) loops on `libc::read` against a dup'd master
fd and calls `vt100::Parser::process` on every chunk. The UI thread
locks the parser and renders its screen (`App::render_*` paths).
Headless capture lives at `src/app.rs:8423` (review gate) and
`src/app.rs:8696` (rebase gate) - both consume stdout from the
harness child and parse the top-level JSON envelope, reaching into
`envelope["structured_output"]` for the fields. The review gate
uses the convenience `Command::output()` because it has no kill
path; the rebase gate uses `Command::spawn()` + `wait_with_output()`
inside a dedicated nested thread instead, so the harness child's
PID can be stashed in `RebaseGateState::child_pid` immediately
after spawning. The dedicated nested thread lets the spawning
thread `crossbeam_channel::select!` between the output-completion
channel and the gate's private MCP-event channel to forward live
progress without blocking on the child.

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
exit gives the reader its EOF. The global-assistant teardown adds
one extra layer on top of `Session::kill`:
`App::teardown_global_session` at `src/app.rs:9256` kills the
child, drops the `SessionEntry` (which joins the reader via
`Drop`), drops the MCP server, removes the temp MCP config file,
and drains any buffered keystrokes - symmetric with the work-item
cleanup path so new global-assistant state cannot leak across
opens. The headless gates (review and rebase) bypass `Session`
entirely. The review gate runs `Command::output()` inside a
background thread and has no cancel path - its lifecycle is
governed by `Output` returning, and `poll_review_gate` drops the
gate state once that arrives. The rebase gate is the opposite case:
it runs `Command::spawn()` + `wait_with_output()` with
`Command::process_group(0)` so the harness child becomes the
leader of its own process group. The PID is stashed in
`RebaseGateState::child_pid` (an `Arc<Mutex<Option<u32>>>` shared
with the spawning sub-thread). `App::drop_rebase_gate` reads that
slot and `libc::killpg(pid, SIGKILL)`s the **entire process
group**, not just the harness PID, so claude AND any `git rebase`
/ `git add` / `git rebase --continue` subprocesses it has started
all die at once. Without `process_group(0)` the harness would
inherit workbridge's process group, so a `kill(pid, SIGKILL)` on
the claude PID alone would leave its `git` subprocesses orphaned
and still mutating the worktree that `spawn_delete_cleanup` is
about to remove. Mirrors the pattern in `Session::force_kill`,
which uses `libc::killpg` for the same reason; `Session::spawn`
gets the new group via `libc::setsid` in `pre_exec` while the
rebase gate uses the simpler `Command::process_group(0)` because
it does not need a controlling terminal. Work-item delete
(`delete_work_item_by_id`) and force-quit (`force_kill_all`)
both call `drop_rebase_gate` to stop in-flight rebases before
their write-side calls race the worktree removal. The PID is
cleared by the sub-thread after `wait_with_output` returns, so a
concurrent `drop_rebase_gate` can never `killpg` a stale-PID
slot.

The pre-spawn window (default-branch resolution, `git fetch`, MCP
server start, temp-config write) cannot rely on `child_pid`
because the harness has not been spawned yet. To close that race,
`RebaseGateState::cancelled` is an `Arc<AtomicBool>` set by
`drop_rebase_gate` BEFORE the SIGKILL. The background thread
polls the flag at every phase boundary and exits cleanly (dropping
its MCP server and removing its temp config) on a `true` reading.
The harness sub-thread stashes the PID into `child_pid` FIRST
and then re-checks the flag, in that order: stashing first means
that for every interleaving with `drop_rebase_gate` either the
drop path sees the PID and `killpg`s it, or the sub-thread sees
the (sticky) cancellation flag and `killpg`s the group itself.
The flag's stickiness (once set, never cleared) is the
load-bearing property. The flag covers the entire pre-spawn
lifecycle; the PID covers everything after spawn. To make the
cancellation race unhittable from the start, the gate state is
inserted into `App.rebase_gates` BEFORE the background thread is
spawned, so any `drop_rebase_gate` call sees the entry even if
the thread has not been scheduled yet.

There is one final cancellation check between the harness exit
and the background thread's `backend.append_activity` call. If
the flag is set there, the thread exits without writing the
activity log entry and without sending the result through `tx`.
Without this check, a delete that races the harness exit could
let the background thread call `append_activity` after
`backend.delete` has already moved the active log to `archive/`;
because `append_activity` opens with `OpenOptions::create(true)`,
it would recreate an orphan active activity log for a deleted
work item.

The single-flight admission (`UserActionKey::RebaseOnMain`) is
now released only when `drop_rebase_gate` sees the slot owned by
the same work item it is dropping; otherwise dropping a stale
gate for one item could clear the global slot while a different
item still owns it, admitting an overlapping rebase.

**Codex (secondary, not implemented)**: **supported**. The
lifecycle contract is a POSIX process-group protocol, not a
harness-specific one. As long as Codex does not install a SIGTERM
handler that swallows the signal (it does not, as of the public CLI
behaviour), the existing `Session` struct handles it unchanged.

### C11 - Read-only sessions

**Claude (reference)**: The review gate passes `read_only: true` to
`McpSocketServer::start` at `src/app.rs:8375`. The server at
`src/mcp.rs:80` stores the flag into `SessionMcpConfig` and threads
it through `handle_message`, which filters `tools/list` (see
`src/mcp.rs` around line 439) and rejects mutating `tools/call`
(line 608). The unit tests
`read_only_mode_exposes_only_read_tools` and
`read_only_mode_rejects_mutating_tool_calls` in `src/mcp.rs:1510`
pin the contract. The rebase gate's `McpSocketServer::start` at
`src/app.rs:8599` is intentionally NOT read-only:
`read_only: false` is passed because the harness must call
`workbridge_log_event` to stream live `rebase_progress` events to
the spinning right-pane indicator. The rebase gate does NOT use
the harness to persist its outcome - the prompt explicitly tells
the harness not to call `workbridge_set_status`, and
`poll_rebase_gate` writes a `rebase_completed` / `rebase_failed`
activity log entry directly via `App.backend.append_activity` once
the harness exits. Status / plan / title MCP events that arrive on
the gate's private channel are dropped (see `Ok(_)` arm in
`spawn_rebase_gate`) so a misbehaving harness cannot rename the
work item or overwrite its plan as a side effect of running a
rebase. The rebase gate is the only headless spawn site that runs
read-write; the read-only path remains the default for any future
"this is an opinion, not a driver" gate.

**Codex (secondary, not implemented)**: **supported**. Read-only
enforcement is entirely inside the workbridge MCP server, which is
harness-agnostic. A Codex adapter just sets the same flag.

### C12 - Session identity

**Claude (reference)**: Sessions are stored in `App::sessions` keyed
by `(WorkItemId, WorkItemStatus)` and inserted at
`src/app.rs:4646`. Stage transitions orphan old entries, which are
killed by the periodic liveness sweep. The poll handler in
`poll_review_gate` at `src/app.rs:8846` explicitly kills the
current session and respawns when a gate rejects or errors. The
global assistant drawer uses a simpler identity rule: exactly one
live session at a time, torn down on every drawer close and
re-spawned fresh on every drawer open via
`App::toggle_global_drawer` calling `teardown_global_session`
(`src/app.rs:9256`) and `spawn_global_session` (`src/app.rs:9272`);
see also `docs/UI.md` "Global assistant drawer session lifetime".
The rebase gate's "session" is implicit: it is keyed by
`WorkItemId` in `App.rebase_gates` and lives only as long as the
background thread's `wait_with_output` call (or until
`drop_rebase_gate` SIGKILLs the child via the stashed PID, see
C10). Each press of `m` spawns a fresh harness child; there is no
resume. Single-flight admission via `UserActionKey::RebaseOnMain`
(and the per-`WorkItemId` map check in `start_rebase_on_main`)
prevents overlapping rebases on the same item.

**Codex (secondary, not implemented)**: **supported**. Identity is
owned by workbridge; the harness only needs to exit when signalled.
A Codex adapter that uses Codex's own session-resume feature MUST
defeat it at spawn time so workbridge's fresh-session invariant is
not bypassed.

### C13 - No env leakage

**Claude (reference)**: Neither `build_claude_cmd`,
`spawn_global_session`, the review gate spawn, nor the rebase gate
spawn sets any harness-specific environment variable on the child.
The child inherits the parent environment (so the user's `$PATH`,
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

Source: `App::build_claude_cmd` at `src/app.rs:4137`, followed by
the `--mcp-config` append inside `finish_session_open` at
`src/app.rs:4089`. Cwd: the work item's worktree path. The
positional prompt MUST precede `--mcp-config`; see the regression
test `build_claude_cmd_prompt_before_mcp_config` at
`src/app.rs:13104`.

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

Source: `std::process::Command::new("claude")` at
`src/app.rs:8423`. Cwd: inherited (unspecified). The review gate
does NOT pass `--dangerously-skip-permissions` because
`--print` is non-interactive and never prompts. The review gate
does NOT pass `--allowedTools`; it relies on the read-only MCP
server to hide mutating tools.

### RP2b - Headless rebase-gate argv

```text
claude
  --print
  --dangerously-skip-permissions
  -p '<inline rebase prompt: rebase steps, do-not-push, JSON shape>'
  --output-format json
  --json-schema '{"type":"object","properties":{"success":{"type":"boolean"},"conflicts_resolved":{"type":"boolean"},"detail":{"type":"string"}},"required":["success","detail"]}'
  --mcp-config /tmp/workbridge-rebase-mcp-<uuid>.json
```

Source: `std::process::Command::new("claude")` at
`src/app.rs:8696`. Cwd: the work-item's worktree path (set
explicitly via `Command::current_dir`). Unlike RP2, the rebase gate
spawns via `Command::spawn()` + `Child::wait_with_output()` rather
than `Command::output()` so the harness child's PID can be stashed
in `RebaseGateState::child_pid`; this is what lets
`drop_rebase_gate` SIGKILL the harness on delete / force-quit (see
C10). The spawn also passes `Command::process_group(0)` so the
harness becomes the leader of its own process group; on
cancellation `drop_rebase_gate` calls `libc::killpg` against that
group so any `git rebase` / `git add` subprocesses claude has
spawned die at the same time as claude itself. Without the new
group, a `kill(pid, SIGKILL)` on the claude PID alone would leave
those git subprocesses orphaned and still mutating the worktree.
The rebase gate DOES pass `--dangerously-skip-permissions`
because `claude --print` cannot display an interactive consent
prompt and the rebase task requires write-side `git rebase` /
`git add` calls. The rebase gate does NOT pass `--system-prompt`;
the rebase task definition lives in the `-p` positional payload
because it has no template variables to expand. The MCP server
backing the rebase gate is started with `read_only: false` so the
harness can call `workbridge_log_event` to stream `rebase_progress`
events to the spinning right-pane indicator. The prompt explicitly
tells the harness NOT to call `workbridge_set_status` - the
audit-trail record is written by `poll_rebase_gate` directly via
`App.backend.append_activity` (see RP6) so the harness does not
have to make persistence decisions.

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

Source: `build_mcp_config` at `src/mcp.rs:1382`. For work-item
sessions, `extra_servers` (user-configured per-repo entries) are
inserted first; the workbridge server is appended last so it wins
on name collision. The socket path is produced by
`socket_path_for_session` at `src/mcp.rs:1428`.

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

Source: inline JSON literal in `build_claude_cmd` at
`src/app.rs:4175`. Passed as the argument to `--settings` on
Planning spawns only. The harness fires the command after every
`TodoWrite` tool call; the command greps stdin (the tool payload)
for `workbridge_set_plan` and, if missing, emits a stderr
reminder the model sees on its next turn.

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

Source: `src/app.rs:8442` ("The structured output is in the
`structured_output` field."). The harness MUST produce an envelope
whose structured body conforms to the `--json-schema` payload in
RP2; workbridge uses `.as_bool()` and `.as_str()` with safe
defaults, so absence of either field is interpreted as "not
approved, empty detail".

### RP6 - Rebase gate JSON envelope

The rebase gate parses the same top-level envelope, reaching into
the same `structured_output` field, but expects a different shape:

```json
{
  "structured_output": {
    "success": true,
    "conflicts_resolved": false,
    "detail": "rebased onto origin/main, fast-forward only"
  }
}
```

Source: `src/app.rs:8766`. `success` MUST be `true` if and only if
the worktree is now rebased onto `origin/<base>`; the harness is
expected to run `git rebase --abort` and report `success=false` on
any give-up path. `conflicts_resolved` is informational and used
only for the human-readable status summary. As with RP5,
`.as_bool()` / `.as_str()` defaults treat a missing field as
"failed, empty detail".

The rebase gate does NOT trust `success: true` blindly. Before
emitting `RebaseResult::Success`, the spawning thread runs `git -C
<worktree_path> merge-base --is-ancestor origin/<base> HEAD`
against the same worktree the harness ran in. If that command
exits non-zero (origin is not an ancestor of HEAD) the gate
downgrades the result to `RebaseResult::Failure` with a reason
naming the ancestry mismatch, so a hallucinated envelope, a
harness that ran the wrong command, or a stale stdout cannot
produce a false "Rebased onto origin/<base>" status in the UI.
This is the user-facing-claim verification mandated by CLAUDE.md.

For the verification to be sound, `refs/remotes/origin/<base>`
MUST point at the just-fetched tip. The phase 2 fetch therefore
uses an explicit refspec
`+<base>:refs/remotes/origin/<base>` instead of the shorthand
`git fetch origin <base>`. The shorthand only updates the
remote-tracking ref via git's "opportunistic remote-tracking
branch update", which depends on the remote's configured fetch
refspec covering `<base>`; in repos cloned with `--single-branch`
of a different branch, or with a customised
`[remote "origin"] fetch` refspec that omits `<base>`, the
shorthand would only update FETCH_HEAD and the verification
would silently compare against a stale ref. The leading `+`
allows non-fast-forward updates so a force-pushed base branch is
also handled.

The rebase gate's audit trail (the "later session viewing this
work item can see the rebase happened" record) is written on the
background thread via `App.backend.append_activity`, NOT on the
UI thread - the local backend implementation opens and writes the
activity log file, so doing it from `poll_rebase_gate` would
violate the absolute blocking-I/O-on-the-UI-thread invariant. The
spawning thread owns an `Arc<dyn WorkItemBackend>` clone (cloned
from `App.backend` at `spawn_rebase_gate` setup time); after the
ancestry verification above runs, it builds the activity entry,
calls `append_activity`, and stashes any error string in
`RebaseResult::*::activity_log_error`. The poll loop reads that
field and suffixes the error onto the user-visible status message
so the user can see when the audit trail did not land.

The entry's `event_type` is `rebase_completed` for success or
`rebase_failed` for failure, and the payload carries
`base_branch`, `conflicts_resolved` / `conflicts_attempted`, and
the harness's `reason` (failure case only). The gate prompt
explicitly tells the harness NOT to call `workbridge_set_status`
to leave a record - the work item is already `Implementing`, so a
status update would be a no-op transition that the App's
StatusUpdate validator rejects, and the activity log is the
correct place for the audit trail anyway.

## Target Trait Sketch

The following Rust sketch describes the provider-agnostic interface a
future harness abstraction would expose. It is illustrative, not
prescriptive: no file in workbridge implements it today. The key
property is that **no vendor name appears anywhere in this block** -
if you need to add a vendor-specific concept here, the contract is
wrong and C1-C13 should be tightened first.

```rust
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A pluggable LLM coding harness. One implementation per CLI.
pub trait Harness: Send + Sync {
    /// Short stable id used in config and logs (e.g. "ref", "alt").
    fn id(&self) -> &'static str;

    /// Spawn an interactive PTY-backed session. The returned handle
    /// owns the child process and the PTY master fd. See C1, C2, C9.
    fn spawn_interactive(
        &self,
        cfg: InteractiveSpawnConfig,
    ) -> Result<Box<dyn HarnessSession>, HarnessError>;

    /// Run a one-shot headless session and return a single structured
    /// result. Used for the review gate today. See C1, C9.
    fn run_headless(
        &self,
        cfg: HeadlessSpawnConfig,
    ) -> Result<HeadlessResult, HarnessError>;
}

/// Scope of a session. Controls cwd (C2), tool allowlist (C5), and
/// mutation policy (C11).
pub enum HarnessScope {
    WorkItem,
    ReviewGate,
    Global,
}

/// Whether the session is allowed to mutate state via MCP tools. The
/// MCP-server side enforcement (C11) is mandatory; the CLI allowlist
/// is defence in depth.
pub enum MutationPolicy {
    ReadWrite,
    ReadOnly,
}

/// Delivery mechanism for stage reminders (C8). Implementations choose
/// how to make the reminder fire; the caller only picks whether one
/// exists.
pub enum StageReminder {
    /// No extra reminder beyond the system prompt.
    None,
    /// A tool-observing reminder that nudges the model if a specific
    /// MCP tool has not yet been called in the session.
    RequireToolCall { tool_name: String, message: String },
}

pub struct InteractiveSpawnConfig {
    pub scope: HarnessScope,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub mcp_socket_path: PathBuf,
    pub mutation_policy: MutationPolicy,
    pub allowed_mcp_tools: Vec<String>,
    pub system_prompt: Option<String>,
    pub auto_start_message: Option<String>,
    pub stage_reminder: StageReminder,
}

pub struct HeadlessSpawnConfig {
    pub scope: HarnessScope,
    pub cwd: PathBuf,
    pub mcp_socket_path: PathBuf,
    pub mutation_policy: MutationPolicy,
    pub allowed_mcp_tools: Vec<String>,
    pub system_prompt: String,
    pub initial_prompt: String,
    pub output_schema: JsonSchemaDescriptor,
    pub timeout: Option<Duration>,
}

pub struct JsonSchemaDescriptor {
    pub schema_json: String,
}

/// Output channel for an interactive session. The two variants are
/// equal weight: an interactive adapter MAY emit structured events
/// if it has them, but every adapter MUST be able to fall back to
/// the byte stream path (C9) because workbridge drives the PTY-to-
/// vt100 pipeline from a raw fd today.
pub enum HarnessOutput {
    Bytes,
    Events,
}

pub enum HarnessEvent {
    Stdout(Vec<u8>),
    ToolCall { name: String, args: String },
    ToolResult { name: String, body: String },
}

pub trait HarnessSession: Send {
    /// Write keystrokes (or events) into the session. For a PTY-backed
    /// adapter this writes to the master fd; for an event-backed
    /// adapter it queues an input event.
    fn write_input(&mut self, data: &[u8]) -> Result<(), HarnessError>;

    /// Resize the terminal viewport. See C9.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), HarnessError>;

    /// Liveness poll. Never blocks. See C10.
    fn is_alive(&mut self) -> bool;

    /// Graceful shutdown: SIGTERM on process group, grace, then
    /// SIGKILL. See C10.
    fn request_shutdown(&mut self, grace: Duration);

    /// Force kill, used from the crash/panic path. See C10.
    fn force_kill(&mut self);
}

pub struct HeadlessResult {
    pub exit_success: bool,
    pub structured: serde_json::Value,
    pub stderr: String,
}

pub enum HarnessError {
    Io(std::io::Error),
    NotInstalled,
    UnsupportedClause(&'static str),
    ProtocolMismatch(String),
}

// Provider-agnosticism checklist (enforced by grep, see "Verification"):
//
// 1. No type or field in this block may contain a vendor name.
// 2. No CLI flag literal may appear in this block - flags live inside
//    individual Harness implementations.
// 3. Structured-output handling uses serde_json::Value so an adapter
//    that emits a non-nested envelope can flatten inside its own
//    run_headless without changing this trait.
// 4. HarnessOutput has Bytes and Events variants at equal weight;
//    neither is the "default". The reference bytestream adapter is a
//    Harness impl, not a special case of this trait.
```

## Known Spawn Sites

These are the only places in `src/` that launch an LLM harness child
process today. Any new spawn site MUST update this table **and**
update the Implementation Map section above.

| File          | Line  | Mode        | Scope      | Cwd                                       |
|---------------|-------|-------------|------------|-------------------------------------------|
| `src/app.rs`  | 4636  | Interactive | WorkItem   | Work-item worktree                        |
| `src/app.rs`  | 8423  | Headless    | ReviewGate | inherited                                 |
| `src/app.rs`  | 8696  | Headless    | RebaseGate | Work-item worktree                        |
| `src/app.rs`  | 9384  | Interactive | Global     | `$TMPDIR/workbridge-global-assistant-cwd` |

All four sites go through `src/session.rs:57` (`Session::spawn`) for
the interactive path; the headless review gate uses
`std::process::Command::output()` directly, and the headless rebase
gate uses `Command::spawn()` + `Child::wait_with_output()` so the
harness child's PID can be stashed in `RebaseGateState::child_pid`
for the cleanup-path SIGKILL described in C10. Argv is built in
`App::build_claude_cmd` at `src/app.rs:4672` for the work-item
path, inlined at `src/app.rs:8423` for the review gate, inlined
at `src/app.rs:8696` for the rebase gate, and inlined at
`src/app.rs:9272` (`spawn_global_session`) for the global assistant.
The rebase gate is the second headless spawn site: it runs `claude
--print --dangerously-skip-permissions --output-format json
--json-schema ... --mcp-config <tempfile>` with cwd set to the
work-item's worktree path so the harness can run `git rebase
origin/<main>` in the right repo and resolve any conflicts in
place. Global assistant teardown lives at `src/app.rs:9256`
(`App::teardown_global_session`); see C10 and C12 for why each
drawer open spawns a fresh session and each close fully tears it
down.

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
- 2026-04-15: Dropped the in-worktree `.mcp.json` write from the
  work-item interactive spawn path (`finish_session_open`). All
  three spawn sites now deliver the MCP config exclusively through
  `--mcp-config <tempfile>` under `std::env::temp_dir()`; no
  workbridge state file is ever written into the user's worktree.
  This brings the C4 description in line with the "file injection"
  invariant cross-referenced in C2 (CLAUDE.md severity overrides,
  review rule added in commit `acafae8`). Observable motivation:
  new work items rooted in repos that did NOT gitignore
  `.mcp.json` (e.g. `Wordlike`, `GymApp`, `webometer`) were being
  dirtied on session spawn. Updated C4 and RP1 source references
  to the new work-item spawn layout (`--mcp-config` append now
  lives inside `finish_session_open` at `src/app.rs:4089`,
  `build_claude_cmd` is at `src/app.rs:4137`). Also refreshed
  every remaining `src/app.rs`, `src/session.rs`, and `src/mcp.rs`
  line citation in the Implementation Map, Reference Payloads, and
  Known Spawn Sites table to match the current tree in one sweep:
  work-item `Session::spawn` 3931 -> 4101, review-gate
  `Command::new` 7784 -> 7954, global `Session::spawn` 8313 ->
  8483, `build_claude_cmd` 3967 -> 4137, `stage_system_prompt`
  4138 -> 4308, `poll_review_gate` 8003 -> 8041,
  `teardown_global_session` 8185 -> 8355, `spawn_global_session`
  8201 -> 8371, plus every argv-push / comment / test anchor in
  C3/C5/C6/C7/C8/C9/C11 and RP2-RP5. The table and Implementation
  Map are now byte-accurate against the current tree so the
  "table and Implementation Map must stay in sync with the code"
  rule holds in full.
- 2026-04-15: Added the rebase-gate headless spawn site at
  `src/app.rs:8696` for the new `m` keybinding (auto-rebase on
  main). The rebase gate is the second headless `claude` spawn
  site and the first one that runs read-write: it passes
  `--dangerously-skip-permissions` (because the harness must run
  `git rebase` / `git add` / `git rebase --continue`), sets
  `read_only: false` on its `McpSocketServer::start`, and uses a
  private `Sender<McpEvent>` for live progress streaming so its
  events do not pollute the main TUI MCP dispatch loop. Updated
  the Known Spawn Sites table from three to four entries; updated
  C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13 to call
  out the rebase-gate-specific deltas; added RP2b (rebase-gate
  argv) and RP6 (rebase-gate JSON envelope). Also bumped review
  gate / global session line citations to match the current tree
  after the rebase-gate insertion (review-gate `Command::new`
  7954 -> 8423, global `Session::spawn` 8483 retained, work-item
  `--mcp-config` append 4089 -> 4583, `build_claude_cmd` 4137 ->
  4672, `stage_system_prompt` 4308 retained, `poll_review_gate`
  8041 -> 8792, `teardown_global_session` 8355 -> 9256,
  `spawn_global_session` 8371 -> 9272). The table and
  Implementation Map remain in sync with the code.
- 2026-04-16: Hardened the rebase gate's lifecycle and persistence
  in response to a Codex review pass. The harness child is now
  spawned via `Command::spawn()` + `Child::wait_with_output()`
  instead of `Command::output()`; its PID is stashed in a new
  `RebaseGateState::child_pid: Arc<Mutex<Option<u32>>>` slot so
  `App::drop_rebase_gate` can `libc::kill(pid, SIGKILL)` the
  harness on cleanup paths. `delete_work_item_by_id` and
  `force_kill_all` both now call `drop_rebase_gate` (mirroring the
  existing `drop_review_gate` calls) so deleting a work item or
  force-quitting workbridge while a rebase is in flight cannot
  leave the harness racing the worktree removal that follows on
  the cleanup thread. Updated C9 (spawn pattern), C10 (kill path),
  C11 (read_only justification), C12 (lifecycle), RP2b (spawn
  pattern + read_only justification), Known Spawn Sites prose
  (spawn pattern), and RP6 (audit trail). Also dropped the
  prompt's `workbridge_set_status` instruction - setting status
  to `Implementing` while the work item was already `Implementing`
  was a no-op transition that the App's StatusUpdate validator
  would have rejected, and the gate's private MCP channel was
  already discarding the resulting `McpEvent::StatusUpdate` (the
  `Ok(_)` arm). The audit trail is now written by
  `poll_rebase_gate` directly via `App.backend.append_activity` as
  a `rebase_completed` / `rebase_failed` activity log entry. RP6
  documents the new entry shape.
- 2026-04-16: Fourth Codex pass on the rebase gate found two
  more cancellation races. (1) P1: The harness sub-thread was
  checking `cancelled` BEFORE stashing the PID into `child_pid`,
  which left a window where `drop_rebase_gate` could fire
  between the check and the stash, find `None` in the slot, and
  silently fail to `killpg` the group. The sub-thread then
  stashed the PID and waited normally, leaving claude (and its
  `git rebase` subprocesses, since `process_group(0)` is in
  effect) running against a worktree the cleanup thread was
  about to remove. The fix flips the order: the PID is stashed
  FIRST, then `cancelled` is re-checked. The flag's stickiness
  (once set, never cleared) means every interleaving converges
  on either the drop path or the sub-thread killpg-ing the
  group. (2) P2: The background thread's
  `backend.append_activity` call had no cancellation guard, so a
  delete racing the harness exit could let the thread append to
  the work item's activity log AFTER `backend.delete` had
  archived the active log, recreating an orphan active log via
  `OpenOptions::create(true)`. The thread now re-checks
  `cancelled` immediately before the append and exits without
  writing or sending the result if the flag is set. C10 updated
  with both new ordering rules.
- 2026-04-16: Third Codex pass on the rebase gate found two more
  issues. (1) P1: The cancellation path was using `libc::kill`
  against the claude PID alone, but claude was inheriting
  workbridge's process group, so subprocesses claude spawned for
  its shell tool (`git rebase`, `git add`, `git rebase --continue`)
  would survive as orphans and keep mutating the worktree after
  cancellation. The harness is now spawned with
  `Command::process_group(0)` so it becomes the leader of its own
  group, and `drop_rebase_gate` (plus the harness sub-thread's
  post-spawn cancellation arm) calls `libc::killpg` against that
  group, taking down claude and every git subprocess at once.
  Mirrors the `Session::force_kill` pattern. (2) P1: The phase 2
  fetch was using the shorthand `git fetch origin <base>`, which
  only updates `refs/remotes/origin/<base>` via git's
  "opportunistic remote-tracking branch update" - that depends on
  the remote's configured fetch refspec covering `<base>`, so in
  repos with `--single-branch` clones or customised refspecs it
  would silently leave `origin/<base>` stale and make the
  `merge-base --is-ancestor` verification check an old commit. The
  fetch now uses an explicit `+<base>:refs/remotes/origin/<base>`
  refspec so the remote-tracking ref is guaranteed to point at the
  just-fetched tip. C10 (process-group cancellation), RP2b
  (`process_group(0)`), and RP6 (explicit refspec rationale) all
  updated.
- 2026-04-16: Second Codex pass on the rebase gate uncovered three
  additional issues, all addressed in the same commit. (1) P0:
  The activity log append from the previous round was running on
  the UI thread, violating the absolute blocking-I/O invariant.
  The append now runs on the background thread; `RebaseResult`
  variants gain an `activity_log_error: Option<String>` field
  that travels back through the result channel and is suffixed
  onto the user-visible status message in `poll_rebase_gate`, so
  failures are surfaced rather than swallowed. (2) P1:
  `drop_rebase_gate` was unconditionally clearing the
  `UserActionKey::RebaseOnMain` slot, so dropping a stale gate
  for one work item could clear the global single-flight slot
  while a different item still owned it, admitting an overlapping
  rebase. The helper now only ends the user action when the slot
  is currently owned by the work item being dropped. (3) P1: A
  cancellation race in the pre-spawn window (default-branch
  resolution, `git fetch`, MCP server start, temp-config write,
  the harness sub-thread's post-spawn pre-PID-stash window) could
  let the harness keep running against a worktree that
  `spawn_delete_cleanup` was about to remove. The gate now carries
  a `RebaseGateState::cancelled: Arc<AtomicBool>` flag set by
  `drop_rebase_gate` BEFORE the SIGKILL; the background thread
  polls the flag at every phase boundary and the harness sub-thread
  checks it again immediately after `Command::spawn` returns. To
  make the race unhittable from the start, the gate state is now
  inserted into `App.rebase_gates` BEFORE the background thread is
  spawned. C10 documents the full cancellation contract; RP6
  documents the off-UI-thread persistence path.
