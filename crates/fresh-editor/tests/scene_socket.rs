//! File Commander's scene socket, end to end: a real `SceneBridge` on a Unix
//! socket and a client speaking its protocol, with messages built and read by
//! types generated from `scene.proto` (`scene_proto/`). Covers the hello, the
//! version check, typed input, frames held back until the client reports
//! what it applied, and a scroll sent as copies of rows the client has.
#![cfg(unix)]

#[path = "scene_proto/fresh.scene.v1.rs"]
#[allow(clippy::all)]
mod scene_proto;

use fresh::webui::{apply_step, build_editor, SceneBridge};
use prost::Message;
use scene_proto as pb;
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push(v as u8 | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn varint(stream: &mut UnixStream) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let mut b = [0u8];
        stream.read_exact(&mut b).ok()?;
        v |= u64::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The test's end of the socket.
struct App {
    stream: UnixStream,
}

impl App {
    fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect");
        stream.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        Self { stream }
    }

    fn send(&mut self, kind: u64, message: impl Message) {
        let payload = message.encode_to_vec();
        let mut buf = Vec::new();
        put_varint(&mut buf, kind);
        put_varint(&mut buf, payload.len() as u64);
        buf.extend_from_slice(&payload);
        self.stream.write_all(&buf).expect("send");
    }

    /// The next message, or `None` when nothing arrives in time or the
    /// connection ended.
    fn receive(&mut self) -> Option<(u64, Vec<u8>)> {
        let kind = varint(&mut self.stream)?;
        let len = varint(&mut self.stream)? as usize;
        let mut payload = vec![0; len];
        self.stream.read_exact(&mut payload).ok()?;
        Some((kind, payload))
    }

    fn frame(&mut self) -> Option<pb::Frame> {
        let (kind, payload) = self.receive()?;
        assert_eq!(kind, 2, "a Frame");
        Some(pb::Frame::decode(&payload[..]).expect("decode a Frame"))
    }
}

/// One pass of the daemon's loop for the bridge: read, push, write.
fn pass(bridge: &mut SceneBridge, ed: &mut fresh::app::Editor) {
    let (mut cols, mut rows) = (COLS, ROWS);
    let poll = bridge.poll(ed, &mut cols, &mut rows);
    if poll.mutated || poll.frame_owed || poll.resize_dirty {
        apply_step(ed, &json!({}));
        bridge.push_scene(ed, COLS, ROWS);
    }
    bridge.maintain();
}

/// Passes until the client's bytes have been read.
fn settle_bridge(bridge: &mut SceneBridge, ed: &mut fresh::app::Editor) {
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(20));
        pass(bridge, ed);
    }
}

