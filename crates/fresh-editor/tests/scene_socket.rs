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
    stranger.send(16, pb::ClientHello { version: 99, telemetry: false, ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let (kind, payload) = stranger.receive().expect("an Error");
    assert_eq!(kind, 3);
    let error = pb::Error::decode(&payload[..]).unwrap();
    assert!(error.reason.contains("protocol 99"), "{}", error.reason);
    assert!(stranger.receive().is_none(), "then the connection ends");

    // Hello: the whole scene.
    let mut app = App::connect(&path);
    app.send(16, pb::ClientHello { version: 1, telemetry: true, ..Default::default() });
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
    rich_compose_edits_keep_source_and_retry_obsolete_input(&root, &work);
    rich_navigation_uses_measured_lines_after_resolving_a_key_chord(&root, &work);
    rich_source_regions_belong_to_each_client_and_expire_after_edits(&root,&work);
    drop(std::fs::remove_dir_all(&root));
}

fn rich_source_regions_belong_to_each_client_and_expire_after_edits(root: &std::path::Path, work: &std::path::Path) {
    let original = "# Long\n\n".to_string()+&(0..4000).map(|i|format!("Paragraph {i}: enough text to cross the source window.\n\n")).collect::<String>();
    let file = work.join("regions.md"); std::fs::write(&file,&original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    for _ in 0..50 { apply_step(&mut ed,&json!({})); std::thread::sleep(Duration::from_millis(10)); }
    let path=root.join("regions.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut a=App::connect(&path); let mut b=App::connect(&path);
    for app in [&mut a,&mut b] { app.send(16,pb::ClientHello { version:1,rich_compose:true,..Default::default() }); }
    settle_bridge(&mut bridge,&mut ed);
    let hello_a=pb::ServerHello::decode(&a.receive().unwrap().1[..]).unwrap().scene.unwrap();
    let hello_b=pb::ServerHello::decode(&b.receive().unwrap().1[..]).unwrap().scene.unwrap();
    let first=|frame:&pb::Frame| frame.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap().clone();
    let mut scope_a=pane_scope(hello_a.window_id.unwrap(),&first(&hello_a));
    let mut scope_b=pane_scope(hello_b.window_id.unwrap(),&first(&hello_b));
    assert_eq!(first(&hello_a).compose.unwrap().source_start,0);
    let moved=rich_step(&mut bridge,&mut ed,&mut a,&mut scope_a,pb::ComposeInput { id:1,region:Some(150_000),..Default::default() });
    assert!(moved.input_reply.as_ref().is_some_and(|r|!r.accepted&&r.needs_layout));
    assert!(first(&moved).compose.unwrap().source_start>100_000);
    assert_eq!(ed.active_window().focused_view().viewport.top_byte(),0,"requesting content did not scroll the terminal");
    let escape=|id| pb::ComposeInput { id,kind:17,payload:pb::Key { key:"Escape".into(),..Default::default() }.encode_to_vec(),..Default::default() };
    // Complete A's request, and prove B's original source scope is still valid.
    assert!(rich_step(&mut bridge,&mut ed,&mut a,&mut scope_a,escape(1)).input_reply.unwrap().accepted);
    let mut peer=rich_step(&mut bridge,&mut ed,&mut b,&mut scope_b,escape(1));
    for _ in 0..3 {
        if peer.input_reply.as_ref().unwrap().accepted { break; }
        peer=rich_step(&mut bridge,&mut ed,&mut b,&mut scope_b,escape(1));
    }
    assert!(peer.input_reply.unwrap().accepted,"one client did not inherit the other client's region");
    let typed=rich_step(&mut bridge,&mut ed,&mut a,&mut scope_a,pb::ComposeInput { id:2,kind:17,
        payload:pb::Key { key:"X".into(),..Default::default() }.encode_to_vec(),..Default::default() });
    assert!(typed.input_reply.as_ref().unwrap().accepted);
    let fresh=first(&typed).compose.unwrap();
    assert_eq!(fresh.source_start,0,"a stale byte hint expires after a source edit");
    assert_eq!(fresh.cursors.iter().find(|c|c.primary).unwrap().position,1);
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn pane_scope(window: u64, pane: &pb::Pane) -> pb::ComposeScope {
    let c = pane.compose.as_ref().unwrap();
    pb::ComposeScope { window_id: window, leaf: pane.leaf, buffer: pane.buffer.unwrap(),
        buffer_revision: c.buffer_revision, projection_revision: c.projection_revision,
        selection_revision: c.selection_revision }
}

fn rich_step(bridge: &mut SceneBridge, ed: &mut fresh::app::Editor, app: &mut App,
    scope: &mut pb::ComposeScope, mut input: pb::ComposeInput) -> pb::Frame {
    input.scope = Some(scope.clone());
    let request=input.id;
    app.send(27, input);
    settle_bridge(bridge, ed);
    for _ in 0..10 {
        let frame=app.frame().expect("a reply to the revisioned input");
        for update in &frame.panes {
            if let Some(pane) = &update.pane {
                if pane.leaf == scope.leaf && pane.compose.is_some() { *scope = pane_scope(scope.window_id, pane); }
            }
        }
        app.send(26,pb::Applied { seq:frame.seq });
        if frame.input_reply.as_ref().is_some_and(|reply|reply.id==request) { return frame; }
        settle_bridge(bridge,ed);
    }
    panic!("input reply was not delivered");
}

fn rich_navigation_uses_measured_lines_after_resolving_a_key_chord(root: &std::path::Path, work: &std::path::Path) {
    let config = root.join("home/.config/fresh/config.json");
    let mut settings: serde_json::Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    settings["keybindings"] = json!([{
        "keys":[{"key":"F6","modifiers":["ctrl"]},{"key":"F7","modifiers":["ctrl"]}],
        "action":"move_down", "when":"normal"
    }]);
    std::fs::write(config, settings.to_string()).unwrap();
    let original = "# Heading\n\nWi café iii WWW tail.\n";
    let file = work.join("navigation.md");
    std::fs::write(&file, original).unwrap();
    let mut ed = build_editor(COLS, ROWS, &[file.clone()], true).unwrap();
    for _ in 0..12 { apply_step(&mut ed, &json!({})); }
    apply_step(&mut ed, &json!({"action":"markdownToggleCompose"}));
    for _ in 0..50 { apply_step(&mut ed, &json!({})); std::thread::sleep(Duration::from_millis(10)); }
    let path = root.join("navigation.sock");
    let mut bridge = SceneBridge::bind(&path, &ed).unwrap();
    let mut app = App::connect(&path);
    app.send(16, pb::ClientHello { version: 1, rich_compose: true, ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let scene = pb::ServerHello::decode(&app.receive().unwrap().1[..]).unwrap().scene.unwrap();
    let pane = scene.panes.iter().find_map(|p| p.pane.as_ref().filter(|p| p.compose.is_some())).unwrap();
    let mut scope = pane_scope(scene.window_id.unwrap(), pane);
    let stop = |source: usize, x: f64, affinity: u32| pb::ComposeStop { source: source as u64, x, affinity };
    let row = |number: u64, y: f64, height: f64, start: usize, end: usize, points: Vec<(usize,f64)>| {
        let first = stop(points.first().unwrap().0, points.first().unwrap().1, 0);
        let last = stop(points.last().unwrap().0, points.last().unwrap().1, 1);
        let mut stops: Vec<_> = points.into_iter().map(|(s,x)| stop(s,x,0)).collect();
        stops.push(last.clone());
        pb::ComposeLine { number, y, height, source_start: start as u64, source_end: end as u64,
            start: Some(first), end: Some(last), stops }
    };
    let mut body: Vec<(usize,f64)> = Vec::new();
    let mut x = 0.0;
    for (i,ch) in original[11..24].char_indices() {
        body.push((11+i,x));
        x += if ch == 'W' {18.0} else if ch == 'i' {4.0} else if ch == ' ' {5.0} else {9.0};
    }
    body.push((24,x));
    let mut tail = Vec::new();
    x = 0.0;
    for (i,ch) in original[24..original.len()-1].char_indices() {
        tail.push((24+i,x));
        x += if ch == 'W' {18.0} else {9.0};
    }
    tail.push((original.len()-1,x));
    let mut geometry = pb::ComposeLayout { id: 7, page_height: 75.0, source_start: 0,
        source_end: original.len() as u64, document_len: original.len() as u64, total_lines: 6, height: 134.0, style_revision:1,
        lines: vec![
            row(0,0.0,30.0,0,5,vec![(0,0.0),(1,10.0),(2,20.0),(3,36.0),(4,46.0),(5,54.0)]),
            row(1,30.0,30.0,5,10,vec![(5,0.0),(6,10.0),(7,21.0),(8,34.0),(9,44.0)]),
            row(2,60.0,14.0,10,11,vec![(10,0.0)]),
            row(3,74.0,20.0,11,24,body), row(4,94.0,20.0,24,original.len(),tail),
            row(5,114.0,20.0,original.len(),original.len(),vec![(original.len(),0.0)]),
        ] };
    for i in 0..geometry.lines.len()-1 {
        let end = geometry.lines[i].end.as_ref().unwrap().source;
        if end == geometry.lines[i+1].start.as_ref().unwrap().source {
            geometry.lines[i].stops.retain(|s|s.source != end || s.affinity != 0);
        }
    }
    let cursor = |ed: &fresh::app::Editor| ed.active_window().active_cursors().primary().position;
    let action = |id, name: &str, layout: Option<pb::ComposeLayout>| pb::ComposeInput { id, kind: 21,
        payload: pb::Action { name: name.into(), args_json: String::new() }.encode_to_vec(),
        layout_id: layout.as_ref().map_or(7,|l|l.id), layout, ..Default::default() };
    let prefix = pb::ComposeInput { id: 1, kind: 17, payload: pb::Key { key:"F6".into(),ctrl:true,..Default::default() }.encode_to_vec(), ..Default::default() };
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,prefix).input_reply.unwrap().accepted);
    let mut chord = pb::ComposeInput { id: 2, kind: 17, payload: pb::Key { key:"F7".into(),ctrl:true,..Default::default() }.encode_to_vec(), ..Default::default() };
    let reply = rich_step(&mut bridge,&mut ed,&mut app,&mut scope,chord.clone()).input_reply.unwrap();
    assert!(!reply.accepted && reply.needs_layout, "the resolved motion waits for geometry");
    assert_eq!(cursor(&ed),0);
    // A prompt appearing during the measurement round trip remains usable.
    // Its raw UI key does not consume or re-resolve the deferred chord action.
    apply_step(&mut ed,&json!({"action":"command_palette"}));
    bridge.push_scene(&mut ed,COLS,ROWS);
    let prompt = app.frame().unwrap();
    assert!(prompt.palette.as_ref().is_some_and(|p|p.value.is_some()));
    assert!(prompt.panes.iter().all(|p|p.pane.as_ref().and_then(|p|p.compose.as_ref()).is_none_or(|c|!c.keyboard)));
    app.send(26,pb::Applied { seq:prompt.seq });
    app.send(17,pb::Key { key:"Escape".into(),..Default::default() });
    settle_bridge(&mut bridge,&mut ed);
    let closed = app.frame().unwrap();
    for update in &closed.panes {
        if let Some(pane) = update.pane.as_ref().filter(|p|p.compose.is_some()) { scope=pane_scope(scope.window_id,pane); }
    }
    app.send(26,pb::Applied { seq:closed.seq });
    chord.layout_id = 7; chord.layout = Some(geometry.clone());
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,chord).input_reply.unwrap().accepted);
    assert_eq!(cursor(&ed),5,"Down moved within the heading, not to the next terminal row");
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(3,"move_up",None)).input_reply.unwrap().accepted);
    assert_eq!(cursor(&ed),0,"the measured geometry was cached by this client");
    let end = rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(4,"move_line_end",None)).input_reply.unwrap();
    assert!(end.accepted); assert_eq!(cursor(&ed),5); assert_eq!(end.carets[0].affinity,1);
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(5,"move_down",None)).input_reply.unwrap().accepted);
    assert_eq!(cursor(&ed),9,"an upstream wrap boundary belongs to the preceding visual line");
    let point = pb::ComposeInput { id:6,position:Some(13),..Default::default() };
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,point).input_reply.unwrap().accepted);
    for (id,name,expected,send) in [(7,"move_up",10,true),(8,"move_up",7,false),(9,"move_down",10,false),(10,"move_down",13,false)] {
        let reply = rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(id,name,send.then(||geometry.clone()))).input_reply.unwrap();
        if !reply.accepted {
            assert!(reply.needs_layout,"a decoration change requires current geometry");
            assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(id,name,Some(geometry.clone()))).input_reply.unwrap().accepted);
        }
        assert_eq!(cursor(&ed),expected,"pixel goal survives the short blank row");
    }
    let mut zoomed=geometry.clone(); zoomed.id=8; zoomed.style_revision=2; zoomed.height*=3.0;
    for line in &mut zoomed.lines {
        line.y*=3.0; line.height*=3.0;
        line.start.as_mut().unwrap().x*=3.0; line.end.as_mut().unwrap().x*=3.0;
        for stop in &mut line.stops { stop.x*=3.0; }
    }
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(11,"select_down",Some(zoomed))).input_reply.unwrap().accepted);
    assert_eq!(ed.active_window().active_cursors().primary().anchor,Some(13));
    assert_eq!(cursor(&ed),25,"changed typography resets the previous pixel goal");
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(12,"move_document_start",None)).input_reply.unwrap().accepted);
    let mut sparse = geometry.clone(); sparse.id=8; sparse.lines.retain(|line|line.number!=3);
    let missing = rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(13,"move_page_down",Some(sparse))).input_reply.unwrap();
    assert!(!missing.accepted && missing.needs_layout); assert_eq!(missing.need_y,Some(75.0)); assert_eq!(cursor(&ed),0);
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(13,"move_page_down",Some(geometry.clone()))).input_reply.unwrap().accepted);
    assert_eq!(cursor(&ed),11,"PageDown followed measured heading heights");
    apply_step(&mut ed,&json!({"key":"ArrowDown"}));
    assert_eq!(cursor(&ed),original.len(),"a subsequent terminal input did not inherit native geometry");
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    settle_bridge(&mut bridge,&mut ed);
    bridge.push_scene(&mut ed,COLS,ROWS);
    let updated = app.frame().unwrap();
    for update in updated.panes {
        if let Some(pane) = update.pane.filter(|p|p.compose.is_some()) { scope = pane_scope(scope.window_id,&pane); }
    }
    assert!(!original.is_char_boundary(18));
    geometry.lines[3].stops[0].source = 18;
    let mut invalid = action(14,"move_down",Some(geometry)); invalid.scope=Some(scope);
    app.send(27,invalid); settle_bridge(&mut bridge,&mut ed);
    let (kind,payload) = app.receive().expect("malformed geometry gets an error");
    assert_eq!(kind,3);
    assert!(pb::Error::decode(&payload[..]).unwrap().reason.contains("does not match"));
    assert_eq!(cursor(&ed),original.len(),"invalid UTF-8 caret positions cannot move the cursor");
    let mut malformed = App::connect(&path);
    malformed.send(16,pb::ClientHello { version:1,rich_compose:true,..Default::default() });
    settle_bridge(&mut bridge,&mut ed);
    let fresh = pb::ServerHello::decode(&malformed.receive().unwrap().1[..]).unwrap().scene.unwrap();
    let pane = fresh.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    malformed.send(27,pb::ComposeInput { id:1,scope:Some(pane_scope(fresh.window_id.unwrap(),pane)),kind:17,
        payload:vec![0x0a,0x01,0xff],..Default::default() });
    settle_bridge(&mut bridge,&mut ed);
    assert_eq!(malformed.receive().unwrap().0,3,"malformed nested key payload is rejected");
    apply_step(&mut ed,&json!({"key":"ArrowUp"}));
    assert_ne!(cursor(&ed),original.len(),"a failed scene input also removes its navigation context");
}

