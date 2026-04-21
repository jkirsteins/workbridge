# Workbridge Workflow Design (Draft)

Date: 2026-04-03
Status: Draft - captured from design interview

> **Note (2026-04-15)**: This draft uses "Claude" as shorthand for the
> running coding agent because that was the only adapter at the time of
> writing. The actual wiring is now pluggable via the `AgentBackend`
> trait in `crate::agent_backend`; the authoritative contract lives in
> `docs/harness-contract.md`. Read "Claude" in this draft as "the
> configured agent backend" - today that's `ClaudeCodeBackend`, but a
> second adapter (e.g. Codex) would inherit the same workflow.

---

## 1. Overview

Workbridge work items represent pieces of work inside one or more repos. Work progresses through multiple stages (like a Jira board), with deep coding agent integration (Claude Code, Codex, and future adapters) at each stage. The TUI hosts an MCP server that serves as the communication backbone between agent sessions and the application.

---

## 2. Stages

```
Backlog --> Planning --> Implementing --> Review --> Done
                             |
                             v
                          BLOCKED
                          (returns to Implementing)
```

### 2.1 Backlog

- Raw idea. Keywords/title only, no plan fleshed out.
- No agent session.
- User creates items manually (no auto-import from GitHub for MVP).
- Must have 1+ repo association from creation (data model invariant).

### 2.2 Planning

- Refine the plan by discussing with Claude interactively.
- User opens a agent session (presses Enter) and drives the conversation.
- Claude logs decisions and reasoning to the activity log via MCP.
- Plan is passed to the backend for storage. Backend decides where to store:
  - Local backend: stored in work item JSON
  - GitHub Issues backend: posted to PR description or issue body
- User manually advances to Implementing when satisfied with the plan (MVP).
- Future: configurable approval gates (e.g. invoke a skill, require someone to approve plan via PR, etc.).

### 2.3 Implementing

- Claude works autonomously from the approved plan.
- When user approves plan and advances to Implementing, the system auto-spawns a agent session with the plan as context.
- User can watch live PTY output (current model).
- User can type directly into the PTY to give guidance mid-work (current model).
- Concurrency: 1 actively implementing item at a time (default). Configurable limit. Queued items show position: `[IM:Q1]`, `[IM:Q2]`.
- When the active item finishes, the next queued item auto-starts.

### 2.4 BLOCKED

- Claude signals it needs user input via MCP tool: `set_status(item_id, "BLOCKED", "reason")`.
- TUI updates the badge and notifies the user.
- User navigates to the item, reads Claude's question, types a response into the PTY.
- Claude reads the response, calls MCP to unblock: `set_status(item_id, "Implementing")`.
- Item returns to Implementing stage.

### 2.5 Review

- Claude signals it is done implementing and requests review via MCP. The TUI handles PR creation as part of the transition. The PR is incidental to the stage change - what matters is Claude signaling it finished.
- A fresh agent session is available for the user to:
  - Address review feedback interactively
  - Run review skills (e.g. adversarial-review, review-loop)
  - Ask for a summary of what happened in prior stages
- User can take over Claude and drive how to address review comments.
- TUI monitors for external reviewer comments and CI status.
- User explicitly approves and advances to Done.

### 2.6 Done

- Work complete, PR merged.
- No active agent session.
- Item visible for a configurable period (e.g. 7 days), then auto-archived.

---

## 3. Stage Transitions

| From | To | Trigger | Automation |
|------|----|---------|------------|
| Backlog | Planning | User: Shift+Right | None |
| Planning | Implementing | User: Shift+Right (confirms plan) | Auto-spawn Claude with plan context |
| Implementing | BLOCKED | Claude: MCP `set_status(BLOCKED)` | Badge update, user notification |
| BLOCKED | Implementing | Claude: MCP `set_status(Implementing)` | Badge update |
| Implementing | Review | Claude: MCP signals done | TUI creates PR, transitions stage |
| Review | Done | User: Shift+Right | None (MVP) |
| Any | Previous | User: Shift+Left | Session cleanup if needed |

### Future transitions (post-MVP)

