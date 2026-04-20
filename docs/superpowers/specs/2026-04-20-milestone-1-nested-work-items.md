# Milestone 1 - Nested Work Items

Date: 2026-04-20
Status: Approved (pending implementation)
Part of: [2026-04-20-multi-pr-work-support-design.md](2026-04-20-multi-pr-work-support-design.md)

**Scope:** 1 PR (large is fine)
**Dependencies:** None; this is the foundational PR that Milestone 2 depends on
**Assumption:** the codebase may shift significantly between design and implementation. This plan describes *what* and *why*, not file paths or function signatures. The implementer is expected to map the design onto whatever the code looks like at the time.

---

## 1. Summary

Introduce hierarchy into workbridge's work-item model. A new `Epic` kind groups `Task` kinds (today's work items) into a scoped tree. The existing Delete action becomes Close (no hard delete in the normal UX). A new terminal `Closed` stage is added to Tasks. Epic stages are fully derived from children, with Closed children treated as not-applicable.

This is the foundational milestone; it does not touch MCP, harness invocation, or the CLI surface.

---

## 2. Product thesis

Large features rarely ship as a single PR. Today a user who recognises that a feature needs multiple PRs has no structural way to express that in workbridge: they either mash all the work into one item (losing per-PR granularity) or create disconnected items (losing the "these belong together" signal). Nested work items give them a first-class container.

A "Close" action that preserves records instead of destroying them also fixes a long-standing ergonomic problem: today Delete is a one-way door, and users have no way to abandon work without losing its audit trail.

---

## 3. Scope

### In scope

- `WorkItemKind { Task, Epic }` discriminator on work items.
- Parent/child relationship (every work item has an optional parent Epic).
- Scoped left-pane navigation with breadcrumbs.
- Derived Epic stages (`Backlog`, `In Progress`, `Done`) plus explicit `Closed` flag on Epics.
- New terminal `Closed` stage for Tasks.
- Delete action repurposed as Close (cascade on Epics, with progress modal).
- Creation flows: explicit Epic creation, group-existing-into-Epic, scoped-creation, explicit Move-to modal.
- Filter/search: always global, results annotated with breadcrumbs, selection sets scope to match's parent.
- Row rendering: Epic rows with folder glyph, `EPIC` badge, derived stage short-code, progress counter, rolled-up warning signals from descendants.

### Out of scope (covered by Milestone 2)

- Any MCP tool changes.
- Extensions to `workbridge_get_context` for parent info.
- Agent-driven work-item creation.

### Out of scope (future milestones)

- Hard-delete via UI. No user action in this milestone or later removes records; Close is the only human-reachable outcome.
- Cross-repo constraints on Epics. Epics are pure grouping and place no constraint on descendants' repos.
- Stage mutation via MCP.
- Dependency / blocking links between Tasks.

---

## 4. Data shape (abstract)

Every work item is one of two kinds:

- **`Task`** - a workable unit. Carries the existing `repo_associations` (branch, worktree, PR, etc.). Has a stage from the set `{Backlog, Planning, Implementing, Blocked, Review, Mergequeue, Done, Closed}`.
- **`Epic`** - a container. No branch, no worktree, no PR, no session. Carries a single `closed: bool` flag and no other stage field. Its displayed stage is derived from children unless `closed == true`.

Both kinds carry a `parent_id` pointing to an Epic (or null for top-level). Arbitrary nesting depth. Cycles prohibited.

### Migration

Existing flat work items migrate trivially as `Task` with `parent_id = null`. No user-visible change for projects that never create Epics. The serialization format must be backwards-compatible so older workbridge state files continue to load.

---

## 5. Left-pane navigation

- **Root scope** shows parentless work items (both Tasks and Epics).
- **Drilling in** (Enter on an Epic, or equivalent keybinding) replaces the list with that Epic's direct children and updates the breadcrumb.
- **Drilling up** (Esc, or clicking a breadcrumb segment) restores the parent scope.
- **Drilling into a Task** continues to open the right-panel session view as today; it does not change the left-pane scope.
- **Breadcrumb** is always visible above the list. Format: `root > Epic A > Epic B > ...`. Segments are navigable via click and keyboard.
- Depth is unlimited.

### Scope state

The current scope is a piece of UI state independent of selection. Changing scope changes what the list enumerates but does not implicitly change the right-panel session if one is open on a Task elsewhere. (This is consistent with today's behavior: the right panel is bound to its Task, not to the list's current position.)

---

## 6. Stages

### Task stages

The existing set plus a new terminal stage:

```
Backlog, Planning, Implementing, Blocked, Review, Mergequeue, Done, Closed
```

