# Harness Contract

## Purpose

This document is the single authoritative spec for what **any** LLM
coding harness must do to be plugged into workbridge. It is written in
harness-neutral language (the clauses say "harness", not a vendor
name) so that any adapter satisfying the clauses can be plugged in. If
a clause cannot be written without naming a vendor, the clause is
wrong.

The currently-wired adapters and their per-clause status live in the
Adapter Compatibility Matrix below. The contract is written against
the reference adapter; the matrix tracks how each other adapter
satisfies each clause (`supported` / `workaround` / `not implemented`).

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

- **Harness**: the external LLM coding CLI that workbridge spawns.
  See the Adapter Compatibility Matrix below for the current
  adapters and their per-clause status.
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
- **Display name**: the human-readable vendor label returned by
  `AgentBackendKind::display_name()` (e.g. `"Claude Code"`,
  `"Codex"`). The display name is used ONLY in UI text that
  identifies the harness actually running in a live session (C14,
  below). It is NEVER used as a default when no harness is
  committed - the UI renders the neutral placeholder
  `App::SESSION_TITLE_NONE` (`"Session"`) in that case.

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

**Prompt parity across harnesses** (RCA 2026-04-18): the system
prompts MUST be written in imperative, harness-neutral language that
explicitly neutralises the model's baked-in operating instructions.
Specifically, every interactive prompt in `prompts/stage_prompts.json`
opens with a `HARNESS DIRECTIVE OVERRIDE:` block that forbids the
harness from falling back to its default "assume the user wants the
work done unless they explicitly ask for a plan" prior (Codex's
default behaviour at time of writing). Without this block, Codex
jumps straight to implementation in planning sessions because its
prior outweighs a descriptive `"You are a planning assistant..."`
opening; Claude is forgiving of descriptive phrasing because its
training weights system prompts more heavily, which masks the bug
during single-harness testing. Pinned by
`prompts::tests::all_interactive_prompts_have_harness_directive_override`.
Every prompt uses `MUST / MUST NOT` wording rather than
`"You are ..."` role framing for the same reason: an imperative
directive is legible to both models as an instruction, where a role
description is only legible to a model whose training rewards
following role descriptions.

The review-gate prompt is exempt from the override block because it
is headless + JSON-only and has no baked-in "just do the work"
prior to override.

### C7 - Auto-start prompt

For interactive spawns in `Planning`, `Implementing`, and
`Review` (when there are pending review-gate findings), workbridge
MUST pass a literal initial user message. This lets the session do
useful work before the user types anything, and it is the only
mechanism that guarantees the harness actually calls its own tools
(which in turn exercises the MCP path).

The auto-start message MUST NOT read as a concrete implementation
request; it MUST defer to the system prompt as the source of truth
for what the session should do. The current message
(`auto_start_default`) is:

> "Follow the instructions in your system prompt. Begin with the
> first action your system prompt specifies (interview the user for
> planning stages, execute the plan for implementation stages,
> present review findings for review stages, etc.). Do not interpret
> this auto-start message as a concrete implementation request."

The earlier message `"Explain who you are and start working."` was
reworded on 2026-04-18 because Codex's prior interpreted "start
working" as a concrete implementation instruction, overriding the
system prompt's planning-stage directives. The replacement names
the stage-appropriate first action explicitly so both Codex and
Claude route through the system prompt. Pinned by
`prompts::tests::auto_start_messages_defer_to_system_prompt`.

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
embedded without rewriting the `session` module.

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

### C14 - Display name is downstream of live session state

Adapters advertise a display name via
`AgentBackendKind::display_name()`. Workbridge uses that display
name ONLY to render UI text that identifies the harness actually
running (or committed to run) in a live session: the right-panel
Session tab title, the dead-session placeholder, and the Ctrl+\\
toggle hint. The display name MUST NOT leak into UI strings
outside that live-session context.

