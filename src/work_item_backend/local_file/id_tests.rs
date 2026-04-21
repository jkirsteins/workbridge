//! Display-ID allocation tests and legacy-record migration tests for
//! `LocalFileBackend`.
//!
//! ## `display_id`
//!
//! Every work item created through `LocalFileBackend` gets a stable,
//! human-readable `display_id` of the form `<slug>-<N>`, where the
//! slug is the final path component of the first repo association
//! and N is a monotonic per-slug counter persisted in
//! `id-counters.json`. Numbers are never reused even after delete;
//! counters survive process restart; corrupt counter files are
//! tolerated. These tests pin those invariants.
//!
//! ## Missing-`id` backward compatibility
//!
//! Work item files written before the `id` field was added must
//! still load cleanly. `WorkItemRecord::id` carries
//! `#[serde(default = "placeholder_work_item_id")]`, and both
//! `list()` and `read_record()` overwrite the deserialized value
//! with `LocalFile(<file path>)` immediately after parsing, so the
//! placeholder never escapes the backend layer. Records with a
//! *present-but-malformed* `id` value still fail strict
//! deserialization and surface as `CorruptRecord`, as does
//! genuinely malformed JSON.

use std::collections::HashMap;
use std::fs;

use super::LocalFileBackend;
use super::test_helpers::{make_request, temp_dir};
use crate::work_item::{WorkItemId, WorkItemStatus};
use crate::work_item_backend::WorkItemBackend;

#[test]
fn create_assigns_display_id() {
    let (_tmp, dir) = temp_dir("display-id-first");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(make_request("/tmp/foo/workbridge", "first"))
        .unwrap();

    assert_eq!(
        record.display_id.as_deref(),
        Some("workbridge-1"),
        "first item in `workbridge` repo should be workbridge-1"
    );
}

#[test]
fn display_id_counts_per_repo() {
    let (_tmp, dir) = temp_dir("display-id-per-repo");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    // Three items in `foo`, interleaved with two in `bar`. The
    // per-slug counter must be independent: `foo` advances 1->2->3
    // while `bar` stays at 1 until its first item is created, then
    // advances 1->2 while `foo` stays at wherever it was.
    let f1 = backend.create(make_request("/repos/foo", "f1")).unwrap();
    let b1 = backend.create(make_request("/repos/bar", "b1")).unwrap();
    let f2 = backend.create(make_request("/repos/foo", "f2")).unwrap();
    let f3 = backend.create(make_request("/repos/foo", "f3")).unwrap();
    let b2 = backend.create(make_request("/repos/bar", "b2")).unwrap();

    assert_eq!(f1.display_id.as_deref(), Some("foo-1"));
    assert_eq!(f2.display_id.as_deref(), Some("foo-2"));
    assert_eq!(f3.display_id.as_deref(), Some("foo-3"));
    assert_eq!(b1.display_id.as_deref(), Some("bar-1"));
    assert_eq!(b2.display_id.as_deref(), Some("bar-2"));
}

#[test]
fn display_id_never_reuses_on_delete() {
    let (_tmp, dir) = temp_dir("display-id-no-reuse");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let r1 = backend.create(make_request("/repos/foo", "one")).unwrap();
    let r2 = backend.create(make_request("/repos/foo", "two")).unwrap();
    let r3 = backend.create(make_request("/repos/foo", "three")).unwrap();
    assert_eq!(r1.display_id.as_deref(), Some("foo-1"));
    assert_eq!(r2.display_id.as_deref(), Some("foo-2"));
    assert_eq!(r3.display_id.as_deref(), Some("foo-3"));

    // Delete the middle item. Its number (2) must never be reused.
    backend.delete(&r2.id).unwrap();

    let r4 = backend.create(make_request("/repos/foo", "four")).unwrap();
    assert_eq!(
        r4.display_id.as_deref(),
        Some("foo-4"),
        "deleted IDs leave permanent gaps; the counter always advances"
    );
}

#[test]
fn counter_persists_across_backend_instances() {
    let (_tmp, dir) = temp_dir("display-id-persist");

    // Instance 1: allocate foo-1.
    {
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
        let r = backend.create(make_request("/repos/foo", "one")).unwrap();
        assert_eq!(r.display_id.as_deref(), Some("foo-1"));
    }

    // Instance 2: same dir, fresh backend. The counter file on
    // disk is the only shared state; if it is read on startup the
    // next ID must be foo-2, not foo-1.
    {
        let backend = LocalFileBackend::with_dir(dir).unwrap();
        let r = backend.create(make_request("/repos/foo", "two")).unwrap();
        assert_eq!(
            r.display_id.as_deref(),
            Some("foo-2"),
            "counter must survive backend drop/recreate via id-counters.json"
        );
    }
}

#[test]
fn legacy_record_without_display_id_deserializes() {
    // Migration-compat: an on-disk JSON written before the
    // `display_id` field existed must still load cleanly with
    // `display_id: None`.
    let (_tmp, dir) = temp_dir("display-id-legacy");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let legacy_path = dir.join("legacy.json");
    let legacy_json = r#"{
            "id": {"LocalFile": "__SELF__"},
            "title": "Pre-feature item",
            "status": "Backlog",
            "kind": "Own",
            "repo_associations": [
                {"repo_path": "/repos/foo", "branch": null}
            ]
        }"#
    .replace("__SELF__", legacy_path.to_str().unwrap());
    fs::write(&legacy_path, legacy_json).unwrap();

    let result = backend.list().unwrap();
    assert!(
        result.corrupt.is_empty(),
        "legacy record must not surface as corrupt: {:?}",
        result.corrupt
    );
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].display_id, None);
    assert_eq!(result.records[0].title, "Pre-feature item");
}

