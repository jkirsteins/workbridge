//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::path::PathBuf;

use crate::work_item::{WorkItem, WorkItemId, WorkItemStatus};
use crate::worktree_service::WorktreeService;

use super::*;

impl super::App {
    /// Remove recently-closed PRs from cached `repo_data`. Called after
    /// `poll_unlinked_cleanup` and after `drain_fetch_results` to ensure stale
    /// fetch data doesn't resurrect closed PRs in the unlinked list.
    pub fn apply_cleanup_evictions(&mut self) {
        for (repo_path, branch) in &self.cleanup_evicted_branches {
            if let Some(rd) = self.repo_data.get_mut(repo_path)
                && let Ok(ref mut prs) = rd.prs
            {
                prs.retain(|pr| pr.head_branch != *branch);
            }
        }
    }

    /// Delete Done work items whose `done_at` timestamp exceeds the
    /// configured `archive_after_days` retention period. Returns the
    /// remaining (non-archived) records for assembly.
    pub(super) fn auto_archive_done_items(
        &mut self,
        records: Vec<crate::work_item_backend::WorkItemRecord>,
    ) -> Vec<crate::work_item_backend::WorkItemRecord> {
        let archive_days = self.config.defaults.archive_after_days;
        if archive_days == 0 {
            return records;
        }

        let now =
            match crate::side_effects::clock::system_now().duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_secs(),
                Err(e) => {
                    self.status_message =
                        Some(format!("System clock error, skipping auto-archive: {e}"));
                    return records;
                }
            };
        let archive_secs = archive_days * 86400;

        let mut kept = Vec::with_capacity(records.len());
        let mut archived_count = 0u32;
        let mut all_warnings: Vec<String> = Vec::new();

        for record in records {
            if let Some(done_at) = record.done_at
                && now.saturating_sub(done_at) >= archive_secs
            {
                let mut warnings: Vec<String> = Vec::new();
                // Done items cannot have an in-flight worktree-create
                // thread (create only runs for pre-Implementing stages),
                // so `orphan_worktrees` is declared for the signature's
                // sake and any unexpected entries are forwarded into the
                // warnings buffer so they do not get silently dropped.
                let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
                match self.delete_work_item_by_id(&record.id, &mut warnings, &mut orphan_worktrees)
                {
                    Ok(()) => {
                        archived_count += 1;
                        all_warnings.extend(warnings);
                        for orphan in orphan_worktrees {
                            all_warnings.push(format!(
                                "auto-archive saw in-flight worktree {} in {} - \
                                 left in place, clean up manually",
                                orphan.worktree_path.display(),
                                orphan.repo_path.display(),
                            ));
                        }
                    }
                    Err(e) => {
                        all_warnings.push(format!("delete {}: {e}", record.title));
                        kept.push(record);
                    }
                }
                continue;
            }
            kept.push(record);
        }

        if archived_count > 0 {
            if all_warnings.is_empty() {
                self.status_message = Some(format!("Auto-archived {archived_count} done item(s)"));
            } else {
                self.status_message = Some(format!(
                    "Auto-archived {archived_count} done item(s) (warnings: {})",
                    all_warnings.join("; ")
                ));
            }
        } else if !all_warnings.is_empty() {
            self.status_message = Some(format!("Auto-archive errors: {}", all_warnings.join("; ")));
        }