- Auto-advance Review -> Done when PR merged and CI green
- Configurable approval gates per transition
- Auto-import GitHub issues as Backlog items

---

## 4. Data Model

### 4.1 WorkItemStatus (expanded)

Current: `Todo`, `InProgress`
New: `Backlog`, `Planning`, `Implementing`, `Blocked`, `Review`, `Done`

### 4.2 Invariants

1. Every work item must have 1+ repo association (existing - preserved).
2. Backend determines available repos. E.g. GitHub Issues backend exposes only 1 repo. Local backend can expose all known local repos. This is a fundamental data model constraint.
3. Worktrees must be on named branches, not detached HEAD (existing - preserved).
4. Worktrees must NOT be on default branch (existing - preserved).
5. At most one open PR per branch (existing - preserved).
6. Issue linkage derived from configurable branch name regex (existing - preserved).
7. Transient metadata derives from git/GitHub, never stored (existing - preserved).
8. Unlinked PRs are not work items until imported (existing - preserved).
9. One registered repo = one GitHub remote (existing - preserved).
10. Plan storage is backend-polymorphic. The plan is passed to the backend, and the backend decides where/how to store it.
11. Only N items can be actively implementing at a time (default N=1, configurable). Remaining items are queued with visible position.
12. Done items are visible for a configurable period, then auto-archived.
13. Activity log entries are immutable once written (append-only).
14. Every agent session identifies itself to the MCP server with a work item ID.

### 4.3 Activity Log (new)

Per-work-item persistent log. Hybrid model: structured system events + freeform Claude/user notes.

Stored by the backend (e.g. local backend: `session-{uuid}.json` alongside work item file).

**Structured events** (system-generated):

```json
{ "timestamp": "...", "event_type": "stage_changed", "payload": { "from": "Planning", "to": "Implementing" } }
{ "timestamp": "...", "event_type": "pr_created", "payload": { "number": 42, "url": "..." } }
{ "timestamp": "...", "event_type": "review_received", "payload": { "reviewer": "alice", "decision": "changes_requested" } }
{ "timestamp": "...", "event_type": "ci_status", "payload": { "status": "failing", "url": "..." } }
{ "timestamp": "...", "event_type": "session_started", "payload": { "stage": "Implementing" } }
{ "timestamp": "...", "event_type": "session_ended", "payload": { "stage": "Implementing", "reason": "completed" } }
```

**Freeform notes** (Claude or user via MCP):

```json
{ "timestamp": "...", "event_type": "note", "payload": { "author": "claude", "text": "Decided to use trait-based approach for..." } }
{ "timestamp": "...", "event_type": "note", "payload": { "author": "user", "text": "Changed requirement: also handle X" } }
```

---

## 5. MCP Server

The TUI hosts an MCP server. agent sessions connect to it. Each session identifies itself with a work item ID.

### 5.1 Tools

**Core tools:**

- `get_context(work_item_id)` - Returns: current stage, plan text, work item metadata (title, repos, branches)
- `log_event(work_item_id, event_type, payload)` - Write a structured event or freeform note to the activity log
- `set_status(work_item_id, status, reason?)` - Signal BLOCKED/unblock, or signal done/requesting review. TUI updates work item stage accordingly.

**Query tools:**

- `query_log(work_item_id, filter?)` - Search/filter the activity log. Filter by event_type, date range, keyword.
- `get_review_comments(work_item_id)` - Fetch PR review comments from GitHub
- `get_ci_status(work_item_id)` - Get current CI/check status for the PR

### 5.2 Session Model: Fresh Per Stage

Each stage transition that involves Claude spawns a fresh agent session. Rationale:

- The plan is the handoff contract between stages - forces completeness
- Resilient to crashes - fresh session can pick up from persisted state
- Different stages benefit from different Claude configurations (system prompts, allowed tools)
- Context window stays clean for each stage's actual work
- Aligns with MCP model - each session connects to the same server

**System prompt approach: minimal + on-demand**