#[test]
fn corrupt_counter_file_does_not_panic() {
    // A manually corrupted `id-counters.json` must be tolerated:
    // the backend logs a warning, starts the counter from zero,
    // and the next save rewrites a valid file from scratch.
    let (_tmp, dir) = temp_dir("display-id-corrupt-counter");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    fs::write(dir.join("id-counters.json"), "{bad json").unwrap();

    let r = backend
        .create(make_request("/repos/foo", "after corruption"))
        .unwrap();
    assert_eq!(
        r.display_id.as_deref(),
        Some("foo-1"),
        "after corruption the counter starts fresh"
    );

    // The save path must have rewritten the file as valid JSON.
    let contents = fs::read_to_string(dir.join("id-counters.json")).unwrap();
    let parsed: HashMap<String, u64> =
        serde_json::from_str(&contents).expect("counter file should be valid JSON after save");
    assert_eq!(parsed.get("foo").copied(), Some(1));
}

#[test]
fn counter_file_is_not_treated_as_work_item() {
    // The counter file lives next to work item JSONs. list()
    // must skip it rather than reporting it as corrupt. Without
    // the skip, every normal startup would surface a fake
    // "corrupt JSON" entry in the UI.
    let (_tmp, dir) = temp_dir("display-id-counter-skip");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    backend.create(make_request("/repos/foo", "one")).unwrap();

    let result = backend.list().unwrap();
    assert!(
        result.corrupt.is_empty(),
        "id-counters.json should not be reported as corrupt: {:?}",
        result.corrupt
    );
    assert_eq!(result.records.len(), 1);
}

/// Legacy v1 JSON without the `id` field.
const LEGACY_WITHOUT_ID: &str = r#"{
        "title": "Legacy item",
        "status": "Implementing",
        "kind": "Own",
        "repo_associations": [
            {"repo_path": "/repos/foo", "branch": "feature/x"}
        ]
    }"#;

#[test]
fn legacy_record_missing_id_loads_cleanly_via_list() {
    let (_tmp, dir) = temp_dir("missing-id-list");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let legacy_path = dir.join("legacy-no-id.json");
    fs::write(&legacy_path, LEGACY_WITHOUT_ID).unwrap();

    // Precondition: the file really does not contain an `id` key.
    let raw_before = fs::read_to_string(&legacy_path).unwrap();
    assert!(
        !raw_before.contains("\"id\""),
        "precondition: legacy file must not contain an id field"
    );

    let result = backend.list().unwrap();
    assert!(
        result.corrupt.is_empty(),
        "legacy record without id must not surface as corrupt: {:?}",
        result.corrupt
    );
    assert_eq!(result.records.len(), 1);
    let record = &result.records[0];
    assert_eq!(record.title, "Legacy item");
    assert_eq!(record.status, WorkItemStatus::Implementing);
    // The id must be `LocalFile(<path of the file>)` - the
    // placeholder from `#[serde(default)]` has been overwritten
    // by `list()` with the real on-disk path.
    assert_eq!(record.id, WorkItemId::LocalFile(legacy_path));
}

#[test]
fn legacy_record_missing_id_loads_cleanly_via_read() {
    let (_tmp, dir) = temp_dir("missing-id-read");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let legacy_path = dir.join("legacy-read.json");
    fs::write(&legacy_path, LEGACY_WITHOUT_ID).unwrap();

    // `read()` must apply the same placeholder overwrite as
    // `list()` so callers that bypass `list()` (direct
    // `backend.read(&id)` after the id is known) also recover
    // legacy records transparently.
    let record = backend
        .read(&WorkItemId::LocalFile(legacy_path.clone()))
        .expect("legacy record without id must read cleanly");
    assert_eq!(record.title, "Legacy item");
    assert_eq!(record.status, WorkItemStatus::Implementing);
    assert_eq!(record.id, WorkItemId::LocalFile(legacy_path));
}

#[test]
fn corrupt_json_still_surfaces_in_corrupt_list() {
    // Genuine corruption (malformed JSON) must NOT be swept up by
    // the missing-id serde default. It has to keep surfacing as a
    // CorruptRecord with a "corrupt JSON" reason.
    let (_tmp, dir) = temp_dir("missing-id-still-corrupt");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    fs::write(dir.join("broken.json"), "{ this is not json").unwrap();

    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 0);
    assert_eq!(result.corrupt.len(), 1);
    assert!(
        result.corrupt[0].reason.contains("corrupt JSON"),
        "reason should still mention corrupt JSON, got: {}",
        result.corrupt[0].reason
    );
}

#[test]
fn malformed_id_value_still_surfaces_as_corrupt() {
    // Boundary case: the `id` key is present but its value is not
    // a valid `WorkItemId` (here, a bare string instead of a
    // tagged enum). Strict deserialization must fail - the
    // `#[serde(default)]` fallback only kicks in when the field
    // is absent, not when it's present-but-wrong - and the record
    // must surface as `CorruptRecord`. Guards against a future
    // refactor that "helps" by synthesizing a placeholder for any
    // parse error on the id field.
    let (_tmp, dir) = temp_dir("malformed-id-still-corrupt");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    fs::write(
        dir.join("bad-id.json"),
        r#"{
                "id": "not a valid WorkItemId",
                "title": "Broken item",
                "status": "Implementing",
                "kind": "Own",
                "repo_associations": []
            }"#,
    )
    .unwrap();

    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 0);
    assert_eq!(result.corrupt.len(), 1);
    assert!(
        result.corrupt[0].reason.contains("corrupt JSON"),
        "reason should mention corrupt JSON, got: {}",
        result.corrupt[0].reason
    );
}
