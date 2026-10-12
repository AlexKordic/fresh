//! E2E: `editor.setLinesChangedScope` limits the `lines_changed` batches a
//! plugin gets to the buffers it asked for, and a buffer its scope takes in
//! later has its visible lines offered to it then — even though another
//! plugin was offered them already, which marked them seen.
//!
//! Two plugins: one takes every buffer and says so in the status bar; the
//! other starts out wanting no buffer, and draws a `★` over the first letter
//! of each batch it gets, so the screen shows whether it got one.

#![cfg(feature = "plugins")]

use crate::common::harness::{copy_plugin_lib, EditorTestHarness, HarnessOptions};
use std::fs;

const EVERY_BUFFER: &str = r#"
const editor = getEditor();

globalThis.every_lines_changed = function(data: { buffer_id: number }): void {
  editor.setStatus(`every buffer got ${data.buffer_id}`);
};
editor.on("lines_changed", "every_lines_changed");
"#;

const SCOPED: &str = r#"
const editor = getEditor();

editor.setLinesChangedScope({ buffers: [] });
globalThis.scoped_lines_changed = function(data: {
  buffer_id: number;
  lines: { byte_start: number }[];
}): void {
  for (const line of data.lines) {
    editor.addConceal(data.buffer_id, "test-scoped", line.byte_start, line.byte_start + 1, "★");
  }
};
editor.on("lines_changed", "scoped_lines_changed");

globalThis.scoped_take_this_buffer = function(): void {
  editor.setLinesChangedScope({ buffers: [editor.getActiveBufferId()] });
};
editor.registerCommand("Scoped Lines: Take This Buffer", "", "scoped_take_this_buffer", null);
"#;

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn a_scoped_plugin_gets_a_buffers_lines_once_its_scope_takes_the_buffer() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    fs::create_dir(&project_root).unwrap();
    let plugins_dir = project_root.join("plugins");
    fs::create_dir(&plugins_dir).unwrap();
    copy_plugin_lib(&plugins_dir);
    fs::write(plugins_dir.join("test_every_buffer.ts"), EVERY_BUFFER).unwrap();
    fs::write(plugins_dir.join("test_scoped.ts"), SCOPED).unwrap();

    let file = project_root.join("notes.txt");
    fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

    let mut harness = EditorTestHarness::create(
        100,
        12,
        HarnessOptions::new()
            .with_working_dir(project_root.clone())
            .without_empty_plugins_dir(),
    )
    .unwrap();
    harness.open_file(&file).unwrap();
    harness.render().unwrap();

    // The plugin that takes every buffer gets the visible lines; the scoped
    // one, which wants none, does not.
    harness
        .wait_until(|h| h.get_status_bar().contains("every buffer got"))
        .unwrap();
    harness.wait_for_async_quiescence(4).unwrap();
    let screen = harness.screen_to_string();
    assert!(
        screen.contains("alpha") && !screen.contains('★'),
        "a plugin scoped to no buffer must get no lines.\nScreen:\n{screen}"
    );

    // Taking the buffer in offers its lines again, to the scoped plugin too.
    harness
        .run_palette_command("Scoped Lines: Take This Buffer")
        .unwrap();
    harness
        .wait_until(|h| {
            let screen = h.screen_to_string();
            screen.contains("★lpha") && screen.contains("★eta") && screen.contains("★amma")
        })
        .unwrap();
    harness.assert_no_plugin_errors();
}
