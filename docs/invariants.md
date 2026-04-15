# Invariants

Invariants are the non-negotiable rules of the system. They define what
WorkBridge considers valid. Code must enforce them. If reality violates
an invariant, the system surfaces an error rather than guessing.

These are requirements, not features. All implementation decisions should
be evaluated against these rules.

## The Rules

### 1. One work item = one or more repo associations

A work item is an independent entity anchored by a backend record (local
file in v1, GitHub Issue/Project later). Each work item has one or more
repo associations, where each association optionally has a worktree. A work
item can exist before any worktree is created (pre-planning state), but it
must always be associated with at least one repo. If no repo can be
assigned, the work item cannot be created - an error is shown instead.

If a worktree disappears (removed externally), the repo association's
worktree_path is cleared on the next scan. The work item persists because
the backend record is the source of truth, not the worktree. The PR and
issue on GitHub remain the permanent external record.

### 2. A worktree must be on a named branch

Detached HEAD worktrees are not valid work items. The branch name is the
identity of the work -- it drives issue linkage, PR discovery, and display.
Without a branch name, none of the derivation chain works.

This invariant applies to worktrees that exist. A repo association with
branch: None and no worktree is a pre-planning state, not a violation.

### 3. A worktree must not be on the default branch

The default branch (main, master, or configured) is the trunk. It is not
"work in progress." The main worktree of a repo is on the default branch,
but it is not a work item. Additional worktrees on the default branch are
an error.

### 4. One branch = at most one open PR

If multiple open PRs share the same head branch, WorkBridge cannot determine
which is current. This is an error state on the work item. The user must
close the stale PR.

### 5. Issue linkage is derived from the branch name

The branch name is the contract for issue linkage. A configurable regex
extracts zero or one issue identifier from the branch name. If the regex
matches, the issue is linked. If it doesn't match, there is no issue. There
is no manual override for this at the UI level -- the override file
(see [data-assembly.md](data-assembly.md)) exists for edge cases but is
not a primary workflow.

This invariant does not apply when no branch exists. A repo association
in pre-planning state (branch: None) has no issue linkage.

### 6. Derive transient metadata, backends anchor identity

Transient metadata (PR status, CI checks, git state) is derived on every
scan from git and GitHub, never stored. However, work item identity, title,
status, and repo associations are anchored by a backend record (local file
in v1). The backend record is the persistent source of truth for what work
items exist. Everything else is derived.

The only persistent state is:

- The list of registered repositories (config file)
- Backend records, including their per-item activity logs and archived
  activity logs for deleted items (stored in platform data directory)
- Optional per-worktree override files (rare, for edge cases)

If it can be derived from git or GitHub, it must not be stored. Backend
records store only identity and structural data that cannot be derived.

### 7. Unlinked PRs are not work items

A GitHub PR whose branch does not match any work item's repo associations
is an "unlinked" PR. Unlinked PRs appear in a separate group in the left
panel (hidden when empty) and can be imported into a work item via a
backend record. They cannot have sessions and cannot be edited until
imported. Importing creates a backend record and promotes the unlinked
PR to a full work item.

### 8. One registered repo = one GitHub remote

Each registered repo maps to exactly one GitHub owner/repo for API calls,
derived from the `origin` remote URL. If the remote is not GitHub, GitHub
features are disabled for that repo. There is no support for multiple
GitHub remotes (e.g., fork + upstream) in v1.

## Why Strict Invariants

The alternative to strict invariants is heuristics: "if there are two open
PRs, pick the most recent one." Heuristics work most of the time, but when
they fail, they fail silently. The user sees wrong data and doesn't know it.

Strict invariants fail loudly. The user sees an error message with the
conflicting data and a suggested fix. This costs a few seconds of attention
but prevents decisions based on incorrect state.

The system can always be loosened later if a strict rule proves too
restrictive. Loosening a strict system is safe. Tightening a loose system
breaks existing workflows.

## Consequences

These invariants have direct consequences for the user:

- **Want issue linkage?** Name your branch with the issue number prefix.
  No config UI, no linking step, no "associate issue" dialog.

- **Want to track follow-up work on the same issue?** Create a new branch
  (e.g., `42-followup`). It becomes a new work item. The aggregation view
  groups them by issue.

