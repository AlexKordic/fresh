//! The web scene's JSON, byte for byte, across a set of scenarios.
//!
//! The bridge's insides change (how it renders, how it gathers the scene);
//! what it sends a browser must not. Each scenario's scene — the browser's
//! (`scene_value`) and the grid client's (`scene_value_grid`) — is compared
//! with the file recorded for it in `tests/fixtures/scene_golden/`, as the
//! exact text `serde_json` writes, with the scratch directory's path replaced
//! by `$WORK` and the pacing hint `poll.active` (whether a timer happens to be
//! pending) masked. `UPDATE_SCENE_GOLDEN=1` records the files again.
//!
//! The editor runs in a scratch HOME with a config that turns off what moves
//! on its own (the welcome screen, the orchestrator dock, update checks), so
//! two runs see the same scene.

use fresh::webui::{apply_step, build_editor, scene_value, scene_value_grid};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const COLS: u16 = 100;
const ROWS: u16 = 30;

fn settle(ed: &mut fresh::app::Editor) {
    for _ in 0..12 {
        apply_step(ed, &json!({}));
    }
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scene_golden")
}

/// The scenarios, in order; each continues from the one before.
fn scenarios() -> Vec<(&'static str, Vec<Value>)> {
    vec![
        ("open", vec![]),
        ("typed", vec![json!({"key": "End"}), json!({"type": " // edited"})]),
        ("paged", vec![json!({"key": "PageDown"}), json!({"key": "PageDown"})]),
        ("palette", vec![json!({"action": "command_palette"}), json!({"type": "split"})]),
        ("palette_closed", vec![json!({"key": "Escape"})]),
        ("menu", vec![json!({"action": "menu_open", "args": {"name": "File"}})]),
        ("menu_closed", vec![json!({"key": "Escape"})]),
        ("explorer", vec![json!({"action": "toggle_file_explorer"})]),
        ("explorer_closed", vec![json!({"action": "toggle_file_explorer"})]),
        ("settings", vec![json!({"action": "open_settings"})]),
        ("settings_closed", vec![json!({"key": "Escape"})]),
        ("split", vec![json!({"action": "split_vertical"})]),
        ("search", vec![json!({"action": "search"}), json!({"type": "line"})]),
        ("search_closed", vec![json!({"key": "Escape"})]),
    ]
}

fn sample(work: &Path) -> PathBuf {
    let text: String = (1..=120)
        .map(|n| format!("fn line_{n}() -> u32 {{ let value = {n}; value * 2 }} // line {n}\n"))
        .collect();
    let path = work.join("sample.rs");
    std::fs::write(&path, text).expect("write sample");
    std::fs::write(work.join("notes.md"), "# Notes\n\nA second file for the explorer.\n").expect("write notes");
    path
}

#[test]
fn web_scene_json_is_unchanged() {
    let root = std::env::temp_dir().join(format!("fresh-scene-golden-{}", std::process::id()));
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(home.join(".config/fresh")).expect("home");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::write(
        home.join(".config/fresh/config.json"),
        json!({
            "check_for_updates": false,
            "theme": "dark",
            "plugins": {
                "orchestrator": {"enabled": true, "settings": {"autoOpenDock": "never"}},
                "welcome_screen": {"enabled": true, "settings": {"showOnStartup": false}}
            }
        })
        .to_string(),
    )
    .expect("config");
    // SAFETY (process-global state): this test binary runs this one test.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
    std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
    std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
    let file = sample(&work);
    std::env::set_current_dir(&work).expect("cwd");
    let work_text = work.canonicalize().unwrap_or(work.clone()).display().to_string();

    let mut ed = build_editor(COLS, ROWS, &[file], true).expect("editor");
    settle(&mut ed);

    let update = std::env::var_os("UPDATE_SCENE_GOLDEN").is_some();
    let dir = golden_dir();
    if update {
        std::fs::create_dir_all(&dir).expect("golden dir");
    }
    let mut mismatches = Vec::new();
    for (name, steps) in scenarios() {
        for step in &steps {
            apply_step(&mut ed, step);
        }
        settle(&mut ed);
        for (kind, scene) in [
            ("web", scene_value(&mut ed, COLS, ROWS)),
            ("grid", scene_value_grid(&mut ed, COLS, ROWS)),
        ] {
            let text = scene
                .to_string()
                .replace(&work_text, "$WORK")
                .replace(&work.display().to_string(), "$WORK")
                .replace(r#""poll":{"active":true}"#, r#""poll":{"active":"?"}"#)
                .replace(r#""poll":{"active":false}"#, r#""poll":{"active":"?"}"#);
            let path = dir.join(format!("{name}.{kind}.json"));
            if update {
                std::fs::write(&path, &text).expect("write golden");
            } else {
                let expected = std::fs::read_to_string(&path).unwrap_or_default();
                if expected != text {
                    let at = expected.bytes().zip(text.bytes()).take_while(|(a, b)| a == b).count();
                    let from = at.saturating_sub(80);
                    mismatches.push(format!(
                        "{name}.{kind}: differs at byte {at}\n  recorded: {}\n  now:      {}",
                        expected.get(from..(at + 120).min(expected.len())).unwrap_or(""),
                        text.get(from..(at + 120).min(text.len())).unwrap_or("")
                    ));
                }
            }
        }
    }
    drop(std::fs::remove_dir_all(&root));
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n\n"));
}
