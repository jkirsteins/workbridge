# Milestone 2 - MCP Agent-Driven Work-Item Creation

Date: 2026-04-20
Status: Approved (pending implementation)
Part of: [2026-04-20-multi-pr-work-support-design.md](2026-04-20-multi-pr-work-support-design.md)

**Scope:** 1 PR (large is fine)
**Dependencies:** Milestone 1 (nested work items) must have shipped
**Assumption:** the codebase may shift significantly between design and implementation. This plan describes *what* and *why*, not file paths or function signatures. The implementer is expected to map the design onto whatever the code looks like at the time.

---

## 1. Summary

Expose workbridge's work-item model to every harness session via MCP, so an agent mid-session can create new work items (Tasks or Epics) instead of cramming follow-up work into the current Task. The agent can read the hierarchy, extends the context tool with parent/breadcrumb info, and calls an extended create tool. The UI surfaces agent-created items with a toast and a transient list marker.

This milestone treats the existing global-assistant work-item create tool and the (new) per-session create tool as **one tool** callable from both contexts.

---

## 2. Product thesis

Agents frequently discover mid-execution that a task should fan out into multiple PRs - either because the initial plan was too coarse, or because an unplanned but genuinely in-scope issue surfaces. Today they have two bad options: cram the extra work into the current Task (losing per-PR granularity) or ask the user to manually create the sibling.

Milestone 1 gave us the hierarchy primitive. Milestone 2 gives the agent the affordance to populate it directly, while keeping the user in control of destructive actions (no close / rename / reparent / stage changes via MCP).

Particularly valuable during implementation stages: when an agent encounters an out-of-scope bug or tangent, it can file a new sibling Task under the same Epic rather than stuffing the detour into the current PR.

---

## 3. Dependencies

Requires Milestone 1 shipped. Specifically the following must already exist:

