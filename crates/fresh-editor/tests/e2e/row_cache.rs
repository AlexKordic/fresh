//! A frame redraws the rows whose inputs changed and takes the rest from the
//! row cache (`render_line::row_cache`). Debug builds check every frame the
//! cache serves against the same frame drawn without it, so the e2e corpus
//! as a whole checks what the cache returns; these tests check that it is
//! used.

use crate::common::harness::EditorTestHarness;
use crossterm::event::{KeyCode, KeyModifiers};
use fresh::test_api::{row_cache_counts, RowCacheCounts};
use tempfile::TempDir;

/// What the row cache did while `step` ran.
fn rows(
    harness: &mut EditorTestHarness,
    step: impl FnOnce(&mut EditorTestHarness),
) -> RowCacheCounts {
    let before = row_cache_counts();
    step(harness);
    row_cache_counts().since(before)
}

fn open_lines(harness: &mut EditorTestHarness, dir: &TempDir, count: usize) {
    let text: String = (1..=count)
        .map(|i| format!("line {i:05} of the row cache test\n"))
        .collect();
    let path = dir.path().join("rows.txt");
    std::fs::write(&path, text).unwrap();
    harness.open_file(&path).unwrap();
    harness.render().unwrap();
    harness.render().unwrap();
}

#[test]
fn a_wheel_scroll_draws_only_the_rows_that_came_into_view() {
    let dir = TempDir::new().unwrap();
    let mut harness = EditorTestHarness::new(80, 24).unwrap();
    open_lines(&mut harness, &dir, 300);

    let scroll = rows(&mut harness, |h| {
        h.mouse_scroll_down(20, 10).unwrap();
        h.render().unwrap();
    });
    let shown = (scroll.reused + scroll.drawn) / 2;
    assert!(shown >= 18, "two frames of a full pane: {scroll:?}");
    assert!(
        scroll.drawn <= 8,
        "a wheel notch brings a few rows into view; the rest are reused: {scroll:?}"
    );
    harness.assert_screen_contains("line 00004");
}

#[test]
fn moving_the_cursor_draws_its_old_and_new_lines() {
    let dir = TempDir::new().unwrap();
    let mut harness = EditorTestHarness::new(80, 24).unwrap();
    open_lines(&mut harness, &dir, 300);

    let step = rows(&mut harness, |h| {
        h.send_key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        h.render().unwrap();
    });
    assert!(
        step.reused >= 15,
        "the lines the cursor left alone are reused: {step:?}"
    );
    assert!(
        step.drawn <= 4,
        "the cursor's old and new lines are drawn: {step:?}"
    );
}

#[test]
fn an_unchanged_frame_draws_nothing_but_the_cursor_line() {
    let dir = TempDir::new().unwrap();
    let mut harness = EditorTestHarness::new(80, 24).unwrap();
    open_lines(&mut harness, &dir, 300);

    let idle = rows(&mut harness, |h| h.render().unwrap());
    assert!(idle.reused >= 18, "{idle:?}");
    assert!(idle.drawn <= 2, "{idle:?}");
}