- System prompt states: current stage, work item ID, available MCP tools
- Claude must call `get_context()` to learn about the work item
- Claude must call `query_log()` to learn history from prior stages
- This keeps the system prompt small and lets Claude pull only what it needs

### 5.3 Claude Invocation Per Stage

- **Planning**: Interactive session. System prompt explains stage and MCP tools. User drives the conversation.
- **Implementing**: Autonomous session. System prompt + plan text injected. Claude starts working immediately.
- **Review**: Interactive session. System prompt explains stage and MCP tools. User drives.

### 5.4 MCP Activity Tracking

The MCP server keeps track of EVERYTHING happening per work item:

- All tool calls, stage changes, PR events, CI updates, notes
- Stored persistently by the backend (local backend uses a session JSON file with UUID tied to the work item)
- New agent sessions can resume work fresh by querying this log
- This is the "memory" that bridges fresh sessions across stages

---

## 6. UI Design

### 6.1 Two View Modes

Toggled via a global keybind (e.g. Tab) at the root work item overview level.

#### A) Flat List View (default)

```
+-- Work Items (flat + badges) --------+-- Claude Session --------+
|                                       |                          |
|  [BL] Add caching to API             |  [PTY output or          |
|  [PL] Refactor auth middleware        |   placeholder]           |
|  [IM] Fix race condition in fetcher   |                          |
|  [BK] Fix race condition in fetcher   |                          |
|  [RV] Update CI pipeline  PR#42 ok   |                          |
|  [DN] Remove legacy endpoints         |                          |
|                                       |                          |
+---------------------------------------+--------------------------+
| Context bar: title | repo | branch                               |
+------------------------------------------------------------------+
| Status bar                                                       |
+------------------------------------------------------------------+
```

- Each item shows a color-coded stage badge: [BL] [PL] [IM] [BK] [RV] [DN]
- Items in implementation queue show position: [IM:Q2] (2nd in queue)
- BLOCKED items highlighted distinctly (e.g. red/yellow badge)
- PR badge, CI badge, multi-repo indicator (existing) still shown
- Shift+Right / Shift+Left to advance/retreat stages
- Enter to open/focus agent session (spawns if needed for the current stage)

#### B) Board View (Kanban columns)

```
+-- Backlog ----+-- Planning ---+-- Implementing -+-- Review -----+
|               |               |                 |               |
| Add caching   | Refactor auth | Fix race cond.  | Update CI     |
|               |               |   [working...]  |   PR#42 ok    |
|               |               |                 |               |
+---------------+---------------+-----------------+---------------+
| Status bar                                                      |
+-----------------------------------------------------------------+
```

- Full-width layout, no right panel visible
- Traditional vertical Kanban columns
- Done column collapsed/hidden by default
- BLOCKED items shown with distinct styling in the Implementing column
- Navigate: Arrow keys between items and columns
- Shift+Right/Left to move items between stages
- **Enter on an item**: transitions to two-panel layout:
  - Left panel collapses to show only items in the selected item's current stage
  - Right panel shows PTY session
  - Ctrl+] returns to full board view

### 6.2 Right Panel (consistent across views)

- Always shows PTY output from the agent session for the selected work item
- User can type directly into it (forwarded to PTY stdin)
- Shows placeholder when no session is active

### 6.3 Keyboard Shortcuts (new/changed)

| Key | Context | Action |
|-----|---------|--------|
| Tab | Root overview | Toggle flat list / board view |
| Shift+Right | Item selected | Advance to next stage |
| Shift+Left | Item selected | Retreat to previous stage |
| Enter | Item selected | Focus right panel, spawn agent session if needed |
| Ctrl+] | Right panel focused | Return to left panel / board view |
| Ctrl+N | Left panel | Quick-start: create Planning item and spawn Claude immediately |
| Ctrl+B | Left panel | Create new backlog work item |
| Up/Down | Left panel / board | Navigate items |
| Left/Right | Board view | Navigate between columns |

---

## 7. User Journeys

### 7.1 Happy Path: Idea to Done

