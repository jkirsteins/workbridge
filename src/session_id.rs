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

/// Check whether Claude Code has a session transcript on disk for the
/// given UUID.
///
/// Claude Code stores each session as `<session_id>.jsonl` under
/// `~/.claude/projects/<encoded-cwd>/`, where `<encoded-cwd>` is the
/// project's absolute path with `/`, `.`, and `_` mangled to `-`. The
/// exact encoding is private to Claude Code and has historically
/// changed (e.g. underscore handling), so this helper does NOT try to
/// reconstruct the encoded directory name. Instead it scans every
/// project subdirectory and reports `true` if any of them contains a
/// file whose stem matches the UUID.
///
/// The scan is one `read_dir` of `~/.claude/projects` plus one cheap
/// `Path::is_file()` syscall per subdirectory. On a typical workstation
/// with a few hundred project directories this completes in well under
/// a millisecond, which is fast enough to call from the UI thread (see
/// `docs/UI.md` "Blocking I/O Prohibition" - this is bounded local
/// stat I/O, not git/network/large-file I/O).
///
/// Returns `false` if the home directory cannot be resolved or the
/// projects directory does not exist (which itself implies Claude Code
/// has never run, so no session can possibly exist).
///
/// `finish_session_open` calls this helper to decide between
/// `--resume <uuid>` (file exists, prior conversation should be
/// restored) and `--session-id <uuid>` (no prior file, create a new
/// session under the deterministic UUID). The check happens once
/// before the spawn, so the user never sees a transient "No
/// conversation found" error and there is no probe state machine to
/// maintain.
pub fn session_exists_on_disk(session_id: uuid::Uuid) -> bool {
    let Some(user_dirs) = directories::UserDirs::new() else {
        return false;
    };
    let projects = user_dirs.home_dir().join(".claude").join("projects");
    session_exists_in(&projects, session_id)
}

/// Pure-path implementation of [`session_exists_on_disk`] that takes
/// the projects root directory as an explicit argument so it can be
/// exercised under a temp dir from the unit tests.
pub fn session_exists_in(projects_dir: &std::path::Path, session_id: uuid::Uuid) -> bool {
    let target = format!("{session_id}.jsonl");
    let Ok(entries) = std::fs::read_dir(projects_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.path().join(&target).is_file() {
            return true;
        }
    }
    false
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

    /// `session_exists_in` must report `false` for a UUID that was
    /// never written under the projects root, so a fresh install (or
    /// a brand-new (work_item, stage) tuple) drives the spawn flow
    /// down the `--session-id` branch instead of the `--resume` one.
    #[test]
    fn session_exists_in_returns_false_when_file_missing() {
        let tmp = tempfile::tempdir().expect("create temp projects root");
        // Project subdir present but no transcript file.
        std::fs::create_dir_all(tmp.path().join("-tmp-fake-project"))
            .expect("create empty project subdir");
        let id = uuid::Uuid::from_u128(0xdeadbeef_dead_beef_dead_beefdeadbeef);
        assert!(!session_exists_in(tmp.path(), id));
    }

    /// `session_exists_in` must report `true` when ANY project
    /// subdirectory under the projects root contains a `<uuid>.jsonl`
    /// file, regardless of which encoded subdirectory holds it. This
    /// is the property that lets `finish_session_open` survive a
    /// future change to Claude Code's cwd-encoding scheme without
    /// silently regressing to the spawn-fresh path on every restart.
    #[test]
    fn session_exists_in_finds_file_in_any_project_subdir() {
        let tmp = tempfile::tempdir().expect("create temp projects root");
        let project = tmp.path().join("-some-encoded-cwd");
        std::fs::create_dir_all(&project).expect("create project subdir");
        let id = uuid::Uuid::from_u128(0xfeedface_cafe_d00d_beef_b0bafacecafe);
        std::fs::write(project.join(format!("{id}.jsonl")), b"{}\n")
            .expect("write transcript stub");
        assert!(session_exists_in(tmp.path(), id));
    }

    /// `session_exists_in` must not match a transcript with a
    /// different UUID, even when the filenames share a prefix or stem
    /// substring. The check is by exact stem, not by `contains`.
    #[test]
    fn session_exists_in_does_not_match_unrelated_uuid() {
        let tmp = tempfile::tempdir().expect("create temp projects root");
        let project = tmp.path().join("-encoded");
        std::fs::create_dir_all(&project).expect("create project subdir");
        let other = uuid::Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
        std::fs::write(project.join(format!("{other}.jsonl")), b"{}\n")
            .expect("write unrelated transcript");
        let target = uuid::Uuid::from_u128(0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111);
        assert!(!session_exists_in(tmp.path(), target));
    }

    /// `session_exists_in` must tolerate a missing projects root
    /// (Claude Code has never run on this machine) without panicking
    /// and must report `false` so the spawn flow still proceeds.
    #[test]
    fn session_exists_in_tolerates_missing_projects_root() {
        let id = uuid::Uuid::from_u128(0x1);
        assert!(!session_exists_in(
            std::path::Path::new("/definitely/not/a/real/path/qwertyuiop"),
            id,
        ));
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