fn rich_compose_edits_keep_source_and_retry_obsolete_input(root: &std::path::Path, work: &std::path::Path) {
    let original = "# Heading\r\n\r\nSome **strong** café text.\r\n\r\n![alt](images/a.png)\r\n";
    let file = work.join("compose.md");
    std::fs::write(&file, original).unwrap();
    let mut ed = build_editor(COLS, ROWS, &[file.clone()], true).unwrap();
    for _ in 0..12 { apply_step(&mut ed, &json!({})); }
    apply_step(&mut ed, &json!({"action":"markdownToggleCompose"}));
    for _ in 0..50 {
        apply_step(&mut ed, &json!({}));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ed.active_window().focused_view().view_mode, fresh::state::ViewMode::PageView,
        "the real Compose plugin enabled its presentation");
    let path = root.join("compose.sock");
    let mut bridge = SceneBridge::bind(&path, &ed).unwrap();
    let mut grid = App::connect(&path);
    let mut rich = App::connect(&path);
    grid.send(16, pb::ClientHello { version: 1, ..Default::default() });
    rich.send(16, pb::ClientHello { version: 1, rich_compose: true, ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let grid_scene = pb::ServerHello::decode(&grid.receive().unwrap().1[..]).unwrap().scene.unwrap();
    assert!(grid_scene.panes.iter().all(|p| p.pane.as_ref().unwrap().compose.is_none()));
    let scene = pb::ServerHello::decode(&rich.receive().unwrap().1[..]).unwrap().scene.unwrap();
    let pane = scene.panes.iter().find_map(|p| p.pane.as_ref().filter(|p| p.compose.is_some())).unwrap();
    let compose = pane.compose.as_ref().unwrap();
    assert!(compose.spans.iter().any(|s| s.role == 1), "a semantic heading");
    assert_eq!(compose.images.len(), 1);
    assert_eq!(compose.images[0].target, "images/a.png");
    let scope = |pane: &pb::Pane| {
        let c = pane.compose.as_ref().unwrap();
        pb::ComposeScope { window_id: scene.window_id.unwrap(), leaf: pane.leaf, buffer: pane.buffer.unwrap(),
            buffer_revision: c.buffer_revision, projection_revision: c.projection_revision,
            selection_revision: c.selection_revision }
    };
    let initial = scope(pane);
    let at = original.find("café").unwrap();
    rich.send(27, pb::ComposeInput { id: 1, scope: Some(initial.clone()), position: Some(at as u64), ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let placed = rich.frame().unwrap();
    assert!(placed.input_reply.unwrap().accepted);
    assert_eq!(ed.active_window().active_cursors().primary().position, at);
    rich.send(26, pb::Applied { seq: placed.seq });
    let before_edit = scope(placed.panes.iter().find_map(|p| p.pane.as_ref().filter(|p| p.compose.is_some())).unwrap());
    let key = pb::ComposeInput { id: 2, scope: Some(before_edit.clone()), kind: 17,
        payload: pb::Key { key: "X".into(), ..Default::default() }.encode_to_vec(), ..Default::default() };
    let mut obsolete_cursor = key.clone();
    obsolete_cursor.scope = Some(initial);
    rich.send(27, obsolete_cursor);
    settle_bridge(&mut bridge, &mut ed);
    let stale_cursor = rich.frame().unwrap();
    assert!(!stale_cursor.input_reply.unwrap().accepted, "cursor-only changes invalidate pending input too");
    rich.send(26, pb::Applied { seq: stale_cursor.seq });
    rich.send(27, key.clone());
    settle_bridge(&mut bridge, &mut ed);
    let typed = rich.frame().unwrap();
    assert!(typed.input_reply.unwrap().accepted);
    rich.send(26, pb::Applied { seq: typed.seq });
    let current = scope(typed.panes.iter().find_map(|p| p.pane.as_ref().filter(|p| p.compose.is_some())).unwrap());
    let source = |ed: &fresh::app::Editor| {
        let state = ed.active_window().active_state();
        String::from_utf8(state.buffer.slice_bytes(0..state.buffer.len())).unwrap()
    };
    assert_eq!(source(&ed), original.replacen("café", "Xcafé", 1));
    // Replaying an accepted request cannot type the character twice.
    rich.send(27, key);
    settle_bridge(&mut bridge, &mut ed);
    let duplicate = rich.frame().unwrap();
    assert!(duplicate.input_reply.unwrap().accepted);
    assert_eq!(source(&ed), original.replacen("café", "Xcafé", 1));
    rich.send(26, pb::Applied { seq: duplicate.seq });
    let mut undo = pb::ComposeInput { id: 3, scope: Some(before_edit), kind: 21,
        payload: pb::Action { name: "undo".into(), args_json: String::new() }.encode_to_vec(), ..Default::default() };
    rich.send(27, undo.clone());
    settle_bridge(&mut bridge, &mut ed);
    let stale = rich.frame().unwrap();
    assert!(!stale.input_reply.unwrap().accepted, "obsolete source geometry is refused");
    assert_eq!(source(&ed), original.replacen("café", "Xcafé", 1));
    rich.send(26, pb::Applied { seq: stale.seq });
    undo.scope = Some(current);
    rich.send(27, undo);
    settle_bridge(&mut bridge, &mut ed);
    assert!(rich.frame().unwrap().input_reply.unwrap().accepted);
    assert_eq!(source(&ed), original, "Fresh's normal undo restores the exact CRLF source");
    assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes(), "presentation never saved or rewrote the source");
}