- `Done` and `Closed` are both terminal.
- `Done` means completed-as-intended (typically a merged PR).
- `Closed` means abandoned / won't-do. Reachable only via the Close action. No merged PR required.
- Closed Tasks are hidden by the default list filter; a "Closed" filter (or "show all") reveals them.

### Epic stages (derived)

The Epic does not store a stage field. Its displayed stage is computed on demand.

**Derivation rule** (Closed children are ignored for derivation purposes):

1. If `closed == true` on the Epic itself -> display `Closed`. Stop.
2. Let `effective = children whose stage != Closed && (for Epic children) closed != true`.
3. If `effective` is empty -> `Backlog`.
4. If every item in `effective` is in `Done` -> `Done`.
5. If any item in `effective` is in a non-Backlog, non-terminal stage -> `In Progress`.
6. Otherwise (effective pool is all Backlog) -> `Backlog`.

Consequences:
- `3 Closed + 1 Done` -> Epic `Done` (effective pool = 1 Done).
- `3 Closed only` -> Epic `Backlog` (effective pool = empty).
- Nested Epics derive bottom-up; the rule applies recursively.

Epics are **never manually stageable**. The only explicit state an Epic carries is its `closed` flag.

---

## 7. Close action (replaces Delete)

- Single user action, keeping the existing keybinding and menu entry. The UI label may remain "Delete" for familiarity; the semantics are Close.
- Records are **never hard-deleted by this action**. Everything persists, visible in the "Closed" filter.

### On a Task

- Sets stage to `Closed`.
- Runs the existing cleanup side effects the current Delete triggers (worktree removal, etc.).
- No confirmation modal beyond what exists today.

### On an Epic

1. **Confirmation modal** lists what will be closed: the Epic title + a recursive count and (for a small set) the names of descendant Tasks and Epics. Phrasing makes it clear records are preserved, not destroyed.
2. **Progress modal** while the cascade runs. Shows per-descendant progress (closing Task X, closing Epic Y).
3. On completion:
   - All descendant Tasks: stage -> `Closed` + existing cleanup side effects.
   - All descendant Epics: `closed = true`.
   - The target Epic: `closed = true`.