1. User presses Ctrl+N to quick-start a new session.
2. Item appears with [PL] badge and a agent session spawns immediately in Planning mode.
3. Claude asks the user what they want to work on, and sets the title/description via MCP.
4. User discusses approach with Claude: "We should use Redis with a 5min TTL..."
5. Claude logs decisions to activity log via MCP.
6. Plan is written to the backend.
7. User satisfied, presses Ctrl+] to return to list.
8. User presses Shift+Right -> moves to [IM].
9. System auto-spawns Claude with plan context. Claude starts working.
10. User can watch live PTY output or switch to other items.
11. Claude finishes, signals done via MCP.
12. TUI creates PR, item moves to [RV].
13. User reviews, runs review skills, monitors CI.
14. External reviewer approves. User presses Shift+Right -> [DN].
15. Item visible for 7 days, then archived.

### 7.2 Claude Gets Blocked

1. Item is in [IM], Claude is working.
2. Claude encounters ambiguity: "Should I use async or sync IO?"
3. Claude calls MCP: `set_status(item_id, "BLOCKED", "Need decision: async vs sync IO")`
4. TUI updates badge to [BK], shows reason in context bar or notification.
5. User navigates to item, presses Enter.
6. User types: "Use async, we need non-blocking for the event loop"
7. Claude reads response, calls MCP to unblock.
8. Badge returns to [IM], Claude continues.

### 7.3 Review Rework

1. Item in [RV], PR has "changes requested".
2. User sees review badge, opens agent session.
3. User: "Address the reviewer's comments about error handling"
4. Claude queries review comments via MCP, makes changes, pushes.
5. User monitors for re-review.
6. Reviewer approves, user advances to [DN].

### 7.4 Implementation Queue

1. Item A is actively implementing (1 concurrent limit).
2. User moves Item B to Implementing -> shows [IM:Q1].
3. User moves Item C to Implementing -> shows [IM:Q2].
4. Item A finishes -> Item B auto-starts [IM]. Item C becomes [IM:Q1].

### 7.5 Board View Navigation

1. User presses Tab to switch to board view.
2. Full-width Kanban shows: Backlog | Planning | Implementing | Review columns.
3. User arrow-keys to "Fix race condition" in Implementing column.
4. User presses Enter.
5. View transitions: left panel shows only Implementing items, right panel shows PTY.
6. User interacts with Claude in PTY.
7. User presses Ctrl+] -> returns to full board view.

### 7.6 Asking Claude for Context/History

1. Item is in [RV], new agent session was just spawned.
2. User: "What decisions were made during planning?"
3. Claude calls `query_log(item_id, { event_type: "note", stage: "Planning" })` via MCP.
4. Claude summarizes the planning decisions for the user.

---

## 8. Automation Summary

### What the system automates (MVP):

- Spawning agent session with correct context when entering Implementing
- PR creation when transitioning from Implementing to Review
- Activity log maintenance (all system events logged automatically)
- Queue management for concurrent implementation limit
- CI/review status polling and badge updates
- Done item auto-archival after configurable period
- Session cleanup on stage transitions
- BLOCKED detection via MCP (Claude signals, TUI responds)

### What stays manual (MVP):

- All stage transitions initiated by user (Shift+Arrow)
- Quick-starting new sessions (Ctrl+N) and creating backlog items (Ctrl+B)
- Approving plans (user judgment)
- Final approval to move to Done
- Choosing which review skills to run
- Responding to BLOCKED items

### Future automation (post-MVP):

- Configurable approval gates per stage transition
- Auto-advance Review -> Done when PR merged + CI green
- Auto-import items from GitHub issues
- Configurable concurrent implementation limit via settings UI
- Skill-based approval gates (e.g. run a review skill before allowing Review -> Done)

---

## 9. Open Questions

1. How should the MCP server be discovered by Claude? Options: stdio transport, SSE transport, or a local socket.
2. Should the board view show item counts per column header?
3. What is the exact archive behavior - soft delete, move to hidden file, or separate archive directory?
4. Should BLOCKED items block the queue slot (preventing next queued item from starting)?
5. How should the TUI notify the user of BLOCKED items when they're looking at a different item? (Sound? Flash? Badge color change might be missed.)
