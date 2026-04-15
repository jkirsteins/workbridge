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
///
/// `transition_count` is the backend's monotonic stage-transition
/// counter. It is appended as a decimal suffix so that two visits to
/// the same `(id, stage)` tuple at different counter values produce
/// DIFFERENT UUIDs: this is the property that prevents a cycle like
/// `Planning -> Implementing -> Planning` or `Review -> Implementing`
/// rework from resuming a prior transcript. The counter is frozen
/// into the hash in the same byte position for every record, so
/// records that were written before the field existed deserialize
/// with the serde default (`0`) and produce the same UUIDs they
/// historically had - i.e. count=0 is backward compatible with the
/// pre-field scheme.
fn canonical_name(id: &WorkItemId, stage: WorkItemStatus, transition_count: u64) -> String {
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
    format!("{id_part}:{stage_part}:{transition_count}")
}

/// Compute the deterministic Claude Code session UUID for a given
/// work item, workflow stage, and stage-transition count.
///
/// Same inputs always produce the same UUID. Different stages of the
/// same work item yield different UUIDs, so each stage has its own
/// isolated resumable history. Cycling back to a stage the work item
/// has been at before yields a DIFFERENT UUID from the earlier visit
/// because `transition_count` has advanced in the backend record,
/// so e.g. `Review -> Implementing` rework and `Blocked -> Planning`
/// retreat do NOT silently resume the old transcript - they start a
/// fresh one under a fresh UUID, matching the intent of invariant 13
/// "fresh session per stage transition".
///
/// Records that predate the `stage_transition_count` field
/// deserialize with the serde default (`0`), which makes every
/// pre-field record hash as if it had been written with count `0`
/// from the start. There is no older on-disk derivation to stay
/// byte-compatible with: the session-ID scheme was introduced on
/// this feature branch and the transition-count nonce was added in
/// the same branch before shipping, so users never ran workbridge
/// against a scheme that omitted the count suffix.
pub fn session_id_for(id: &WorkItemId, stage: WorkItemStatus, transition_count: u64) -> uuid::Uuid {
    let name = canonical_name(id, stage, transition_count);
    uuid::Uuid::new_v5(&WORKBRIDGE_SESSION_NAMESPACE, name.as_bytes())
}

/// Outcome of probing Claude Code's project transcript store for a
/// specific session UUID.
///
/// The probe deliberately distinguishes "definitely missing" from
/// "could not tell" so the spawn flow can refuse to guess when
/// filesystem I/O fails. Codex adversarial review flagged the earlier
/// `-> bool` return type as unsafe: a permission denied, FUSE stat
/// failure, or unreadable subdirectory would silently return `false`,
/// which would make `begin_session_open` spawn with
/// `--session-id <uuid>` under the (wrong) assumption that no
/// transcript existed. On a degraded home directory that quietly
/// loses the user's prior conversation context with no error surfaced
/// anywhere. The enum forces callers to handle the indeterminate case
/// explicitly instead of falling through to "Fresh".
#[derive(Debug, PartialEq, Eq)]
pub enum SessionProbe {
    /// A `<session_id>.jsonl` transcript file was found under some
    /// project subdirectory of `~/.claude/projects`. The caller
    /// should spawn with `--resume <uuid>` to restore the prior
    /// conversation.
    Exists,
    /// No matching transcript was found and every I/O syscall along
    /// the probe path either succeeded or failed with `NotFound`.
    /// Either the projects root itself does not exist (Claude Code
    /// has never run on this machine) or none of its subdirectories
    /// contain the target filename. The caller should spawn with
    /// `--session-id <uuid>` to create a new session under the
    /// deterministic UUID; the next restart's probe will hit and
    /// resume it.
    Missing,
    /// An I/O error prevented the probe from reaching a definitive
    /// answer (permission denied on `~/.claude/projects` or one of
    /// its subdirectories, FUSE mount failure, `stat` error on a
    /// candidate file, etc.). The contained string is a
    /// human-readable message suitable for the status bar. The
    /// caller MUST NOT fall through to either `--resume` or
    /// `--session-id`: both are wrong under different halves of the
    /// uncertainty, and the user needs to see the error so they can
    /// fix the underlying condition and retry.
    Indeterminate(String),
}

