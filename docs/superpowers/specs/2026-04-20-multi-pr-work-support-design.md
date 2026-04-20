# Design: Multi-PR Work Support

Date: 2026-04-20
Status: Approved (pending implementation)

## Goal

Give workbridge a first-class way to represent and manage a single feature
that is deliberately split across more than one pull request, and give an
agent mid-session the affordance to populate that structure without asking
the user to stop and create siblings by hand.

Today workbridge has no structural way to express "these work items belong
together." A user who recognises that a feature needs multiple PRs must
either cram unrelated work into one ticket (losing per-PR granularity and
review hygiene) or create disconnected tickets (losing the umbrella signal).
An agent that discovers mid-execution that a task should fan out has no
affordance to file sibling work items and has to interrupt the user to do
it manually.

## Success criteria

- A user can create a container work item ("Epic"), nest other work items
  under it to arbitrary depth, move work items between parents, and see
  where they are via a scoped breadcrumb UI.
- Closing an Epic cascade-closes its descendants without hard-deleting
  records. Records persist and remain visible via a "show Closed" filter.
- An agent running inside a per-work-item session can read the work-item
  tree and create new siblings or children via MCP without spawning any
  branches, worktrees, or PRs as a side effect. The user is in control of
  every destructive or reorganising action; MCP's write surface is
  create-only.
- Every change respects workbridge's existing project invariants: no
  blocking I/O on the UI thread, no silent fallbacks to defaults, no
  linter suppressions, `docs/invariants.md` untouched, harness-contract
  parity required for any MCP-injection change.

## Approach

The design is split into two sequential, independently-shippable PRs so
each can land, be reviewed, and be exercised on its own.

### Milestone 1 - Nested work items (foundational)

Introduces a `WorkItemKind { Task, Epic }` discriminator plus a `parent_id`
field. Tasks are today's work items; Epics are pure grouping containers
with no branch, worktree, PR, or session. Epic stages are derived from
their non-Closed children; the only explicit state an Epic carries is a
`closed` flag. The existing Delete action is repurposed as Close: records
are never hard-deleted by normal UX and remain visible under a "show
Closed" filter. A new terminal `Closed` stage is added to Tasks. The
left pane gains scoped navigation with a breadcrumb; filter/search
becomes global with breadcrumb annotations on matches. Epic rows render
with a purple `EPIC` badge, a folder glyph, the derived-stage
short-code, a `(done / total-non-closed)` progress counter, and
rolled-up warning signals from descendants (CI failing, worktree
dirty, unread PR comments, etc.). A new doc
`docs/work-item-hierarchy.md` codifies the model and the invariants
that must be enforced below the UI, not merely by UI guards.

See [2026-04-20-milestone-1-nested-work-items.md](2026-04-20-milestone-1-nested-work-items.md)
for the full milestone design.

### Milestone 2 - MCP agent-driven work-item creation (depends on M1)

Exposes the work-item model to every harness session over MCP. Three
surface changes:

1. A create MCP tool callable from any session (per-work-item or
   global-assistant). `parent` is a required argument (Epic ID or an
   explicit `null` for root); there are no implicit defaults. The tool
   creates the record only, with no eager branch / worktree / PR
   provisioning - exactly the same state a human-created ticket would
   be in before its first session.
2. A hierarchy-read MCP tool returning a flat list with parent pointers.
   Default filter excludes Closed items; `include_closed = true` reveals
   them.
3. `workbridge_get_context` is extended additively with the caller's
   `id`, `kind`, `parent_id`, `breadcrumb`, and `ancestor_epic_ids` so
   "where am I, what would a sibling look like?" is one call.

Write scope is strictly create-only. No close / rename / reparent /
stage mutation via MCP; every destructive or reorganising action remains
human-only. Successful creates fire a breadcrumbed toast and mark the
new row with a transient `NEW` marker that clears on first view and
persists across restarts. Failed creates are silent by design - they
are already visible in the session transcript.

See [2026-04-20-milestone-2-mcp-work-item-creation.md](2026-04-20-milestone-2-mcp-work-item-creation.md)
for the full milestone design.

## Sequencing

Milestone 1 has no dependencies. It ships first.

Milestone 2 requires M1's kind discriminator, parent/child relationship,
Closed stage, and the transient-marker row-renderer hook. It ships second,
in its own PR.

Each milestone is a large but single PR. Neither milestone is gated on any
external work; both are self-contained changes to workbridge.

## Invariants

The design depends on a small set of invariants that must hold below the
UI layer. M1 codifies the data-model invariants in
`docs/work-item-hierarchy.md`; M2 adds MCP-boundary invariants to the
same doc plus the clause table in `docs/harness-contract.md`.

Data-model invariants (from M1):

1. Epic-kind work items never carry `repo_associations`, `branch`,
   `worktree_path`, `pr`, or a session. Attempting to set any of these
   on an Epic is an error.
2. Task-kind work items never have children. `parent_id` can point only
   to an Epic (or null).