        kept
    }

    /// Build the display list from current `work_items` and `unlinked_prs`.
    ///
    /// Groups (each hidden if empty):
    /// 1. REVIEW REQUESTS - PRs where the user is requested as reviewer
    /// 2. BLOCKED (repo) - Blocked work items (red header, shown first)
    /// 3. UNLINKED - PRs not yet imported as work items
    /// 4. ACTIVE (repo) - non-Backlog, non-Done, non-Blocked work items
    /// 5. BACKLOGGED (repo) - Backlog work items, grouped by repo
    /// 6. DONE (repo) - Done work items, grouped by repo
    pub fn build_display_list(&mut self) {
        let mut list = Vec::new();

        // When drilling down from board view, show only items matching
        // the drill-down stage. Otherwise show the full grouped list.
        if let Some(ref drill_stage) = self.board_drill_stage {
            for i in 0..self.work_items.len() {
                let matches = if *drill_stage == WorkItemStatus::Implementing {
                    self.work_items[i].status == WorkItemStatus::Implementing
                        || self.work_items[i].status == WorkItemStatus::Blocked
                } else if *drill_stage == WorkItemStatus::Review {
                    self.work_items[i].status == WorkItemStatus::Review
                        || self.work_items[i].status == WorkItemStatus::Mergequeue
                } else {
                    self.work_items[i].status == *drill_stage
                };
                if matches {
                    list.push(DisplayEntry::WorkItemEntry(i));
                }
            }
        } else {
            // REVIEW REQUESTS group (hidden if empty).
            if !self.review_requested_prs.is_empty() {
                list.push(DisplayEntry::GroupHeader {
                    label: "REVIEW REQUESTS".to_string(),
                    count: self.review_requested_prs.len(),
                    kind: GroupHeaderKind::Normal,
                });
                // Sort direct-to-you rows to the top of the block so the
                // most actionable reviews are always surfaced first. The
                // sort key is the `is_direct_request` boolean (false < true
                // but we negate below so direct comes first) and Rust's
                // `sort_by_key` is stable, so the original `gh` order is
                // preserved within each bucket. When the login is unknown
                // (fetch has not yet reported one), every row classifies
                // as team and the original order is preserved unchanged.
                let login = self.current_user_login.as_deref();
                let mut indices: Vec<usize> = (0..self.review_requested_prs.len()).collect();
                indices.sort_by_key(|&i| {
                    u8::from(!self.review_requested_prs[i].is_direct_request(login))
                });
                for i in indices {
                    list.push(DisplayEntry::ReviewRequestItem(i));
                }
            }

            // Partition work items into blocked, active, backlogged, and done.
            let mut blocked: Vec<usize> = Vec::new();
            let mut active: Vec<usize> = Vec::new();
            let mut backlogged: Vec<usize> = Vec::new();
            let mut done: Vec<usize> = Vec::new();
            for i in 0..self.work_items.len() {
                if self.work_items[i].status == WorkItemStatus::Done {
                    done.push(i);
                } else if self.work_items[i].status == WorkItemStatus::Backlog {
                    backlogged.push(i);
                } else if self.work_items[i].status == WorkItemStatus::Blocked {
                    blocked.push(i);
                } else {
                    active.push(i);
                }
            }

            // BLOCKED group first (red, attention-grabbing).
            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "BLOCKED",
                &blocked,
                GroupHeaderKind::Blocked,
            );

            // UNLINKED group (hidden if empty).
            if !self.unlinked_prs.is_empty() {
                list.push(DisplayEntry::GroupHeader {
                    label: "UNLINKED".to_string(),
                    count: self.unlinked_prs.len(),
                    kind: GroupHeaderKind::Normal,
                });
                for i in 0..self.unlinked_prs.len() {
                    list.push(DisplayEntry::UnlinkedItem(i));
                }
            }

            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "ACTIVE",
                &active,
                GroupHeaderKind::Normal,
            );
            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "BACKLOGGED",
                &backlogged,
                GroupHeaderKind::Normal,
            );
            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "DONE",
                &done,
                GroupHeaderKind::Normal,
            );
        }

        // Reset scroll offset when the display list is rebuilt so stale
        // offsets from a previous list shape (view mode toggle, drill-down,
        // item deletion) do not carry over. ratatui will re-clamp on the
        // next render frame based on the selected item.
        self.list_scroll_offset.set(0);

        self.display_list = list;

        // Restore selection by identity (WorkItemId or unlinked branch name)
        // so that selection survives reassembly even when display indices
        // change due to non-deterministic backend ordering or item additions.
        let mut restored = false;
        if let Some(ref target_id) = self.selected_work_item {
            for (i, entry) in self.display_list.iter().enumerate() {
                if let DisplayEntry::WorkItemEntry(wi_idx) = entry
                    && let Some(wi) = self.work_items.get(*wi_idx)
                    && wi.id == *target_id
                {
                    self.selected_item = Some(i);
                    restored = true;
                    break;
                }
            }
        }
        if !restored && let Some(ref target) = self.selected_unlinked_branch {
            let (target_repo, target_branch) = target;
            for (i, entry) in self.display_list.iter().enumerate() {
                if let DisplayEntry::UnlinkedItem(ul_idx) = entry
                    && let Some(ul) = self.unlinked_prs.get(*ul_idx)
                    && ul.branch == *target_branch
                    && ul.repo_path == *target_repo
                {
                    self.selected_item = Some(i);
                    restored = true;
                    break;
                }
            }
        }
        if !restored && let Some(ref target) = self.selected_review_request_branch {
            let (target_repo, target_branch) = target;
            for (i, entry) in self.display_list.iter().enumerate() {
                if let DisplayEntry::ReviewRequestItem(rr_idx) = entry
                    && let Some(rr) = self.review_requested_prs.get(*rr_idx)
                    && rr.branch == *target_branch
                    && rr.repo_path == *target_repo
                {
                    self.selected_item = Some(i);
                    restored = true;
                    break;
                }
            }
        }
        if !restored {
            // Previously selected item is gone. Clear identity trackers
            // and fall back to first selectable item or None.
            self.selected_work_item = None;
            self.selected_unlinked_branch = None;
            self.selected_review_request_branch = None;
            self.selected_item = self.display_list.iter().position(is_selectable);
        }

        // Keep board cursor in sync after reassembly.
        self.sync_board_cursor();
    }

    /// Sub-group work item indices by repo and emit group headers + entries.
    /// Each repo gets its own header: "ACTIVE (workbridge)" with a count.
    /// If all items share the same repo, the header is just "ACTIVE (repo)".
    pub(super) fn push_repo_groups(
        work_items: &[WorkItem],
        list: &mut Vec<DisplayEntry>,
        label: &str,
        indices: &[usize],
        kind: GroupHeaderKind,
    ) {
        if indices.is_empty() {
            return;
        }

        // Collect unique repos in order of first appearance. Uses the
        // shared `repo_slug_from_path` helper so the slug displayed in
        // the group header can never drift from the slug baked into a
        // work item's `display_id` (e.g. `#workbridge-42`).
        let mut repo_order: Vec<String> = Vec::new();
        let mut by_repo: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for &i in indices {
            let repo = work_items[i].repo_associations.first().map_or_else(
                || "unknown".to_string(),
                |a| crate::work_item::repo_slug_from_path(&a.repo_path),
            );
            by_repo.entry(repo.clone()).or_default().push(i);
            if !repo_order.contains(&repo) {
                repo_order.push(repo);
            }
        }

        // Sort each repo's items in reverse-workflow order so MQ items
        // precede RV, RV precedes IM, IM precedes PL - items closest to
        // shipping appear at the top of the ACTIVE bucket. Stable sort
        // preserves the existing backend path order within a stage as the
        // tiebreaker. This is a no-op for the BLOCKED / BACKLOGGED / DONE
        // callers because every item in those buckets shares a single
        // status.
        for items in by_repo.values_mut() {
            items.sort_by_key(|&i| work_items[i].status.active_group_rank());
        }

        for repo in &repo_order {
            let items = &by_repo[repo];
            list.push(DisplayEntry::GroupHeader {
                label: format!("{label} ({repo})"),
                count: items.len(),
                kind: kind.clone(),
            });
            for &i in items {
                list.push(DisplayEntry::WorkItemEntry(i));
            }
        }
    }

    /// Sync the identity trackers (`selected_work_item`, `selected_unlinked_branch`)
    /// from the current `selected_item` index. Called after any navigation that
    /// changes `selected_item` so that reassembly can restore the correct entry.
    pub(crate) fn sync_selection_identity(&mut self) {
        self.selected_work_item = None;
        self.selected_unlinked_branch = None;
        self.selected_review_request_branch = None;
        let Some(idx) = self.selected_item else {
            return;
        };
        match self.display_list.get(idx) {
            Some(DisplayEntry::WorkItemEntry(wi_idx)) => {
                if let Some(wi) = self.work_items.get(*wi_idx) {
                    self.selected_work_item = Some(wi.id.clone());
                }
            }
            Some(DisplayEntry::UnlinkedItem(ul_idx)) => {
                if let Some(ul) = self.unlinked_prs.get(*ul_idx) {
                    self.selected_unlinked_branch = Some((ul.repo_path.clone(), ul.branch.clone()));
                }
            }
            Some(DisplayEntry::ReviewRequestItem(rr_idx)) => {
                if let Some(rr) = self.review_requested_prs.get(*rr_idx) {
                    self.selected_review_request_branch =
                        Some((rr.repo_path.clone(), rr.branch.clone()));
                }
            }
            _ => {}
        }
    }

    // -- Navigation helpers --

    /// Move selection to the next selectable item in the display list.
    ///
    /// Sets `recenter_viewport_on_selection` on a successful move so the
    /// next render re-centers the viewport on the new selection. The
    /// flag is deliberately NOT set on the "no further selectable item"
    /// branch, since the selection and viewport both stay put.
    pub fn select_next_item(&mut self) {
        let start = self.selected_item.map_or(0, |idx| idx + 1);
        for i in start..self.display_list.len() {
            if is_selectable(&self.display_list[i]) {
                self.selected_item = Some(i);
                self.sync_selection_identity();
                self.recenter_viewport_on_selection.set(true);
                return;
            }
        }
        // If nothing found after current position, keep current selection.
    }

    /// Move selection to the previous selectable item in the display list.
    ///
    /// Sets `recenter_viewport_on_selection` on a successful move so the
    /// next render re-centers the viewport on the new selection.
    pub fn select_prev_item(&mut self) {
        let start = match self.selected_item {
            Some(idx) if idx > 0 => idx - 1,
            Some(_) => return, // at position 0, nowhere to go
            None => {
                // Nothing selected, select the last selectable item.
                if let Some(pos) = self.display_list.iter().rposition(is_selectable) {
                    self.selected_item = Some(pos);
                    self.sync_selection_identity();
                    self.recenter_viewport_on_selection.set(true);
                }
                return;
            }
        };
        for i in (0..=start).rev() {
            if is_selectable(&self.display_list[i]) {
                self.selected_item = Some(i);
                self.sync_selection_identity();
                self.recenter_viewport_on_selection.set(true);
                return;
            }
        }
        // If nothing found before current position, keep current selection.
    }

    /// Get the `WorkItemId` for the currently selected work item, if any.
    /// Returns None if nothing is selected or the selection is an unlinked PR.
    /// In board mode (without drill-down), delegates to the board cursor.
    pub fn selected_work_item_id(&self) -> Option<WorkItemId> {
        if self.view_mode == ViewMode::Board && !self.board_drill_down {
            return self.board_selected_work_item_id();
        }
        let idx = self.selected_item?;
        match self.display_list.get(idx)? {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                self.work_items.get(*wi_idx).map(|wi| wi.id.clone())
            }
            _ => None,
        }
    }

    // -- Board view helpers --

    /// Get indices into `self.work_items` for items matching the given status.
    /// For the Implementing column, also includes Blocked items.
    pub fn items_for_column(&self, status: WorkItemStatus) -> Vec<usize> {
        self.work_items
            .iter()
            .enumerate()
            .filter(|(_, wi)| {
                if status == WorkItemStatus::Implementing {
                    wi.status == WorkItemStatus::Implementing
                        || wi.status == WorkItemStatus::Blocked
                } else if status == WorkItemStatus::Review {
                    wi.status == WorkItemStatus::Review || wi.status == WorkItemStatus::Mergequeue
                } else {
                    wi.status == status
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Resolve the board cursor to a `WorkItemId`.
    pub fn board_selected_work_item_id(&self) -> Option<WorkItemId> {
        let col_status = *BOARD_COLUMNS.get(self.board_cursor.column)?;
        let items = self.items_for_column(col_status);
        let row = self.board_cursor.row?;
        let wi_idx = items.get(row)?;
        self.work_items.get(*wi_idx).map(|wi| wi.id.clone())
    }

    /// Sync the board cursor after reassembly or stage changes.
    /// Clamps row to the column's item count and tries to follow the
    /// selected work item if it moved columns.
    pub fn sync_board_cursor(&mut self) {
        // If we have a selected work item, try to find it in the board.
        if let Some(ref target_id) = self.selected_work_item {
            for (col_idx, status) in BOARD_COLUMNS.iter().enumerate() {
                let items = self.items_for_column(*status);
                for (row_idx, &wi_idx) in items.iter().enumerate() {
                    if let Some(wi) = self.work_items.get(wi_idx)
                        && wi.id == *target_id
                    {
                        self.board_cursor.column = col_idx;
                        self.board_cursor.row = Some(row_idx);
                        return;
                    }
                }
            }
        }

        // Selected item not found in any column - clamp to current column.
        let items = self.items_for_column(
            BOARD_COLUMNS
                .get(self.board_cursor.column)
                .copied()
                .unwrap_or(WorkItemStatus::Backlog),
        );
        if items.is_empty() {
            self.board_cursor.row = None;
        } else if let Some(row) = self.board_cursor.row {
            self.board_cursor.row = Some(row.min(items.len() - 1));
        } else {
            self.board_cursor.row = Some(0);
        }
    }

    /// Update `selected_work_item` from the board cursor position.
    pub fn sync_selection_from_board(&mut self) {
        self.selected_work_item = self.board_selected_work_item_id();
    }

    /// Cycle view mode: `FlatList` -> Board -> Dashboard -> `FlatList`. Also
    /// syncs cursor state when leaving Board mode so the `FlatList` cursor
    /// stays on the selected work item.
    pub fn toggle_view_mode(&mut self) {
        match self.view_mode {
            ViewMode::FlatList => {
                self.view_mode = ViewMode::Board;
                self.sync_board_cursor();
            }
            ViewMode::Board => {
                self.view_mode = ViewMode::Dashboard;
                self.board_drill_down = false;
                self.board_drill_stage = None;
            }
            ViewMode::Dashboard => {
                self.view_mode = ViewMode::FlatList;
                // Sync flat list selection from whichever work item is
                // currently selected, so the cursor lands on it when we
                // land back in the list view.
                if let Some(ref target_id) = self.selected_work_item {
                    for (i, entry) in self.display_list.iter().enumerate() {
                        if let DisplayEntry::WorkItemEntry(wi_idx) = entry
                            && let Some(wi) = self.work_items.get(*wi_idx)
                            && wi.id == *target_id
                        {
                            self.selected_item = Some(i);
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Build the target path for a new worktree.
    ///
    /// Uses `config.defaults.worktree_dir` as the subdirectory under the
    /// repo root, and sanitizes the branch name (replacing `/` with `-`)
    /// for the leaf directory name.
    pub(super) fn worktree_target_path(
        repo_path: &std::path::Path,
        branch: &str,
        worktree_dir: &str,
    ) -> PathBuf {
        let sanitized = branch.replace('/', "-");
        repo_path.join(worktree_dir).join(sanitized)
    }

    /// Find an existing worktree that can be safely reused in place of
    /// calling `git worktree add`. A worktree is reusable only when all
    /// three conditions hold:
    ///
    /// 1. It is registered with git for the target `branch`.
    /// 2. It is NOT the main worktree (`is_main = false`). Reusing the
    ///    user's primary repo checkout would spawn Claude sessions inside
    ///    it and drop workbridge state there, violating
    ///    invariant #3 in `docs/invariants.md`.
    /// 3. Its canonicalized path equals the canonicalized `wt_target` the
    ///    import/session-spawn flow would have created. This rules out
    ///    adopting unrelated worktrees the user made manually or that
    ///    another tool created at a different location.
    ///
    /// When no safe match is found, returns `None` and the caller should
    /// fall through to `create_worktree`, which surfaces git's own "branch
    /// already checked out" error for the truly conflicting cases.
    pub(super) fn find_reusable_worktree(
        ws: &dyn WorktreeService,
        repo_path: &std::path::Path,
        branch: &str,
        wt_target: &std::path::Path,
    ) -> Option<crate::worktree_service::WorktreeInfo> {
        let target_canonical = crate::config::canonicalize_path(wt_target).ok()?;
        ws.list_worktrees(repo_path).ok()?.into_iter().find(|w| {
            if w.is_main {
                return false;
            }
            if w.branch.as_deref() != Some(branch) {
                return false;
            }
            crate::config::canonicalize_path(&w.path)
                .is_ok_and(|existing_canonical| existing_canonical == target_canonical)
        })
    }

    /// Open or focus a session for the currently selected work item.
    ///
    /// If a session already exists for this work item, focuses the
    /// right panel. If no session exists, the caller must have already
    /// recorded a harness choice via `c` / `x` / `o`; otherwise this
    /// method pushes a hint toast and does nothing. This is the
    /// breaking behaviour-change from the v1 plan (Milestone 3): Enter
    /// no longer spawns a session without an explicit harness pick.
    pub fn open_session_for_selected(&mut self) {
        let Some(idx) = self.selected_item else {
            return;
        };
        let wi_idx = match self.display_list.get(idx) {
            Some(DisplayEntry::WorkItemEntry(i)) => *i,
            _ => return,
        };
        let Some(wi) = self.work_items.get(wi_idx) else {
            return;
        };
        let work_item_id = wi.id.clone();

        // If session already exists and is alive, just focus right panel.
        // If the session is dead, remove it and fall through to spawn a new one.
        if let Some(existing_key) = self.session_key_for(&work_item_id) {
            let is_alive = self
                .sessions
                .get(&existing_key)
                .is_some_and(|entry| entry.alive);
            if is_alive {
                self.focus = FocusPanel::Right;
                self.status_message = Some("Right panel focused - press Ctrl+] to return".into());
                return;
            }
            self.sessions.remove(&existing_key);
        }

        // Breaking change from the v1 plan (Milestone 3): Enter on a
        // work-item row with no live session is now a no-op unless a
        // harness has been picked (via `c` / `x`). The hint teaches
        // the new keybinding without taking a silent-default action
        // the user did not request.
        if !self.harness_choice.contains_key(&work_item_id) {
            self.push_toast("press c / x to open this work item with a specific harness".into());
            return;
        }

        self.spawn_session(&work_item_id);
    }
}