/// Probe Claude Code's session transcript store for the given UUID.
///
/// Claude Code stores each session as `<session_id>.jsonl` under
/// `~/.claude/projects/<encoded-cwd>/`, where `<encoded-cwd>` is the
/// project's absolute path with `/`, `.`, and `_` mangled to `-`. The
/// exact encoding is private to Claude Code and has historically
/// changed (e.g. underscore handling), so this helper does NOT try to
/// reconstruct the encoded directory name. Instead it scans every
/// project subdirectory and reports `Exists` if any of them contains a
/// file whose stem matches the UUID.
///
/// **Blocking I/O - background-thread only.** The scan performs
/// `std::fs::read_dir` on `~/.claude/projects` plus a
/// `Path::metadata()` stat per candidate subdirectory. On a typical
/// local workstation it completes in well under a millisecond, but
/// the latency is unbounded in the general case: it grows linearly
/// with the number of project directories and stalls on slow /
/// network-mounted home directories, permission delays, and
/// FUSE-backed filesystems. Calling this on the UI thread would
/// freeze the TUI for real users, which violates the absolute
/// "Blocking I/O Prohibition" rule in `docs/UI.md`.
///
/// The production caller is the background worker spawned by
/// `App::begin_session_open`, which co-locates this disk probe with
/// the deterministic UUID derivation and the
/// `WorkItemBackend::read_plan` filesystem read. The UI thread
/// receives only the resolved `(session_id, spawn_flag)` pair via
/// `SessionOpenPlanResult` and never re-checks. There is no UI-thread
/// fallback - if you need this answer on a tick handler, hand it off
/// to a background thread first.
///
/// Returns `SessionProbe::Indeterminate` when the home directory
/// cannot be resolved (the only plausible cause on a sane
/// installation is a broken environment), so even that edge case
/// surfaces visibly instead of silently spawning `Fresh`.
pub fn session_exists_on_disk(session_id: uuid::Uuid) -> SessionProbe {
    let Some(user_dirs) = directories::UserDirs::new() else {
        return SessionProbe::Indeterminate(
            "Could not resolve the user's home directory to probe \
             ~/.claude/projects"
                .to_string(),
        );
    };
    let projects = user_dirs.home_dir().join(".claude").join("projects");
    session_exists_in(&projects, session_id)
}

