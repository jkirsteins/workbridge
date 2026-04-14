//! Deterministic Claude Code session identifiers derived from
//! `(work_item_id, stage)`.
//!
//! Each `(WorkItemId, WorkItemStatus)` pair maps to a stable UUID v5
//! computed against a fixed workbridge-specific namespace. Spawning Claude
//! Code with `--resume <uuid>` therefore re-attaches to the previous
//! session when workbridge is restarted, and switching stages yields a
//! different UUID so histories stay isolated per stage.
//!
//! The scheme is intentionally pure - nothing is persisted in workbridge's
//! data model. The UUID is recomputed from first principles every time it
//! is needed.
//!
//! See `docs/work-items.md` "Session identity and resumption" for the
//! end-to-end behaviour including the resume-first / `--session-id`
//! fallback used when no Claude session yet exists for the computed UUID.

use crate::work_item::{WorkItemId, WorkItemStatus};

/// Namespace used to derive deterministic Claude Code session IDs from
/// `(work_item_id, stage)`.
///
/// **Never change this constant.** Rotating it would invalidate every
/// existing workbridge -> Claude Code session link and silently orphan
/// users' past conversation history across a workbridge upgrade. The
/// value was generated once via `uuidgen` during the initial
/// implementation and is not derived from anything else, which keeps the
/// scheme independent from upstream constants that may shift over time.
pub const WORKBRIDGE_SESSION_NAMESPACE: uuid::Uuid =
    uuid::uuid!("fc74edbf-b1c0-4b9f-b838-1c1c90d0226c");

/// Build the canonical name string hashed into the UUID v5 derivation.
///
/// The format is frozen: any change here shifts every derived session ID
/// and breaks resume for existing work items. The variant tag prefix
/// (`local-file:`, `github-issue:`, `github-project:`) ensures that two
/// backends with cosmetically similar identifier strings cannot collide.
///
/// `LocalFile` paths are resolved to their absolute form with
/// `std::path::absolute` (a pure path-manipulation call, no filesystem
/// I/O, so it is safe to call from the UI thread). If resolution fails
/// the raw path is used as-is; this only affects platforms where the
/// fallback cannot produce an absolute path and is documented so future
/// readers understand why the helper does not propagate the error.
///
/// The stage is serialised via `serde_json` and then stripped of the
/// surrounding quotes, guaranteeing the spelling matches exactly what the
/// backend persists for each variant (`Planning`, `Implementing`, etc.)
/// rather than being tied to `Debug` output which could drift.
fn canonical_name(id: &WorkItemId, stage: WorkItemStatus) -> String {
    let id_part = match id {
        WorkItemId::LocalFile(path) => {
            let absolute = std::path::absolute(path).unwrap_or_else(|_| path.clone());
            format!("local-file:{}", absolute.to_string_lossy())
        }
        WorkItemId::GithubIssue {
            owner,
            repo,
            number,
        } => format!("github-issue:{owner}/{repo}#{number}"),
        WorkItemId::GithubProject { node_id } => format!("github-project:{node_id}"),
    };
    let stage_json = serde_json::to_string(&stage).unwrap_or_default();
    let stage_part = stage_json.trim_matches('"');
    format!("{id_part}:{stage_part}")
}

/// Compute the deterministic Claude Code session UUID for a given
/// work item and workflow stage.
///
/// Same inputs always produce the same UUID. Different stages of the
/// same work item yield different UUIDs, so each stage has its own
/// isolated resumable history.
pub fn session_id_for(id: &WorkItemId, stage: WorkItemStatus) -> uuid::Uuid {
    let name = canonical_name(id, stage);
    uuid::Uuid::new_v5(&WORKBRIDGE_SESSION_NAMESPACE, name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Version;

    #[test]
    fn same_inputs_produce_same_uuid() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/foo.json"));
        let a = session_id_for(&id, WorkItemStatus::Planning);
        let b = session_id_for(&id, WorkItemStatus::Planning);
        assert_eq!(a, b, "deterministic derivation must be stable");
    }

    #[test]
    fn different_stages_produce_different_uuids() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/foo.json"));
        let planning = session_id_for(&id, WorkItemStatus::Planning);
        let implementing = session_id_for(&id, WorkItemStatus::Implementing);
        let review = session_id_for(&id, WorkItemStatus::Review);
        assert_ne!(planning, implementing, "stage change must shift the UUID");
        assert_ne!(implementing, review, "stage change must shift the UUID");
        assert_ne!(planning, review, "stage change must shift the UUID");
    }

    /// The variant tag prefix in `canonical_name` must prevent collisions
    /// between a `LocalFile(path)` and a `GithubIssue` whose hashed
    /// string happens to look similar. We construct the closest possible
    /// cosmetic match on both sides and verify they still diverge.
    #[test]
    fn different_backends_with_similar_fields_do_not_collide() {
        let local = WorkItemId::LocalFile(PathBuf::from("github-issue:owner/repo#1"));
        let gh = WorkItemId::GithubIssue {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 1,
        };
        let a = session_id_for(&local, WorkItemStatus::Planning);
        let b = session_id_for(&gh, WorkItemStatus::Planning);
        assert_ne!(
            a, b,
            "variant tag prefix must keep LocalFile and GithubIssue disjoint",
        );
    }

    /// `GithubProject` must also be disjoint from the other two backends.
    #[test]
    fn github_project_is_disjoint_from_other_backends() {
        let project = WorkItemId::GithubProject {
            node_id: "owner/repo#1".to_string(),
        };
        let local = WorkItemId::LocalFile(PathBuf::from("owner/repo#1"));
        let a = session_id_for(&project, WorkItemStatus::Planning);
        let b = session_id_for(&local, WorkItemStatus::Planning);
        assert_ne!(
            a, b,
            "GithubProject must not collide with LocalFile for similar field strings",
        );
    }

    #[test]
    fn derived_uuids_are_v5() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/v5-check.json"));
        let u = session_id_for(&id, WorkItemStatus::Planning);
        assert_eq!(
            u.get_version(),
            Some(Version::Sha1),
            "session IDs must be UUID v5 (SHA-1)",
        );
    }

    /// Sanity check: the canonical name format is frozen. If this test
    /// fails, someone has changed the hashed string, which would shift
    /// every derived UUID and break resume for existing work items.
    /// Update the expected strings here only after a deliberate migration
    /// plan for users already running workbridge.
    #[test]
    fn canonical_name_format_is_frozen() {
        let gh = WorkItemId::GithubIssue {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number: 42,
        };
        assert_eq!(
            canonical_name(&gh, WorkItemStatus::Implementing),
            "github-issue:acme/widgets#42:Implementing",
        );

        let proj = WorkItemId::GithubProject {
            node_id: "PVT_kwDOABCDEF".to_string(),
        };
        assert_eq!(
            canonical_name(&proj, WorkItemStatus::Review),
            "github-project:PVT_kwDOABCDEF:Review",
        );
    }
}
