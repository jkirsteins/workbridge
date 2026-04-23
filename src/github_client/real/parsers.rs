use serde_json::Value;

use crate::github_client::{GithubError, GithubIssue, GithubPr};

/// Parse a single PR JSON object (from gh pr list --json) into a `GithubPr`.
pub(super) fn parse_pr_from_value(v: &Value) -> Result<GithubPr, GithubError> {
    let number = v
        .get("number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| GithubError::ParseError("PR missing 'number' field".into()))?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    let head_branch = v
        .get("headRefName")
        .and_then(|h| h.as_str())
        .unwrap_or("")
        .to_string();

    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("OPEN")
        .to_string();

    let is_draft = v
        .get("isDraft")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();

    let review_decision = parse_review_decision_raw(v);
    let status_check_rollup = parse_check_status_raw(v);

    // headRepositoryOwner is an object with a "login" field, e.g.
    // {"login": "contributor"}. It may be null or absent.
    let head_repo_owner = v
        .get("headRepositoryOwner")
        .and_then(|o| o.get("login"))
        .and_then(|l| l.as_str())
        .map(std::string::ToString::to_string);

    // author is an object with a "login" field, e.g. {"login": "user"}.
    let author = v
        .get("author")
        .and_then(|o| o.get("login"))
        .and_then(|l| l.as_str())
        .map(std::string::ToString::to_string);

    let mergeable = v
        .get("mergeable")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // reviewRequests is a mixed array of user and team objects. gh
    // returns user entries with a "login" field and team entries with
    // a "slug" field (plus "__typename" on newer gh versions). We
    // classify defensively - presence of a login makes it a user,
    // presence of a slug without a login makes it a team - so the
    // parser tolerates both current and future gh JSON shapes. When
    // the field is absent (open-PR fetch path) both vecs end up empty.
    let (requested_reviewer_logins, requested_team_slugs) = parse_review_requests(v);

    Ok(GithubPr {
        number,
        title,
        state,
        is_draft,
        head_branch,
        url,
        review_decision,
        status_check_rollup,
        head_repo_owner,
        author,
        mergeable,
        requested_reviewer_logins,
        requested_team_slugs,
    })
}

/// Split the `reviewRequests` JSON array from `gh pr list --json` into
/// user-login and team-slug vecs. See the comment in
/// `parse_pr_from_value` for the classification rules.
fn parse_review_requests(v: &Value) -> (Vec<String>, Vec<String>) {
    let mut logins = Vec::new();
    let mut slugs = Vec::new();
    let Some(arr) = v.get("reviewRequests").and_then(|r| r.as_array()) else {
        return (logins, slugs);
    };
    for entry in arr {
        if let Some(login) = entry.get("login").and_then(|l| l.as_str()) {
            logins.push(login.to_string());
        } else if let Some(slug) = entry.get("slug").and_then(|s| s.as_str()) {
            slugs.push(slug.to_string());
        } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
            // Some gh versions expose team identity under "name"
            // instead of "slug". Fall through to capture the team
            // name so the badge still renders something meaningful.
            slugs.push(name.to_string());
        }
    }
    (logins, slugs)
}

