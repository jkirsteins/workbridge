# Data Assembly

WorkBridge assembles work item metadata from backend records, local git
state, and GitHub API calls. Backend records (local files in v1) anchor
work item identity and structure. Transient metadata (PR status, CI checks,
git state) is derived on the fly, cached in memory, and discarded when
the process exits.

## The Derivation Chain

Starting from a backend record, WorkBridge derives everything else:

```
backend record exists (work item id, title, status, repo associations)
  -> for each repo association with a branch:
    -> match worktree by branch name
      -> worktree_path, git state (dirty, ahead/behind)
    -> GitHub API: list PRs with head=branch
      -> PR number, title, state, checks, reviewers, mergeable state
    -> regex extracts issue number from branch name
      -> GitHub API: get issue #N
        -> title, labels, state
  -> for each repo association without a branch:
    -> pre-planning state, no derivation possible
```

Backend records provide identity and structure. Everything else is derived
from git and GitHub. The user creates work items (which creates a backend
record) and sets the branch name. All other metadata is assembled
automatically.

## Data Tiers

Data is grouped into tiers by how it is obtained:

### Tier -1: Backend records (instant, always available)

- Work item id, title, status
- Backend-provided `display_id` (e.g. `workbridge-42`), optional, stable
- Repo associations (repo path, branch name, optional PR identity)

This data comes from local file reads (v1) and is always available. It
defines what work items exist and their structure. The UI can render a
work item list immediately from backend records alone, before any git
or GitHub data is fetched.

`display_id` is copied straight through from the `WorkItemRecord` onto
the assembled `WorkItem` during `reassemble()`. The assembly layer does
not derive it - the backend assigns it at `create()` time and the value
never changes for the life of the record. Records created before the
field existed pass through as `None`. See `docs/work-items.md` "Display
IDs" for the format, uniqueness invariant, and counter-file details.

Repo associations may include a `pr_identity` snapshot (number, title,
url) persisted at merge time. After a PR is merged it leaves the open-PR
list, so the assembly layer uses the persisted identity as a fallback to
keep Done items showing their PR link.

Done items that were merged before `pr_identity` persistence was added
have their identity backfilled at startup: a background thread queries
`gh pr list --state merged --author @me` once per repo and matches
branches. This is a one-time migration - once all Done items have
`pr_identity` on disk, the backfill code can be removed.

### Tier 0: Local git (instant, always available)

- Worktree path and existence (matched by branch name)
- Dirty/clean status (not yet implemented - hardcoded to false)
- Ahead/behind remote tracking branch (not yet implemented - hardcoded to 0/0)

This data comes from local git operations that complete in milliseconds.
It is always available, even offline. Combined with Tier -1 data, the UI
renders the sidebar with work item names and local git status on startup.

Note: the GitState struct exists in the data model and is populated during
assembly, but dirty/ahead/behind values are currently hardcoded placeholders.
Real git state derivation is planned.

### Tier 1: Git remote (fast, usually available)

- Does the remote branch exist?
- Remote tracking status (up to date, behind, ahead, diverged)

This requires network access to the git remote but is a lightweight
operation (ls-remote or fetch --dry-run). Available unless the network
is down or the remote is unreachable.

### Tier 2: GitHub API (async, rate-limited)

- Pull requests for the branch
- PR status, checks, reviewers, mergeable state (conflict detection)
- Issue details, labels, assignees
- Issue state (open/closed)

PR list calls use `--author @me` to filter to the authenticated user's
PRs. This ensures the user's PRs are always returned regardless of how
many total open PRs exist in the repo (the per-call limit of 500 is more
than sufficient for a single user). Unlinked PR discovery also only
shows the user's own PRs.

In addition to the user's own PRs, the fetcher also queries for
review-requested PRs using the `review-requested:@me` GitHub search
filter. These are PRs authored by others where the current user has
been requested as a reviewer.

The `reassemble()` function produces a 4-tuple:
`(work_items, unlinked_prs, review_requested_prs, reopen_ids)`. The
`collect_review_requested_prs()` helper filters out any
review-requested PRs that are already claimed by an existing work item,
so only genuinely untracked review requests appear in the sidebar.
Review-requested PRs are distinct from unlinked PRs - unlinked PRs are
the user's own PRs without a work item, while review-requested PRs are
other people's PRs where the user is a reviewer.

