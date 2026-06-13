use std::fs;

use serde_json::json;

use super::helpers::AftProcess;

#[test]
fn edit_match_preview_returns_diff_and_leaves_disk_and_undo_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    let original = "hello old world\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-edit-preview",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "storage_dir": storage.path(),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let resp = aft.send(
        &json!({
            "id": "edit-preview",
            "command": "edit_match",
            "file": file,
            "match": "old",
            "replacement": "new",
            "preview": true,
            "include_diff_content": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "preview failed: {resp:?}");
    assert_eq!(resp["preview"], true);
    let preview_diff = resp["preview_diff"].as_str().expect("preview_diff");
    assert!(
        preview_diff.contains("-hello old world"),
        "diff: {preview_diff}"
    );
    assert!(
        preview_diff.contains("+hello new world"),
        "diff: {preview_diff}"
    );
    assert_eq!(resp["diff"]["before"], original);
    assert_eq!(resp["diff"]["after"], "hello new world\n");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    let undo = aft.send(&json!({"id": "undo-after-preview", "command": "undo"}).to_string());
    assert_eq!(
        undo["success"], false,
        "preview must not create undo state: {undo:?}"
    );
    assert_eq!(undo["code"], "no_undo_history");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn apply_patch_preview_returns_diff_and_leaves_disk_and_undo_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.txt");
    let original = "alpha\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-patch-preview",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "storage_dir": storage.path(),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let patch_text = "*** Begin Patch\n*** Update File: file.txt\n@@\n-alpha\n+beta\n*** End Patch";
    let resp = aft.send(
        &json!({
            "id": "patch-preview",
            "command": "apply_patch",
            "patch_text": patch_text,
            "preview": true,
            "include_diff_content": true,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "preview failed: {resp:?}");
    assert_eq!(resp["preview"], true);
    let preview_diff = resp["preview_diff"].as_str().expect("preview_diff");
    assert!(preview_diff.contains("-alpha"), "diff: {preview_diff}");
    assert!(preview_diff.contains("+beta"), "diff: {preview_diff}");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    let undo = aft.send(&json!({"id": "undo-after-patch-preview", "command": "undo"}).to_string());
    assert_eq!(
        undo["success"], false,
        "preview must not create undo state: {undo:?}"
    );
    assert_eq!(undo["code"], "no_undo_history");

    let status = aft.shutdown();
    assert!(status.success());
}