/// Pure-path implementation of [`session_exists_on_disk`] that takes
/// the projects root directory as an explicit argument so it can be
/// exercised under a temp dir from the unit tests.
///
/// Every syscall is classified:
///
/// - `NotFound` on `projects_dir` itself -> `Missing` (Claude Code
///   has never written a transcript here, which is a clean "no
///   session" answer - spawn fresh).
/// - An entry that is not a directory (regular file, broken
///   symlink, device, etc.) is skipped silently. Claude Code only
///   stores transcripts under per-project subdirectories, so
///   anything else in the projects root is benign - typically
///   `.DS_Store` on macOS, a README a user dropped in, or a tool-
///   created marker file. An earlier version of this helper
///   immediately `stat`ed `<entry>/<uuid>.jsonl` without
///   filtering, which made a regular-file `.DS_Store` blow up
///   with `NotADirectory` and globally block every session open
///   via the `Indeterminate` path. The Codex adversarial review
///   flagged that as a P0: one benign file in the user's projects
///   directory must not be a fatal probe error.
/// - `NotFound` on a candidate `<subdir>/<uuid>.jsonl` inside a
///   confirmed project directory -> skip to the next subdirectory
///   (this one simply has no match).
/// - `NotFound` on `entry.path()` itself (race: a directory
///   vanished between `read_dir` and the `metadata` call) ->
///   skip silently. The clean-miss semantic is preserved.
/// - Any other error on any syscall -> `Indeterminate(...)` with
///   a message that names the failing path. This covers
///   permission-denied, FUSE failures, and other real blockers
///   the caller needs to surface to the user.
///
/// We deliberately do NOT use `entries.flatten()` (which silently
/// drops per-entry errors) or bare `Path::is_file()` (which
/// collapses stat failures to `false`). Both were in the original
/// implementation and both hid real I/O errors as "no session".
pub fn session_exists_in(projects_dir: &std::path::Path, session_id: uuid::Uuid) -> SessionProbe {
    let target = format!("{session_id}.jsonl");
    let entries = match std::fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Projects root does not exist -> Claude Code has never
            // run, no transcript can possibly exist. This is the
            // clean-miss path; spawning fresh is correct.
            return SessionProbe::Missing;
        }
        Err(e) => {
            return SessionProbe::Indeterminate(format!(
                "Failed to read {}: {e}",
                projects_dir.display()
            ));
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                return SessionProbe::Indeterminate(format!(
                    "Failed to enumerate {}: {e}",
                    projects_dir.display()
                ));
            }
        };
        let entry_path = entry.path();
        // Filter to real directories only. `std::fs::metadata`
        // follows symlinks, so a symlink pointing at a valid
        // project directory is still classified correctly. A
        // symlink pointing at a regular file is correctly
        // rejected by `is_dir()`. A broken symlink surfaces as
        // `NotFound` from `metadata`, which we treat as a benign
        // "not a project directory" and skip.
        match std::fs::metadata(&entry_path) {
            Ok(meta) if meta.is_dir() => { /* fall through to the
                 * target probe below */
            }
            Ok(_) => continue, // regular file, socket, device, etc.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Broken symlink or race where the entry
                // disappeared after `read_dir`. Not a probe
                // failure - skip it.
                continue;
            }
            Err(e) => {
                return SessionProbe::Indeterminate(format!(
                    "Failed to stat {}: {e}",
                    entry_path.display()
                ));
            }
        }
        let candidate = entry_path.join(&target);
        match std::fs::metadata(&candidate) {
            Ok(meta) if meta.is_file() => return SessionProbe::Exists,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return SessionProbe::Indeterminate(format!(
                    "Failed to stat {}: {e}",
                    candidate.display()
                ));
            }
        }
    }
    SessionProbe::Missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Version;

    #[test]
    fn same_inputs_produce_same_uuid() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/foo.json"));
        let a = session_id_for(&id, WorkItemStatus::Planning, 0);
        let b = session_id_for(&id, WorkItemStatus::Planning, 0);
        assert_eq!(a, b, "deterministic derivation must be stable");
    }

    #[test]
    fn different_stages_produce_different_uuids() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/foo.json"));
        let planning = session_id_for(&id, WorkItemStatus::Planning, 0);
        let implementing = session_id_for(&id, WorkItemStatus::Implementing, 1);
        let review = session_id_for(&id, WorkItemStatus::Review, 2);
        assert_ne!(planning, implementing, "stage change must shift the UUID");
        assert_ne!(implementing, review, "stage change must shift the UUID");
        assert_ne!(planning, review, "stage change must shift the UUID");
    }

    /// Codex adversarial review finding: cycling back to a previously
    /// used stage must yield a DIFFERENT UUID than the first visit so
    /// the second visit creates a fresh session transcript instead of
    /// resuming the prior one. The `stage_transition_count` nonce is
    /// what makes this work: the second visit happens at a higher
    /// count value, so `(id, Planning, 0)` and `(id, Planning, 2)`
    /// hash to different UUIDs. This test pins that property for
    /// both Planning-cycle-back and Review -> Implementing rework
    /// scenarios.
    #[test]
    fn cycling_back_to_same_stage_produces_different_uuids() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/cycle.json"));
        // Planning -> Implementing -> Blocked -> Planning:
        // counts 0, 1, 2, 3. The two Planning UUIDs MUST differ.
        let first_planning = session_id_for(&id, WorkItemStatus::Planning, 0);
        let second_planning = session_id_for(&id, WorkItemStatus::Planning, 3);
        assert_ne!(
            first_planning, second_planning,
            "cycling back to Planning must not reuse the prior transcript",
        );
        // Review -> Implementing rework at count 4. The second
        // Implementing visit MUST NOT collide with the first
        // (count 1).
        let first_implementing = session_id_for(&id, WorkItemStatus::Implementing, 1);
        let rework_implementing = session_id_for(&id, WorkItemStatus::Implementing, 4);
        assert_ne!(
            first_implementing, rework_implementing,
            "Review -> Implementing rework must not resume the prior \
             Implementing transcript",
        );
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
        let a = session_id_for(&local, WorkItemStatus::Planning, 0);
        let b = session_id_for(&gh, WorkItemStatus::Planning, 0);
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
        let a = session_id_for(&project, WorkItemStatus::Planning, 0);
        let b = session_id_for(&local, WorkItemStatus::Planning, 0);
        assert_ne!(
            a, b,
            "GithubProject must not collide with LocalFile for similar field strings",
        );
    }

    #[test]
    fn derived_uuids_are_v5() {
        let id = WorkItemId::LocalFile(PathBuf::from("/tmp/v5-check.json"));
        let u = session_id_for(&id, WorkItemStatus::Planning, 0);
        assert_eq!(
            u.get_version(),
            Some(Version::Sha1),
            "session IDs must be UUID v5 (SHA-1)",
        );
    }

    /// `session_exists_in` must report `Missing` for a UUID that was
    /// never written under the projects root, so a fresh install (or
    /// a brand-new (work_item, stage) tuple) drives the spawn flow
    /// down the `--session-id` branch instead of the `--resume` one.
    #[test]
    fn session_exists_in_returns_missing_when_file_missing() {
        let tmp = tempfile::tempdir().expect("create temp projects root");
        // Project subdir present but no transcript file.
        std::fs::create_dir_all(tmp.path().join("-tmp-fake-project"))
            .expect("create empty project subdir");
        let id = uuid::Uuid::from_u128(0xdeadbeef_dead_beef_dead_beefdeadbeef);
        assert_eq!(session_exists_in(tmp.path(), id), SessionProbe::Missing);
    }

    /// `session_exists_in` must report `Exists` when ANY project
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
        assert_eq!(session_exists_in(tmp.path(), id), SessionProbe::Exists);
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
        assert_eq!(session_exists_in(tmp.path(), target), SessionProbe::Missing);
    }

    /// `session_exists_in` must treat a missing projects root
    /// (Claude Code has never run on this machine) as a clean miss:
    /// `Missing`, not `Indeterminate`. This is the fresh-install
    /// happy path and it must not surface a scary error to the user.
    #[test]
    fn session_exists_in_treats_missing_projects_root_as_missing() {
        let id = uuid::Uuid::from_u128(0x1);
        assert_eq!(
            session_exists_in(
                std::path::Path::new("/definitely/not/a/real/path/qwertyuiop"),
                id,
            ),
            SessionProbe::Missing,
        );
    }

    /// Codex adversarial review finding: a read_dir failure that is
    /// NOT `NotFound` (permission denied on `~/.claude/projects`,
    /// FUSE mount error, etc.) must surface as `Indeterminate(...)`
    /// so the caller can refuse to guess. The earlier `-> bool`
    /// version silently returned `false` on any error, which would
    /// make `begin_session_open` choose `--session-id` and quietly
    /// lose the user's prior conversation context.
    ///
    /// Cross-platform portability: we can't reliably simulate
    /// permission-denied on the top-level directory from a unit test
    /// because test runners may run as root on CI. Instead we point
    /// the probe at a path that is a regular FILE rather than a
    /// directory. On every POSIX platform and on Windows
    /// `std::fs::read_dir` returns `NotADirectory`/`InvalidInput`
    /// (not `NotFound`), which exercises the "any other error"
    /// branch and must be classified as `Indeterminate`.
    #[test]
    fn session_exists_in_returns_indeterminate_on_non_notfound_error() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        // Write a regular file at the path we're going to probe.
        let file_path = tmp.path().join("not-a-directory");
        std::fs::write(&file_path, b"workbridge probe target").expect("write stub file");
        let id = uuid::Uuid::from_u128(0xcafe_f00d_0000_0000_0000_0000_0000_0001);
        match session_exists_in(&file_path, id) {
            SessionProbe::Indeterminate(msg) => {
                assert!(
                    msg.contains(&file_path.display().to_string()),
                    "error message should name the failing path, got: {msg}",
                );
            }
            other => {
                panic!("expected Indeterminate for non-NotFound read_dir error, got {other:?}")
            }
        }
    }

    /// Codex adversarial review finding: a regular file in the
    /// projects root (e.g. macOS `.DS_Store`, a README a user
    /// dropped in, a tool-created marker) must NOT cause the probe
    /// to return `Indeterminate` - that would globally block every
    /// session open. The earlier version `stat`ed
    /// `<entry>/<uuid>.jsonl` without first checking that `<entry>`
    /// was a directory, so a regular-file entry blew up with
    /// `NotADirectory` and fell into the `Indeterminate` path. The
    /// fixed version filters entries to directories first and
    /// silently skips anything else. This test pins the skip
    /// behaviour: a projects root containing nothing but a regular
    /// file must return `Missing`, not `Indeterminate`, and a
    /// projects root containing a regular file alongside a real
    /// project directory with the transcript must return `Exists`.
    #[test]
    fn session_exists_in_skips_regular_files_in_projects_root() {
        let tmp = tempfile::tempdir().expect("create temp projects root");
        // Plant a `.DS_Store`-style regular file.
        std::fs::write(tmp.path().join(".DS_Store"), b"\x00Mac\x00\x01").expect("write .DS_Store");
        let id = uuid::Uuid::from_u128(0xcafe_f00d_0000_0000_0000_0000_0000_0003);

        // 1. Projects root with ONLY a benign file -> Missing.
        assert_eq!(
            session_exists_in(tmp.path(), id),
            SessionProbe::Missing,
            "a regular file in the projects root must not block \
             the probe - it must be skipped silently",
        );

        // 2. Same projects root plus a real project directory
        // holding the target transcript -> Exists.
        let project = tmp.path().join("-valid-project");
        std::fs::create_dir_all(&project).expect("create project subdir");
        std::fs::write(project.join(format!("{id}.jsonl")), b"{}\n")
            .expect("write transcript stub");
        assert_eq!(
            session_exists_in(tmp.path(), id),
            SessionProbe::Exists,
            "a valid project directory must still be found when \
             the projects root also contains benign regular files",
        );
    }

    /// Broken symlinks in the projects root must also be skipped
    /// silently. The probe follows symlinks via `metadata()`, so
    /// a symlink pointing at a deleted target surfaces as
    /// `NotFound` from `metadata(entry.path())`. That is classified
    /// as a benign "not a project directory" case and should NOT
    /// block session opens.
    #[test]
    #[cfg(unix)]
    fn session_exists_in_skips_broken_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("create temp projects root");
        // Dangling symlink pointing at a path that was never
        // created.
        symlink(
            tmp.path().join("this-target-never-existed"),
            tmp.path().join("-dead-link"),
        )
        .expect("create dangling symlink");
        let id = uuid::Uuid::from_u128(0xbad_c0de_0000_0000_0000_0000_0000_0004);
        assert_eq!(
            session_exists_in(tmp.path(), id),
            SessionProbe::Missing,
            "a broken symlink in the projects root must be skipped \
             silently, not elevated to Indeterminate",
        );
    }

    /// Sanity check: the canonical name format is frozen. If this test
    /// fails, someone has changed the hashed string, which would shift
    /// every derived UUID and break resume for existing work items.
    /// Update the expected strings here only after a deliberate migration
    /// plan for users already running workbridge.
    ///
    /// The frozen format is
    /// `<backend-tag>:<id-fields>:<stage>:<transition_count>` where the
    /// trailing `<transition_count>` is the persistent nonce added in
    /// response to the Codex adversarial-review finding that cycling
    /// stages reused prior transcripts. Count `0` is the canonical
    /// default for records that predate the field.
    #[test]
    fn canonical_name_format_is_frozen() {
        let gh = WorkItemId::GithubIssue {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number: 42,
        };
        assert_eq!(
            canonical_name(&gh, WorkItemStatus::Implementing, 0),
            "github-issue:acme/widgets#42:Implementing:0",
        );
        // A non-zero transition count must be reflected in the
        // frozen string, so a cycle back to the same stage lands
        // on a different UUID.
        assert_eq!(
            canonical_name(&gh, WorkItemStatus::Implementing, 7),
            "github-issue:acme/widgets#42:Implementing:7",
        );

        let proj = WorkItemId::GithubProject {
            node_id: "PVT_kwDOABCDEF".to_string(),
        };
        assert_eq!(
            canonical_name(&proj, WorkItemStatus::Review, 0),
            "github-project:PVT_kwDOABCDEF:Review:0",
        );
    }
}