#[test]
fn an_app_gets_the_scene_and_drives_the_editor_over_the_socket() {
    let root = std::env::temp_dir().join(format!("fresh-scene-socket-{}", std::process::id()));
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(home.join(".config/fresh")).expect("home");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::write(
        home.join(".config/fresh/config.json"),
        json!({
            "check_for_updates": false,
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
    let text: String = (1..=200).map(|n| format!("line {n} of the sample\n")).collect();
    let file = work.join("sample.txt");
    std::fs::write(&file, text).expect("sample");
    std::env::set_current_dir(&work).expect("cwd");
    let mut ed = build_editor(COLS, ROWS, &[file], true).expect("editor");
    for _ in 0..12 {
        apply_step(&mut ed, &json!({}));
    }
    let path = root.join("scene.sock");
    let mut bridge = SceneBridge::bind(&path, &ed).expect("bind");
    assert_eq!(bridge.socket_path(), Some(path.as_path()));

    // A client naming another protocol version is told why, and dropped.
    let mut stranger = App::connect(&path);
    stranger.send(16, pb::ClientHello { version: 99, telemetry: false });
    settle_bridge(&mut bridge, &mut ed);
    let (kind, payload) = stranger.receive().expect("an Error");
    assert_eq!(kind, 3);
    let error = pb::Error::decode(&payload[..]).unwrap();
    assert!(error.reason.contains("protocol 99"), "{}", error.reason);
    assert!(stranger.receive().is_none(), "then the connection ends");

    // Hello: the whole scene.
    let mut app = App::connect(&path);
    app.send(16, pb::ClientHello { version: 1, telemetry: true });
    settle_bridge(&mut bridge, &mut ed);
    let (kind, payload) = app.receive().expect("a ServerHello");
    assert_eq!(kind, 1);
    let hello = pb::ServerHello::decode(&payload[..]).unwrap();
    assert_eq!(hello.version, 1);
    let scene = hello.scene.unwrap();
    assert_eq!((scene.width, scene.height), (Some(u32::from(COLS)), Some(u32::from(ROWS))));
    assert_eq!(scene.rows.len(), usize::from(ROWS), "every row of the grid");
    assert!(scene.rows.iter().any(|r| r.text.contains("line 1 of the sample")));
    assert!(scene.status_bar.and_then(|s| s.value).is_some(), "the status bar");
    assert!(!scene.menus.map(|m| m.menus).unwrap_or_default().is_empty(), "the menus");

    // Its size, then a key: a frame that shows it, with timing.
    app.send(19, pb::Resize { cols: u32::from(COLS), rows: u32::from(ROWS) });
    settle_bridge(&mut bridge, &mut ed);
    assert_eq!(bridge.wanted_size(), Some((COLS, ROWS)));
    app.send(17, pb::Key { key: "x".into(), ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let frame = app.frame().expect("a frame for the key");
    assert_eq!(frame.seq, 1);
    assert!(frame.rows.iter().any(|r| r.text.contains("xline 1")), "the typed row");
    let timing = frame.timing.expect("timing");
    assert_eq!(timing.ack, 3, "the hello, the resize and the key were applied");
    app.send(26, pb::Applied { seq: 1 });

    // Without reports, at most two frames go out; the next waits, and the
    // client that catches up gets one frame with everything since.
    for key in ["a", "b", "c", "d"] {
        app.send(17, pb::Key { key: key.into(), ..Default::default() });
        settle_bridge(&mut bridge, &mut ed);
    }
    let first = app.frame().expect("a frame");
    let second = app.frame().expect("a second frame");
    assert_eq!((first.seq, second.seq), (2, 3));
    assert!(app.frame().is_none(), "the third waits for the client");
    app.send(26, pb::Applied { seq: 3 });
    settle_bridge(&mut bridge, &mut ed);
    let caught_up = app.frame().expect("the newest scene");
    assert_eq!(caught_up.seq, 4);
    assert!(caught_up.rows.iter().any(|r| r.text.contains("xabcdline 1")), "every key since");
    app.send(26, pb::Applied { seq: 4 });

    // A wheel notch scrolls: rows the client has come as copies.
    app.send(18, pb::Mouse { kind: 6, col: 10, row: 10, n: 1, ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let mut copies = 0;
    while let Some(frame) = app.frame() {
        copies += frame.copies.len() / 3;
        app.send(26, pb::Applied { seq: frame.seq });
        settle_bridge(&mut bridge, &mut ed);
    }
    assert!(copies > 0, "the scroll moved rows as copies");

    // An action by name opens the palette.
    app.send(21, pb::Action { name: "command_palette".into(), args_json: String::new() });
    settle_bridge(&mut bridge, &mut ed);
    let frame = app.frame().expect("a frame with the palette");
    assert!(frame.palette.and_then(|p| p.value).is_some(), "the palette opened");

    drop(app);
    settle_bridge(&mut bridge, &mut ed);
    assert!(!bridge.is_busy(), "the app left");
    drop(bridge);
    assert!(!path.exists(), "the bridge removes its socket");
    drop(std::fs::remove_dir_all(&root));
}
