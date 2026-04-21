//! Pure parsing helpers for the various porcelain outputs produced by
//! git CLI commands used by `GitWorktreeService`. Each helper is a
//! pure function with no I/O, so the unit tests at the bottom of the
//! file exercise the parsers directly without a real git repo.

use std::path::PathBuf;

use super::super::WorktreeInfo;

/// Parse porcelain output from `git worktree list --porcelain` into
/// `WorktreeInfo` entries.
///
/// The porcelain format produces blocks separated by blank lines. Each
/// block contains lines like:
///   worktree /path/to/worktree
///   HEAD <sha>
///   branch refs/heads/<name>
/// or:
///   worktree /path/to/worktree
///   HEAD <sha>
///   detached
///
/// The first block is always the main worktree.
pub(super) fn parse_porcelain(output: &str) -> Vec<WorktreeInfo> {
    let mut result = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    let mut is_first = true;

    for line in output.lines() {
        if line.is_empty() {
            // End of a block - emit the entry if we have a path.
            if let Some(path) = current_path.take() {
                result.push(WorktreeInfo {
                    path,
                    branch: current_branch.take(),
                    is_main: is_first,
                    has_commits_ahead: None,
                    dirty: None,
                    untracked: None,
                    unpushed: None,
                    behind_remote: None,
                });
                is_first = false;
            }
            continue;
        }

        if let Some(path_str) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // Strip refs/heads/ prefix to get the short branch name.
            current_branch = Some(
                branch_ref
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_ref)
                    .to_string(),
            );
        } else if line == "detached" {
            current_branch = None;
        }
    }

    // Handle the last block if there was no trailing blank line.
    if let Some(path) = current_path.take() {
        result.push(WorktreeInfo {
            path,
            branch: current_branch.take(),
            is_main: is_first,
            has_commits_ahead: None,
            dirty: None,
            untracked: None,
            unpushed: None,
            behind_remote: None,
        });
    }

    result
}

/// Parse `git status --porcelain -uall` output into
/// `(dirty, untracked)`:
///
/// - `dirty` is true when at least one line describes a tracked-file
///   change (modified, staged, renamed, deleted). Porcelain v1 lines
///   for tracked changes have two status characters in columns 1-2
///   followed by a space; untracked lines start with `??`. Ignored
///   lines start with `!!` and count as neither.
/// - `untracked` is true when at least one line starts with `??`.
///
/// Empty output means "clean worktree". Pure function, no I/O, so
/// the parser can be exercised from unit tests without a real repo.
pub(super) fn parse_status_porcelain(output: &str) -> (bool, bool) {
    let mut dirty = false;
    let mut untracked = false;
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("??") {
            untracked = true;
        } else if !line.starts_with("!!") {
            // Non-ignored change: `!!` is an explicitly ignored
            // file (neither dirty nor untracked); anything else
            // counts as a dirty tracked-file change.
            dirty = true;
        }
    }
    (dirty, untracked)
}

/// Parse `git rev-list --left-right --count HEAD...@{u}` output into
/// `(unpushed, behind_remote)`.
///
/// The `HEAD...@{u}` symmetric-difference syntax paired with
/// `--left-right --count` produces a single line with two integers
/// separated by whitespace: left-side (HEAD-only) and right-side
/// (`@{u}`-only). Left = commits that exist locally but not on the
/// upstream = "unpushed"; right = commits on the upstream but not
/// local = "`behind_remote`".
///
/// Returns `None` for any output that does not parse as two
/// non-negative integers. Callers should only invoke this after
/// verifying the git command exited successfully - a non-zero exit
/// typically means the branch has no configured upstream, in which
/// case both counts should stay `None` rather than being coerced
/// to zero.
pub(super) fn parse_rev_list_left_right(output: &str) -> Option<(u32, u32)> {
    let trimmed = output.trim();
    let mut parts = trimmed.split_whitespace();
    let left = parts.next()?.parse::<u32>().ok()?;
    let right = parts.next()?.parse::<u32>().ok()?;
    // Reject trailing garbage so "1 2 3" does not silently parse.
    if parts.next().is_some() {
        return None;
    }
    Some((left, right))
}

