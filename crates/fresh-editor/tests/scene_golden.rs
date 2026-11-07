//! The scenes the bridges send, across a set of scenarios.
//!
//! The bridges' insides change (how they render, how they gather and encode
//! the scene); what they send must not. Each scenario's scene is compared
//! with the files recorded for it in `tests/fixtures/scene_golden/`, with
//! the scratch directory's path replaced by `$WORK`:
//!
//! - `<name>.web.json`: the browser's scene (`scene_value`), as the exact
//!   text `serde_json` writes, with the pacing hint `poll.active` (whether a
//!   timer happens to be pending) masked;
//! - `<name>.app.txt`: the scene File Commander's app builds from its socket's
//!   protobuf messages, decoded with types generated from `scene.proto`
//!   (`scene_proto/`), as text (`Client::dump`). A new client's hello and a
//!   client that got every frame since the first scenario must build the
//!   same scene.
//!
//! `UPDATE_SCENE_GOLDEN=1` records the files again. The editor runs in a
//! scratch HOME with a config that turns off what moves on its own (the
//! welcome screen, the orchestrator dock, update checks), so two runs see the
//! same scene. `scene.proto` itself must be what the encoders write.

#[path = "scene_proto/client.rs"]
mod client;
#[path = "scene_proto/fresh.scene.v1.rs"]
#[allow(clippy::all)]
mod scene_proto;

use fresh::webui::{apply_step, build_editor, scene_value, wire_schema, WireProbe};
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
        (
            "typed",
            vec![json!({"key": "End"}), json!({"type": " // edited"})],
        ),
        (
            "paged",
            vec![json!({"key": "PageDown"}), json!({"key": "PageDown"})],
        ),
        (
            "scrolled",
            vec![json!({"kind": "scrolldown", "col": 20, "row": 10})],
        ),
        (
            "palette",
            vec![
                json!({"action": "command_palette"}),
                json!({"type": "split"}),
            ],
        ),
        ("palette_closed", vec![json!({"key": "Escape"})]),
        (
            "menu",
            vec![json!({"action": "menu_open", "args": {"name": "File"}})],
        ),
        ("menu_closed", vec![json!({"key": "Escape"})]),
        ("explorer", vec![json!({"action": "toggle_file_explorer"})]),
        (
            "explorer_closed",
            vec![json!({"action": "toggle_file_explorer"})],
        ),
        ("settings", vec![json!({"action": "open_settings"})]),
        ("settings_closed", vec![json!({"key": "Escape"})]),
        ("split", vec![json!({"action": "split_vertical"})]),
        (
            "search",
            vec![json!({"action": "search"}), json!({"type": "line"})],
        ),
        ("search_closed", vec![json!({"key": "Escape"})]),
    ]
}

