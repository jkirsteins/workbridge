//! Work-item operations subsystem - PR open, rebase start, create,
//! set-branch recovery, delete prompt.
//!
//! Covers the user-action-level work-item commands that do NOT
//! belong to a more specific subsystem:
//! `open_selected_pr_in_browser`, `start_rebase_on_main`,
//! `create_work_item_with` and its quickstart variants, the
//! `set_branch` recovery dialog (`open_set_branch_dialog`,
//! `cancel_set_branch_dialog`, `confirm_set_branch_dialog`), and
//! the delete-prompt dialog (`open_delete_prompt`,
//! `cancel_delete_prompt`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{
    DisplayEntry, QUICKSTART_TITLE, RebaseTarget, UserActionKey, UserActionPayload,
    WorktreeCreateResult,
};
use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::{CreateWorkItem, RepoAssociationRecord};

impl super::App {
    /// Open the currently selected entry's PR in the default browser via
    /// the macOS `open` command. Sets a status message with the PR label on
    /// success, or "No PR to open" when the selection has no PR.
    ///
    /// The subprocess is spawned on a background thread: even though
    /// `open` is a local shell-out (not remote I/O), running it on the UI
    /// thread would still violate the `[ABSOLUTE]` blocking-I/O invariant
    /// in `docs/UI.md` as soon as `open` stalls for any reason (e.g. a
    /// slow `LaunchServices` dispatch). The child's status and stderr are
    /// intentionally discarded - this is a best-effort UX affordance and
    /// surfacing `open` errors would add more noise than value.
    ///
    /// Not routed through `try_begin_user_action` because the user-action
    /// guard is scoped to remote I/O (`gh`, network, `git fetch`/`push`/
    /// `pull`) per `docs/UI.md` "User action guard", and `open` is none of
    /// those.
    pub fn open_selected_pr_in_browser(&mut self) {
        match self.selected_pr_target() {
            Some((url, label)) => {
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("open").arg(&url).status();
                });
                self.shell.status_message = Some(format!("Opening {label}"));
            }
            None => {
                self.shell.status_message = Some("No PR to open".into());
            }
        }
    }

    /// Resolve the currently selected left-panel entry to a
    /// `(WorkItemId, repo path, branch)` triple suitable for the
    /// rebase-onto-main flow. Returns `None` if the selection is not a
    /// work item, has no repo association with both a worktree and a
    /// branch set, or is the only thing the caller could rebase
    /// (group headers, unlinked items, review-request items).
    ///
    /// The default-branch comparison is intentionally NOT done here:
    /// `default_branch` shells out to git, and this helper runs on the
    /// UI thread. A "branch == default branch" rebase is a no-op that
    /// the harness will detect on its own; gating it on the UI thread
    /// would require an unconditional blocking call that we cannot
    /// afford. The single-flight admission and the harness's idempotent
    /// `git rebase` are sufficient defence-in-depth.
    ///
    /// Pure: does not spawn, does not shell out, does not mutate
    /// `self`. Mirrors the shape of `selected_pr_target` so the
    /// dispatch site reads the same way.
    pub(crate) fn selected_rebase_target(&self) -> Option<RebaseTarget> {
        let idx = self.selected_item?;
        let entry = self.display_list.get(idx)?;
        match entry {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                let wi = self.work_items.get(*wi_idx)?;
                let assoc = wi
                    .repo_associations
                    .iter()
                    .find(|a| a.worktree_path.is_some() && a.branch.is_some())?;
                // Carry the worktree path, not the registered repo path:
                // each git worktree has its own HEAD, and the rebase MUST
                // run against this work item's branch, not whatever the
                // main checkout has checked out. The `.find` above already
                // filtered out associations without a worktree, so the
                // `as_ref()?` here is infallible defence-in-depth.
                Some(RebaseTarget {
                    wi_id: wi.id.clone(),
                    worktree_path: assoc.worktree_path.clone()?,
                    branch: assoc.branch.clone()?,
                })
            }
            DisplayEntry::UnlinkedItem(_)
            | DisplayEntry::ReviewRequestItem(_)
            | DisplayEntry::GroupHeader { .. } => None,
        }
    }

    /// Entry point for the `m` keybinding. Resolves the selected
    /// rebase target and either spawns the rebase gate or sets a
    /// "nothing to rebase" status message. Goes through
    /// `spawn_rebase_gate` which itself routes through
    /// `try_begin_user_action` for single-flight admission.
    pub fn start_rebase_on_main(&mut self) {
        let Some(target) = self.selected_rebase_target() else {
            self.shell.status_message = Some("No branch to rebase".into());
            return;
        };
        // Reject a rebase on a work item that already has a rebase gate
        // in flight before talking to the user-action guard, so the
        // status message names the right cause.
        if self.rebase_gates.contains_key(&target.wi_id) {
            self.shell.status_message = Some("Rebase already in progress for this item".into());
            return;
        }
        // Reject a rebase while the work item has a live interactive
        // session OR a live terminal tab. The rebase gate spawns a
        // headless Claude in the same worktree; if the interactive
        // session or the user's shell is concurrently editing files or
        // running git commands, the two processes will race on the
        // index and working tree, producing nondeterministic rebase
        // results or index-lock errors. Pure in-memory check (no I/O):
        // reads session_key_for + the SessionEntry.alive flag populated
        // by check_liveness, and terminal_sessions for the Terminal tab.
        let has_live_session = self
            .session_key_for(&target.wi_id)
            .and_then(|key| self.sessions.get(&key))
            .is_some_and(|entry| entry.alive);
        let has_live_terminal = self
            .terminal_sessions
            .get(&target.wi_id)
            .is_some_and(|entry| entry.alive);
        if has_live_session || has_live_terminal {
            self.shell.status_message =
                Some("Cannot rebase while a session is active for this item".into());
            return;
        }
        self.spawn_rebase_gate(target);
    }

    /// Spawn a background thread to fetch the branch and create a worktree
    /// for a freshly imported work item. If another worktree creation is
    /// already in flight, falls back to a status message instead of blocking.
    pub(super) fn spawn_import_worktree(
        &mut self,
        wi_id: WorkItemId,
        repo_path: PathBuf,
        branch: String,
        title: &str,
    ) {
        if self.is_user_action_in_flight(&UserActionKey::WorktreeCreate) {
            self.shell.status_message = Some(format!(
                "Imported: {title} (worktree queued - another in progress)"
            ));
            return;
        }

        // Admit the user action BEFORE spawning the background thread
        // so there is no window in which a freshly-spawned thread could
        // be running while the helper is in the rejecting state. The
        // `is_user_action_in_flight` check above already guarantees the
        // slot is free (UI-thread serialization), so this call is
        // effectively infallible; the `is_none()` branch is
        // defense-in-depth against a future async entry point.
        if self
            .try_begin_user_action(
                UserActionKey::WorktreeCreate,
                Duration::ZERO,
                format!("Importing: {title}..."),
            )
            .is_none()
        {
            self.shell.status_message = Some(format!(
                "Imported: {title} (worktree queued - another in progress)"
            ));
            return;
        }

        let wt_dir = self.services.config.defaults.worktree_dir.clone();
        let ws = Arc::clone(&self.services.worktree_service);
        let wi_id_clone = wi_id.clone();
        let title_clone = title.to_string();

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            Self::run_import_worktree_thread(
                ws.as_ref(),
                wi_id_clone,
                repo_path,
                branch,
                &title_clone,
                &wt_dir,
                &tx,
            );
        });

        self.attach_user_action_payload(
            &UserActionKey::WorktreeCreate,
            UserActionPayload::WorktreeCreate { rx, wi_id },
        );
        self.shell.status_message = Some(format!("Imported: {title} (creating worktree...)"));
    }

    /// Body of the background thread spawned by `spawn_import_worktree`.
    /// Fetches the branch, resolves or creates a worktree, and reports
    /// the outcome back through `tx`.
    fn run_import_worktree_thread(
        ws: &dyn crate::worktree_service::WorktreeService,
        wi_id: WorkItemId,
        repo_path: PathBuf,
        branch: String,
        title: &str,
        wt_dir: &str,
        tx: &crossbeam_channel::Sender<WorktreeCreateResult>,
    ) {
        if ws.fetch_branch(&repo_path, &branch).is_err() {
            let _ = tx.send(WorktreeCreateResult {
                wi_id,
                repo_path,
                branch: Some(branch.clone()),
                path: None,
                error: Some(format!(
                    "Imported: {title} - could not fetch branch '{branch}' from origin. \
                     Manual checkout required."
                )),
                open_session: false,
                branch_gone: false,
                reused: false,
                stale_worktree_path: None,
            });
            return;
        }
        let wt_target = Self::worktree_target_path(&repo_path, &branch, wt_dir);
        // Reuse an existing worktree only if it lives at the exact
        // expected location (wt_target) and is NOT the main worktree.
        // See `find_reusable_worktree` for rationale.
        let reused_wt = Self::find_reusable_worktree(ws, &repo_path, &branch, &wt_target);
        let (wt_result, reused) = reused_wt.map_or_else(
            || (ws.create_worktree(&repo_path, &branch, &wt_target), false),
            |existing_wt| (Ok(existing_wt), true),
        );
        match wt_result {
            Ok(wt_info) => {
                let _ = tx.send(WorktreeCreateResult {
                    wi_id,
                    repo_path,
                    branch: Some(branch),
                    path: Some(wt_info.path),
                    error: None,
                    open_session: false,
                    branch_gone: false,
                    reused,
                    stale_worktree_path: None,
                });
            }
            Err(crate::worktree_service::WorktreeError::BranchLockedToWorktree {
                ref locked_at,
                ..
            }) => {
                let _ = tx.send(WorktreeCreateResult {
                    wi_id,
                    repo_path,
                    branch: Some(branch),
                    path: None,
                    error: Some(format!(
                        "Imported: {title} - branch is locked to a stale worktree at '{}'\n\
                         (likely from an interrupted rebase).",
                        locked_at.display(),
                    )),
                    open_session: false,
                    branch_gone: false,
                    reused: false,
                    stale_worktree_path: Some(locked_at.clone()),
                });
            }
            Err(e) => {
                let _ = tx.send(WorktreeCreateResult {
                    wi_id,
                    repo_path,
                    branch: Some(branch),
                    path: None,
                    error: Some(format!("Imported: {title} (worktree not created: {e})")),
                    open_session: false,
                    branch_gone: false,
                    reused: false,
                    stale_worktree_path: None,
                });
            }
        }
    }

    /// Create a new work item with explicit parameters from the creation
    /// dialog. Accepts user-provided title, selected repos, and a branch
    /// name (required).
    pub fn create_work_item_with(
        &mut self,
        title: &str,
        description: Option<String>,
        repos: Vec<PathBuf>,
        branch: &str,
    ) -> Result<(), String> {
        if repos.is_empty() {
            let msg = "No repos selected".to_string();
            self.shell.status_message = Some(msg.clone());
            return Err(msg);
        }

        // Filter out repos whose git directory is missing. This guards
        // against stale cache entries or repos selected before their
        // .git dir disappeared.
        let valid_repos: Vec<PathBuf> = repos
            .into_iter()
            .filter(|repo_path| {
                self.active_repo_cache
                    .iter()
                    .any(|r| r.path == *repo_path && r.git_dir_present)
            })
            .collect();

        if valid_repos.is_empty() {
            let msg = "No selected repos have a git directory".to_string();
            self.shell.status_message = Some(msg.clone());
            return Err(msg);
        }

        let repo_associations: Vec<RepoAssociationRecord> = valid_repos
            .into_iter()
            .map(|repo_path| RepoAssociationRecord {
                repo_path,
                branch: Some(branch.to_string()),
                pr_identity: None,
            })
            .collect();

        let request = CreateWorkItem {
            title: title.to_string(),
            description,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations,
        };

        match self.services.backend.create(request) {
            Ok(_record) => {
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_flags.repos_changed = true;
                self.shell.status_message = Some(format!("Created: {title}"));
                Ok(())
            }
            Err(e) => {
                let msg = format!("Create error: {e}");
                self.shell.status_message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Create a quick-start work item in Planning status and immediately spawn
    /// a Claude session without asking the user anything. The Claude agent
    /// will ask the user what they want to work on and set title/description
    /// via MCP tools.
    ///
    /// Returns `Err("MULTIPLE_REPOS")` when the repo cannot be determined
    /// automatically and the caller should fall back to the create dialog.
    pub fn create_quickstart_work_item(&mut self) -> Result<(), String> {
        let repo = self.resolve_quickstart_repo()?;

        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let suffix = crate::create_dialog::random_suffix();
        let branch = format!("{username}/quickstart-{suffix}");

        let request = CreateWorkItem {
            title: QUICKSTART_TITLE.to_string(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo,
                branch: Some(branch),
                pr_identity: None,
            }],
        };

        match self.services.backend.create(request) {
            Ok(record) => {
                let wi_id = record.id;
                self.reassemble_work_items();
                self.fetcher_flags.repos_changed = true;
                // Set identity so build_display_list restores selection.
                self.selected_work_item = Some(wi_id.clone());
                self.build_display_list();
                self.spawn_session(&wi_id);
                Ok(())
            }
            Err(e) => {
                let msg = format!("Create error: {e}");
                self.shell.status_message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Create a quick-start work item for a specific repo. Used by the
    /// create dialog fallback when the user selects a repo from multiple
    /// options. Creates a Planning item and spawns a Claude session.
    pub fn create_quickstart_work_item_for_repo(&mut self, repo: PathBuf) -> Result<(), String> {
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let suffix = crate::create_dialog::random_suffix();
        let branch = format!("{username}/quickstart-{suffix}");

        let request = CreateWorkItem {
            title: QUICKSTART_TITLE.to_string(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo,
                branch: Some(branch),
                pr_identity: None,
            }],
        };

        match self.services.backend.create(request) {
            Ok(record) => {
                let wi_id = record.id;
                self.reassemble_work_items();
                self.fetcher_flags.repos_changed = true;
                self.selected_work_item = Some(wi_id.clone());
                self.build_display_list();
                self.spawn_session(&wi_id);
                Ok(())
            }
            Err(e) => {
                let msg = format!("Create error: {e}");
                self.shell.status_message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Determine the repo to use for a quick-start work item.
    ///
    /// Strategy:
    /// 1. Exactly one managed repo with a git directory - use it.
    /// 2. Multiple repos - return "`MULTIPLE_REPOS`" so the caller opens the
    ///    creation dialog with the repo picker focused. CWD is deliberately
    ///    not consulted: when there is a real choice to make, the user should
    ///    pick explicitly every time.
    /// 3. No repos at all - return an error message.
    pub(super) fn resolve_quickstart_repo(&self) -> Result<PathBuf, String> {
        let git_repos: Vec<&PathBuf> = self
            .active_repo_cache
            .iter()
            .filter(|r| r.git_dir_present)
            .map(|r| &r.path)
            .collect();

        match git_repos.len() {
            0 => Err("No managed repos available. Add one in Settings (?)".to_string()),
            1 => Ok(git_repos[0].clone()),
            _ => Err("MULTIPLE_REPOS".to_string()),
        }
    }

    /// Open the "Set branch name" recovery modal for a work item.
    ///
    /// The dialog is prefilled with a generated slug in the same
    /// `{username}/{slug}-{suffix}` shape the create dialog produces
    /// (see `create_dialog::auto_fill_branch`). The `pending` parameter
    /// records what action should be re-driven after the branch is
    /// persisted, so a branchless Enter or advance gesture can resume
    /// without the user having to repeat it.
    ///
    /// This method only mutates `self.set_branch_dialog`; it does not
    /// touch the backend or the work item list.
    pub fn open_set_branch_dialog(
        &mut self,
        wi_id: WorkItemId,
        pending: crate::create_dialog::PendingBranchAction,
    ) {
        let title = self
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .map(|w| w.title.clone())
            .unwrap_or_default();
        let slug = crate::create_dialog::slugify(&title);
        let slug = crate::create_dialog::truncate_slug(&slug, crate::create_dialog::MAX_SLUG_LEN);
        let suffix = crate::create_dialog::random_suffix();
        // Match `create_quickstart_work_item` and
        // `CreateDialog::auto_fill_branch`: use $USER when available and
        // fall back to a generic "user" literal otherwise.
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let default = if slug.is_empty() {
            format!("{username}/workitem-{suffix}")
        } else {
            format!("{username}/{slug}-{suffix}")
        };
        let mut input = rat_widget::text_input::TextInputState::new();
        input.set_text(&default);
        self.set_branch_dialog = Some(crate::create_dialog::SetBranchDialog {
            wi_id,
            input,
            pending,
        });
    }

    /// Dismiss the "Set branch name" modal without mutating anything.
    /// Safe to call when the modal is not visible.
    pub fn cancel_set_branch_dialog(&mut self) {
        self.set_branch_dialog = None;
    }

    /// Persist the branch name typed into the "Set branch name" modal
    /// and re-drive whichever action opened the dialog in the first
    /// place (see `PendingBranchAction`).
    ///
    /// Applies the branch to every repo association that currently has
    /// `branch.is_none()`, matching the "one branch per item" convention
    /// of `create_work_item_with`. On failure the dialog stays open so
    /// the user can retry or press Esc.
    pub fn confirm_set_branch_dialog(&mut self) {
        let Some(dlg) = self.set_branch_dialog.take() else {
            return;
        };
        let branch = dlg.input.text().trim().to_string();
        if branch.is_empty() {
            // Restore the dialog so the user can edit the field.
            self.shell.status_message = Some("Branch name cannot be empty".into());
            self.set_branch_dialog = Some(dlg);
            return;
        }

        // Collect the list of repo associations that need a branch.
        let targets: Vec<PathBuf> =
            if let Some(w) = self.work_items.iter().find(|w| w.id == dlg.wi_id) {
                w.repo_associations
                    .iter()
                    .filter(|a| a.branch.is_none())
                    .map(|a| a.repo_path.clone())
                    .collect()
            } else {
                self.shell.status_message = Some("Work item not found".into());
                return;
            };

        if targets.is_empty() {
            // Defensive: if the user somehow opened the dialog for an
            // item that already has a branch on every repo, treat it as
            // a no-op but still re-drive the pending action so the
            // gesture is not silently lost.
            self.shell.status_message = Some("Branch already set".into());
        } else {
            for repo_path in &targets {
                if let Err(e) = self
                    .services
                    .backend
                    .update_branch(&dlg.wi_id, repo_path, &branch)
                {
                    self.shell.status_message = Some(format!("Failed to set branch: {e}"));
                    // Restore the dialog so the user can retry.
                    self.set_branch_dialog = Some(dlg);
                    return;
                }
            }
            self.reassemble_work_items();
            self.build_display_list();
            self.fetcher_flags.repos_changed = true;
        }

        // Re-drive the pending action that opened the dialog.
        match dlg.pending {
            crate::create_dialog::PendingBranchAction::SpawnSession => {
                self.spawn_session(&dlg.wi_id);
            }
            crate::create_dialog::PendingBranchAction::Advance { from, to } => {
                self.apply_stage_change(&dlg.wi_id, from, to, "user");
            }
        }
    }

    /// Open the delete confirmation modal for the currently selected work
    /// item.
    ///
    /// Does not touch the backend, shell out to git, or spawn any cleanup.
    /// The dialog body warns that any uncommitted changes will be lost so
    /// we can unconditionally pass `--force` to the background cleanup
    /// thread without blocking the UI thread on `git status --porcelain`.
    /// The actual work runs in `confirm_delete_from_prompt` after the
    /// user presses 'y'.
    pub fn open_delete_prompt(&mut self) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            self.shell.status_message = Some("No work item selected".into());
            return;
        };
        self.open_delete_prompt_for(work_item_id);
    }

    /// Variant of `open_delete_prompt` that targets a specific work item by
    /// ID rather than reading `selected_work_item_id()`. Used by the
    /// branch-gone dialog which already knows the target and must not
    /// depend on `selected_item` because Board view stores the cursor in
    /// `board_cursor` instead.
    pub fn open_delete_prompt_for(&mut self, work_item_id: WorkItemId) {
        // Look up the target work item to fetch its title for the modal.
        let Some(target) = self.work_items.iter().find(|w| w.id == work_item_id) else {
            self.shell.status_message = Some("Work item not found".into());
            return;
        };

        let title = target.title.clone();
        self.delete_target_wi_id = Some(work_item_id);
        self.delete_target_title = Some(title);
        self.delete_flow.prompt_visible = true;
    }

    /// Dismiss the delete confirmation modal without deleting anything.
    /// Safe to call when the modal is not visible; it just clears any
    /// residual target state.
    pub fn cancel_delete_prompt(&mut self) {
        self.delete_flow.prompt_visible = false;
        self.delete_target_wi_id = None;
        self.delete_target_title = None;
    }
}