- **Want to work on someone else's PR?** Import it from the unlinked
  group. This creates a backend record. Now it's your work item.

- **Merged and done?** Delete the work item. The backend record is removed
  and worktrees can be cleaned up. GitHub has the history.

The system trades flexibility for predictability. Every work item behaves
the same way. Every branch follows the same rules. There are no special
cases.

### 9. Tests must not modify production state

All persistence in tests uses mocks or temp directories. Tests must never
read or write the real config file, the real backend data directory, or
any other production path. Config persistence in tests uses
`InMemoryConfigProvider`. Filesystem operations use `std::env::temp_dir()`
with cleanup. A test that touches production state is a bug.

### 10. Backend determines available repos

Every work item is tied to a backend, which exposes what repos are
available. A work item must commit to one or more of the available repos
at creation time. For example, a GitHub Issues backend always allows only
one repo. The local backend can allow all known repos locally. This is a
fundamental data model constraint.

### 11. Plan storage is backend-polymorphic

The plan is passed to the backend, and the backend decides where and how
to store it. The local backend stores it in the work item JSON. A GitHub
Issues backend would post it to the PR description or issue body. The
caller does not choose the storage mechanism.

### 12. Workflow stages are explicit

Work items progress through explicit stages: Backlog, Planning,
Implementing, Blocked, Review, Done. Stage transitions are user-initiated
(MVP). The Blocked stage is a sub-state of Implementing where Claude needs
user input. Done can also be derived from merged PRs (existing behavior).

**Blocked -> Planning retreat:** When Claude blocks due to a missing
implementation plan, the user may retreat from Blocked to Planning. This
is an explicit user opt-in (prompted, never automatic). The retreat clears
the existing plan and spawns a retroactive planning session that analyzes
the branch's existing commits to produce a plan. The plan clear only
proceeds if the status transition succeeds.

### 13. Fresh Claude session per stage

Each stage transition that involves Claude spawns a fresh session. The plan
is the handoff contract between stages. Different stages have different
system prompts. All code-changing stages (Implementing, Review) must
instruct Claude to commit all work before finishing.

**"Fresh" means per `(WorkItemId, WorkItemStatus)`, not per workbridge
process.** Each `(wi_id, stage)` tuple is assigned a deterministic
Claude Code session UUID (UUID v5, derived from a workbridge-specific
namespace; see `src/session_id.rs`). Spawning for a tuple that Claude
Code has already seen reattaches to its prior transcript via
`claude --resume <uuid>`; spawning for a tuple Claude Code has not
seen creates a new session under the deterministic UUID via
`claude --session-id <uuid>`. The scheme is pure: nothing is
persisted in the workbridge data model and the UUID is recomputed
from first principles on every spawn, so the invariant survives
workbridge restarts, crashes, and backend format changes that do not
touch the identifying fields. The user-facing semantic is that
quitting workbridge mid-stage does not lose Claude's conversational
context for that stage.

Stage transitions still produce a new session because the tuple
changes: `(wi_id, Implementing) -> (wi_id, Review)` yields a
different UUID, so the Review session is structurally unable to see
the Implementing transcript. The plan remains the handoff contract.
Cross-stage context bleed is impossible by construction, not by
process isolation.

**Enforcement:** Sessions are keyed by `(WorkItemId, WorkItemStatus)` in the
HashMap. When a work item's stage changes, the old session key becomes
unreachable - lookups use the new stage, so the old session cannot be
found or reused. Orphaned sessions (stage mismatch) are detected and
killed during periodic liveness checks.

**Session lifecycle per stage:**
- Backlog and Done: no session.
- Planning: Claude helps refine the plan. When finalized, Claude calls
  `workbridge_set_plan` via MCP. This persists the plan and automatically
  advances to Implementing (the planning session becomes an orphan and is
  killed on the next liveness check).
- Implementing: a session starts with the plan in the system prompt and
  either continues the `(wi_id, Implementing)` transcript (on restart) or
  creates a new one under that tuple's deterministic UUID. Claude
  implements the plan, then calls `workbridge_set_status` to advance.
- Review: a session starts for addressing review feedback, resuming the
  `(wi_id, Review)` transcript if one already exists.