4. If any step fails partway (e.g. a worktree can't be cleaned up), the modal surfaces the partial outcome truthfully. Do not claim success when it isn't.

### Reopening

- Closed Task: moving it to any non-terminal stage (existing mechanism) is the reopen.
- Closed Epic: explicitly un-setting the `closed` flag (new action, e.g. "Reopen"). Reopening an Epic does not automatically reopen its descendants; they remain Closed individually and the user reopens them as needed. Epic's derived stage resumes immediately.

---

## 8. Creation flows

Four flows, all supported:

1. **Explicit Epic creation.** The "new work item" flow has a kind toggle (Task / Epic). An Epic can be created empty. If created while a scope is active, it lands inside that scope.
2. **Group existing into Epic.** Selection-based action: select one or more work items in the current scope, invoke "Group into Epic...", modal prompts for the new Epic's title. The selected items become children of the new Epic; the new Epic lands in whatever scope is currently active. Items from different repos can be grouped together (no constraint).
3. **Scoped creation.** Creating a work item while inside an Epic scope parents it to that Epic. This is the fast path for building out an Epic's children.
4. **Explicit move.** "Move to..." action opens a fuzzy-search picker listing all Epics (with breadcrumbs) plus a "Move to root" option. Cycle-excluded (can't move an Epic under its own descendant). Moving preserves all state; only `parent_id` changes.

---

## 9. Filter / search

Filters and search are always global. The scoped view does not constrain filter results.

- When a filter or search is active, the list renders a flat result list spanning the entire project.
- Each match row is annotated with its full breadcrumb path. The breadcrumb is visible in the row (not a tooltip) and uses the same format as the top breadcrumb.
- Activating a match (Enter / selection) opens the Task/Epic and sets the current scope to the match's parent. Clearing the filter returns the user to that scope - not to wherever they were before the filter began.

The existing search entry and keybindings are reused; only the results renderer changes.

---

## 10. Row rendering

### Task row

Unchanged from today.

### Epic row

- **Disclosure/folder glyph** prefix (e.g. `> ` collapsed, an alternative glyph if this Epic is the currently-scoped one).
- **`EPIC` badge** in white text on a purple background. Distinct from any existing stage badge palette so at-a-glance "this is a container" recognition works in a mixed list.
- **Derived-stage short-code badge** reusing the existing short-code vocabulary:
  - `B` -> Backlog
  - `IM` -> In Progress (reuses the existing short-code even though the Epic stage name is "In Progress"; the `EPIC` badge disambiguates)
  - `D` -> Done
  - `C` -> Closed
  Do not introduce long-form labels.
- **Item title**.
- **Progress counter** `(n_done / n_total_non_closed)`. Closed children are N/A for both numerator and denominator; otherwise the counter drifts and loses meaning as items get closed.
- **Rolled-up warning flags** from any descendant (direct or indirect):
  - CI failing somewhere inside
  - Worktree dirty somewhere inside
  - Unread PR review comments somewhere inside
  - Any other per-Task signal that exists today
  
  Reuses the same visual affordances Task rows already carry. The Epic row shows the roll-up badge when any descendant has the corresponding condition.

### Transient markers

The row renderer needs a hook for transient per-row markers (e.g. "NEW"). Milestone 2 introduces the first consumer. Design the hook in Milestone 1 so Milestone 2 plugs in without a second pass over the renderer.

---

## 11. Invariants (enforced at the data/service layer)

These must be enforced below the UI, not merely by UI guards:

1. `Epic` kind work items never carry `repo_associations`, `branch`, `worktree_path`, `pr`, or a session. Attempting to set any of these on an Epic is an error.
2. `Task` kind work items never have children. `parent_id` can point only to an Epic (or null).
3. A work item's `parent_id` chain must not cycle.
4. A `Task`'s stage is one of the eight listed in Section 6.
5. An Epic's stored stage-relevant state is exactly one bit: `closed`. There is no derived-stage cache; the UI computes on demand (or via a memoized pure function over the tree).
6. The Close action does not hard-delete records.

### Doc home for these invariants

`docs/invariants.md` is immutable per project rules and must not be modified. A **new doc** (proposed name: `docs/work-item-hierarchy.md`) holds these invariants. This doc becomes the canonical reference for the nesting model and is referenced by Milestone 2.

---

## 12. Docs updates

### New

- **`docs/work-item-hierarchy.md`**: Task/Epic kinds, parent/child model, scoped navigation, derivation rule, Close semantics, and Section 11 invariants. This doc is created in this PR and referenced by subsequent milestones.

### Updated

- Whatever existing docs describe the UI (e.g. `docs/ui.md` or equivalent): add sections for breadcrumb navigation, Epic row rendering, filter/search annotations, transient markers hook.
- Whatever existing docs describe stages (if any): add `Closed` terminal stage; describe terminal semantics.
- `CONTRIBUTING.md`: mention `docs/work-item-hierarchy.md` as required reading for work-item-model changes.

### Explicitly not touched

- `docs/invariants.md` (immutable per project rules).
- `docs/harness-contract.md` (no harness / MCP changes in this milestone; if a change to this file seems necessary, that is a signal that scope has expanded).
- `docs/cli.md` (no CLI surface changes in this milestone).

---

## 13. Project rules the implementer must respect

These are project invariants independent of the feature design. The plan calls them out because they constrain how the feature is built:

- **Blocking I/O on the main UI thread is always prohibited.** The cascade-close's worktree-cleanup and any git shell-outs must run on background threads with results propagated via the existing channel pattern.
- **User-facing claims must reflect actual outcomes.** If the cascade partially fails, the toast and progress modal must say so; they must not claim "Closed 5 items" if one failed.
- **Preferring built-in ratatui widgets over custom implementations.** Breadcrumb rendering, the progress modal, and Epic rows should compose from existing widgets where feasible. Only introduce custom widgets when a built-in cannot be composed to fit.
- **Session titles remain downstream of live harness state.** This milestone does not touch session titles.
- **No linter suppressions, no skipped hooks.** If CI fails, fix the underlying issue.

---

## 14. Acceptance criteria

The PR is ready when a user can:

1. Create a new Epic explicitly via the creation flow.
2. Create Tasks inside an Epic by scoping in and using the normal creation action.
3. Group existing Tasks into a new Epic via selection + "Group into Epic...".
4. Navigate into an Epic, see only its direct children, see the breadcrumb, navigate out via Esc or breadcrumb click.
5. Move a Task between Epics via the Move-to modal; observe that the Task's branch, worktree, and PR are unaffected.
6. Move an Epic (including its subtree) between parents; observe that descendants come along.
7. See an Epic's derived-stage badge update automatically as children progress through stages.
8. Close a Task and observe it disappear from the default filter; reveal it via the "show all" or "Closed" filter.
9. Close an Epic with descendants; observe the confirmation modal naming what will close, the progress modal during cascade, and the Epic + descendants all in Closed state afterward. Records persist.
10. Reopen a Closed Epic; observe descendants remain Closed individually; observe the Epic's derived stage resumes.
11. Filter for a stage globally from within a scoped view; see matches from the whole project each annotated with its breadcrumb; select one and be placed in its parent scope.
12. See Epic rows with the `EPIC` badge, progress counter, and rolled-up warning signals behaving correctly across mixed scenarios (CI fail on a deep descendant, dirty worktree on a direct child, etc.).
13. Load a workbridge state file from before this PR and observe all existing work items loading as top-level Tasks with no visible disruption.
