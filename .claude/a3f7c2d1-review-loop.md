# Claude Adversarial Review Loop - Work Item Grouping

## Requirements

**User-facing goals:**
1. Work items should be visually grouped by activity level - unlinked PRs, active work, and backlogged items in distinct sections
2. Empty groups should be hidden to avoid clutter

**Implementation goals:**
1. `build_display_list()` partitions work items into UNLINKED, ACTIVE (non-Backlog), and BACKLOGGED (Backlog) groups with headers
2. Fix duplicated stage badge (`[IM]`) on the metadata line (line 2) of work item entries
3. Fix excessive 2-space indentation on work item entries vs unlinked items
4. Add test coverage for grouping edge cases (all-backlog, all-active, empty, mixed with unlinked)

## Session Config
- Base: origin/master
- Commit strategy: amend last
- Build: cargo build
- Lint: cargo clippy
- Test: cargo test

## Rounds

### Round 1

**Claude reviewer verdict:** needs-attention

**Finding:**
[F-1] Snapshot tests select a GroupHeader instead of a work item, disabling detail-panel and context-bar coverage (Confidence: 0.95)
- 6 tests set `selected_item = Some(0)` which now points to a GroupHeader instead of a WorkItemEntry
- Affected: work_item_selected_no_session, right_panel_focused_with_session, work_item_with_context_bar, work_item_context_bar_no_labels, work_item_context_bar_with_status, work_item_with_errors_no_session
- These tests now assert the welcome screen instead of work item details
