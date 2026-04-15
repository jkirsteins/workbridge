# Work Items

A work item is WorkBridge's central abstraction. It represents one unit of
in-progress software development work.

## Definition

A work item is anchored by a persistent backend record (a local JSON file
in v1) and enriched with derived metadata from git and GitHub. It combines:

- **A backend record** (mandatory, provides identity, title, status, repo associations)
- **A local git worktree** (optional, matched by branch name)
- **A GitHub issue** (optional, derived from branch name)
- **A GitHub pull request** (optional, derived from branch-to-PR lookup)

These components are called "puzzle pieces." The backend record is always
present. The worktree, issue, and PR are discovered automatically and
attached when found.

## Puzzle Pieces

### Backend Record (mandatory)

The backend record anchors the work item's identity. It is a local JSON file
(in v1) that stores:

- Work item ID (file path)
- Title
- Status (Backlog, Planning, Implementing, Blocked, or Review)
- Repo associations (repo path + optional branch name + optional PR identity snapshot)
- done_at (optional Unix timestamp, set when the item enters Done state)

The UI can render a work item list immediately from backend records alone,
before any git or GitHub data is fetched.

### Worktree (optional)

Matched by branch name from the backend record's repo associations. When a
worktree exists, WorkBridge has:

- A path on disk (where the code lives)
- A branch name (the identity of the work)
- A location to spawn a Claude Code session

Note: git dirty/clean status and ahead/behind counts are defined in the
data model (GitState) but currently hardcoded to false/0/0. Real git state
derivation is planned but not yet implemented.

### GitHub Issue (optional)

Linked when the branch name matches the issue pattern (default: starts with
a number). For example, `42-resize-bug` links to issue #42. The issue gives
WorkBridge:

- Issue title, state (open/closed)
- Labels

If the branch name has no matching number, there is no issue. This is fine -
the work item simply has no issue piece.

### Pull Request (optional)

Discovered by querying GitHub for open PRs whose head branch matches the
work item's branch. The PR gives WorkBridge:

- PR number, title, state
- Draft status
- Review decision (approved, changes requested, etc.)
- CI check status
- URL

If no live PR exists for the branch but the backend record is Done and has
a persisted PR identity snapshot (saved at merge time), assembly synthesizes
a PrInfo with PrState::Merged from the snapshot. This ensures Done items
continue displaying their PR link after the branch/PR is cleaned up.

For Done items that were merged before `pr_identity` persistence existed,
a one-time startup backfill queries GitHub for merged PRs and populates
the snapshot. This migration code (in `salsa.rs` / `app.rs`) can be
removed once no Done items with `pr_identity=None` remain on disk.

If no live PR exists and no persisted PR identity applies, there is no PR
piece.

## Work Item Kind

Each work item has a `WorkItemKind` that determines its workflow:

- **Own** (default) - The user's own work. Follows the full six-stage
  workflow: Backlog -> Planning -> Implementing -> Blocked -> Review -> Done.
  Created when importing an unlinked PR or creating a new work item from
  scratch.

- **ReviewRequest** - A PR where the user was requested as a reviewer.
  Restricted to a two-stage workflow: Review -> Done. These items appear
  in the "REVIEW REQUESTS" group in the sidebar with an "[RR]" badge.

### Review request behavior

Review-requested PRs are discovered via the `review-requested:@me` GitHub
search filter and displayed in a dedicated "REVIEW REQUESTS" section in the
sidebar. Before import, they show an "R" prefix.

Pressing Enter on a review request imports it directly into the Review stage
(not Backlog), since reviewing is the only meaningful action. A worktree is
created for the reviewer to inspect the code.

#### Reviewer identity display

Each review-request row shows a compact reviewer badge at the right edge of
the list row, after the `PR#N [draft]` stack:

- `[you]` - the current GitHub user is directly listed in the PR's
  requested reviewers. Direct request wins over team request: if the PR
  also requested a team the user belongs to, the row still renders as
  `[you]`.
- `[team-slug]` - a single team was requested (no direct user request).
- `[team-slug +N]` - multiple teams were requested. The first team is
  named; N is the count of remaining teams.

No badge renders when the fetch returned no reviewer identity at all
(degenerate case - `gh` should never return such a row from the
`review-requested:@me` search).