/// Parse a single issue JSON object (from gh issue view --json) into a `GithubIssue`.
pub(super) fn parse_issue_from_value(v: &Value) -> Result<GithubIssue, GithubError> {
    let number = v
        .get("number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| GithubError::ParseError("issue missing 'number' field".into()))?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("OPEN")
        .to_string();

    let labels = v
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|label| {
                    // gh returns labels as objects with a "name" field
                    label
                        .get("name")
                        .and_then(|n| n.as_str())
                        .or_else(|| label.as_str())
                        .map(std::string::ToString::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(GithubIssue {
        number,
        title,
        state,
        labels,
    })
}

/// Summarize the statusCheckRollup array from gh into a single raw string.
///
/// The gh CLI returns statusCheckRollup as an array of objects, each with a
/// "status" or "conclusion" field. This function reduces that array to a
/// single summary string:
/// - If the array is empty or missing: ""
/// - If any check has conclusion "FAILURE": "FAILURE"
/// - If any check has status "PENDING" or "`IN_PROGRESS`" (and none failed): "PENDING"
/// - If all checks have conclusion "SUCCESS": "SUCCESS"
/// - Otherwise: "UNKNOWN"
pub(super) fn parse_check_status_raw(v: &Value) -> String {
    let checks = match v.get("statusCheckRollup").and_then(|s| s.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return String::new(),
    };

    let mut has_pending = false;
    let mut has_failure = false;
    let mut has_success = false;

    for check in checks {
        // gh returns each check with either "conclusion" (completed checks)
        // or "status" (in-progress checks). conclusion can be null for
        // in-progress checks.
        let conclusion = check
            .get("conclusion")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let status = check.get("status").and_then(|s| s.as_str()).unwrap_or("");

        match conclusion {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => has_success = true,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "STALE" => has_failure = true,
            _ => {
                // No conclusion yet - check the status field
                match status {
                    "COMPLETED" => has_success = true,
                    "IN_PROGRESS" | "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" => {
                        has_pending = true;
                    }
                    _ => has_pending = true,
                }
            }
        }
    }

    if has_failure {
        "FAILURE".to_string()
    } else if has_pending {
        "PENDING".to_string()
    } else if has_success {
        "SUCCESS".to_string()
    } else {
        "UNKNOWN".to_string()
    }
}

/// Extract the reviewDecision string from gh JSON output.
///
/// gh returns reviewDecision as a string ("APPROVED", "`CHANGES_REQUESTED`",
/// "`REVIEW_REQUIRED`") or an empty string / null if no review has happened.
fn parse_review_decision_raw(v: &Value) -> String {
    v.get("reviewDecision")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::super::super::GithubPr;
    use super::{
        parse_check_status_raw, parse_issue_from_value, parse_pr_from_value,
        parse_review_decision_raw,
    };

    #[test]
    fn parse_pr_all_fields_populated() {
        let json = r#"{
            "number": 14,
            "title": "Refactor backend",
            "headRefName": "refactor-backend",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/14",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "ci"},
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.number, 14);
        assert_eq!(pr.title, "Refactor backend");
        assert_eq!(pr.head_branch, "refactor-backend");
        assert_eq!(pr.state, "OPEN");
        assert!(!pr.is_draft);
        assert_eq!(pr.url, "https://github.com/owner/repo/pull/14");
        assert_eq!(pr.review_decision, "APPROVED");
        assert_eq!(pr.status_check_rollup, "SUCCESS");
        // No headRepositoryOwner in JSON -> None
        assert_eq!(pr.head_repo_owner, None);
    }

    #[test]
    fn parse_pr_head_repository_owner() {
        let json = r#"{
            "number": 30,
            "title": "Fork PR",
            "headRefName": "fix-typo",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/30",
            "reviewDecision": "",
            "statusCheckRollup": [],
            "headRepositoryOwner": {"login": "contributor"}
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.head_repo_owner, Some("contributor".to_string()));

        // Null headRepositoryOwner -> None
        let json_null = r#"{
            "number": 31,
            "title": "PR with null owner",
            "headRefName": "feature",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/31",
            "headRepositoryOwner": null
        }"#;
        let v2: Value = serde_json::from_str(json_null).unwrap();
        let pr2 = parse_pr_from_value(&v2).unwrap();
        assert_eq!(pr2.head_repo_owner, None);
    }

    #[test]
    fn parse_pr_empty_check_rollup() {
        let json = r#"{
            "number": 5,
            "title": "Quick fix",
            "headRefName": "quick-fix",
            "state": "OPEN",
            "isDraft": true,
            "url": "https://github.com/owner/repo/pull/5",
            "reviewDecision": "",
            "statusCheckRollup": []
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.number, 5);
        assert!(pr.is_draft);
        assert_eq!(pr.review_decision, "");
        assert_eq!(pr.status_check_rollup, "");
    }

    #[test]
    fn parse_pr_null_check_rollup() {
        let json = r#"{
            "number": 6,
            "title": "No checks",
            "headRefName": "no-checks",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/6",
            "reviewDecision": null
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.status_check_rollup, "");
        assert_eq!(pr.review_decision, "");
    }

    #[test]
    fn parse_pr_mixed_check_statuses_failure_wins() {
        let json = r#"{
            "number": 88,
            "title": "Fix auth",
            "headRefName": "112-fix-auth",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/88",
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"},
                {"status": "COMPLETED", "conclusion": "FAILURE", "name": "test"},
                {"status": "IN_PROGRESS", "conclusion": "", "name": "deploy"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.status_check_rollup, "FAILURE");
        assert_eq!(pr.review_decision, "CHANGES_REQUESTED");
    }

    #[test]
    fn parse_pr_mixed_check_statuses_pending_without_failure() {
        let json = r#"{
            "number": 90,
            "title": "WIP feature",
            "headRefName": "wip-feature",
            "state": "OPEN",
            "isDraft": true,
            "url": "https://github.com/owner/repo/pull/90",
            "reviewDecision": "REVIEW_REQUIRED",
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"},
                {"status": "IN_PROGRESS", "conclusion": "", "name": "test"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.status_check_rollup, "PENDING");
    }

    #[test]
    fn parse_pr_closed_and_merged_states() {
        let closed_json = r#"{
            "number": 100,
            "title": "Closed PR",
            "headRefName": "closed-branch",
            "state": "CLOSED",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/100",
            "reviewDecision": "",
            "statusCheckRollup": []
        }"#;

        let merged_json = r#"{
            "number": 101,
            "title": "Merged PR",
            "headRefName": "merged-branch",
            "state": "MERGED",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/101",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": []
        }"#;

        let v_closed: Value = serde_json::from_str(closed_json).unwrap();
        let pr_closed = parse_pr_from_value(&v_closed).unwrap();
        assert_eq!(pr_closed.state, "CLOSED");

        let v_merged: Value = serde_json::from_str(merged_json).unwrap();
        let pr_merged = parse_pr_from_value(&v_merged).unwrap();
        assert_eq!(pr_merged.state, "MERGED");
    }

    #[test]
    fn parse_issue_all_fields() {
        let json = r#"{
            "number": 7,
            "title": "Add authentication",
            "state": "OPEN",
            "labels": [
                {"name": "enhancement"},
                {"name": "security"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let issue = parse_issue_from_value(&v).unwrap();

        assert_eq!(issue.number, 7);
        assert_eq!(issue.title, "Add authentication");
        assert_eq!(issue.state, "OPEN");
        assert_eq!(issue.labels, vec!["enhancement", "security"]);
    }

    #[test]
    fn parse_issue_closed_state() {
        let json = r#"{
            "number": 12,
            "title": "Fixed bug",
            "state": "CLOSED",
            "labels": []
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let issue = parse_issue_from_value(&v).unwrap();

        assert_eq!(issue.state, "CLOSED");
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_issue_no_labels_field() {
        let json = r#"{
            "number": 15,
            "title": "No labels",
            "state": "OPEN"
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let issue = parse_issue_from_value(&v).unwrap();

        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_review_decision_variants() {
        let cases = vec![
            (r#"{"reviewDecision": "APPROVED"}"#, "APPROVED"),
            (
                r#"{"reviewDecision": "CHANGES_REQUESTED"}"#,
                "CHANGES_REQUESTED",
            ),
            (
                r#"{"reviewDecision": "REVIEW_REQUIRED"}"#,
                "REVIEW_REQUIRED",
            ),
            (r#"{"reviewDecision": ""}"#, ""),
            (r#"{"reviewDecision": null}"#, ""),
            (r"{}", ""),
        ];

        for (json, expected) in cases {
            let v: Value = serde_json::from_str(json).unwrap();
            assert_eq!(
                parse_review_decision_raw(&v),
                expected,
                "failed for input: {json}"
            );
        }
    }

    #[test]
    fn parse_check_status_all_success() {
        let json = r#"{
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS"},
                {"status": "COMPLETED", "conclusion": "NEUTRAL"},
                {"status": "COMPLETED", "conclusion": "SKIPPED"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(parse_check_status_raw(&v), "SUCCESS");
    }

    #[test]
    fn parse_check_status_queued_is_pending() {
        let json = r#"{
            "statusCheckRollup": [
                {"status": "QUEUED", "conclusion": ""}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(parse_check_status_raw(&v), "PENDING");
    }

    #[test]
    fn parse_pr_list_json_array() {
        // Simulate a full gh pr list --json response
        let json = r#"[
            {
                "number": 1,
                "title": "First PR",
                "headRefName": "feature-a",
                "state": "OPEN",
                "isDraft": false,
                "url": "https://github.com/o/r/pull/1",
                "reviewDecision": "APPROVED",
                "statusCheckRollup": [
                    {"status": "COMPLETED", "conclusion": "SUCCESS"}
                ]
            },
            {
                "number": 2,
                "title": "Second PR",
                "headRefName": "feature-b",
                "state": "OPEN",
                "isDraft": true,
                "url": "https://github.com/o/r/pull/2",
                "reviewDecision": "",
                "statusCheckRollup": []
            }
        ]"#;

        let items: Vec<Value> = serde_json::from_str(json).unwrap();
        let prs: Vec<GithubPr> = items
            .iter()
            .map(|v| parse_pr_from_value(v).unwrap())
            .collect();

        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[0].status_check_rollup, "SUCCESS");
        assert_eq!(prs[1].number, 2);
        assert!(prs[1].is_draft);
        assert_eq!(prs[1].status_check_rollup, "");
    }

    // -----------------------------------------------------------------------
    // reviewRequests parsing (user vs team split)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_pr_review_requests_absent_empty_vecs() {
        let json = r#"{
            "number": 1,
            "title": "No review data",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/1"
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert!(pr.requested_reviewer_logins.is_empty());
        assert!(pr.requested_team_slugs.is_empty());
    }

    #[test]
    fn parse_pr_review_requests_user_entries() {
        let json = r#"{
            "number": 2,
            "title": "Two users requested",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/2",
            "reviewRequests": [
                {"__typename": "User", "login": "alice"},
                {"__typename": "User", "login": "bob"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert_eq!(pr.requested_reviewer_logins, vec!["alice", "bob"]);
        assert!(pr.requested_team_slugs.is_empty());
    }

    #[test]
    fn parse_pr_review_requests_team_slug() {
        let json = r#"{
            "number": 3,
            "title": "Team requested",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/3",
            "reviewRequests": [
                {"__typename": "Team", "slug": "core-team"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert!(pr.requested_reviewer_logins.is_empty());
        assert_eq!(pr.requested_team_slugs, vec!["core-team"]);
    }

    #[test]
    fn parse_pr_review_requests_team_name_fallback() {
        // Some gh versions may expose the team under "name" instead of
        // "slug". The parser should still capture it.
        let json = r#"{
            "number": 4,
            "title": "Team requested (name)",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/4",
            "reviewRequests": [
                {"name": "backend-team"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert_eq!(pr.requested_team_slugs, vec!["backend-team"]);
    }

    #[test]
    fn parse_pr_review_requests_mixed_users_and_teams() {
        let json = r#"{
            "number": 5,
            "title": "Mixed",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/5",
            "reviewRequests": [
                {"__typename": "User", "login": "alice"},
                {"__typename": "Team", "slug": "core-team"},
                {"__typename": "User", "login": "bob"},
                {"__typename": "Team", "slug": "frontend"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert_eq!(pr.requested_reviewer_logins, vec!["alice", "bob"]);
        assert_eq!(pr.requested_team_slugs, vec!["core-team", "frontend"]);
    }
}
