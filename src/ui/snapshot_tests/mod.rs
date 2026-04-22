use std::path::PathBuf;

use ratatui_core::backend::TestBackend;
use ratatui_core::terminal::Terminal;

use super::draw_to_buffer;
use crate::app::{
    App, DisplayEntry, FocusPanel, ReviewGateOrigin, ReviewGateState, StubBackend, UserActionKey,
    ViewMode, is_selectable,
};
use crate::theme::Theme;
use crate::work_item::{
    BackendType, CheckStatus, MergeableState, PrInfo, PrState, RepoAssociation, ReviewDecision,
    UnlinkedPr, WorkItem, WorkItemError, WorkItemId, WorkItemStatus,
};

/// Helper: render the app into a `TestBackend` and return the buffer as a string.
pub(super) fn render(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default_theme();
    terminal
        .draw(|frame: &mut ratatui_core::terminal::Frame<'_>| {
            draw_to_buffer(frame.area(), frame.buffer_mut(), app, &theme);
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut lines = Vec::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.last().is_some_and(std::string::String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

/// Create an App with predefined work items and unlinked PRs
/// without going through the backend.
pub(super) fn app_with_items(work_items: Vec<WorkItem>, unlinked_prs: Vec<UnlinkedPr>) -> App {
    let mut app = App::new();
    app.work_items = work_items;
    app.unlinked_prs = unlinked_prs;
    app.build_display_list();
    app
}

pub(super) fn make_work_item(
    id_suffix: &str,
    title: &str,
    status: WorkItemStatus,
    pr: Option<PrInfo>,
    repo_count: usize,
) -> WorkItem {
    let mut associations = Vec::new();
    for i in 0..repo_count.max(1) {
        associations.push(RepoAssociation {
            repo_path: PathBuf::from(format!("/repo/{id_suffix}/{i}")),
            branch: Some(format!("branch-{id_suffix}")),
            worktree_path: None,
            pr: if i == 0 { pr.clone() } else { None },
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        });
    }
    WorkItem {
        display_id: None,
        id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{id_suffix}.json"))),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: title.to_string(),
        description: None,
        status,
        status_derived: false,
        repo_associations: associations,
        errors: Vec::new(),
    }
}

pub(super) fn make_pr_info(number: u64, checks: CheckStatus) -> PrInfo {
    PrInfo {
        number,
        title: format!("PR #{number}"),
        state: PrState::Open,
        is_draft: false,
        review_decision: ReviewDecision::None,
        checks,
        mergeable: MergeableState::Unknown,
        url: format!("https://github.com/o/r/pull/{number}"),
    }
}

pub(super) fn make_unlinked_pr(branch: &str, number: u64, is_draft: bool) -> UnlinkedPr {
    UnlinkedPr {
        repo_path: PathBuf::from("/repo/unlinked"),
        pr: PrInfo {
            number,
            title: format!("Unlinked PR #{number}"),
            state: PrState::Open,
            is_draft,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: format!("https://github.com/o/r/pull/{number}"),
        },
        branch: branch.to_string(),
    }
}

mod part1;
mod part2;
mod part3;
mod part4;