Sessions gain conversational context only from the deterministic
transcript for the same `(wi_id, stage)` tuple and from MCP tools
(`workbridge_get_context`, `workbridge_set_plan`,
`workbridge_set_status`). They do NOT gain context from sessions
belonging to a different stage of the same work item or from any
other work item: those tuples yield different UUIDs and therefore
different Claude Code transcripts. Manual advance from Planning to
Implementing is blocked - the plan must be set via MCP to trigger
the transition.

The review gate's ephemeral `claude --print` subprocess and the global
assistant drawer are deliberately outside this scheme (they do not
participate in the deterministic UUID derivation) because they are
one-shot or separate-scope sessions that must not share identity with
the work-item stage session.

### 14. Branch is required at creation

A work item cannot be created without a branch name. The branch is the
identity of the work (invariant 2) and is needed to create worktrees and
spawn sessions. The creation dialog auto-fills a branch from the title
but the user can edit it. An empty branch is rejected at validation time.

### 15. Render tick must be >= 120fps

The UI timer must fire at 8ms or faster (~120fps). PTY output from
embedded sessions arrives on background reader threads and is fed to the
vt100 parser, but only a timer-driven re-render makes those updates
visible. A slower tick (e.g. 200ms) causes visibly progressive rendering
of paste events, scrolling output, and other PTY content that should
appear instantaneous.

Heavy background work (liveness checks, fetch drains, signal handling)
is throttled inside the timer handler to run only every ~200ms
(BACKGROUND_TICK_DIVISOR). The fast tick drives rendering only; it must
not increase the frequency of expensive periodic work.

## Authorized Invariant Edits

`CLAUDE.md` treats `docs/invariants.md` as effectively immutable:
any edit is P0 unless covered by a specific, recorded session
authorization naming the exact bullet, the user's rationale, and the
scope boundary. This section is the persistent record of such
authorizations so adversarial reviewers (human or automated) can
verify that every edit to this file was sanctioned.

Format per entry: date, PR / work-item ID, invariant bullet touched,
user rationale, scope boundary, and (if relevant) the skill or review
channel that carried the authorization. Entries are append-only: once
recorded, do not rewrite or remove a prior authorization - add a new
entry if a subsequent edit refines or supersedes it.

### 2026-04-15, PR #91, invariant 13 "Fresh Claude session per stage"

- **Bullet touched:** The entire body of invariant 13, including the
  new "`Fresh` means per `(WorkItemId, WorkItemStatus)`, not per
  workbridge process" paragraph, the stage-transitions-still-change-
  the-tuple paragraph, the revised per-stage session lifecycle
  list, the rewritten "Sessions gain conversational context only
  from the deterministic transcript for the same `(wi_id, stage)`
  tuple and from MCP tools" clause, and the closing note about the
  review gate and global assistant being outside the scheme.
- **User rationale (verbatim from PR #91 task description):** "If
  we quit workbridge and resume, it should re-spawn the last known
  claude sessions using their session IDs. There should be 1
  session to resume, so when we switch phases, we should update
  it." Quitting and re-entering workbridge must not wipe Claude's
  in-stage conversational context for an active work item; each
  stage still keeps its own isolated transcript.
- **Scope boundary:** The edit relaxes the "one fresh session per
  stage transition" contract ONLY for the restart-resume case
  within the same `(WorkItemId, WorkItemStatus)` tuple. Stage
  transitions (Planning -> Implementing -> Review) still produce
  a new deterministic UUID, so the Review session is structurally
  unable to see the Implementing transcript and cross-stage
  context bleed remains impossible by construction. The review
  gate's ephemeral `claude --print` subprocess and the global
  assistant drawer remain explicitly outside the scheme. No other
  invariant bullet is touched and no unrelated "session hygiene"
  rule is relaxed.
- **Authorization channel:** Codex adversarial review flagged the
  unauthorised edit on 2026-04-15 during PR #91's review-gate
  stage. The user was presented with a summary of the conflict
  and two explicit options (A: authorise the invariant edit, B:
  drop the resume feature). The user replied "Option A - feel
  free to edit invariant-13" in the same review session. This
  record pins that authorization to the specific edit so future
  reviewers can verify the edit matches the scope above without
  re-reading the conversation transcript.