- Work-item kind discriminator (`Task` / `Epic`).
- Parent/child relationship (`parent_id` on work items).
- Closed stage on Tasks and `closed` flag on Epics.
- Scoped navigation + breadcrumb format (used by this milestone's toast).

If any of the above is missing, this milestone cannot ship as designed.

---

## 4. Scope

### In scope

- **Extended create MCP tool** with explicit required `parent` field (nullable for root).
- **New hierarchy-read MCP tool** returning a flat list + parent pointers.
- **Extended `workbridge_get_context`** returning caller's `id`, `kind`, `parent_id`, breadcrumb, ancestor Epic IDs.
- **Exposure** of these tools to every harness session at every stage (not only the global assistant).
- **Toast + transient "NEW" marker UI** for agent-created items.
- **`docs/harness-contract.md` updates** for the MCP-injection surface change.
- **New UI-conventions doc section** codifying the externally-originated item marker.

### Out of scope

- Close / rename / reparent / stage mutation via MCP. All destructive and reorganizing actions remain human-only.
- Eager provisioning of branch / worktree / PR from MCP create. Stays lazy; same as human creation.
- Per-session permission gating of which work items the harness can read. The harness sees the whole project's (non-closed) tree. This is an acceptable trust boundary because it matches the trust boundary for the rest of the session (the agent can read any file in the repo anyway).
- User-facing surfacing of MCP create failures. Silent by design (see Section 7).

---

## 5. The create tool (extended)

### Naming

One tool, reused. If a tool already exists for the global-assistant create flow, extend it with the `parent` parameter and make it callable from any session. If not, introduce the tool and expose it uniformly.

### Required arguments

Exactly what a human must fill in the "New Work Item" UI form. Today that is approximately:

- `title` (string, required)
- `description` (string, required or optional depending on the current UI; match the UI's requiredness exactly)
- `kind`: `"Task"` or `"Epic"`
- For `kind == Task`: `repo` (repo identifier registered with workbridge)
- `parent`: either a `WorkItemId` (must resolve to an Epic) or explicitly `null` (root-level)

**No implicit defaults** for `parent`. The caller must provide it. This matches the project rule "silent fallbacks to a default are a P0 violation."

### Optional arguments

Anything the human form auto-derives should be auto-derived the same way when the caller omits it:

- `branch` (for Tasks): default = slugified title via the same helper the UI uses.
- `initial_stage`: default = `Backlog`.

If the caller provides an override, honour it and validate it against the same rules the UI enforces.

### No eager provisioning

The tool creates the work-item record and nothing else. No `git worktree add`, no branch creation, no PR, no session spawn. The resulting record is in the same state as "a human created the ticket and did not start a session."

Branch / worktree / PR creation happens later through the existing "start session" path, the same way it does today for human-created items.

### Return value

```
{
  id: WorkItemId,
  breadcrumb: "root > Epic A > Epic B > New Title"
}
```

The agent can echo the breadcrumb back to the user in chat. The breadcrumb format must match exactly the format used in the UI breadcrumb bar and the Move-to modal; drift here confuses users who are learning the layout.

### Errors

Structured MCP errors with specific messages. At minimum:

- `ParentNotFound` - `parent` does not resolve to an existing work item.
- `ParentIsTask` - `parent` resolves to a Task (parents must be Epics).
- `ParentIsClosed` - `parent` resolves to an Epic with `closed == true`. Closed Epics cannot accept new children; the agent must reopen first (via a human) or pick a different parent.
- `RepoNotRegistered` - `repo` is not a registered workbridge repo.
- `RequiredFieldMissing` - a required field is absent.
- `InvalidFieldForKind` - e.g. `kind == Epic` but the caller provided `repo` (Epics must not carry repo info).

Errors are returned to the agent through standard MCP error channels. They are **not** surfaced to the user (see Section 7 for rationale).

### Validation per kind

- `kind == Task`: `repo` is required; `parent` can be null or an Epic ID.
- `kind == Epic`: `repo` / `branch` / any other Task-only field must not be provided; `parent` can be null or another Epic ID (nested Epics supported to arbitrary depth).

---

## 6. The hierarchy-read tool (new)

A dedicated tool for enumerating the work-item tree.

### Returns

A flat list of work items. Each entry contains:

- `id`
- `title`
- `kind` (`"Task"` or `"Epic"`)
- `parent_id` (nullable)
- `stage`
  - For Tasks: the actual stage.
  - For Epics: the derived display stage, or `"Closed"` if the explicit flag is set.
- `repo` (Tasks only)
- `closed` (Epics only; the explicit flag)

### Default filter

Closed items excluded by default. Rationale: Closed items are rarely relevant for "where should I put this new thing?" decisions and can be numerous in long-lived projects.

### Arguments

- `include_closed: bool` (default `false`): when `true`, also returns Closed Tasks and Closed Epics.

### Pagination

None. Workbridge projects are not expected to contain thousands of active items. If size becomes an issue later, add filtering parameters in a follow-up.

### Ordering

Implementation-defined but stable across calls. A reasonable default: depth-first, parents before children, so an agent walking the list can reconstruct the tree in one pass.

---

## 7. Extended `workbridge_get_context`

The existing context tool gains additional fields so the most common agent decision ("where am I; what would a sibling look like?") is one tool call, not a traversal.

### New fields in the response

- `id`: the calling session's work-item ID.
- `kind`: the calling work-item's kind.
- `parent_id`: the calling work-item's parent (nullable).
- `breadcrumb`: `"root > Epic A > Epic B > this Task"` - the full path to the caller.
- `ancestor_epic_ids`: the ordered list of Epic IDs from root down to the immediate parent. Empty for root-level items. Useful for "create a sibling of me" in one write call.

### Backwards compatibility

The extension is purely additive. Existing consumers of `workbridge_get_context` that don't look at the new fields continue to work unchanged.

### Global assistant

When called from the global-assistant context (not from a per-work-item session), these new fields are absent or null. The agent in that context knows it is not bound to a specific work item.

---

## 8. Exposure

### Who gets the tools

Every harness session. That includes:

- Per-work-item sessions (Claude, Codex, and any future harness adapter).
- The global-assistant session.

### Gating

**No hard stage gate.** Planning, Implementing, Review, Mergequeue - all stages can call the create tool.

Soft-gating is done via tool descriptions. The create tool's description communicates:

> Use this to decompose work into additional tickets when the scope exceeds a single PR, or to surface out-of-scope work that deserves its own PR.

The description guides the agent toward appropriate use. The project rules (no silent fallbacks, explicit parent, no eager provisioning) are structural; they do not rely on the agent's judgement.

### Harness-contract implications

Adding new / extended MCP tools is an MCP-injection surface change and therefore requires `docs/harness-contract.md` updates in the same PR. See Section 11.

---

## 9. UI on agent-created items

### Toast

When the create tool returns successfully, a toast fires:

```
Agent created Task 'Foo' under root > Epic A > Epic B
```

Non-blocking, auto-dismissing, consistent with existing workbridge toast vocabulary. The breadcrumb in the toast uses the same format as the breadcrumb bar.

### Transient "NEW" marker

The new work item's row in the list carries a transient `NEW` marker until the user views the item. "Views" means:

- The user navigates (selects) the row.
- The user drills into it (Tasks: opens the session view; Epics: scopes into it).

On first view, the marker clears. The mechanism must persist the "viewed" state across app restarts for items that existed before the restart; if a user is offline while an agent creates items, the marker should still be present when they come back.

The marker's visual design reuses the **transient markers hook** introduced by Milestone 1's row renderer. This is Milestone 2's first consumer of that hook.

### Silent on failed MCP create attempts

Failed creates are not surfaced to the user. The agent sees a structured MCP error and can retry or adjust. Rationale: noise suppression. Failed tool calls should be rare and are already visible in the session transcript for anyone who wants to investigate.

If field experience shows the agent burning cycles on bad creates in a loop, add a "repeated failure" toast in a follow-up. Do not design it in now.

### The marker is generic

The externally-originated-item marker convention is not MCP-specific. It applies to any new work item that arrived from outside the user's direct action. Future sources (webhooks, CI signals, other agents) should reuse the same visual vocabulary.

---

## 10. Invariants (enforced at the MCP tool boundary)

1. The create tool's `parent` argument is always explicit (Epic ID or null). No inference from caller context, no default value.
2. The create tool cannot produce cycles. Since it only creates, cycles shouldn't be possible, but the parent-is-Epic + parent-exists checks must run regardless.
3. The create tool never performs eager branch / worktree / PR provisioning.
4. The hierarchy-read tool is read-only and excludes Closed items by default.
5. The context extension is additive; existing fields behave as before.
6. MCP create failures are not surfaced to the user.

### Doc home

These MCP-layer invariants belong in `docs/work-item-hierarchy.md` (created in Milestone 1) under a new "programmatic creation" section, plus in `docs/harness-contract.md` as part of the MCP-injection surface description.

---

## 11. Docs updates

### New

- **UI-conventions section** (in an existing UI doc, or a new `docs/ui-conventions.md` - implementer's call) codifying the externally-originated item marker:
  - When it appears (any item whose creation was not a direct user action).
  - How it clears (first view / selection / drill-in).
  - Visual style (reuses the transient markers hook from Milestone 1).
  - How it composes with other row affordances (EPIC badge, stage badge, rolled-up signals).
  - Which event sources qualify (starts with MCP; extensible).

### Updated

- **`docs/harness-contract.md`**: this is an MCP-injection surface change per the project rules. Required updates:
  - New rows in the tool-injection tables for: the extended/new create tool, the hierarchy-read tool, and the extension of `workbridge_get_context`.
  - New clause entries (per the existing `Cn` convention) describing what any harness adapter must support to consume these tools. At minimum: the harness must surface tool responses and structured errors to the agent in a way the agent can act on.
  - Every existing adapter's column gets a `supported` / `workaround` / `not supported` entry with a short justification.
  - If a new adapter is added in parallel, its column must populate every existing clause, not only the new ones (enforced by existing project rules).
- **`docs/work-item-hierarchy.md`** (created in Milestone 1): add a "Programmatic creation via MCP" section covering Sections 5-10 of this plan at a high level, with the canonical MCP schema referenced.

### Explicitly not touched

- `docs/invariants.md` (immutable per project rules).
- `docs/cli.md` (no CLI surface changes; the MCP bridge entry point - if it already exists - is untouched by this milestone unless the implementer needs to extend it to support new tool routing, in which case `docs/cli.md` must be updated accordingly).

---

## 12. Project rules the implementer must respect

These are project invariants independent of the feature design. The plan calls them out because they constrain how the feature is built:

- **No silent fallbacks to defaults.** The `parent` field is always explicit; if a caller omits it the tool errors, not defaults.
- **No blocking I/O on the main UI thread.** The create tool's state update must run on a background thread with results propagated via existing channels. (By design the tool does no filesystem or git work - record creation only - so this should be easy.)
- **User-facing claims reflect actual state.** The toast fires only after the create has committed to workbridge state; if commit fails, no toast. The breadcrumb in the toast must match where the record actually landed.
- **Harness-contract parity.** Every existing harness adapter must declare its support stance for every new clause. An adapter that doesn't document itself against the new clauses is incomplete and must not land.
- **No linter suppressions, no skipped hooks.** If CI fails, fix the underlying issue.
- **Prefer built-in ratatui widgets.** The toast and transient marker reuse existing UI primitives where possible.

---

## 13. Acceptance criteria

### Agent-side (with MCP access inside a session)

1. Calling the read tool returns a flat list of non-Closed work items with parent pointers; the agent can reconstruct the tree.
2. Calling the read tool with `include_closed = true` additionally returns Closed items.
3. Calling the create tool with `parent` set to a specific Epic ID creates a new Task visible in the UI under that Epic.
4. Calling the create tool with `parent = null` creates a new top-level work item.
5. Calling the create tool with `kind = Epic` creates a new Epic; subsequent calls can place Tasks inside it.
6. Calling the create tool with invalid input returns a structured error with a specific code:
   - Nonexistent parent -> `ParentNotFound`
   - Parent is a Task -> `ParentIsTask`
   - Parent is a Closed Epic -> `ParentIsClosed`
   - Missing required field -> `RequiredFieldMissing`
   - `kind = Epic` with repo-specific fields -> `InvalidFieldForKind`
7. No branch, worktree, or PR is created as a side effect of any create call.
8. Calling `workbridge_get_context` from a per-work-item session returns the caller's `id`, `kind`, `parent_id`, `breadcrumb`, and `ancestor_epic_ids`.
9. Calling `workbridge_get_context` from the global-assistant session returns the existing fields, with the new fields absent or null.

### User-side

10. When an agent successfully creates a work item, a toast appears with the correct breadcrumb, and the new row shows a `NEW` marker in the list.
11. The `NEW` marker clears on first view (selection or drill-in) and stays cleared across app restarts.
12. No UI surface appears for failed MCP create attempts.
13. Agent-created Epics render with both the `EPIC` badge (from Milestone 1) and the `NEW` marker simultaneously; the two do not visually conflict.

### Contract-side

14. `docs/harness-contract.md` contains new rows/clauses for the new/extended tools, with every adapter column populated.
15. `docs/work-item-hierarchy.md` has a "Programmatic creation via MCP" section.
16. The UI-conventions doc codifies the externally-originated item marker.

---

## 14. Open questions / notes for the implementer

These are deliberately left open; the plan is intentionally abstract where implementation details depend on codebase state at the time.

- **Tool naming.** If there is an existing create tool for the global assistant, extend it rather than introduce a parallel tool. If not, pick a name consistent with the workbridge MCP namespace (e.g. `workbridge_create_work_item`, `workbridge_list_work_items`).
- **Schema format.** Follow whatever schema convention the existing workbridge MCP tools use. The fields listed in Sections 5-7 are the semantic contract; the exact JSON/schema shape is the implementer's call.
- **Persistence of the "viewed" flag.** The `NEW` marker needs to persist across restarts. If the existing work-item state file is a natural place for this, use it; otherwise a companion state file is fine. The key is that killing and restarting the TUI does not re-surface markers on items the user already saw.
- **How the global assistant learns about Milestone 1's parent field.** If the existing global-assistant create tool is deployed somewhere out-of-band, its schema update needs to ship alongside this PR.
