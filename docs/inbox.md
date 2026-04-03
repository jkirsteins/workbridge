# Unlinked PRs

When WorkBridge fetches open PRs from GitHub for registered repositories,
some PRs may not match any work item's repo associations. These are called
"unlinked PRs" and appear in a separate "Unlinked" group in the sidebar.

## How Unlinked PRs Are Identified

During assembly, WorkBridge tracks which (repo_path, branch) pairs are
claimed by work items. Any PR whose branch is not claimed by a work item
is collected as an unlinked PR.

```
for each registered repo:
  fetch open PRs from GitHub

  for each PR:
    if (repo_path, head_branch) is claimed by a work item:
      -> attached to that work item, skip
    else:
      -> unlinked PR, shown in the Unlinked group
```

Common reasons a PR is unlinked:

- The work was started on another machine and no local work item exists
- The local work item was deleted but the PR is still open
- A collaborator opened a PR that has no corresponding work item

## Importing Unlinked PRs

An unlinked PR can be imported into the backend, which creates a new work
item with status InProgress, associates it with the PR's repo and branch,
and (if no local worktree exists) creates a worktree by fetching the branch
from origin.

After import, the PR's (repo_path, branch) is claimed by the new work item
and it no longer appears in the Unlinked group.

Fork PRs (where the PR's repository owner differs from the local repo's
owner) are handled correctly: they appear as unlinked when their branch is
not claimed, and disappear once imported.

## Scope

The Unlinked group only shows PRs from registered repositories. PRs on
repos not registered with WorkBridge do not appear. This is intentional -
WorkBridge cannot create worktrees for repos it does not know about.

## Planned

- Filtering unlinked PRs by review state, repo, or author
- Showing PRs where the user is a reviewer (not the author)
- Showing GitHub Issues assigned to the user that have no branch yet
