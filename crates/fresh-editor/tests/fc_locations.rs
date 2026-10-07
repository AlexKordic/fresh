//! FC opens must never inherit another host's authority, even for equal paths.
use crate::common::harness::{EditorTestHarness, HarnessOptions};

#[test]
fn endpoint_routing_preserves_dirty_local_tabs_and_remote_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let path = root.join("alpha.txt");
    std::fs::write(&path, "LOCAL_ONLY\n").unwrap();
    let mut h = EditorTestHarness::create(
        100,
        30,
        HarnessOptions::new()
            .with_working_dir(root.clone())
            .with_empty_plugins_dir(),
    )
    .unwrap();
    h.open_file(&path).unwrap();
    h.type_text("DIRTY_LOCAL").unwrap();
    let local = h.editor().active_window_id();
    let local_text = h.get_buffer_content().unwrap();
    h.editor_mut()
        .fc_open_location(
            "127.0.0.1",
            root.clone(),
            None,
            vec![path.display().to_string()],
        )
        .unwrap();
    let one = h.editor().active_window_id();
    assert_ne!(one, local);
    assert!(
        !h.editor_mut().process_pending_file_opens(),
        "Remote placeholder opened a local file"
    );
    h.editor_mut()
        .fc_open_location(
            "127.0.0.2",
            root.clone(),
            None,
            vec![path.display().to_string()],
        )
        .unwrap();
    let two = h.editor().active_window_id();
    assert_ne!(one, two);
    h.editor_mut()
        .fc_open_location(
            "127.0.0.1",
            root.clone(),
            None,
            vec![path.display().to_string()],
        )
        .unwrap();
    assert_eq!(
        h.editor().active_window_id(),
        one,
        "Same remote location duplicated a workspace"
    );
    h.editor_mut()
        .fc_open_location("-", root, None, vec![])
        .unwrap();
    assert_eq!(h.editor().active_window_id(), local);
    assert_eq!(
        h.get_buffer_content().unwrap(),
        local_text,
        "Routing discarded dirty local text"
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "LOCAL_ONLY\n",
        "Routing saved dirty text"
    );
}

#[test]
fn location_protocol_rejects_ambiguous_paths_and_ssh_options() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut h = EditorTestHarness::create(
        100,
        30,
        HarnessOptions::new()
            .with_working_dir(root.clone())
            .with_empty_plugins_dir(),
    )
    .unwrap();
    assert!(h
        .editor_mut()
        .fc_open_location("-oProxyCommand=bad", root.clone(), None, vec![])
        .is_err());
    assert!(h
        .editor_mut()
        .fc_open_location("box", "relative".into(), None, vec![])
        .is_err());
    assert!(h
        .editor_mut()
        .fc_open_location("box", root, None, vec!["relative.txt".into()])
        .is_err());
}