The `reopen_ids` vector contains WorkItemIds for Done ReviewRequest items
whose branch still appears in the review-requested set from GitHub. The
caller (App::reassemble_work_items) uses this to re-open those items back
to Review, handling the case where a PR author re-requests review after
an initial review was completed. A suppression set prevents false re-opens
from stale GitHub data immediately after review submission.

This requires GitHub API access with authentication. Responses take
200-500ms typically. Rate-limited to 5000 requests/hour with a token.

The status bar shows a spinner ("Refreshing GitHub data") while Tier 2
fetches are in progress. The spinner tracks the number of in-flight repo
fetches and only clears when all repos have reported back, so multi-repo
setups show continuous activity for the entire fetch cycle. This provides
visibility into the background refresh cycle, especially during the
initial startup fetch.

### Tier 3: Derived (pure computation)

- Work item composite status (synthesized from all tiers)
- Aggregation group keys
- Display labels and sort order

Computed locally from the assembled data. Recomputed whenever any
upstream tier updates.

## Refresh Strategy

### On Startup

1. Load backend records for all backends (Tier -1, sync, fast local I/O)
2. Render the sidebar immediately with work item titles and statuses
3. Spawn background threads per registered repo for Tier 0, 1, and 2 data
4. As each thread sends results, assemble full work items and re-render

### Periodic Refresh

Tier 2 data is refreshed on a configurable interval (default: 120 seconds).
Each refresh cycle re-queries GitHub for all active work items. Tier 0 and
Tier 1 data is refreshed more frequently (every tick, ~1 second) since it
is cheap.

### Event-Driven Refresh

Certain user actions should trigger an immediate refresh of relevant data:

- User pushes from a session -> refresh PR status for that branch
- User switches to a work item -> refresh all tiers for that item
- User returns to the sidebar after being in a session -> refresh stale items

Event detection can use:

- File system watching on `.git/refs/` (branch pointer changed)
- Tmux output pattern matching (detecting push/pull commands)
- Focus change events (user switched panels)

### Offline Behavior

When GitHub API calls fail (network down, rate limited, auth expired),
WorkBridge continues operating with Tier 0 and Tier 1 data. GitHub-derived
pieces (issue, PR) show a stale indicator or are absent.

The UI should distinguish between "no PR exists" and "couldn't check for
PRs." A missing badge means no PR. A badge with a stale/error indicator
means the data couldn't be refreshed.

## The Override File (planned, not yet implemented)

In rare cases, automatic derivation fails or is wrong. The branch name
might not encode an issue number, or the user wants to associate a branch
with a different issue than what the name suggests.

The planned design is a per-worktree override file at:

```
.git/worktrees/<worktree-name>/workbridge.json
```

For the main worktree (if tracked):

```
.git/workbridge.json
```

This file would contain only the fields that override automatic derivation:

```json
{
  "issue": 42
}
```

Note: with the backend-anchored model, the override file is less
necessary since work item identity and repo associations are stored in
backend records. The override file would remain for edge cases where the
branch name does not encode the correct issue number. It would be optional,
local to the machine, never committed, and cleaned up automatically when
the worktree is removed.

## API Budget

With 5000 GitHub API requests per hour and a 120-second refresh cycle,
the budget per cycle is ~166 requests. Each work item costs approximately:

- 1 request: search PRs by head branch (filtered to `--author @me`)
- 1 request: get issue details (if linked)
- 1 request: get PR check runs (if PR exists)

So ~2-3 requests per work item per cycle. At 166 requests per cycle, the
system supports ~55-80 active work items before approaching rate limits.
This is well beyond typical usage. The `--author @me` filter keeps the
PR list response small regardless of total repo PR count, so even repos
with thousands of open PRs stay within budget.

For users with many repos and many open branches, WorkBridge should
prioritize refreshing work items that are currently visible or recently
active, and deprioritize items that are scrolled off-screen or idle.

## Open Questions

- Should WorkBridge cache Tier 2 data to disk so that restarts don't
  require a full re-fetch? This would make startup faster but adds a
  cache invalidation problem. Current stance: no, in-memory only.
  A full refresh on startup with 10 work items takes ~2 seconds, which
  is acceptable for a TUI.

- Should the derivation chain be extensible? For example, a plugin that
  derives data from Linear or Jira instead of GitHub Issues. This is
  architecturally clean (swap the Tier 2 data source) but adds complexity.
  Defer to v2.

- Should WorkBridge pre-fetch data for unlinked PRs (GitHub PRs without
  a matching work item)? These are discovered as part of the regular PR
  list fetch per repo, so they come "for free." The only cost is
  displaying them in the UI.