/// Extract the worktree path from git's "already used by worktree at"
/// error message. Git formats this as:
///   fatal: 'branch' is already used by worktree at '/path/to/wt'
/// Returns `None` if the pattern is not found or the path cannot be
/// extracted.
pub(super) fn parse_locked_worktree_path(msg: &str) -> Option<PathBuf> {
    let marker = "is already used by worktree at '";
    let start = msg.find(marker)? + marker.len();
    let end = msg[start..].find('\'')?;
    Some(PathBuf::from(&msg[start..start + end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_porcelain tests (pure unit tests, no git CLI)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_porcelain_single_main_worktree() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main\n\
                       \n";
        let result = parse_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/repo"));
        assert_eq!(result[0].branch, Some("main".to_string()));
        assert!(result[0].is_main);
    }

    #[test]
    fn parse_porcelain_multiple_worktrees() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main\n\
                       \n\
                       worktree /home/user/repo-wt\n\
                       HEAD def5678\n\
                       branch refs/heads/feature-x\n\
                       \n";
        let result = parse_porcelain(output);
        assert_eq!(result.len(), 2);
        assert!(result[0].is_main);
        assert_eq!(result[0].branch, Some("main".to_string()));
        assert!(!result[1].is_main);
        assert_eq!(result[1].branch, Some("feature-x".to_string()));
    }

    #[test]
    fn parse_porcelain_detached_head() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main\n\
                       \n\
                       worktree /home/user/repo-detached\n\
                       HEAD 9999999\n\
                       detached\n\
                       \n";
        let result = parse_porcelain(output);
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].branch, None);
        assert!(!result[1].is_main);
    }

    #[test]
    fn parse_porcelain_no_trailing_newline() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main";
        let result = parse_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/repo"));
        assert!(result[0].is_main);
    }

    // -----------------------------------------------------------------------
    // parse_status_porcelain tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_status_porcelain_clean() {
        assert_eq!(parse_status_porcelain(""), (false, false));
    }

    #[test]
    fn parse_status_porcelain_only_untracked() {
        // `??` lines are untracked.
        let output = "?? new-file.txt\n?? another.md\n";
        assert_eq!(parse_status_porcelain(output), (false, true));
    }

    #[test]
    fn parse_status_porcelain_only_dirty() {
        // Modified (` M`), staged add (`A `), staged rename (`R ` old -> new).
        let output = " M src/main.rs\nA  src/new.rs\nR  a.rs -> b.rs\n";
        assert_eq!(parse_status_porcelain(output), (true, false));
    }

    #[test]
    fn parse_status_porcelain_dirty_and_untracked() {
        let output = " M src/main.rs\n?? new-file.txt\n";
        assert_eq!(parse_status_porcelain(output), (true, true));
    }

    #[test]
    fn parse_status_porcelain_ignored_lines_are_neither() {
        // `!!` lines are ignored files (git status --porcelain --ignored)
        // and must not count toward `dirty` or `untracked`.
        let output = "!! target/\n";
        assert_eq!(parse_status_porcelain(output), (false, false));
    }

    #[test]
    fn parse_status_porcelain_mixed_with_ignored() {
        let output = " M src/lib.rs\n?? new.rs\n!! target/debug\n";
        assert_eq!(parse_status_porcelain(output), (true, true));
    }

    #[test]
    fn parse_status_porcelain_deleted_counts_as_dirty() {
        let output = " D src/old.rs\n";
        assert_eq!(parse_status_porcelain(output), (true, false));
    }

    #[test]
    fn parse_status_porcelain_blank_lines_ignored() {
        // A spurious blank line must not tip the parser into false state.
        let output = " M a.rs\n\n?? b.rs\n";
        assert_eq!(parse_status_porcelain(output), (true, true));
    }

    // -----------------------------------------------------------------------
    // parse_rev_list_left_right tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_rev_list_left_right_clean() {
        // No divergence from upstream: "0\t0".
        assert_eq!(parse_rev_list_left_right("0\t0\n"), Some((0, 0)));
    }

    #[test]
    fn parse_rev_list_left_right_ahead_only() {
        // Two unpushed local commits, upstream not behind.
        assert_eq!(parse_rev_list_left_right("2\t0\n"), Some((2, 0)));
    }

    #[test]
    fn parse_rev_list_left_right_behind_only() {
        // Upstream has 3 commits local does not: behind but not ahead.
        assert_eq!(parse_rev_list_left_right("0\t3\n"), Some((0, 3)));
    }

    #[test]
    fn parse_rev_list_left_right_diverged() {
        // Both ahead and behind (classic rebase-needed state).
        assert_eq!(parse_rev_list_left_right("2\t3\n"), Some((2, 3)));
    }

    #[test]
    fn parse_rev_list_left_right_space_separator() {
        // `git rev-list --count` uses tabs but split_whitespace is tolerant.
        assert_eq!(parse_rev_list_left_right("4 7\n"), Some((4, 7)));
    }

    #[test]
    fn parse_rev_list_left_right_no_trailing_newline() {
        assert_eq!(parse_rev_list_left_right("1\t2"), Some((1, 2)));
    }

    #[test]
    fn parse_rev_list_left_right_empty_returns_none() {
        // Empty output (e.g. git exited 0 but produced nothing) is not
        // a valid "0\t0" answer and must not silently parse as clean.
        assert_eq!(parse_rev_list_left_right(""), None);
    }

    #[test]
    fn parse_rev_list_left_right_malformed_returns_none() {
        // Non-numeric output (shouldn't happen in practice) is rejected.
        assert_eq!(parse_rev_list_left_right("foo\tbar\n"), None);
    }

    #[test]
    fn parse_rev_list_left_right_trailing_garbage_rejected() {
        // A three-number line would mean we're being fed unexpected
        // output; refuse to parse rather than silently picking the
        // first two.
        assert_eq!(parse_rev_list_left_right("1\t2\t3\n"), None);
    }

    #[test]
    fn parse_rev_list_left_right_single_number_rejected() {
        assert_eq!(parse_rev_list_left_right("5\n"), None);
    }

    // -----------------------------------------------------------------------
    // parse_locked_worktree_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_locked_worktree_path_extracts_path() {
        let msg = "fatal: 'my-branch' is already used by worktree at '/repos/myrepo/.worktrees/my-branch'";
        assert_eq!(
            parse_locked_worktree_path(msg),
            Some(PathBuf::from("/repos/myrepo/.worktrees/my-branch")),
        );
    }

    #[test]
    fn parse_locked_worktree_path_multiline() {
        let msg = "Preparing worktree (checking out 'b')\n\
                   fatal: 'b' is already used by worktree at '/p'";
        assert_eq!(parse_locked_worktree_path(msg), Some(PathBuf::from("/p")));
    }

    #[test]
    fn parse_locked_worktree_path_unrelated_error() {
        assert_eq!(parse_locked_worktree_path("some other error"), None);
    }

    #[test]
    fn parse_locked_worktree_path_no_trailing_quote() {
        // Marker present but no closing quote -> None.
        let msg = "is already used by worktree at '";
        assert_eq!(parse_locked_worktree_path(msg), None);
    }
}
