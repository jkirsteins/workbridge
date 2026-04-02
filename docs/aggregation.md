# Aggregation

> STATUS: NOT IMPLEMENTED. This document describes the target design.

Work items are flat by default: one list, one item per worktree.
Aggregation provides alternate lenses over the same list without changing
the underlying data model.

## Why Aggregation

A developer might have 8 work items across 3 repos. Some share an issue
(follow-up work on the same bug). Some are related by repo. The flat list
answers "what worktrees do I have?" but not "what's the full picture on
issue #42?" or "what am I doing in the backend repo?"

Aggregation regroups the flat list by a derived key. No data changes. The
same work items appear, just nested under headings.

## Aggregation Modes

### Flat (default)

No grouping. Work items listed by sort order.

```
42-resize-bug          #42 bug     PR#15 review
refactor-backend                   PR#14 approved
42-followup            #42 bug     PR#18 draft
112-fix-auth           #112        PR#88 draft
```

### By Issue

Work items grouped by their linked issue. Items without an issue appear
under a separate heading.

```
workbridge#42 - Session output garbled after resize
  42-resize-bug        PR#15 review
  42-followup          PR#18 draft

backend-api#112 - Fix auth token expiry
  112-fix-auth         PR#88 draft

(no issue)
  refactor-backend     PR#14 approved
```

This is the view that answers "what's the full status of issue #42?"
It reveals when an issue has multiple in-flight branches.

### By Repository

Work items grouped by their parent repository.

```
workbridge (3 items)
  42-resize-bug        #42 bug     PR#15 review
  refactor-backend                 PR#14 approved
  42-followup          #42 bug     PR#18 draft

backend-api (1 item)
  112-fix-auth         #112        PR#88 draft
```

This is the natural view for developers who context-switch between
projects and want to see all work in one repo together.

### By Status

Work items grouped by their composite status.

```
Active (2)
  42-resize-bug        #42 bug     PR#15 review
  112-fix-auth         #112        PR#88 draft

Approved (1)
  refactor-backend                 PR#14 approved

Draft (1)
  42-followup          #42 bug     PR#18 draft
```

This view answers "what needs my attention right now?"

## Sorting Within Groups

Within each aggregation group, work items are sorted by a secondary key.
The default secondary sort is by status priority (active first, then
in-review, then draft, then idle/dead).

The user can change the sort order independently of the aggregation mode.

## Aggregation is a View Concern

Aggregation does not affect the data model. There is no "issue entity"
or "repo entity" that owns work items. The grouping key is derived on
the fly from each work item's assembled data:

- By issue: the issue number extracted from the branch name
- By repo: the repo path from the worktree's parent
- By status: the composite status from data assembly

If two work items share issue #42, they appear under the same heading.
But there is no #42 object that persists or has its own state.

## Open Questions

- Should aggregation state persist across restarts? Current stance: no,
  default to flat view on startup. But if the user always uses "by repo"
  view, having to switch every time is friction.

- Are there other useful aggregation keys? By reviewer, by label, by
  last-active-time-bucket ("today", "this week", "older")? These are
  derivable but add UI complexity.

- Should the inbox items appear inside aggregation groups? For example,
  a remote-only PR for branch 42-fixup could appear under the issue #42
  group. This blurs the line between work items and inbox items but
  provides a more complete picture per-issue.