fn sample(work: &Path) -> PathBuf {
    let text: String = (1..=120)
        .map(|n| format!("fn line_{n}() -> u32 {{ let value = {n}; value * 2 }} // line {n}\n"))
        .collect();
    let path = work.join("sample.rs");
    std::fs::write(&path, text).expect("write sample");
    std::fs::write(
        work.join("notes.md"),
        "# Notes\n\nA second file for the explorer.\n",
    )
    .expect("write notes");
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
    let work_text = work
        .canonicalize()
        .unwrap_or(work.clone())
        .display()
        .to_string();

    let mut ed = build_editor(COLS, ROWS, &[file], true).expect("editor");
    settle(&mut ed);

    let update = std::env::var_os("UPDATE_SCENE_GOLDEN").is_some();
    let dir = golden_dir();
    if update {
        std::fs::create_dir_all(&dir).expect("golden dir");
    }
    let mut mismatches = Vec::new();
    // A client that gets every frame, from the first scenario on.
    let mut probe = WireProbe::new(&ed);
    let mut client = client::Client::default();
    let mut copied = 0;
    for (name, steps) in scenarios() {
        for step in &steps {
            apply_step(&mut ed, step);
        }
        settle(&mut ed);
        let text = scene_value(&mut ed, COLS, ROWS)
            .to_string()
            .replace(&work_text, "$WORK")
            .replace(&work.display().to_string(), "$WORK")
            .replace(r#""poll":{"active":true}"#, r#""poll":{"active":"?"}"#)
            .replace(r#""poll":{"active":false}"#, r#""poll":{"active":"?"}"#);
        let path = dir.join(format!("{name}.web.json"));
        if update {
            std::fs::write(&path, &text).expect("write golden");
        } else {
            let expected = std::fs::read_to_string(&path).unwrap_or_default();
            if expected != text {
                let at = expected
                    .bytes()
                    .zip(text.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                let from = at.saturating_sub(80);
                mismatches.push(format!(
                    "{name}.web: differs at byte {at}\n  recorded: {}\n  now:      {}",
                    expected
                        .get(from..(at + 120).min(expected.len()))
                        .unwrap_or(""),
                    text.get(from..(at + 120).min(text.len())).unwrap_or("")
                ));
            }
        }
        // File Commander's socket, after the browser's scene: the app's scene
        // has no menu row, which changes what the browser's would show.
        if let Some((kind, payload)) = probe.next(&mut ed, COLS, ROWS) {
            client.receive(kind, &payload);
            copied += client.rows_copied;
        }
        let mut fresh = client::Client::default();
        let (kind, payload) = WireProbe::new(&ed)
            .next(&mut ed, COLS, ROWS)
            .expect("a hello");
        fresh.receive(kind, &payload);
        let mask = |text: String| {
            text.replace(&work_text, "$WORK")
                .replace(&work.display().to_string(), "$WORK")
        };
        let (hello, frames) = (mask(fresh.dump()), mask(client.dump()));
        if frames != hello {
            mismatches.push(format!("{name}: the frames since the first built another scene than a new client's hello\n{}", first_difference(&hello, &frames)));
        }
        let path = dir.join(format!("{name}.app.txt"));
        if update {
            std::fs::write(&path, &hello).expect("write golden");
        } else {
            let expected = std::fs::read_to_string(&path).unwrap_or_default();
            if expected != hello {
                mismatches.push(format!(
                    "{name}.app: {}",
                    first_difference(&expected, &hello)
                ));
            }
        }
    }
    // Encoding a scene allocates nothing once the buffers have grown: the
    // regions, panes and rows go straight into buffers kept between frames.
    #[cfg(feature = "alloc-stats")]
    {
        for _ in 0..3 {
            apply_step(&mut ed, &json!({"key": "PageDown"}));
            settle(&mut ed);
            drop(probe.next(&mut ed, COLS, ROWS));
        }
        drop(probe.next(&mut ed, COLS, ROWS));
        assert_eq!(
            probe.last_encode_allocs,
            Some((0, 0)),
            "encoding an unchanged scene"
        );
        apply_step(&mut ed, &json!({"key": "PageUp"}));
        settle(&mut ed);
        drop(probe.next(&mut ed, COLS, ROWS));
        assert_eq!(
            probe.last_encode_allocs,
            Some((0, 0)),
            "encoding a frame after paging"
        );
    }
    drop(std::fs::remove_dir_all(&root));
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n\n"));
    assert!(copied > 0, "scrolling sent rows the client had as copies");
}

/// Where two texts part, with a line of context from each.
fn first_difference(expected: &str, actual: &str) -> String {
    let line = expected
        .lines()
        .zip(actual.lines())
        .take_while(|(a, b)| a == b)
        .count();
    format!(
        "differs at line {}\n  recorded: {}\n  now:      {}",
        line + 1,
        expected.lines().nth(line).unwrap_or("(end)"),
        actual.lines().nth(line).unwrap_or("(end)")
    )
}

/// `src/webui/scene.proto` is what the encoders write (regenerate the test
/// types with `scene_proto/generate.sh` after it changes).
#[test]
fn scene_proto_is_what_the_encoders_write() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/webui/scene.proto");
    assert_eq!(
        std::fs::read_to_string(path).unwrap_or_default(),
        wire_schema()
    );
}