Specifically, when no harness is committed to the current context
(no `harness_choice` entry for the selected work item, no
configured global-assistant harness), the UI renders the neutral
`App::SESSION_TITLE_NONE` placeholder (`"Session"`) - NOT the
display name of the workbridge-wide default. Falling back to any
vendor display name in the absence of a live harness is a
user-facing claim about state that the code can locally verify
and is therefore a P0 violation of the Review Policy (see
CLAUDE.md `[ABSOLUTE]` "Session titles downstream of live harness
state"). Adding a new adapter does not change this rule: the
single source of truth for display-name rendering is
`App::agent_backend_display_name`, which does
not consult the static `self.services.agent_backend`.

Adapters themselves MUST NOT:
- Inject their vendor name into prompt text, status messages, or
  tool descriptions advertised to the session (prompts stay
  harness-neutral so a session that reads its own prompt does not
  see a competing brand name).
- Leak their vendor name into error text surfaced to the UI when
  the session failed to spawn (the UI formats the failure via
  `command_name()` and the binary path, both of which are neutral
  CLI facts, not marketing strings).

Claude ref impl: `ClaudeCodeBackend::kind().display_name()` returns
`"Claude Code"`. Codex: `CodexBackend::kind().display_name()`
returns `"Codex"`. Neither is ever rendered except via
`App::agent_backend_display_name` and the first-run harness
picker modal (which lists all user-selectable harnesses by
`display_name()` because listing their brand names there is the
whole point of the modal).

## Adapter Compatibility Matrix

How each currently-wired adapter satisfies each contract clause.
Statuses: `supported` (satisfies the clause with documented CLI
features), `workaround` (satisfies the clause but with a non-obvious
mechanism - see the per-clause note below the table), or
`not implemented` (no adapter wiring exists yet).

| Clause                                  | `claude` (reference) | `codex`     | `opencode`      |
|-----------------------------------------|----------------------|-------------|-----------------|
| C1 - Spawn modes                        | supported            | supported   | not implemented |
| C2 - Working directory                  | supported            | supported   | not implemented |
| C3 - Permissions                        | supported            | supported   | not implemented |
| C4 - MCP injection                      | supported            | workaround  | not implemented |
| C5 - Tool allowlist by spawn type       | supported            | workaround  | not implemented |
| C6 - System prompt injection per stage  | supported            | workaround  | not implemented |
| C7 - Auto-start prompt                  | supported            | supported   | not implemented |
| C8 - Stage reminders                    | supported            | workaround  | not implemented |
| C9 - Output capture                     | supported            | supported   | not implemented |
| C10 - Lifecycle and cancellation        | supported            | supported   | not implemented |
| C11 - Read-only sessions                | supported            | supported   | not implemented |
| C12 - Session identity                  | supported            | supported   | not implemented |
| C13 - No env leakage                    | supported            | supported   | not implemented |
| C14 - Display name downstream           | supported            | supported   | not implemented |

The reference adapter (`claude`) is `supported` on every clause by
definition - the contract is written against it. Any clause the
reference adapter does not satisfy is a bug in the clause text, not
in the adapter. The contract text, per-clause Rust payload (argv,
MCP config JSON, hook payload), and test coverage for the reference
adapter live in `## Reference Payloads (Claude)` and in the
`agent_backend::claude_code` submodule's Rust doc comments.

`opencode` is a future-work stub reachable through
`agent_backend::backend_for_kind` but intentionally excluded from
`AgentBackendKind::all()`, `FromStr`, and every user-facing selection
path. It does not implement any clause and is not wired into any
spawn site. Promoting it to a production adapter requires flipping
every cell off `not implemented` and passing the
`codex_shape_compiles`-equivalent trait surface test.

### Codex adapter notes

The cells below with status `supported` or `workaround` have
adapter-specific details for Codex worth preserving. Each note
describes only what differs from the contract clause's baseline
requirement; the clause text itself is authoritative for what is
required, and the argv is spelled out in `## Reference Payloads
(Codex)`.

**C3 - Permissions**: Codex emits
`--dangerously-bypass-approvals-and-sandbox` on all four spawn paths
(work-item interactive, global assistant, review gate, rebase gate).
The bypass flag is symmetric across all four paths, even on the
conceptually-read-only review gate, because Codex's built-in
`workspace-write` and `read-only` sandbox modes are incompatible with
workbridge's linked-worktree layout: git stores each worktree's
index outside the worktree itself at
`<repo>/.git/worktrees/<slug>/`, which `workspace-write` forbids
writing to. Full rationale and the per-harness permission table are
in README "Per-harness permission model". The flag symmetry satisfies
the harness-contract review rule against asymmetric protections;
the review gate is included explicitly so review skills that shell
out (e.g. `cargo check`) are not silently denied.

**C4 - MCP injection**: Codex has no external-JSON-config
mechanism - MCP config lives in TOML (in `~/.codex/config.toml` or
via `-c key=value` overrides). There is no
`mcp_servers.<name>.config = "<path>"` sub-field that reads an
external JSON; the Codex CLI rejects that shape with "invalid
transport in `mcp_servers.<name>`" (verified live). `CodexBackend`
therefore emits per-field overrides built from a structured
`McpBridgeSpec` (command + args) rather than passing a JSON config
path. Values are TOML-quoted so paths and prompts with special
characters (quotes, newlines, equals signs) survive Codex's TOML
parser. Missing `mcp_bridge` (e.g. socket bind failed) degrades to
omitting the workbridge overrides rather than falling back to
`~/.codex/config.toml`, which would cross-contaminate the user's
personal config with workbridge runtime state. Workbridge still
writes the Claude-shaped MCP config JSON tempfile (used by the
reference adapter and by the on-disk parity path), but Codex's
argv deliberately does not reference it.

**C5 - Tool allowlist by spawn type**: Codex does not expose a
fine-grained MCP tool allowlist at the CLI level - its closest
concepts are `--sandbox` (filesystem/network policy) and
`--approval-policy`. Enforcement relies entirely on the workbridge
MCP server filter, which is the authoritative enforcement boundary
for every adapter regardless (the CLI-level allowlist is defence in
depth on the reference adapter). Not a clause violation; but the
CLI-level defence-in-depth layer is absent for Codex.

**C6 - System prompt injection per stage**: Codex has no
`--system-prompt` flag. `CodexBackend` delivers per-stage prompts via
`--config instructions="<prompt>"` (TOML-quoted). This is observably
different from a true system prompt because the model may weight it
lower, but the clause requirement (per-stage prompt injection at
spawn time) is met.

**C8 - Stage reminders**: Codex has no hook system matching Claude
Code's `PostToolUse` matcher. The Planning-stage reminder is embedded
into the stage prompt delivered via the C6 mechanism, which means
the model sees the reminder once at spawn time rather than re-firing
after each tool call. The clause text is explicit that the delivery
mechanism is unspecified, so this is a valid adapter choice; but it
is strictly weaker than the hook-based reminder because it cannot
re-fire after the first turn.

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
the `agent_backend` module (`src/agent_backend/`), called via `App::build_agent_cmd_with`
from `App::finish_session_open`. Cwd: the work
item's worktree path. The positional prompt MUST precede
`--mcp-config`; see the regression test
`claude_interactive_argv_for_planning` in the `tests` module of
`crate::agent_backend::claude_code`.

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
the `agent_backend` module (`src/agent_backend/`) and handed to
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

Source: `mcp::build_mcp_config`. For work-item
sessions, `extra_servers` (user-configured per-repo entries) are
inserted first; the workbridge server is appended last so it wins
on name collision. The socket path is produced by
`mcp::socket_path_for_session`.

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
the `agent_backend` module (`src/agent_backend/`), installed into argv by
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
the `agent_backend` module (`src/agent_backend/`). The harness MUST produce an envelope whose
structured body conforms to the `--json-schema` payload in RP2;
`parse_review_gate_stdout` uses `.as_bool()` and `.as_str()` with
safe defaults, so absence of either field is interpreted as "not
approved, empty detail". A backend whose headless output shape
differs (e.g. Codex `exec --json` emits an event stream) does its
own extraction inside its `parse_review_gate_stdout` implementation
before returning the same `ReviewGateVerdict` struct.

## Trait Implementation

The provider-agnostic interface described by C1-C14 is implemented in
the `agent_backend` module tree. The `AgentBackend` trait, the
`AgentBackendKind` discriminant, the `SpawnConfig` /
`ReviewGateSpawnConfig` / `ReviewGateVerdict` / `McpBridgeSpec`
structs, and the `backend_for_kind` factory live in the module
aggregator; each adapter's `impl AgentBackend` lives in its own
submodule beside it. The `AgentBackend` trait's Rust doc comments
are the authoritative source for the full trait surface, per-method
clause mappings, and invariants; this document does not reproduce
them.

Every spawn site in the `## Known Spawn Sites` table below resolves
the backend per call - there is no singleton resolver. Work-item,
review-gate, and rebase-gate spawns read the per-item
`harness_choice: HashMap<WorkItemId, AgentBackendKind>` via
`App::backend_for_work_item(wi_id) -> Option<Arc<dyn AgentBackend>>`
and fail closed with a user-visible toast on `None` (no silent
default). The global-assistant spawn resolves
`agent_backend::backend_for_kind(kind)` where `kind` comes from
`App::global_assistant_harness_kind()` (reading
`config.defaults.global_assistant_harness`). The
`App::services::agent_backend` field exists but is NOT consulted by
any spawn path; it is retained only for test stubs and non-spawn
helpers.

Harness-neutrality is enforced by the `codex_shape_compiles` test in
`crate::agent_backend::codex::tests`, which exercises `CodexBackend`
through the same trait surface as `ClaudeCodeBackend` on every
`cargo test` run; and by the review-policy rule in `CLAUDE.md` that
requires any harness-invocation change to update this doc in the
same PR.

## Known Spawn Sites

These are the only places in the `workbridge` crate that launch an LLM
harness child process today. Any new spawn site MUST update this table
**and** update the Adapter Compatibility Matrix above (a new spawn site
may also require per-adapter status refreshes or per-clause notes).

| Spawn site (Rust path)                                 | Mode        | Scope       | Thread     | Cwd                                       |
|--------------------------------------------------------|-------------|-------------|------------|-------------------------------------------|
| `App::finish_session_open` (via `Session::spawn`)       | Interactive | WorkItem    | Background | Work-item worktree                        |
| `App::spawn_review_gate` (headless `Command::output`)   | Headless RO | ReviewGate  | Background | inherited                                 |
| `App::spawn_rebase_gate` (headless `Command::spawn`)    | Headless RW | RebaseGate  | Background | Work-item worktree                        |
| `App::spawn_global_session` (via `Session::spawn`)      | Interactive | Global      | Background | `$TMPDIR/workbridge-global-assistant-cwd` |

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

All four sites go through `Session::spawn` for
the interactive path or `std::process::Command::output()` directly
for the headless path; argv is built by
`ClaudeCodeBackend::build_command` / `::build_review_gate_command`
in the `agent_backend` module on the per-call-resolved
`Arc<dyn AgentBackend>` (from `App::backend_for_work_item` for the
work-item / review-gate / rebase-gate paths, or from
`agent_backend::backend_for_kind` + `global_assistant_harness_kind`
for the global path) - no spawn site constructs a Claude-specific
argv inline and no spawn site consults the singleton
`App::services::agent_backend`. `App::build_agent_cmd_with` is the
thin wrapper the work-item and global spawn sites call on the
resolved trait object. Global assistant teardown lives in
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

## Reference Payloads (Codex)

These are the per-harness equivalents of RP1 / RP2 / RP2b for
Codex. They are the argv that `CodexBackend::build_command` /
`::build_review_gate_command` / `::build_headless_rw_command`
produce for a typical Planning / review-gate / rebase-gate spawn.
Pinned by the `codex_*` tests in the `agent_backend` module test suites.

### RP1c - Codex interactive work-item argv

```
codex
  --dangerously-bypass-approvals-and-sandbox
  [--config mcp_servers.<extra>.command="..."                              ]   # zero or more extras
  [--config mcp_servers.<extra>.args=[...]                                 ]   # (emitted FIRST)
  [--config mcp_servers.<extra>.default_tools_approval_mode="approve"       ]
  --config mcp_servers.workbridge.command="<workbridge exe path>"
  --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<socket path>"]
  --config mcp_servers.workbridge.default_tools_approval_mode="approve"
  --config instructions="<stage system prompt>"
  <auto-start user prompt (if any)>
```

All shell/patch and MCP-tool approvals are bypassed via
`--dangerously-bypass-approvals-and-sandbox`. The per-server
`mcp_servers.<name>.default_tools_approval_mode = "approve"`
overrides remain (defence in depth - the dangerous flag covers MCP
approvals today but the per-server overrides ensure the behaviour
survives a Codex change to that flag's scope).

Codex's built-in sandbox modes (`workspace-write`, `read-only`)
are incompatible with workbridge's linked-worktree layout: git
stores each worktree's index outside the worktree itself, at
`<repo>/.git/worktrees/<slug>/`, and `workspace-write` forbids
writes outside the cwd so `git commit` fails with "Operation not
permitted" when trying to create `<repo>/.git/worktrees/<slug>/
index.lock`. Granting `.git/` as a writable root is blocked by
Codex's protected-paths rule. Full rationale and the per-harness
permission table are in README "Per-harness permission model".

Read-only interactive sessions (global-assistant read-only,
hypothetical future read-only scope; no caller today) omit the
dangerous flag entirely because the MCP-server layer enforces
read-only there and granting write capability would be the bug
the read-only path exists to prevent. The per-server
`default_tools_approval_mode="approve"` is still emitted so
read-only MCP tool calls go through without prompting.

Ordering invariant (R3-F-1): the workbridge primary's `--config
mcp_servers.workbridge.*` overrides MUST be emitted AFTER every
per-repo extra. Codex's `-c key=value` overrides are last-write-wins,
so this ordering structurally guarantees that no extra (whether
named `workbridge` accidentally or maliciously) can clobber the
workbridge bridge entry. Mirrors `crate::mcp::build_mcp_config`,
which inserts the `workbridge` key into the JSON map last for the
same reason. Pinned by `codex_extras_cannot_override_workbridge_primary`
in the `agent_backend` module test suites.

Key-quoting (R3-F-2): each `<extra>` is rendered through
`toml_quote_key` so server names containing characters outside
TOML's bare-key alphabet (`A-Za-z0-9_-`) emit a quoted key fragment
(`mcp_servers."my.server".command=...`) instead of a bare key that
would mis-split the TOML path. Pinned by `toml_quote_key_*` and
`codex_extra_bridge_with_dotted_name_emits_quoted_key`.

Differences from Claude's RP1:
- `--dangerously-bypass-approvals-and-sandbox` instead of
  `--dangerously-skip-permissions`. Both are the vendor-named
  "run without built-in sandbox or approval prompts" flag; the
  rationale for preferring the dangerous-bypass flag over Codex's
  built-in `workspace-write` sandbox is in README "Per-harness
  permission model".
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
--dangerously-bypass-approvals-and-sandbox
exec
  --json
  --config instructions="<review gate system prompt>"
  [--config mcp_servers.<extra>.command="..."  ]   # zero or more extras
  [--config mcp_servers.<extra>.args=[...]      ]   # (emitted FIRST)
  --config mcp_servers.workbridge.command="<workbridge exe path>"
  --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<socket path>"]
  <review skill prompt (e.g. /claude-adversarial-review)>
```

Same workbridge-last ordering invariant as RP1c.

`--dangerously-bypass-approvals-and-sandbox` is a top-level codex
flag and MUST precede `exec` (clap rejects top-level flags inside
the `exec` subcommand). The dangerous flag is included on this
conceptually-read-only path for two reasons: (a) symmetry across
the three Codex spawn paths (RP1c / RP2c / RP2bc), which the
harness-contract review rule on asymmetric protections requires;
and (b) review skills that invoke shell commands (e.g.
`cargo check`) would otherwise be silently denied by the
`workspace-write` sandbox.

The second positional is `exec` (not `--print`) - Codex's headless
mode is a separate subcommand. `--json` switches the event stream
to newline-delimited JSON;
`CodexBackend::parse_review_gate_stdout` keeps only the last
`agent_message` event's `content` field and parses it as the
verdict envelope body. The workbridge MCP bridge is registered via
the per-field `mcp_servers.workbridge.command` / `.args` overrides
(same rationale as RP1c).

### RP2bc - Codex headless rebase-gate argv

```
--dangerously-bypass-approvals-and-sandbox
exec
  --json
  [--config mcp_servers.<extra>.command="..."  ]   # zero or more extras
  [--config mcp_servers.<extra>.args=[...]      ]   # (emitted FIRST)
  --config mcp_servers.workbridge.command="<workbridge exe path>"
  --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<socket path>"]
  <rebase instruction prompt>
```

Same workbridge-last ordering invariant as RP1c.

`--dangerously-bypass-approvals-and-sandbox` is a TOP-LEVEL `codex`
flag and MUST come BEFORE the `exec` subcommand; clap rejects
top-level flags inside the `exec` subcommand. This is the same
placement constraint that previously forced `--ask-for-approval
never` to precede `exec`.

The dangerous flag parallels Claude's
`--dangerously-skip-permissions` which already implies "no approval
prompts and no sandbox" so Claude needs no separate flag.
Pinned by `codex_headless_rw_argv_shape_and_mcp_pre_approval` in
the `agent_backend` module (`src/agent_backend/`).

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