3. A work item's `parent_id` chain must not cycle.
4. A Task's stage is one of `Backlog`, `Planning`, `Implementing`,
   `Blocked`, `Review`, `Mergequeue`, `Done`, `Closed`. The last two are
   terminal; `Done` means completed-as-intended, `Closed` means
   abandoned / won't-do.
5. An Epic's stored stage-relevant state is exactly one bit: `closed`.
   Its displayed stage is derived from descendants (Closed children
   ignored).
6. The Close action does not hard-delete records.

MCP-boundary invariants (from M2):

1. The create tool's `parent` argument is always explicit (Epic ID or
   null). No inference from caller context, no default value.
2. The create tool cannot produce cycles. Since it only creates,
   cycles shouldn't arise, but the parent-is-Epic + parent-exists
   checks must run regardless.
3. The create tool never performs eager branch / worktree / PR
   provisioning.
4. The hierarchy-read tool is read-only and excludes Closed items by
   default.
5. The context extension is purely additive; existing fields behave
   as before.
6. MCP create failures are not surfaced to the user; they are
   returned to the agent through standard MCP error channels.

## Docs touched by each PR

### M1 PR

- New: `docs/work-item-hierarchy.md` (canonical model + invariants).
- Updated: UI-describing docs (breadcrumb navigation, Epic row
  rendering, filter/search annotations, transient markers hook),
  stage docs (new terminal `Closed` stage), and `CONTRIBUTING.md`
  (point readers at `docs/work-item-hierarchy.md`).
- Not touched: `docs/invariants.md` (immutable),
  `docs/harness-contract.md` (no MCP surface change), `docs/cli.md`
  (no CLI surface change).

### M2 PR

- Updated: `docs/harness-contract.md` (new rows for the extended /
  new MCP tools, new clause entries for any harness adapter,
  every existing adapter column populated).
- Updated: `docs/work-item-hierarchy.md` gains a "Programmatic
  creation via MCP" section.
- New or updated: UI-conventions section codifying the
  externally-originated-item marker (applies to any non-user source,
  not just MCP).
- Not touched: `docs/invariants.md` (immutable), `docs/cli.md`
  (no CLI surface change unless the `--mcp-bridge` routing grows
  new tool entries, in which case `docs/cli.md` updates in the same
  PR per project rules).

## Project rules the implementer must respect

These are project invariants independent of this feature. The design
calls them out because they constrain how both milestones are built:

- **Blocking I/O on the main UI thread is always prohibited.** The
  M1 cascade-close's worktree cleanup and every git shell-out must
  run on background threads with results propagated via the existing
  channel pattern. The M2 create tool does no filesystem / git work
  by design, so this should be easy there, but the state-update path
  still must not block the UI thread.
- **No silent fallbacks to defaults.** The M2 create tool's `parent`
  field is always explicit; omitting it is an error, not a default.
  Any future "which Epic should this go under?" question is the
  user's or the agent's to answer, not the code's.
- **User-facing claims reflect actual state.** The M1 cascade-close
  toast and progress modal must truthfully report partial failures.
  The M2 create-success toast fires only after the create has
  committed, and its breadcrumb must match where the record
  actually landed.
- **Prefer built-in ratatui widgets.** Breadcrumb rendering, the
  cascade-close progress modal, the Epic row, and the `NEW` marker
  should compose from existing widgets where feasible.
- **Harness-contract parity.** M2's MCP-injection surface change
  requires `docs/harness-contract.md` updates in the same PR, with
  every existing adapter column populated for every new clause.
- **No linter suppressions, no skipped hooks.** If CI fails, fix
  the underlying issue.
- **`docs/invariants.md` is immutable.** Neither milestone touches it.

## Out of scope

These are deliberately deferred to keep the two milestones shippable:

- Hard-delete via UI. No user action in either milestone removes
  records; Close is the only human-reachable outcome.
- Cross-repo constraints on Epics. Epics place no constraint on their
  descendants' repos; items from different repos can share an Epic.
- Stage mutation via MCP. Every close / rename / reparent / stage
  change remains human-only.
- Dependency / blocking links between Tasks. Out of scope for this
  design and for any follow-on milestone described here.
- Per-session permission gating of which work items a harness can
  read. The harness sees the whole project's non-closed tree. This
  matches the trust boundary for the rest of the session (the agent
  can read any file in the repo anyway).
- Eager provisioning of branch / worktree / PR from MCP create.
  Stays lazy; the same "start session" path that exists today is
  what moves a ticket from record to live work.
- User-facing surfacing of MCP create failures. Silent by design.
  Field experience may add a "repeated failure" toast in a follow-up.

## References

- [Milestone 1 - Nested work items](2026-04-20-milestone-1-nested-work-items.md)
- [Milestone 2 - MCP agent-driven work-item creation](2026-04-20-milestone-2-mcp-work-item-creation.md)
- [docs/work-items.md](../../work-items.md) - the current work-item model
  this design extends.
- [docs/harness-contract.md](../../harness-contract.md) - the MCP
  injection surface M2 updates.
- [docs/invariants.md](../../invariants.md) - immutable; neither
  milestone touches it.
- [CONTRIBUTING.md](../../../CONTRIBUTING.md) - M1 adds a pointer to
  `docs/work-item-hierarchy.md`.