The right-side detail panel for a review-request row shows an authoritative
`Requested from:` line listing every reviewer identity in full - no
truncation. Users appear as `you`, teams appear as `team <slug>`, joined
with `, `. Unlinked-PR detail panels do not show this line.

Direct-to-you rows sort to the top of the REVIEW REQUESTS block so the
most actionable reviews are always surfaced first. Within each bucket
(direct, then team) the order returned by `gh` is preserved via a stable
sort.

Title wrapping on review-request rows uses the same `wrap_two_widths`
helper as regular work-item rows: the first line reserves space for the
PR/draft/reviewer-badge stack; continuation lines wrap to the full panel
width (minus the `R ` marker indent).

The current user's GitHub login is resolved once per fetcher session via
`gh api user` and cached inside `GhCliClient`. Every fetch tick reads the
cache instead of re-shelling. When the lookup has not yet succeeded
(first tick hasn't arrived, or `gh` is missing / unauthenticated), the UI
degrades by classifying every row as team: no row is falsely promoted to
`[you]`, the sort order falls back to the `gh` order, and the
`Requested from:` line omits the `you` entry.

Stage restrictions for ReviewRequest items:

- **advance_stage**: All manual stage advancement is blocked. Review
  requests are completed via the approve/request-changes MCP tools, not
  manual stage advancement.
- **retreat_stage**: Always blocked. There is no valid previous stage for
  a review request in Review.
- **MCP status transitions**: `workbridge_set_status` is blocked. Claude
  sessions should not drive workflow for someone else's PR.
- **MCP review tools**: `workbridge_approve_review` and
  `workbridge_request_changes` are available only for ReviewRequest items.
  These submit a GitHub PR review via `gh pr review` and auto-move the
  item to Done on success. The MCP tools/call handler enforces the
  ReviewRequest kind check server-side.
- **Auto-close on external merge**: ReviewRequest items in Review are
  polled every 30 seconds via `gh pr view <target> --repo
  <owner/repo> --json state,number,title,url`. The ReviewRequest
  poller (`App::poll_review_request_merges` /
  `App::reconstruct_review_request_merge_watches`) and the Mergequeue
  poller (`App::poll_mergequeue` /
  `App::reconstruct_mergequeue_watches`) are generated from a single
  pair of macros (`impl_pr_merge_poll_method!` +
  `impl_pr_merge_reconstruct_method!` in `src/app.rs`) so the
  subprocess path, the 30 s cooldown, the Phase 1 drain, the
  still-eligible guard, the `pr_number` backfill, and the merge-gate
  dispatch cannot drift between the two stages. Per-stage deltas
  (source stage, kind filter, strategy tag, status messages, whether
  the merged branch runs `cleanup_worktree_for_item`) are passed as
  macro arguments. When the PR is detected as merged, the item
  auto-transitions to Done through the merge-gate invariant (`source
  == "pr_merge"`) and the merged PR's identity is persisted to
  `pr_identity` so the assembly fallback keeps the merged-PR link
  visible afterwards. The Claude review session is killed by the
  transition (same behavior as every other Review -> Done transition in
  the codebase); the worktree is left on disk and cleaned up later by
  auto-archive (default 7 days) or immediately by the user with Ctrl+D.
  A closed-without-merge PR is NOT auto-closed (it would bypass the
  merge-gate invariant) - a warning is surfaced instead and the user
  can Ctrl+D to clean up. `gh pr view` failures are stored in the
  detail pane as "Last poll error: ..." and persist across ticks
  instead of vanishing with the transient status message. This poll
  is the *only* code path that can observe a merged review-request
  PR: the `gh pr list --author @me` fetch cannot see it (wrong
  author), the `review-requested:@me` search is `--state open` (wrong
  state), the `pr_identity` fallback is `Done`-only (wrong status),
  and the startup merged-PR backfill is also author-filtered. Without
  this poll, a ReviewRequest item whose PR was merged outside the
  TUI would stay stuck in `[RR][RV]` forever.

### Re-open on re-request

When a reviewer's review request is re-requested on a PR that already has
a completed (Done) ReviewRequest work item, the item is automatically
re-opened back to Review during reassembly. This handles the case where
a PR author pushes changes and re-requests review after an initial review.

To avoid false re-opens from stale GitHub data, recently-submitted review
items are suppressed from re-open detection until fresh data arrives from
the next GitHub fetch cycle.

## Quick-Start Flow

Pressing Ctrl+N starts a quick-start session. When exactly one managed repo
is configured no dialog is shown at all - a Planning work item is created
immediately with a placeholder title ("Quick start") and a session is
spawned at once.

The Claude agent running in this session uses the `planning_quickstart` system
prompt, which instructs it to:
1. Ask the user what they want to work on.
2. Call `workbridge_set_title` via MCP once the task is understood.
3. Proceed through the normal Phase 1 refinement and Phase 2 planning process,
   ending with a `workbridge_set_plan` call.

The title update via MCP is reflected immediately in the left panel. After
the first session sets a real title, any subsequent Planning
session for the same item uses the normal `planning` prompt.

Ctrl+B opens the full creation dialog (title, description, repos, branch) and
creates a Backlog item, matching the previous Ctrl+N behavior.

Repo selection for quick-start follows this priority:
1. The only managed repo with a git directory, if exactly one exists - no
   dialog is shown.
2. Otherwise, a compact "Quick start - select repo" dialog opens. It
   contains only the repo list (no Title, Description, or Branch fields):
   focus lands directly on the list, Up/Down moves the cursor, Space
   picks the cursor row as the selected repo (single-select - any
   previously checked row is cleared so at most one `[x]` is ever
   visible), Enter creates the quick-start item with the same
   placeholder title and auto-generated branch as the single-repo path,
   and Esc cancels. The agent renames the title later via
   `workbridge_set_title`, exactly as in the single-repo flow.

   Single-select is enforced because the quick-start submission path
   only ever spawns a session for a single repo; allowing multi-check
   would silently discard every choice except the first.

The current working directory is deliberately not used to auto-select a
repo: when more than one managed repo is configured there is a real choice
to make, and the user wants to pick explicitly every time rather than have
Ctrl+N silently latch onto whichever repo they happened to launch from.

## Global Assistant Transfer

The global assistant (Ctrl+G) can create work items via the
`workbridge_create_work_item` MCP tool. This allows the user to explore code
and ideas in the global assistant, then transfer that exploration context into
a proper work item for later action.

When Claude calls `workbridge_create_work_item`, it provides:
- A concise title summarizing the work
- A description capturing the exploration context and findings
- The target repo path (must be a managed repo)

The main thread handles the event by:
1. Validating the repo path against the active repo cache
2. Generating a branch name (`{username}/workitem-{suffix}`)
3. Creating the work item in Planning status
4. Closing the global drawer
5. Spawning a planning session for the new work item

The planning session receives the description in its system prompt, so the
exploration context from the global assistant carries forward into the
planning phase.

## Work Item Status

Work items follow a seven-stage workflow:

- **Backlog** - Work has been identified but not started. (Stored as "Backlog" in the backend; legacy "Todo" values are accepted via serde alias.)
- **Planning** - A Claude session produces an implementation plan. Advancing to Implementing requires the plan to be set via `workbridge_set_plan`; manual advance is blocked.
- **Implementing** - Active development. A Claude session works on the code.
- **Blocked** - The implementation is stuck and needs user input. Can move back to Implementing when unblocked.
- **Review** - Implementation is complete and under review. Entering Review from Implementing or Blocked triggers a review gate (async plan-vs-implementation check) and auto-creates a PR.
- **Mergequeue** - Waiting for a PR to be merged externally (e.g., by a CI merge queue, another person, or manual merge outside the TUI). The TUI polls the PR state every 30 seconds and auto-transitions to Done when the PR is detected as merged. Can retreat back to Review.
- **Done** - Work is finished. This status is derived, not directly settable (see below).

### Status transitions

Most forward transitions are triggered by the user via TUI keybinds (advance/retreat). Claude sessions can request a limited set of transitions via the `workbridge_set_status` MCP tool:

- Implementing -> Review (routed through the review gate)
- Implementing -> Blocked
- Blocked -> Implementing
- Blocked -> Review (routed through the review gate)
- Planning -> Implementing

All other transitions must go through TUI keybinds.

### Branch invariant at Backlog -> Planning

A work item may not leave Backlog unless at least one of its repo
associations carries a branch name. `App::advance_stage` enforces this
at the top of the function: when the source status is Backlog and no
association has `branch.is_some()`, the stage change is refused and the
"Set branch name" recovery modal opens instead. Confirming the modal
persists the branch via `WorkItemBackend::update_branch` and re-drives
the same transition. The same modal is opened from `spawn_session`
when the user presses Enter on a Planning/Implementing item whose
repo associations all have `branch.is_none()`, recovering any work
item that reached Planning or later without a branch (e.g. items
created by a now-removed Backlog creation path that stored
`branch: None`). See `docs/UI.md` "Set branch recovery dialog" for the
UI contract.

Claude sessions can also delete the current work item via the `workbridge_delete` MCP tool, available for all non-read-only sessions (both regular work items and review requests). The backend record is deleted and the session is killed immediately on the main thread. Resource cleanup (worktree removal, branch deletion, PR closure) runs asynchronously on a background thread to avoid blocking the UI. Force mode is always used (no interactive dirty-worktree confirmation). See docs/CLEANUP.md for the deletion phases.

### Review gate

When a work item transitions from Implementing or Blocked to Review (whether
user- or MCP-initiated), a review gate runs asynchronously in three phases:

1. **PR existence check** - if the repo has a GitHub remote, the gate verifies
   a pull request exists for the branch. If no PR is found, the gate rejects
   with a message asking the implementer to create one. Repos with no GitHub
   remote skip this phase entirely.

2. **CI check wait** - if the PR has CI checks configured (status check rollup
   is not empty), the gate polls `gh pr checks` every 15 seconds until all
   checks complete. Progress is shown in the right panel (e.g. "2 / 5 CI
   checks green"). If any check fails, the gate rejects immediately with the
   names of the failed checks. If no checks are configured, this phase is
   skipped.

3. **Adversarial code review** - spawns a `claude --print` session with MCP
   access to fetch the plan (via `workbridge_get_plan`) and run `git diff`
   itself, then compares the plan against the implementation. If no plan
   exists, the gate is blocked before it can start. During this phase, Claude
   reports live progress via the `workbridge_report_progress` MCP tool (e.g.
   "Reviewing 8 changed files against plan", "Found 3 potential issues,
   verifying..."). These messages are shown in the right panel.

If the gate approves, the work item advances to Review. If it rejects (at any
phase), the rejection reason is fed back to the implementing Claude session as
rework feedback.

While the gate is running, the work item stays in Implementing (or Blocked) on
the model, but the work-item list row surfaces the substate as a yellow+bold
`[RG]` badge inserted immediately to the right of the stage badge (e.g.
`[IM][RG]` or `[BK][RG]`, and `[RR][IM][RG]` for review-request kind). The
badge is derived from `app.review_gates.contains_key(&wi.id)`, appears the
instant the gate spawns, and disappears the instant `drop_review_gate` removes
the entry on approve / reject / retreat / delete. This lets users tell at a
glance that a row is sitting at the gate without opening the right panel,
since the cyan braille spinner in the left margin is shared with "Claude
actively coding" and on its own is ambiguous. See docs/UI.md "Layout: Flat
List Mode" for the full list row anatomy.

The skill (slash command) used in phase 3 is configurable via
`defaults.review_skill` in `config.toml` (default: `/claude-adversarial-review`).
It can also be edited from the Settings overlay's "Review Gate" tab (press `?`
then Tab to the Review Gate tab, Enter to edit, Enter to save, Esc to cancel).

### Merge gate

Advancing from Review to Done is gated by PR merge. Instead of directly changing status, the user is prompted to choose a merge strategy (squash, merge, or poll). The TUI spawns an async `gh pr merge` command for squash/merge. Done is reached only after GitHub confirms the PR was merged.

If any prerequisite is missing - no repo association, no branch, no GitHub remote, or no open PR - the merge is blocked with an error message and the item stays in Review. Done cannot be set directly via MCP either; it always requires the merge gate.

### Mergequeue (poll strategy)

When the user selects "poll" at the merge prompt, the work item transitions to the Mergequeue state instead of attempting an immediate merge. This is for PRs that can't be merged directly from the TUI - for example, PRs that go through a CI merge queue, require approvals from others, or need to be merged by someone else.

In the Mergequeue state:
- The TUI polls the PR state via `gh pr view <target> --repo <owner/repo> --json state,number,title,url` every 30 seconds, where `<target>` is the PR number when known and the branch name as a fallback. While a poll is in flight, a "Polling PR for merge (<branch>)" activity indicator is shown at the bottom of the screen. `enter_mergequeue` pins `pr_number` on the in-memory watch from `assoc.pr.number` immediately, so the live-entry path always targets the exact PR unambiguously. On app restart, the rebuilt watch starts with `pr_number = None` and falls back to `gh pr view <branch>` for the first poll, then writes the resolved number back onto the watch so subsequent polls are pinned.
- When the PR is detected as merged, the item auto-transitions to Done (via the `"pr_merge"` source, satisfying the merge-gate invariant). The merged PR's identity is persisted into `pr_identity` at this point so the Done item retains its merged-PR link in the UI after the branch is cleaned up.
- If the PR is closed without merging, a warning is shown but the item stays in Mergequeue.
- If `gh pr view` itself fails (auth error, network error, etc.), the error is stored on the work item and shown in the right-side detail pane as "Last poll error: ...". It persists across ticks until the next successful poll, so users do not miss failures when the transient `status_message` gets overwritten.
- The user can retreat back to Review via Shift+Left at any time. This stops polling and clears the watch and any stored poll error.
- The right-side detail pane shows the full PR URL and a multi-line hint: "Waiting for PR to be merged. Polling GitHub every 30s. Shift+Left to move back to Review and stop polling."
- No Claude session runs in this state.
- In the board view, Mergequeue items appear in the Review column with a `[MQ]` prefix.
- On app restart, `reconstruct_mergequeue_watches` rebuilds a watch for every backend record with Mergequeue status, using the record's branch and the resolved GitHub remote. Nothing new has to be persisted at `enter_mergequeue` time, so existing Mergequeue tickets (created before this mechanism existed) resume polling correctly on next launch, even if their PR was merged while the app was closed.

### Derived Done status

During assembly, if any repo association has a merged PR (`PrState::Merged`), the work item's status is set to Done regardless of what the backend record says. This is marked as a derived status (`status_derived = true`). When the status is derived, manual stage transitions (advance/retreat) and MCP transitions are blocked.

This includes synthetic PrInfo produced by the PR identity fallback: when a backend record is Done and has a persisted PR identity snapshot but no live PR, assembly injects a PrInfo with PrState::Merged. The derived-Done logic then fires on this synthetic PR, setting `status_derived = true`. Because the fallback only activates when the backend record is already Done, non-Done items are never forced into derived-Done by a stale snapshot.

A parallel poll exists for ReviewRequest items in Review (see "Auto-close on external merge" under "Review request behavior"). When that poll detects an external merge, the item is advanced to Done via `apply_stage_change` with `source == "pr_merge"` *and* the merged PR's identity is persisted into `pr_identity`. On the next reassembly the same `pr_identity` fallback fires, so the item is reached from both directions: the backend record status is already Done (from the `apply_stage_change`) and the derived-Done logic re-confirms it from the synthetic `PrInfo { state: Merged }`. The net effect is that reviewer-side merges and author-side Mergequeue merges reach the same end state (Done, with `status_derived = true` and a stable merged-PR link in the UI).

### Auto-archive of Done items

Done work items are automatically deleted after a configurable retention period.
The `archive_after_days` config setting (default 7, 0 disables) controls how
long a Done item is kept before cleanup.

The archival clock starts when `done_at` is set on the backend record. This
happens in two cases:

- **Explicit Done** (merge gate): `apply_stage_change` sets `done_at` when the
  item transitions to Done via the merge gate.
- **Derived Done** (merged PR detected during reassembly): if reassembly finds
  a merged PR and derives Done status, it sets `done_at` on the backend record
  if not already present.

If the item retreats from Done (e.g., re-open on re-request for review items),
`done_at` is cleared.

Auto-archive runs during reassembly, after re-open detection. This ordering
ensures that review requests re-opened in the current cycle have their
`done_at` cleared before auto-archive evaluates them. Any record with a
`done_at` timestamp that exceeds the retention period is deleted. The archive
condition checks `done_at` directly - not the backend status field - so both
explicitly-Done and derived-Done items are archived correctly.

Auto-archive skips resource cleanup (steps 4-6: worktree removal, branch
deletion, PR closing) since Done items have already been through the merge
flow. The backend record is deleted, sessions are killed, in-flight
operations are cancelled, and in-memory state is cleared.

## Sessions

Each work item may have associated PTY sessions running inside its worktree.
There are two session types:

- **Claude Code session** - the interactive Claude Code process where the user
  does the actual work. Spawned automatically when entering certain stages.
- **Terminal session** - a shell (`$SHELL`, falling back to `/bin/sh`) launched
  in the worktree directory. Spawned lazily when the user switches to the
  Terminal tab in the right panel. Available whenever the work item has a
  worktree, regardless of whether a Claude Code session exists.

Session states (both types):

- **Alive**: The process is running.
- **Dead**: The process has exited. The worktree still exists.

A dead session does not destroy the work item. The worktree persists, and
the session can be respawned. Dead terminal sessions are automatically
cleaned up and respawned when the user switches to the Terminal tab again.
Only deleting the backend record destroys the work item.

## Work Item Identity

A work item is identified by its backend record ID (a file path in v1).
Backend records define what work items exist. Derived data (worktrees, PRs,
issues) is assembled on top.

This means:

- One work item can span multiple repos (via multiple repo associations).
- One issue can be referenced by multiple work items (different branches,
  different worktrees). A future aggregation view could group them.
- One branch can never have multiple worktrees on the same machine,
  because git prohibits two worktrees on the same branch.

### Backward compatibility with records missing `id`

Records created before the `id` field was added are accepted via the
same `#[serde(default)]` mechanism as the other migrated fields
(`description`, `kind`, `display_id`, `plan`, `done_at`): the `id`
field defaults to a placeholder `LocalFile(PathBuf::new())` produced
by `placeholder_work_item_id()`, and both `list()` and `read()`
immediately overwrite `record.id` with `LocalFile(<file path>)` -
the real on-disk path - right after `serde_json::from_str` returns.
The placeholder therefore never escapes the backend layer. Legacy
files on disk are not rewritten on load; they keep their old shape
until the next modify-write through the normal `modify_record` path,
at which point the current serializer naturally includes the `id`
field. Records with a *present-but-malformed* `id` value (e.g. a
bare string instead of a tagged enum) still fail strict
deserialization and surface as `CorruptRecord`, as do other parse
failures (malformed JSON, missing `title`/`status`, etc.).

## Display IDs

Every work item also carries a backend-provided `display_id: Option<String>`,
a short, human-readable, stable identifier that can be referenced outside
the TUI (commits, PRs, chat messages, grep). This is a separate field from
the internal `WorkItemId` (which is a file path in v1 and thus not
shareable or easy to type); the display ID is what users see and quote.

### Local backend format

The local file backend generates IDs in the form `<repo-slug>-<N>`, where:

- `<repo-slug>` is the final path component of the first repo
  association, using the exact same `repo_slug_from_path` helper that
  drives the work item list's group headers. A repo at
  `/Projects/workbridge` gets the slug `workbridge`, matching what the
  group header shows (`ACTIVE (workbridge)`). The two cannot drift.
- `<N>` is a monotonically increasing per-slug counter that starts at 1.

Example IDs: `workbridge-1`, `workbridge-42`, `other-repo-3`.

### No reuse on delete

Numbers are never reused within a backend instance. Deleting a work item
leaves a permanent gap in its slug's sequence: if you have
`workbridge-1`, `workbridge-2`, `workbridge-3`, delete `workbridge-2`,
then create a new item, the new item is `workbridge-4`, not `-2`. This
preserves the invariant that any reference to a `#<slug>-<N>` is
unambiguous even if the original item has been deleted since.

### Counter persistence

The counter is persisted in `{data_dir}/id-counters.json` as a flat JSON
map of slug to highest-ever-N:

```json
{
  "workbridge": 42,
  "other-repo": 3
}
```

The file stores the high-water mark rather than the next-to-assign, so
the invariant is trivial to read off the file: the next ID for a slug is
always `highest + 1`. Deleting items never touches the counter. Creating
a new item reads the file, increments the slug's entry, writes the file
atomically (via the same `atomic_write` helper used for record JSONs),
and returns `format!("{slug}-{next}")`. Concurrent `create()` calls are
serialized by a `counter_lock: Mutex<()>` field on `LocalFileBackend`.

A missing or corrupt `id-counters.json` is tolerated: the backend logs
a warning and starts with empty counters. Under normal operation the
next save rewrites the file from scratch, so the invariant holds
against anything short of manual file tampering.

### No backfill for pre-existing records

Records created before this feature landed do not carry a `display_id`.
Their on-disk JSON is missing the field entirely; serde deserializes
them with `display_id: None` via `#[serde(default)]`. These items
render in the work item list without an ID subtitle line. They are
deliberately not backfilled, because backfilling would either reuse
numbers (breaking the no-reuse invariant) or assign new numbers that
don't match anything a user or PR already references.

### Display in the work item list

Items with a `display_id` render the ID as a dimmed `#<slug>-<N>`
subtitle line under the title, styled with the same `meta_style` as the
branch subtitle (same muted/selected-highlight color rules). The ID
line sits above the branch line and below any title continuation
lines. Items without a `display_id` (legacy records) skip the line
entirely - row heights are variable, which the list rendering already
supports for title and branch wrap.

## Deleting Work Items

Deleting a work item (Ctrl+D/Delete in the TUI) performs comprehensive cleanup
of all associated resources. The cleanup is best-effort: failures produce
warnings but do not prevent the delete.

### Resources cleaned up

- Backend record (the JSON file)
- Worktree directory on disk
- Local git branch (force-deleted with `-D`)
- Open PR on GitHub (closed via `gh pr close`)
- Active Claude Code session (killed)
- Active terminal session (killed)
- MCP socket server
- In-memory state: rework reasons, review gate findings, no-plan prompt queue,
  merge/rework prompt visibility flags

### Resources preserved

- **Activity log (the .jsonl file)**: moved to
  `<data_dir>/work-items/archive/activity-<id>.jsonl` instead of being
  deleted. The metrics Dashboard reads both the active directory and
  the archive subdirectory, so historical flow events (created, stage
  changes, pr_merged, done) from deleted items still contribute to
  throughput, cycle time, backlog reconstruction, and the
  Created/Backlog sparklines. See `docs/metrics.md` for the aggregator
  behavior and `docs/CLEANUP.md` for the authoritative delete flow.

### Confirmation flow

Ctrl+D/Delete opens a confirmation modal titled "Delete '<title>'?".
The modal body warns that any uncommitted changes in the worktree
will be lost. Pressing `y` (or `Y`) confirms and spawns the
background cleanup thread; pressing `Esc` cancels and closes the
modal without touching anything. All other keys are swallowed while
the modal is visible so stray keystrokes cannot leak into the PTY
session below.

There is no "dirty detection" step: the modal unconditionally warns
about uncommitted changes and the background cleanup always passes
`force=true` to `remove_worktree`, so the UI thread never needs to
shell out to `git status --porcelain`. See `docs/CLEANUP.md` for the
authoritative description of the cleanup path and the background
thread contract.

### Backend-specific cleanup

The `WorkItemBackend` trait provides a `pre_delete_cleanup()` hook called before
the record is deleted. The default implementation is a no-op. Future backends
(GithubIssueBackend, GithubProjectBackend) can override this to close backing
issues or archive project items.

### In-flight operation handling

If worktree creation is in progress for the deleted item, the result is drained
and any orphaned worktree is cleaned up. If PR creation is in progress, it is
cancelled. Pending PR creation queue entries for the deleted item are removed.

## What a Work Item Is NOT

- It is not a task tracker entry. There is no "assigned to," "due date,"
  or "priority" beyond what the linked issue provides.
- It is not shared between machines. Each machine has its own backend
  records and assembles its own view.
- It is not manually configured per-puzzle-piece. The branch-to-issue
  linkage and branch-to-PR lookup are automatic.
