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
    window: u64,
    panes: Vec<pb::Pane>,
    sequence: u64,
}

impl App {
    fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect");
        stream.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        Self { stream, window:0, panes:Vec::new(), sequence:0 }
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
        if kind==1 {
            if let Ok(hello)=pb::ServerHello::decode(&payload[..]) { if let Some(frame)=hello.scene { self.observe(&frame); } }
        }
        Some((kind, payload))
    }

    fn frame(&mut self) -> Option<pb::Frame> {
        let (kind, payload) = self.receive()?;
        assert_eq!(kind, 2, "a Frame");
        let frame=pb::Frame::decode(&payload[..]).expect("decode a Frame");
        self.observe(&frame);
        Some(frame)
    }

    fn observe(&mut self,frame:&pb::Frame) {
        self.sequence = frame.seq;
        if let Some(window)=frame.window_id {self.window=window;}
        if let Some(count)=frame.pane_len {self.panes.resize(count as usize,pb::Pane::default());}
        for update in &frame.panes {
            if let Some(pane)=&update.pane {
                if let Some(compose) = pane.compose.as_ref().filter(|c|c.ready == Some(false)) {
                    assert!(compose.segments.iter().all(|s|s.kind == 0), "pending formatting cannot expose partial conceal/replacement maps");
                    assert!(compose.images.iter().all(|image|!image.rendered && !image.source_preview));
                    assert_eq!(compose.source_end-compose.source_start, compose.text.len() as u64);
                }
                let index=update.index as usize;
                if self.panes.len()<=index {self.panes.resize(index+1,pb::Pane::default());}
                self.panes[index]=pane.clone();
            }
        }
    }
}

/// One pass of the daemon's loop for the bridge: read, push, write.
fn pass(bridge: &mut SceneBridge, ed: &mut fresh::app::Editor) {
    let (mut cols, mut rows) = (COLS, ROWS);
    let poll = bridge.poll(ed, &mut cols, &mut rows);
    let changed = fresh::app::editor_tick(ed, || Ok(())).unwrap();
    if poll.mutated || poll.frame_owed || poll.resize_dirty || changed {
        bridge.push_scene(ed, COLS, ROWS);
    }
    bridge.maintain();
}

fn ready_scene(bridge: &mut SceneBridge, ed: &mut fresh::app::Editor, app: &mut App) -> pb::Frame {
    let hello = pb::ServerHello::decode(&app.receive().expect("rich hello").1[..]).unwrap().scene.unwrap();
    app.send(26, pb::Applied { seq: hello.seq });
    app.stream.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while app.panes.iter().any(|pane| pane.compose.as_ref().is_some_and(|c|c.ready == Some(false))) {
        assert!(std::time::Instant::now() < deadline, "Compose formatting did not complete");
        pass(bridge, ed);
        if let Some(frame) = app.frame() { app.send(26, pb::Applied { seq: frame.seq }); }
        std::thread::sleep(Duration::from_millis(2));
    }
    app.stream.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let errors:Vec<_>=app.panes.iter().filter_map(|pane|pane.compose.as_ref()).filter(|c|!c.error.is_empty()).map(|c|c.error.as_str()).collect();
    assert!(errors.is_empty(), "formatting must succeed, not fall back after a timeout: {errors:?}");
    pb::Frame { seq: app.sequence, window_id: Some(app.window), pane_len: Some(app.panes.len() as u32),
        panes: app.panes.iter().enumerate().map(|(index,pane)|pb::PaneUpdate {index:index as u32,pane:Some(pane.clone())}).collect(), ..Default::default() }
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
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
    }
    let root = std::env::temp_dir().join(format!("fresh-scene-socket-{}", std::process::id()));
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(home.join(".config/fresh")).expect("home");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::write(
        home.join(".config/fresh/config.json"),
        json!({
            "check_for_updates": false,
            "clipboard": {"use_osc52": false, "use_system_clipboard": false},
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

    let mut invalid_tables = App::connect(&path);
    invalid_tables.send(16,pb::ClientHello {version:1,measured_tables:true,..Default::default()});
    settle_bridge(&mut bridge,&mut ed);
    let (kind,payload)=invalid_tables.receive().expect("an invalid table capability is refused");
    assert_eq!(kind,3);
    assert!(pb::Error::decode(&payload[..]).unwrap().reason.contains("require rich_compose"));

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
    rich_viewports_are_independent_and_rebase_queued_scrolls(&root, &work);
    rich_input_intents_rebase_and_cancel_when_the_view_changes(&root, &work);
    rich_images_use_binary_chunks_and_obsolete_reads_do_not_apply(&root,&work);
    rich_image_objects_select_edit_move_delete_and_undo_as_source(&root,&work);
    rich_image_source_previews_reveal_nested_and_reference_markup(&root,&work);
    rich_list_markers_are_source_anchors_not_cell_widths(&root,&work);
    rich_native_ornaments_reveal_and_edit_their_source_without_changing_terminal_compose(&root,&work);
    rich_native_content_is_independent_of_terminal_markdown_measure(&root,&work);
    rich_native_code_frames_reveal_and_edit_parser_owned_source(&root,&work);
    rich_measured_tables_negotiate_source_cells_and_preserve_editor_history(&root,&work);
    rich_table_motion_crosses_disjoint_source_windows(&root,&work);
    rich_table_edges_cross_disjoint_source_windows(&root,&work);
    rich_text_replacements_rebase_and_preserve_source_undo_and_other_cursors(&root,&work);
    scoped_compose_commands_follow_their_window_and_buffer_lifetime(&root);
    drop(std::fs::remove_dir_all(&root));
}

fn scoped_compose_commands_follow_their_window_and_buffer_lifetime(root: &std::path::Path) {
    use fresh_core::api::{LinesBatch, PluginCommand};
    let mut editor = build_editor(COLS, ROWS, &[], false).unwrap();
    apply_step(&mut editor, &json!({"type":"**alpha**\n"}));
    let original = editor.active_session_id();
    let id = editor.active_window().active_buffer();
    let state = editor.active_window().active_state();
    let batch = LinesBatch { window_id: original.0, buffer_id: id, buffer_epoch: state.marker_list.instance_id(), revision: state.buffer.version(), id: 1 };
    let other_root = root.join("other-window");
    std::fs::create_dir_all(&other_root).unwrap();
    let other = editor.create_window_at(other_root, "Other".into());
    editor.set_active_window(other);
    apply_step(&mut editor, &json!({"type":"other\n"}));
    let command = || PluginCommand::AddConceal { buffer_id: id,
        namespace: fresh_core::overlay::OverlayNamespace::from_string("scope-test".into()), start: 0, end: 2,
        replacement: None, epoch: Some(batch.revision), activation: None };
    editor.handle_plugin_command(PluginCommand::ScopedCompose { batch: batch.clone(), command: Box::new(command()) }).unwrap();
    assert_eq!(editor.active_session_id(), other, "applying a background decoration cannot steal workspace focus");
    let shown = |editor: &fresh::app::Editor, source: &str| {
        let state = editor.active_window().active_state();
        state.conceals.project_text(source, 0, &state.marker_list, &[]).unwrap().text
    };
    assert_eq!(shown(&editor, "other\n"), "other\n");
    editor.set_active_window(original);
    assert_eq!(shown(&editor, "**alpha**\n"), "alpha**\n", "the decoration reaches its original window");
    let mut replaced = batch.clone(); replaced.buffer_epoch += 1;
    editor.handle_plugin_command(PluginCommand::ScopedCompose { batch: replaced,
        command: Box::new(PluginCommand::ClearConcealNamespace { buffer_id: id,
            namespace: fresh_core::overlay::OverlayNamespace::from_string("scope-test".into()) }) }).unwrap();
    assert_eq!(shown(&editor, "**alpha**\n"), "alpha**\n", "a different buffer lifetime cannot clear current source decorations");
}

fn rich_measured_tables_negotiate_source_cells_and_preserve_editor_history(root:&std::path::Path,work:&std::path::Path) {
    let text=|ed:&fresh::app::Editor|{let b=&ed.active_window().active_state().buffer;String::from_utf8(b.slice_bytes(0..b.len())).unwrap()};
    let original="# Native table cells\r\n\r\n| Text | Value | Image |\r\n| :--- | :---: | ---: |\r\n| **café 👩🏽‍💻** and [link](target.md) with enough proportional words to wrap within a measured cell | `a\\|b` &amp; | ![**pic**](<a b.png>) |\r\n| | empty | |\r\n| short |\r\n\r\n> A | B\r\n> --- | ---\r\n> quoted | value\r\n";
    let original=original.replace("| short |\r\n", &format!("| short |\r\n| {} | long | |\r\n||||\r\n", "🚀".repeat(50)));
    let file=work.join("native-tables.md"); std::fs::write(&file,&original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("native-tables.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut legacy=App::connect(&path);
    legacy.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    ready_scene(&mut bridge,&mut ed,&mut legacy);
    let old=legacy.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert!(old.tables.is_empty());
    assert!(old.text.contains("| Text | Value | Image |"));
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,measured_tables:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let compose=pane.compose.as_ref().unwrap();
    assert_eq!(compose.tables.len(),2);
    assert_eq!(compose.tables[0].alignments,[1,2,3]);
    assert_eq!(compose.tables[0].rows.len(),6);
    assert!(compose.tables[0].rows[3].cells[1..].iter().all(|c|c.missing));
    assert!(compose.tables[0].rows[5].cells.iter().all(|c|!c.missing && c.start==c.end));
    assert!(compose.text.contains("Text\tValue\tImage\r\n"));
    assert!(compose.text.contains("café 👩🏽‍💻 and link"));
    assert!(compose.text.contains(&"🚀".repeat(50)), "native cells retain long supplementary-character content without wrapping it on the wire");
    assert!(compose.text.contains("a|b &\t\u{fffc}"));
    if let Some(path)=std::env::var_os("FC_COMPOSE_TABLE_SCENE_FILE") {
        let mut fixture=compose.clone(); fixture.asset_authority=1; fixture.asset_document=1;
        std::fs::write(path,fixture.encode_to_vec()).unwrap();
    }
    let empty=compose.tables[0].rows[2].cells[0].start;
    let missing=compose.tables[0].rows[3].content_end;
    let image=compose.images[0].start;
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    let word=original.find("café").unwrap()+2;
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:1,position:Some(word as u64),..Default::default()});
    let current=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert!(current.tables[0].rows[1].cells[0].revealed && !current.tables[0].rows[1].source);
    assert!(current.text.contains("**café 👩🏽‍💻** and [link](target.md)"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:2,kind:20,payload:pb::Paste {text:"X".into()}.encode_to_vec(),..Default::default()});
    assert!(text(&ed).contains("caXfé"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:3,kind:21,payload:pb::Action {name:"undo".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    let separator=original.find(":---").unwrap();
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:4,position:Some(separator as u64),..Default::default()});
    let current=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert!(current.tables[0].rows.iter().any(|r|r.source && r.cells.is_empty()));
    assert!(current.text.contains("| :--- | :---: | ---: |\r\n"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:5,position:Some(empty),..Default::default()});
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:6,kind:20,payload:pb::Paste {text:"filled".into()}.encode_to_vec(),..Default::default()});
    assert!(text(&ed).contains("|filled | empty"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:7,kind:21,payload:pb::Action {name:"undo".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:8,position:Some(missing),..Default::default()});
    assert!(app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().tables[0].rows[3].source);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:9,kind:21,payload:pb::Action {name:"save".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:10,position:Some(image+3),..Default::default()});
    let current=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert!(current.images[0].source_preview && !current.images[0].rendered);
    assert!(current.tables[0].rows[1].cells[2].revealed && !current.tables[0].rows[1].source);
    assert!(current.text.contains("![**pic**](<a b.png>)"));
    if let Some(path)=std::env::var_os("FC_COMPOSE_TABLE_SOURCE_SCENE_FILE") {
        let mut fixture=current.clone(); fixture.asset_authority=1; fixture.asset_document=1;
        std::fs::write(path,fixture.encode_to_vec()).unwrap();
    }
    {
        let data=std::env::var_os("FC_COMPOSE_TABLE_LAYOUT_FILE").map(|path|std::fs::read(path).unwrap())
            .unwrap_or_else(||include_bytes!("scene_proto/fixtures/compose-table-layout.pb").to_vec());
        let mut geometry=pb::ComposeLayout::decode(&data[..]).unwrap();
        let table=current.tables[0].clone();
        let first=geometry.lines.iter().find(|line|line.cell.as_ref().is_some_and(|c|c.table_start==current.tables[0].start && c.row_start==current.tables[0].rows[0].start && c.column==Some(0))).unwrap();
        let expected=first.start.as_ref().unwrap().source as usize;
        geometry.page_height=first.cell.as_ref().unwrap().row_y+1.0;
        let layout_id=geometry.id;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:11,position:Some(0),..Default::default()});
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:12,kind:21,layout_id,layout:Some(geometry.clone()),
            payload:pb::Action {name:"move_page_down".into(),..Default::default()}.encode_to_vec(),..Default::default()});
        assert_eq!(ed.active_window().active_cursors().primary().position,expected,
            "Fresh accepts real Core Text cell geometry and pages into the expected source cell");
        let action=|id,name:&str|pb::ComposeInput {id,kind:21,layout_id,layout:Some(geometry.clone()),
            payload:pb::Action {name:name.into(),..Default::default()}.encode_to_vec(),..Default::default()};
        let header=geometry.lines.iter().find(|line|line.cell.as_ref().is_some_and(|c|c.row_start==table.rows[0].start && c.column==Some(1))).unwrap();
        let parts=geometry.lines.iter().find_map(|line|line.cell.as_ref().filter(|c|c.row_start==table.rows[1].start && c.column==Some(1))).unwrap().parts;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:13,position:Some(header.start.as_ref().unwrap().source),..Default::default()});
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(14,"move_down"));
        let cursor=|ed:&fresh::app::Editor|ed.active_window().active_cursors().primary().position as u64;
        let owned=|position:u64,row:usize|table.rows[row].cells[1].start<=position && position<=table.rows[row].cells[1].end;
        assert!(owned(cursor(&ed),1),"Down stays in the second column");
        let mut id=15;
        for _ in 1..parts {
            rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(id,"move_down")); id+=1;
            assert!(owned(cursor(&ed),1),"wrapped cell lines precede the next logical row");
        }
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(id,"move_down")); id+=1;
        assert!(owned(cursor(&ed),2),"the next row keeps the logical column");
        let anchor=cursor(&ed);
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(id,"select_down"));
        assert_eq!(cursor(&ed),table.rows[3].content_end,"a missing cell enters its real structural source");
        assert_eq!(ed.active_window().active_cursors().primary().anchor,Some(anchor as usize));
        assert!(app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().tables[0].rows[3].source);
        if let Some(path)=std::env::var_os("FC_COMPOSE_TABLE_MISSING_SCENE_FILE") {
            let mut fixture=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().clone(); fixture.asset_authority=1; fixture.asset_document=1;
            std::fs::write(path,fixture.encode_to_vec()).unwrap();
        }
        let data=std::env::var_os("FC_COMPOSE_TABLE_SOURCE_LAYOUT_FILE").map(|path|std::fs::read(path).unwrap())
            .unwrap_or_else(||include_bytes!("scene_proto/fixtures/compose-table-source-layout.pb").to_vec());
        let source_geometry=pb::ComposeLayout::decode(&data[..]).unwrap();
        assert_ne!(source_geometry.style_revision,geometry.style_revision,"exercise a pixel-goal reset at source reveal");
        let mut invalid=source_geometry.clone();
        invalid.lines.iter_mut().find(|line|line.cell.as_ref().is_some_and(|c|c.row_start==table.rows[0].start && c.column==Some(2)))
            .unwrap().cell.as_mut().unwrap().column=Some(99);
        let mut bad=App::connect(&path);
        bad.send(16,pb::ClientHello {version:1,rich_compose:true,measured_tables:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
        let scene=ready_scene(&mut bridge,&mut ed,&mut bad);
        let pane=scene.panes.iter().find_map(|p|p.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
        let before=cursor(&ed);
        bad.send(27,pb::ComposeInput {id:1,scope:Some(pane_scope(scene.window_id.unwrap(),pane)),kind:21,layout_id:invalid.id,layout:Some(invalid),
            payload:pb::Action {name:"move_down".into(),..Default::default()}.encode_to_vec(),..Default::default()});
        settle_bridge(&mut bridge,&mut ed);
        let (kind,payload)=bad.receive().unwrap(); assert_eq!(kind,3);
        assert!(pb::Error::decode(&payload[..]).unwrap().reason.contains("does not match"));
        assert_eq!(cursor(&ed),before,"foreign cell ownership cannot move a cursor");
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:id+1,kind:21,layout_id:source_geometry.id,layout:Some(source_geometry),
            payload:pb::Action {name:"move_down".into(),..Default::default()}.encode_to_vec(),..Default::default()});
        assert!(owned(cursor(&ed),4),"the logical column survives full-width source reveal and pixel-goal reset");
    }
    // A legacy rich client and the table client share source/history, while
    // each keeps the representation it negotiated in its own hello.
    for _ in 0..4 {
        if let Some(frame)=legacy.frame() {
            legacy.send(26,pb::Applied {seq:frame.seq}); settle_bridge(&mut bridge,&mut ed);
        }
    }
    let old=legacy.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert!(old.tables.is_empty());
    assert!(old.text.contains("| Text | Value | Image |"));
    assert_eq!(text(&ed),original,"table navigation and selection do not rewrite source");
}

fn rich_table_motion_crosses_disjoint_source_windows(root:&std::path::Path,work:&std::path::Path) {
    let original="| Left | Right |\n| --- | --- |\n".to_owned()+"| "+&"x".repeat(90_000)+" | previous-value |\n| origin-left | origin-value |\n";
    let wanted=original.find("| x").unwrap() as u64;
    let origin_row=original.find("| origin-left").unwrap() as u64;
    let origin=original.find("origin-value").unwrap() as u64;
    let destination=original.find("previous-value").unwrap() as u64;
    let file=work.join("table-windows.md"); std::fs::write(&file,&original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 {apply_step(&mut ed,&json!({}));}
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("table-windows.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,measured_tables:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let initial=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=initial.panes.iter().find_map(|p|p.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let mut scope=pane_scope(initial.window_id.unwrap(),pane);
    let epoch=pane.compose.as_ref().unwrap().viewport.as_ref().unwrap().epoch;
    viewport_step(&mut bridge,&mut ed,&mut app,pb::ComposeViewportRequest {id:1,scope:Some(scope.clone()),position:origin_row,epoch,..Default::default()});
    scope=pane_scope(app.window,app.panes.iter().find(|p|p.compose.is_some()).unwrap());
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:1,position:Some(origin),..Default::default()});
    let shown=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert!(shown.source_start>=origin_row && shown.source_start>wanted);
    let make_geometry=|id:u64,compose:&pb::ComposePane,row:u64,point:u64,number:u64,total:u64| {
        let table=compose.tables.iter().find(|t|t.rows.iter().any(|r|r.start==row)).unwrap();
        let cell=&table.rows.iter().find(|r|r.start==row).unwrap().cells[1];
        let stop=pb::ComposeStop {source:point,x:110.0,affinity:0};
        pb::ComposeLayout {id,page_height:20.0,source_start:compose.source_start,source_end:compose.source_end,
            document_len:compose.document_len,total_lines:total,height:40.0,style_revision:1,lines:vec![pb::ComposeLine {
                number,y:8.0,height:10.0,source_start:cell.start,source_end:cell.end,stops:vec![stop.clone()],start:Some(stop.clone()),end:Some(stop),
                cell:Some(pb::ComposeCellLine {table_start:table.start,row_start:row,column:Some(1),part:0,parts:1,x:100.0,width:80.0,row_y:0.0,row_height:30.0})
            }]}
    };
    let first=make_geometry(201,shown,origin_row,origin,1,3);
    let action=|layout:Option<pb::ComposeLayout>|pb::ComposeInput {id:2,kind:21,layout_id:layout.as_ref().map_or(0,|l|l.id),layout,
        payload:pb::Action {name:"select_up".into(),..Default::default()}.encode_to_vec(),..Default::default()};
    let reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(Some(first))).input_reply.unwrap();
    assert!(reply.needs_layout && !reply.accepted); assert_eq!(reply.need_region,Some(wanted));
    assert_eq!(ed.active_window().active_cursors().primary().position as u64,origin);
    let mut request=action(None); request.region=Some(wanted);
    let mut reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,request).input_reply.unwrap();
    for _ in 0..10 {
        if reply.compose.as_ref().is_some_and(|c|c.ready!=Some(false)) {break;}
        settle_bridge(&mut bridge,&mut ed);
        reply=rich_reply(&mut bridge,&mut ed,&mut app,&mut scope,2).input_reply.unwrap();
    }
    let measured=reply.compose.as_ref().unwrap();
    assert!(measured.ready!=Some(false));
    assert!(measured.source_start<=wanted && wanted<measured.source_end);
    assert!(measured.source_end<origin,"the new measurement deliberately excludes the original cursor");
    assert_eq!(reply.need_region,Some(wanted),"the measurement reply must retain its requested anchor");
    let target=make_geometry(202,measured,wanted,destination,3,4);
    let mut sparse=target.clone(); sparse.total_lines=5; sparse.lines[0].cell.as_mut().unwrap().parts=2;
    let reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(Some(sparse))).input_reply.unwrap();
    assert!(reply.needs_layout && !reply.accepted); assert!(reply.need_y.is_some());
    assert_eq!(reply.need_region,None,"a later pixel request must take priority over the old measurement anchor");
    assert_eq!(ed.active_window().active_cursors().primary().position as u64,origin);
    let mut target=target; target.id=203;
    let reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(Some(target))).input_reply.unwrap();
    assert!(reply.accepted,"original motion must complete: accepted={}, region={:?}, y={:?}, error={}",reply.accepted,reply.need_region,reply.need_y,reply.error);
    assert_eq!(ed.active_window().active_cursors().primary().position as u64,destination);
    assert_eq!(ed.active_window().active_cursors().primary().anchor,Some(origin as usize));
    assert_eq!(app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().viewport.as_ref().unwrap().position,origin_row);
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_table_edges_cross_disjoint_source_windows(root:&std::path::Path,work:&std::path::Path) {
    let original="| Left | Right |\n| --- | --- |\n| tiny | ".to_owned()+&"x".repeat(90_000)+" |\n\nTail **café**\n";
    let row=original.find("| tiny").unwrap() as u64;
    let short=original.find("tiny").unwrap() as u64;
    let edge=original.find("\n\nTail").unwrap() as u64+1;
    for backwards in [false,true] {
        let file=work.join(format!("table-edge-{backwards}.md")); std::fs::write(&file,&original).unwrap();
        let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
        for _ in 0..12 {apply_step(&mut ed,&json!({}));}
        apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
        let path=root.join(format!("table-edge-{backwards}.sock")); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
        let mut app=App::connect(&path);
        app.send(16,pb::ClientHello {version:1,rich_compose:true,measured_tables:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
        let initial=ready_scene(&mut bridge,&mut ed,&mut app);
        let pane=initial.panes.iter().find_map(|p|p.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
        let mut scope=pane_scope(initial.window_id.unwrap(),pane);
        if backwards {
            let epoch=pane.compose.as_ref().unwrap().viewport.as_ref().unwrap().epoch;
            viewport_step(&mut bridge,&mut ed,&mut app,pb::ComposeViewportRequest {id:1,scope:Some(scope.clone()),position:edge,epoch,..Default::default()});
            scope=pane_scope(app.window,app.panes.iter().find(|p|p.compose.is_some()).unwrap());
        }
        let origin=if backwards {edge} else {short}; let destination=if backwards {short} else {edge};
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:1,position:Some(origin),..Default::default()});
        let shown=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
        let viewport=shown.viewport.as_ref().unwrap().position;
        let geometry=|id:u64,compose:&pb::ComposePane| {
            let mut result=pb::ComposeLayout {id,page_height:100.0,source_start:compose.source_start,source_end:compose.source_end,
                document_len:compose.document_len,total_lines:3,height:3000.0,style_revision:1,..Default::default()};
            if let Some(table)=compose.tables.iter().find(|t|t.rows.iter().any(|r|r.start==row)) {
                let source=table.rows.iter().find(|r|r.start==row).unwrap();
                result.total_lines=2;
                for column in 0..2 {
                    let cell=&source.cells[column]; let x=column as f64*100.0;
                    let point=if column==0 {short} else {cell.start+1};
                    let stop=pb::ComposeStop {source:point,x:x+10.0,affinity:0};
                    result.lines.push(pb::ComposeLine {number:column as u64,y:8.0,height:20.0,source_start:cell.start,source_end:cell.end,
                        stops:vec![stop.clone()],start:Some(stop.clone()),end:Some(stop),
                        cell:Some(pb::ComposeCellLine {table_start:table.start,row_start:row,column:Some(column as u32),part:0,parts:1,x,width:80.0,row_y:0.0,row_height:3000.0})});
                }
            } else {
                let stop=pb::ComposeStop {source:edge,x:10.0,affinity:0};
                result.lines=vec![pb::ComposeLine {number:0,y:0.0,height:20.0,source_start:edge,source_end:edge+1,
                    stops:vec![stop.clone()],start:Some(stop.clone()),end:Some(stop),cell:None}];
            }
            result
        };
        let action=|layout:Option<pb::ComposeLayout>|pb::ComposeInput {id:2,kind:21,layout_id:layout.as_ref().map_or(0,|l|l.id),layout,
            payload:pb::Action {name:if backwards {"select_up"} else {"select_down"}.into(),..Default::default()}.encode_to_vec(),..Default::default()};
        let first=geometry(301,shown);
        let reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(Some(first))).input_reply.unwrap();
        let probe=if backwards {edge-1} else {edge};
        assert!(reply.needs_layout && !reply.accepted); assert_eq!(reply.need_region,Some(probe));
        assert_eq!(ed.active_window().active_cursors().primary().position as u64,origin);
        let mut request=action(None); request.region=Some(probe);
        let mut reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,request).input_reply.unwrap();
        for _ in 0..10 {
            if reply.compose.as_ref().is_some_and(|c|c.ready!=Some(false)) {break;}
            settle_bridge(&mut bridge,&mut ed); reply=rich_reply(&mut bridge,&mut ed,&mut app,&mut scope,2).input_reply.unwrap();
        }
        let measured=reply.compose.as_ref().unwrap(); assert!(measured.ready!=Some(false));
        if !backwards {assert!(origin<measured.source_start,"leaving the tall row excludes the old source cursor");}
        let target=geometry(302,measured);
        assert!(target.lines.iter().all(|line|origin<line.source_start || origin>line.source_end),
            "the new geometry must finish the motion without the old cursor, even if source context includes its blank line");
        let reply=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,action(Some(target))).input_reply.unwrap();
        assert!(reply.accepted,"table edge motion: backwards={backwards}, region={:?}, y={:?}, error={}",reply.need_region,reply.need_y,reply.error);
        assert_eq!(ed.active_window().active_cursors().primary().position as u64,destination);
        assert_eq!(ed.active_window().active_cursors().primary().anchor,Some(origin as usize));
        assert_eq!(app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().viewport.as_ref().unwrap().position,viewport);
        assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    }
}

fn rich_list_markers_are_source_anchors_not_cell_widths(root:&std::path::Path,work:&std::path::Path) {
    let original="# Lists and images\r\n\r\n12. Parent with enough proportional words to wrap across the available measure while keeping the marker separate from the text.\r\n    - [ ] Child with café 👩🏽‍💻, **bold**, `code` and ![tiny](x.png), followed by more words that wrap under the task body rather than its marker.\r\n    - Child two with more words that wrap across a narrow page and stay aligned with the first letter of its own text.\r\n\r\n> - Quoted item with a [link](https://example.test) and enough words to wrap across the page without losing source positions.\r\n\r\n1) An item with a parenthesis marker and enough words to exercise the same measured continuation geometry as other lists.\r\n\r\nSetext heading\r\n--------------\r\n";
    let file=work.join("lists.md"); std::fs::write(&file,original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("lists.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let compose=pane.compose.as_ref().unwrap();
    let bodies:Vec<_>=compose.spans.iter().filter_map(|s|s.content_start).map(|p|original[p as usize..].split_whitespace().next().unwrap()).collect();
    assert_eq!(bodies,["Parent","Child","Child","Quoted","An"]);
    assert_eq!(compose.images.len(),1);
    if let Some(path)=std::env::var_os("FC_COMPOSE_LIST_SCENE_FILE") {
        let mut fixture=compose.clone(); fixture.asset_authority=1; fixture.asset_document=1;
        std::fs::write(path,fixture.encode_to_vec()).unwrap();
    }
    let first=compose.spans.iter().find(|s|s.role==13).unwrap();
    let marker=first.start; let body=first.content_start.unwrap();
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:1,position:Some(marker),..Default::default()});
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:2,kind:20,
        payload:pb::Paste {text:"1".into()}.encode_to_vec(),..Default::default()});
    let current=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert_eq!(current.spans.iter().find(|s|s.role==13).unwrap().content_start,Some(body+1),
        "editing an ordered marker moves its body source anchor");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:3,kind:21,
        payload:pb::Action {name:"undo".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    let current=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
    assert_eq!(current.spans.iter().find(|s|s.role==13).unwrap().content_start,Some(body));
    apply_step(&mut ed,&json!({"action":"save"}));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_native_content_is_independent_of_terminal_markdown_measure(root:&std::path::Path,work:&std::path::Path) {
    let original="# Native width\r\n\r\n| Content | More |\r\n| --- | --- |\r\n| all café 👩🏽‍💻 cell text must survive even when it cannot fit the terminal allocation | tail |\r\n\r\n```rust\r\nlet wide_value = \"all code content stays available\";\r\n```\r\n\r\n1. Parent\r\n    - Child with enough proportional words to demonstrate that nesting does not depend on the terminal page width.\r\n";
    let file=work.join("native-width.md"); std::fs::write(&file,original).unwrap();
    let mut first=None;
    let mut measured_first=None;
    let mut terminal=Vec::new();
    for measure in [24,80,140] {
        let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
        let mut config=ed.config().clone(); config.editor.page_width=Some(measure); ed.set_config(config);
        for _ in 0..12 { apply_step(&mut ed,&json!({})); }
        apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
        let path=root.join(format!("native-width-{measure}.sock")); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
        let mut app=App::connect(&path);
        app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
        ready_scene(&mut bridge,&mut ed,&mut app);
        let compose=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
        assert!(compose.text.contains("all café 👩🏽‍💻 cell text must survive even when it cannot fit the terminal allocation"));
        assert!(compose.text.contains("\r\nlet wide_value = \"all code content stays available\";\r\n"));
        assert!(!compose.text.contains("```"));
        if let Some(first)=&first { assert_eq!(&compose.text,first,"native content cannot depend on editor.page_width={measure}"); }
        else { first=Some(compose.text.clone()); }
        let mut measured=App::connect(&path);
        measured.send(16,pb::ClientHello {version:1,rich_compose:true,measured_tables:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
        ready_scene(&mut bridge,&mut ed,&mut measured);
        let compose=measured.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
        assert_eq!(compose.tables.len(),1);
        assert!(compose.text.contains("all café 👩🏽‍💻 cell text must survive even when it cannot fit the terminal allocation"));
        let data=(compose.text.clone(),compose.segments.clone(),compose.tables.clone());
        if let Some(first)=&measured_first { assert_eq!(&data,first,"native cell content and source mappings cannot depend on editor.page_width={measure}"); }
        else { measured_first=Some(data); }
        let state=ed.active_window().active_state();
        terminal.push(state.conceals.project_text(original,0,&state.marker_list,&[]).unwrap().text);
        apply_step(&mut ed,&json!({"action":"save"}));
        assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    }
    assert_ne!(terminal[0],terminal[1],"the fixture must exercise different terminal layouts");
}

fn rich_native_code_frames_reveal_and_edit_parser_owned_source(root:&std::path::Path,work:&std::path::Path) {
    let text=|ed:&fresh::app::Editor|{let b=&ed.active_window().active_state().buffer;String::from_utf8(b.slice_bytes(0..b.len())).unwrap()};
    let original="# Native code frames\r\n\r\nProportional prose before the code.\r\n\r\n```rust\r\nlet long_value = \"café 👩🏽‍💻 and enough code to wrap at narrow pixel widths without a terminal frame\";\r\nlet second = 2;\r\n```\r\n\r\n> > ```text\r\n> > quoted code\r\n> > > literal greater-than\r\n> > ```\r\n\r\n1. Parent item\r\n\r\n   ```rs\r\n   nested code\r\n   ```\r\n\r\n- ```\r\n  first child code\r\n  ```\r\n\r\n```\r\n```\r\n\r\n```text\r\nunterminated code\r\n";
    let file=work.join("native-code.md"); std::fs::write(&file,original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("native-code.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let compose=pane.compose.as_ref().unwrap();
    assert_eq!(compose.spans.iter().filter(|s|s.role==12).count(),6);
    assert!(!compose.text.contains("```"));
    assert!(compose.text.contains("> literal greater-than\r\n"));
    assert!(!compose.text.contains("> > quoted code"));
    if let Some(path)=std::env::var_os("FC_COMPOSE_CODE_SCENE_FILE") {
        let mut fixture=compose.clone(); fixture.asset_authority=1; fixture.asset_document=1;
        std::fs::write(path,fixture.encode_to_vec()).unwrap();
    }
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    let opening=original.find("```rust").unwrap();
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:1,position:Some((opening+3) as u64),..Default::default()});
    assert!(app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().text.contains("```rust\r\n"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:2,kind:20,payload:pb::Paste {text:"x".into()}.encode_to_vec(),..Default::default()});
    assert!(text(&ed).contains("```xrust"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:3,kind:21,payload:pb::Action {name:"undo".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    let body=original.find("café").unwrap();
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:4,position:Some(body as u64),..Default::default()});
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:5,kind:20,payload:pb::Paste {text:"X".into()}.encode_to_vec(),..Default::default()});
    assert!(text(&ed).contains("Xcafé"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:6,kind:21,payload:pb::Action {name:"undo".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:7,kind:21,payload:pb::Action {name:"save".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    let first=original.find("let long_value").unwrap();
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:8,position:Some(first as u64),..Default::default()});
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:9,kind:17,payload:pb::Key {key:"ArrowLeft".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    assert!(app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().text.contains("```rust"), "ordinary source movement can reveal the hidden opener");
    assert_eq!(text(&ed),original);
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_native_ornaments_reveal_and_edit_their_source_without_changing_terminal_compose(root:&std::path::Path,work:&std::path::Path) {
    let original="# Native ornaments\r\n\r\nSetext title\r\n--------------\r\n\r\n> Quoted proportional words that wrap across the available measure without a repeated terminal bar.\r\n> A joined continuation that stays in the same paragraph.\r\n>\r\n> > Nested quote words that also wrap across the measure and retain their own source positions.\r\n>\r\n> `> literal code`\r\n>\r\n> ***\r\n\r\n***\r\n\r\n---";
    let file=work.join("ornaments.md"); std::fs::write(&file,original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 {apply_step(&mut ed,&json!({}));}
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("ornaments.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap(); let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let compose=pane.compose.as_ref().unwrap();
    assert!(!compose.text.contains(['─','▌']));
    assert!(!compose.text.contains("--------------"));
    assert!(compose.text.contains("> literal code"),"literal code is not a quote marker");
    assert_eq!(compose.spans.iter().filter(|s|s.role==14).count(),3);
    assert!(compose.text.contains("bar. A joined continuation"),"native quote bars cannot unflow joined source lines");
    let state=ed.active_window().active_state();
    let grid=state.conceals.project_text(original,0,&state.marker_list,&[]).unwrap();
    assert!(grid.text.contains('▌') && grid.text.contains('─'),"terminal decorations are still present on the same buffer");
    if let Some(path)=std::env::var_os("FC_COMPOSE_ORNAMENT_SCENE_FILE") {
        let mut fixture=compose.clone(); fixture.asset_authority=1; fixture.asset_document=1;
        std::fs::write(path,fixture.encode_to_vec()).unwrap();
    }
    let mut scope=pane_scope(hello.window_id.unwrap(),pane); let mut id=0;
    let at=|id,position,extend|pb::ComposeInput {id,position:Some(position),extend,..Default::default()};
    let current=|app:&App|app.panes.iter().find_map(|p|p.compose.clone()).unwrap();
    for position in [original.find("Setext").unwrap()+2,original.find("--------------").unwrap()+2] {
        id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(id,position as u64,false));
        assert!(current(&app).text.contains("--------------\r\n"),"title and underline edits reveal exact source");
    }
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(id,original.find("Quoted").unwrap() as u64,false));
    assert!(current(&app).text.contains("> Quoted"));
    let rule=original.rfind("***").unwrap() as u64;
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(id,rule+1,false));
    assert!(current(&app).text.contains("***"));
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:17,
        payload:pb::Key {key:"Delete".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    assert_eq!(current(&app).spans.iter().filter(|s|s.role==14).count(),2);
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:21,
        payload:pb::Action {name:"undo".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(id,rule,false));
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(id,rule+3,true));
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:21,
        payload:pb::Action {name:"copy".into(),..Default::default()}.encode_to_vec(),..Default::default()});
    assert_eq!(ed.clipboard_text(),"***");
    id+=1; rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(id,original.len() as u64,false));
    assert!(current(&app).text.ends_with("---"),"an EOF rule reveals at its collapsed source edge");
    apply_step(&mut ed,&json!({"action":"save"}));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_text_replacements_rebase_and_preserve_source_undo_and_other_cursors(root:&std::path::Path,work:&std::path::Path) {
    let original="# Text\r\n\r\n**café** 👩🏽‍💻 end\r\n\r\nTail\r\n";
    let file=work.join("text-input.md"); std::fs::write(&file,original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 {apply_step(&mut ed,&json!({}));}
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("text-input.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap(); let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let epoch=pane.compose.as_ref().unwrap().viewport.as_ref().unwrap().epoch;
    let context=pane.compose.as_ref().unwrap().text_context.as_ref().unwrap();
    assert_eq!(context.text,original,"native text services receive the exact Markdown source, including concealed syntax");
    assert_eq!((context.source_start,context.utf16_start,context.selection_start,context.selection_end),(0,0,0,0));
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    let text=|ed:&fresh::app::Editor|{let b=&ed.active_window().active_state().buffer;String::from_utf8(b.slice_bytes(0..b.len())).unwrap()};
    let start=original.find("café").unwrap() as u64; let end=start+"café".len() as u64;
    let replacement=|id,revision,value:&str|pb::ComposeInput {id,kind:20,source_revision:Some(revision),buffer_epoch:epoch,
        replacement_start:Some(start),replacement_end:Some(end),replacement_cursor:Some(0),payload:pb::Paste {text:value.into()}.encode_to_vec(),..Default::default()};
    let action=|id,name:&str|pb::ComposeInput {id,kind:21,payload:pb::Action {name:name.into(),..Default::default()}.encode_to_vec(),..Default::default()};
    let secondary=ed.active_cursors_mut().add(fresh::model::cursor::Cursor::new(original.len()-2));
    let revision=scope.buffer_revision;
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,replacement(1,revision,"東京"));
    assert_eq!(text(&ed),original.replacen("café","東京",1));
    assert_eq!(ed.active_cursors().get(secondary).unwrap().position,original.len()-1);
    assert_eq!(ed.active_cursors().count(),2,"an explicit range does not collapse secondary cursors");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(2,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(3,"redo")); assert_eq!(text(&ed),original.replacen("café","東京",1));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(4,"undo")); assert_eq!(text(&ed),original);
    ed.active_cursors_mut().insert_with_id(fresh::model::event::CursorId(0),fresh::model::cursor::Cursor::new(0));
    let captured=scope.buffer_revision;
    *ed.active_cursors_mut().primary_mut()=fresh::model::cursor::Cursor::new(0);
    ed.active_cursors_mut().remove_secondary();
    ed.paste_text("前置 ".into());
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,replacement(5,captured,"東京"));
    assert_eq!(text(&ed),format!("前置 {}",original.replacen("café","東京",1)),"reflow/cursor movement cannot retarget an explicit replacement");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(6,"undo")); assert_eq!(text(&ed),format!("前置 {original}"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(7,"undo")); assert_eq!(text(&ed),original);
    let captured=scope.buffer_revision;
    *ed.active_cursors_mut().primary_mut()=fresh::model::cursor::Cursor::with_selection(start as usize,start as usize+1);
    ed.paste_text("k".into());
    let changed=text(&ed);
    let cancelled=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,replacement(8,captured,"東京"));
    assert!(cancelled.input_reply.unwrap().cancelled); assert_eq!(text(&ed),changed);
    let retry=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,replacement(8,captured,"wrong"));
    assert!(retry.input_reply.unwrap().cancelled); assert_eq!(text(&ed),changed);
    let current=ed.active_window().active_state().buffer.version();
    let mut invalid=replacement(9,current,""); invalid.replacement_start=Some(end-1); invalid.replacement_end=Some(end-1);
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,invalid).input_reply.unwrap().cancelled,"UTF-8 interiors are not legal replacement boundaries");
    let current=ed.active_window().active_state().buffer.version();
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,replacement(10,current,""));
    assert_eq!(text(&ed),original.replacen("café","",1),"empty replacement deletes the intended source range");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(11,"undo")); assert_eq!(text(&ed),changed);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(12,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(13,"save"));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    *ed.active_cursors_mut().primary_mut()=fresh::model::cursor::Cursor::new(0);
    ed.active_cursors_mut().add(fresh::model::cursor::Cursor::new(original.len()));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:14,kind:20,buffer_epoch:epoch,text_input:true,
        payload:pb::Paste {text:"甲\n乙".into()}.encode_to_vec(),..Default::default()});
    assert_eq!(text(&ed),format!("甲\r\n乙{original}甲\r\n乙"),"text input never distributes its lines among cursors like a clipboard column paste");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(15,"undo")); assert_eq!(text(&ed),original);
    let buffer=ed.active_window().active_buffer();
    ed.active_window_mut().mark_buffer_read_only(buffer,true);
    let current=ed.active_window().active_state().buffer.version();
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,replacement(16,current,"not written")).input_reply.unwrap().cancelled);
    assert_eq!(text(&ed),original);
    let blocked=pb::ComposeInput {id:17,kind:20,buffer_epoch:epoch,text_input:true,
        payload:pb::Paste {text:"not written".into()}.encode_to_vec(),..Default::default()};
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,blocked).input_reply.unwrap().cancelled);
    assert_eq!(text(&ed),original);
    ed.active_window_mut().mark_buffer_read_only(buffer,false);
    let mut missing=replacement(18,current,"not written"); missing.replacement_cursor=Some(999_999);
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,missing).input_reply.unwrap().cancelled);
    assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id:19,kind:20,
        payload:pb::Paste {text:"甲\n乙".into()}.encode_to_vec(),..Default::default()});
    assert_eq!(text(&ed),format!("甲{original}乙"),"ordinary clipboard paste retains its column distribution");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(20,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(21,"save"));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    // A dialog can acquire the keyboard after AppKit captured a document
    // replacement. The same input waits, then applies once after raw Escape.
    let revision=scope.buffer_revision;
    apply_step(&mut ed,&json!({"action":"command_palette"}));
    let pending=replacement(22,revision,"東京");
    let waiting=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,pending.clone()).input_reply.unwrap();
    assert!(waiting.needs_layout && !waiting.accepted && !waiting.cancelled);
    assert_eq!(text(&ed),original);
    app.send(17,pb::Key {key:"Escape".into(),..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pending);
    assert_eq!(text(&ed),original.replacen("café","東京",1));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(23,"undo")); assert_eq!(text(&ed),original);
    apply_step(&mut ed,&json!({"action":"command_palette"}));
    let pending=pb::ComposeInput {id:24,kind:20,buffer_epoch:epoch,text_input:true,
        payload:pb::Paste {text:"東京".into()}.encode_to_vec(),..Default::default()};
    let waiting=rich_step(&mut bridge,&mut ed,&mut app,&mut scope,pending.clone()).input_reply.unwrap();
    assert!(waiting.needs_layout && !waiting.accepted && !waiting.cancelled);
    assert_eq!(text(&ed),original);
    app.send(17,pb::Key {key:"Escape".into(),..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pending);
    assert_eq!(text(&ed).matches("東京").count(),2,"literal text waits for its document, then reaches both cursors once");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(25,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(26,"save"));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
    ed.active_cursors_mut().insert_with_id(fresh::model::event::CursorId(0),fresh::model::cursor::Cursor::with_selection(start as usize,end as usize));
    ed.active_cursors_mut().remove_secondary();
    let tail=original.find("Tail").unwrap();
    let other=ed.active_cursors_mut().add(fresh::model::cursor::Cursor::with_selection(tail,tail+4));
    let untouched=ed.active_cursors_mut().add(fresh::model::cursor::Cursor::new(original.len()));
    let captured=ed.active_window().active_state().buffer.version();
    ed.log_and_apply_event(&fresh::model::event::Event::Insert {position:0,text:"前置 ".into(),cursor_id:untouched});
    let extra=|range:std::ops::Range<usize>,cursor:fresh::model::event::CursorId,text:Option<&str>|pb::ComposeReplacementTarget {
        start:Some(range.start as u64),end:Some(range.end as u64),cursor:Some(cursor.0 as u64),text:text.map(str::to_string)};
    let mut batch=replacement(27,captured,"東京\n_"); batch.replacements=vec![extra(tail..tail+4,other,Some("文"))];
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,batch);
    let expected=format!("前置 {}",original.replacen("café","東京\r\n_",1).replacen("Tail","文",1));
    assert_eq!(text(&ed),expected,"both captured ranges rebase, normalize line endings and edit atomically");
    assert_eq!(ed.active_cursors().count(),3);
    assert!(ed.active_cursors().get(fresh::model::event::CursorId(0)).unwrap().anchor.is_none());
    assert!(ed.active_cursors().get(other).unwrap().anchor.is_none());
    assert_eq!(ed.active_cursors().primary_id(),untouched,"replacements do not change the primary cursor identity");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(28,"undo")); assert_eq!(text(&ed),format!("前置 {original}"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(29,"redo")); assert_eq!(text(&ed),expected);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(30,"undo"));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(31,"undo")); assert_eq!(text(&ed),original);
    let snapshot=|ed:&fresh::app::Editor|ed.active_cursors().iter().map(|(id,c)|(id,c.position,c.anchor)).collect::<Vec<_>>();
    let before=snapshot(&ed); let revision=ed.active_window().active_state().buffer.version();
    for (id,target) in [(32,extra(tail..tail+4,fresh::model::event::CursorId(999_999),None)),
        (33,extra(end as usize-1..end as usize,other,None)),
        (34,extra(start as usize..end as usize,other,None)),
        (35,extra(tail..tail+4,fresh::model::event::CursorId(0),None))] {
        let mut batch=replacement(id,revision,"not written"); batch.replacements=vec![target];
        assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,batch).input_reply.unwrap().cancelled);
        assert_eq!(text(&ed),original); assert_eq!(snapshot(&ed),before,"a rejected batch must not move any cursor");
    }
    ed.log_and_apply_event(&fresh::model::event::Event::Insert {position:tail+1,text:"X".into(),cursor_id:untouched});
    let mut batch=replacement(36,revision,"not written"); batch.replacements=vec![extra(tail..tail+4,other,None)];
    assert!(rich_step(&mut bridge,&mut ed,&mut app,&mut scope,batch).input_reply.unwrap().cancelled);
    assert_eq!(text(&ed),original.replacen("Tail","TXail",1),"a stale secondary cannot leave an applied primary edit");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(37,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(38,"save"));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_image_objects_select_edit_move_delete_and_undo_as_source(root:&std::path::Path,work:&std::path::Path) {
    let original="# Pictures\r\n\r\nBefore ![inline](x.png) after café.\r\n\r\n![block](x.png)\r\n\r\n![reference][asset]\r\n\r\n[asset]: <a b.png>\r\n\r\n`![code](not.png)`\r\n";
    let file=work.join("image-edit.md"); std::fs::write(&file,original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    for _ in 0..30 { let _=fresh::webui::scene_value(&mut ed,COLS,ROWS); apply_step(&mut ed,&json!({})); std::thread::sleep(Duration::from_millis(5)); }
    let path=root.join("image-edit.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello = ready_scene(&mut bridge, &mut ed, &mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    let compose=pane.compose.as_ref().unwrap();
    assert_eq!(compose.images.len(),3,"reference images parse, image-looking code does not");
    assert_eq!(compose.text.matches('\u{fffc}').count(),3,"native objects are source-mapped before cell flattening");
    if let Some(path)=std::env::var_os("FC_COMPOSE_IMAGE_SCENE_FILE") {
        let mut fixture=compose.clone(); fixture.asset_authority=1; fixture.asset_document=1;
        std::fs::write(path,fixture.encode_to_vec()).unwrap();
    }
    let image=compose.images[0].clone();
    let point=|id,count|pb::ComposeInput {id,position:Some(image.end),attachment_start:Some(image.start),click_count:count,..Default::default()};
    let key=|id,name:&str,shift|pb::ComposeInput {id,kind:17,payload:pb::Key {key:name.into(),shift,..Default::default()}.encode_to_vec(),..Default::default()};
    let action=|id,name:&str|pb::ComposeInput {id,kind:21,payload:pb::Action {name:name.into(),args_json:String::new()}.encode_to_vec(),..Default::default()};
    let text=|ed:&fresh::app::Editor|{let buffer=&ed.active_window().active_state().buffer;String::from_utf8(buffer.slice_bytes(0..buffer.len())).unwrap()};
    let without=original.replacen("![inline](x.png)","",1);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,point(1,1));
    let cursor=ed.active_window().active_cursors().primary();
    assert_eq!(cursor.position,image.end as usize); assert_eq!(cursor.anchor,Some(image.start as usize));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,key(2,"Delete",false)); assert_eq!(text(&ed),without);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(3,"undo")); assert_eq!(text(&ed),original);
    let edited=rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,point(4,2));
    assert_eq!(ed.active_window().active_cursors().primary().position,image.start as usize+2);
    let source=edited.panes.iter().find_map(|u|u.pane.as_ref().and_then(|p|p.compose.as_ref())).unwrap();
    assert!(!source.images[0].rendered,"double-click reveals the exact markup for source editing");
    assert!(source.images[0].source_preview,"source editing retains a separate preview");
    assert!(source.text.contains("![inline](x.png)"));
    let at=|id,position|pb::ComposeInput {id,position:Some(position),..Default::default()};
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(5,image.start));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,key(6,"ArrowRight",false));
    assert_eq!(ed.active_window().active_cursors().primary().position,image.end as usize);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,key(7,"ArrowLeft",false));
    assert_eq!(ed.active_window().active_cursors().primary().position,image.start as usize);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,key(8,"ArrowRight",true));
    let cursor=ed.active_window().active_cursors().primary(); assert_eq!(cursor.anchor,Some(image.start as usize)); assert_eq!(cursor.position,image.end as usize);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(9,image.end));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,key(10,"Backspace",false)); assert_eq!(text(&ed),without);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(11,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,at(12,image.start));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,key(13,"Delete",false)); assert_eq!(text(&ed),without);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(14,"undo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(15,"save"));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes(),"native object editing and undo preserve exact CRLF bytes");

    // A click on the source-edit preview selects the Markdown range, and the
    // scene's clipboard contains those exact bytes, never a U+FFFC placeholder.
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,point(16,2));
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,point(17,1));
    assert!(app.panes.iter().filter_map(|p|p.compose.as_ref()).any(|p|p.images.iter().any(|i|i.start==image.start && i.rendered && !i.source_preview)));
    let copied=rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(18,"copy"));
    let markup=&original[image.start as usize..image.end as usize];
    assert_eq!(ed.clipboard_text(),markup);
    assert_eq!(copied.clipboard.as_ref().unwrap().text,markup);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(19,"cut"));
    assert_eq!(text(&ed),without); assert_eq!(ed.clipboard_text(),markup);
    let pasted=pb::ComposeInput {id:20,kind:20,payload:pb::Paste {text:markup.into()}.encode_to_vec(),..Default::default()};
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pasted);
    assert_eq!(text(&ed),original,"native paste restores exactly the copied source");
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(21,"undo")); assert_eq!(text(&ed),without);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(22,"redo")); assert_eq!(text(&ed),original);
    rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,action(23,"save"));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_image_source_previews_reveal_nested_and_reference_markup(root:&std::path::Path,work:&std::path::Path) {
    let original="# Nested\r\n\r\n> ![**café**](<a b.png>)\r\n\r\n| Picture | More |\r\n| --- | --- |\r\n| ![a *pic*][asset] | **other** |\r\n\r\n[asset]: <a b.png> \"title\"\r\n";
    let file=work.join("image-source.md"); std::fs::write(&file,original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("image-source.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()}); settle_bridge(&mut bridge,&mut ed);
    let hello=ready_scene(&mut bridge,&mut ed,&mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    let images=pane.compose.as_ref().unwrap().images.clone(); assert_eq!(images.len(),2);
    let mut id=0;
    for image in images {
        id+=1;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,
            position:Some(image.end),attachment_start:Some(image.start),click_count:2,..Default::default()});
        let compose=app.panes.iter().find_map(|p|p.compose.as_ref()).unwrap();
        let annotation=compose.images.iter().find(|i|i.start==image.start).unwrap();
        assert!(annotation.source_preview && !annotation.rendered);
        let markup=&original[image.start as usize..image.end as usize];
        assert!(compose.text.contains(markup),"source reveal retains exact nested/reference markup: {markup:?}");
        assert!(compose.segments.iter().filter(|s|s.source_start<image.end && s.source_end>image.start).all(|s|s.kind==0),
            "crossing table padding and nested emphasis cannot hide image-source characters");
        id+=1;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,
            position:Some(image.end),attachment_start:Some(image.start),click_count:1,..Default::default()});
        id+=1;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:21,
            payload:pb::Action {name:"copy".into(),..Default::default()}.encode_to_vec(),..Default::default()});
        assert_eq!(ed.clipboard_text(),markup,"copy retains reference syntax and Unicode exactly");
        let text=|ed:&fresh::app::Editor|{let b=&ed.active_window().active_state().buffer;String::from_utf8(b.slice_bytes(0..b.len())).unwrap()};
        let without=original.replacen(markup,"",1);
        id+=1;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:21,
            payload:pb::Action {name:"cut".into(),..Default::default()}.encode_to_vec(),..Default::default()});
        assert_eq!(text(&ed),without);
        id+=1;
        rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:20,
            payload:pb::Paste {text:markup.into()}.encode_to_vec(),..Default::default()});
        assert_eq!(text(&ed),original);
        for (name,expected) in [("undo",without.as_str()),("redo",original)] {
            id+=1;
            rich_accepted(&mut bridge,&mut ed,&mut app,&mut scope,pb::ComposeInput {id,kind:21,
                payload:pb::Action {name:name.into(),..Default::default()}.encode_to_vec(),..Default::default()});
            assert_eq!(text(&ed),expected);
        }
    }
    apply_step(&mut ed,&json!({"action":"save"}));
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn rich_images_use_binary_chunks_and_obsolete_reads_do_not_apply(root:&std::path::Path,work:&std::path::Path) {
    // The transport treats compressed bytes as opaque; Image I/O validates and
    // decodes the image in the native client. Include every byte value here so
    // a text conversion cannot accidentally pass a round-trip check.
    let mut bytes=b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend((0..400_017).map(|i|i as u8));
    std::fs::write(work.join("asset x.png"),&bytes).unwrap();
    let file=work.join("assets.md");
    std::fs::write(&file,"# Assets\n\n![pixel](asset%20x.png)\n\nBody\n").unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    let path=root.join("assets.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut app=App::connect(&path);
    app.send(16,pb::ClientHello {version:1,rich_compose:true,..Default::default()});
    settle_bridge(&mut bridge,&mut ed);
    let hello = ready_scene(&mut bridge, &mut ed, &mut app);
    let pane=hello.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap();
    let mut scope=pane_scope(hello.window_id.unwrap(),pane);
    let compose=pane.compose.as_ref().unwrap(); let image=&compose.images[0];
    let request=pb::ComposeAssetRequest {id:1,scope:Some(scope.clone()),start:image.start,end:image.end,
        authority:compose.asset_authority,document:compose.asset_document,..Default::default()};
    assert_ne!(request.authority,0);
    app.send(28,request.clone());
    let mut received=Vec::new(); let mut done=false; let mut moved=false;
    let mut version=Vec::new(); let mut digest=Vec::new();
    for _ in 0..30 {
        settle_bridge(&mut bridge,&mut ed);
        let Some(frame)=app.frame() else { continue; };
        for update in &frame.panes {
            if let Some(pane)=&update.pane { if pane.compose.is_some() { scope=pane_scope(scope.window_id,pane); } }
        }
        app.send(26,pb::Applied {seq:frame.seq});
        for chunk in frame.compose_assets {
            assert_eq!(chunk.id,1); assert!(chunk.error.is_empty(),"{}",chunk.error);
            assert_eq!(chunk.offset,received.len() as u64); assert_eq!(chunk.total,bytes.len() as u64);
            assert!(chunk.data.len()<=128<<10);
            if chunk.done { version=chunk.version.clone(); digest=chunk.digest.clone(); }
            received.extend(chunk.data); done=chunk.done;
            if !moved {
                app.send(17,pb::Key {key:"ArrowRight".into(),..Default::default()});
                moved=true;
            }
        }
        if done { break; }
    }
    assert!(done); assert_eq!(received,bytes);
    assert_eq!(version.len(),32); assert_eq!(digest.len(),32);
    assert_eq!(ed.active_window().active_cursors().primary().position,1,"normal input ran between asset chunks");
    let mut check=request.clone(); check.id=2; check.scope=Some(scope.clone());
    check.known_version=version; check.known_digest=digest.clone();
    app.send(28,check.clone());
    let mut checked=false;
    for _ in 0..30 {
        settle_bridge(&mut bridge,&mut ed);
        let Some(frame)=app.frame() else { continue; };
        app.send(26,pb::Applied {seq:frame.seq});
        for chunk in frame.compose_assets {
            if chunk.id==2 {
                assert!(chunk.unchanged && chunk.done && chunk.error.is_empty());
                assert!(chunk.data.is_empty()); assert_eq!(chunk.total,0); assert_eq!(chunk.digest,digest);
                checked=true;
            }
        }
        if checked { break; }
    }
    assert!(checked,"a conditional check returns no image payload");
    let image_path=work.join("asset x.png");
    let modified=std::fs::metadata(&image_path).unwrap().modified().unwrap();
    let mut changed=bytes.clone(); *changed.last_mut().unwrap()^=1;
    std::fs::write(&image_path,&changed).unwrap();
    std::fs::File::options().write(true).open(&image_path).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
    check.id=3; app.send(28,check);
    let mut replaced=Vec::new(); let mut replacement_done=false;
    for _ in 0..30 {
        settle_bridge(&mut bridge,&mut ed);
        let Some(frame)=app.frame() else { continue; };
        app.send(26,pb::Applied {seq:frame.seq});
        for chunk in frame.compose_assets {
            if chunk.id==3 {
                assert!(!chunk.unchanged && chunk.error.is_empty(),"{}",chunk.error);
                assert_eq!(chunk.offset,replaced.len() as u64);
                replaced.extend(chunk.data);
                if chunk.done { assert_ne!(chunk.digest,digest); replacement_done=true; }
            }
        }
        if replacement_done { break; }
    }
    assert!(replacement_done); assert_eq!(replaced,changed,"image changes load without editing the Markdown buffer");
    // Source edits between submission and delivery cancel the old request;
    // the new frame must terminate its stream with an error, not old bytes.
    let mut obsolete=request.clone(); obsolete.id=4; obsolete.scope=Some(scope.clone());
    app.send(28,obsolete);
    app.send(17,pb::Key {key:"X".into(),..Default::default()});
    let mut rejected=false;
    for _ in 0..10 {
        settle_bridge(&mut bridge,&mut ed);
        let Some(frame)=app.frame() else { continue; };
        app.send(26,pb::Applied {seq:frame.seq});
        for chunk in frame.compose_assets {
            if chunk.id==4 {
                assert!(chunk.done); assert!(chunk.data.is_empty());
                assert!(chunk.error.contains("changed"),"{}",chunk.error);
                rejected=true;
            }
        }
        if rejected { break; }
    }
    assert!(rejected,"obsolete image load received a terminating response");
}

fn rich_source_regions_belong_to_each_client_and_expire_after_edits(root: &std::path::Path, work: &std::path::Path) {
    let original = "# Long\n\n".to_string()+&(0..4000).map(|i|format!("Paragraph {i}: **bold_{i}** and enough text to cross the source window.\n\n")).collect::<String>();
    let file = work.join("regions.md"); std::fs::write(&file,&original).unwrap();
    let mut ed=build_editor(COLS,ROWS,&[file.clone()],true).unwrap();
    for _ in 0..12 { apply_step(&mut ed,&json!({})); }
    apply_step(&mut ed,&json!({"action":"markdownToggleCompose"}));
    for _ in 0..50 { apply_step(&mut ed,&json!({})); std::thread::sleep(Duration::from_millis(10)); }
    let path=root.join("regions.sock"); let mut bridge=SceneBridge::bind(&path,&ed).unwrap();
    let mut a=App::connect(&path); let mut b=App::connect(&path);
    for app in [&mut a,&mut b] { app.send(16,pb::ClientHello { version:1,rich_compose:true,..Default::default() }); }
    settle_bridge(&mut bridge,&mut ed);
    let hello_a = ready_scene(&mut bridge, &mut ed, &mut a);
    let hello_b = ready_scene(&mut bridge, &mut ed, &mut b);
    let first=|frame:&pb::Frame| frame.panes.iter().find_map(|u|u.pane.as_ref().filter(|p|p.compose.is_some())).unwrap().clone();
    let mut scope_a=pane_scope(hello_a.window_id.unwrap(),&first(&hello_a));
    let mut scope_b=pane_scope(hello_b.window_id.unwrap(),&first(&hello_b));
    assert_eq!(first(&hello_a).compose.unwrap().source_start,0);
    let probe=150_000+original[150_000..].find("**bold_").unwrap();
    let probe_end=probe+2+original[probe+2..].find("**").unwrap()+2;
    let probe_buffer=fresh_core::BufferId(scope_a.buffer as usize);
    let projected=|ed:&fresh::app::Editor| {
        let state=ed.active_window().buffer_state(probe_buffer).unwrap();
        state.conceals.project_text(&original[probe..probe_end],probe,&state.marker_list,&[0]).unwrap().text
    };
    assert_eq!(projected(&ed),original[probe..probe_end],"distant text was never offered to the terminal's plugin viewport");
    let request=pb::ComposeInput { id:1,region:Some(150_000),..Default::default() };
    let mut moved=rich_step(&mut bridge,&mut ed,&mut a,&mut scope_a,request.clone());
    for _ in 0..10 {
        if moved.input_reply.as_ref().is_some_and(|r|r.needs_layout) { break; }
        moved=rich_step(&mut bridge,&mut ed,&mut a,&mut scope_a,request.clone());
    }
    assert!(moved.input_reply.as_ref().is_some_and(|r|!r.accepted&&r.needs_layout));
    assert!(moved.input_reply.as_ref().unwrap().compose.as_ref().unwrap().source_start>100_000);
    assert_eq!(a.panes.iter().find_map(|p|p.compose.as_ref()).unwrap().source_start,0,"geometry requests do not replace the visible viewport");
    for _ in 0..100 {
        if projected(&ed)==original[probe+2..probe_end-2] { break; }
        apply_step(&mut ed,&json!({}));
        let _=fresh::webui::scene_value(&mut ed,COLS,ROWS);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(projected(&ed),original[probe+2..probe_end-2],"the native range reached the real Compose plugin");
    assert_eq!(ed.active_window().focused_view().viewport.top_byte(),0,"requesting content did not scroll the terminal");
    let escape=|id| pb::ComposeInput { id,kind:17,payload:pb::Key { key:"Escape".into(),..Default::default() }.encode_to_vec(),..Default::default() };
    // Complete A's request, and prove B's original source scope is still valid.
    rich_accepted(&mut bridge,&mut ed,&mut a,&mut scope_a,escape(1));
    let peer=rich_accepted(&mut bridge,&mut ed,&mut b,&mut scope_b,escape(1));
    assert!(peer.input_reply.unwrap().accepted,"one client did not inherit the other client's region");
    let typed=rich_accepted(&mut bridge,&mut ed,&mut a,&mut scope_a,pb::ComposeInput { id:2,kind:17,
        payload:pb::Key { key:"X".into(),..Default::default() }.encode_to_vec(),..Default::default() });
    assert!(typed.input_reply.as_ref().unwrap().accepted);
    let fresh=first(&typed).compose.unwrap();
    assert_eq!(fresh.source_start,0,"a stale byte hint expires after a source edit");
    assert_eq!(fresh.cursors.iter().find(|c|c.primary).unwrap().position,1);
    assert_eq!(std::fs::read(&file).unwrap(),original.as_bytes());
}

fn viewport_step(
    bridge: &mut SceneBridge,
    ed: &mut fresh::app::Editor,
    app: &mut App,
    request: pb::ComposeViewportRequest,
) -> pb::Frame {
    let id = request.id;
    app.send(29, request);
    settle_bridge(bridge, ed);
    for _ in 0..12 {
        let frame = app.frame().expect("viewport reply frame");
        app.send(26, pb::Applied { seq: frame.seq });
        if frame.viewport_reply.as_ref().is_some_and(|reply| reply.id == id) {
            return frame;
        }
        settle_bridge(bridge, ed);
    }
    panic!("viewport reply was not delivered");
}

fn rich_viewports_are_independent_and_rebase_queued_scrolls(root: &std::path::Path, work: &std::path::Path) {
    let original = "# Scrolling\n\n".to_string()
        + &(0..4000).map(|i| format!("Paragraph {i} with **emphasis** and enough text for distant viewports.\n\n")).collect::<String>();
    let file = work.join("viewports.md");
    std::fs::write(&file, &original).unwrap();
    let mut ed = build_editor(COLS, ROWS, &[file.clone()], true).unwrap();
    for _ in 0..12 { apply_step(&mut ed, &json!({})); }
    apply_step(&mut ed, &json!({"action":"markdownToggleCompose"}));
    for _ in 0..50 {
        apply_step(&mut ed, &json!({}));
        std::thread::sleep(Duration::from_millis(10));
    }
    let path = root.join("viewports.sock");
    let mut bridge = SceneBridge::bind(&path, &ed).unwrap();
    let mut a = App::connect(&path);
    let mut b = App::connect(&path);
    for app in [&mut a, &mut b] {
        app.send(16, pb::ClientHello { version: 1, rich_compose: true, ..Default::default() });
    }
    settle_bridge(&mut bridge, &mut ed);
    ready_scene(&mut bridge, &mut ed, &mut a); ready_scene(&mut bridge, &mut ed, &mut b);
    let active = |app: &App| app.panes.iter().find(|pane| pane.compose.is_some()).unwrap().clone();
    let initial_a = active(&a);
    let scope_a = pane_scope(a.window, &initial_a);
    let text_context = initial_a.compose.as_ref().unwrap().text_context.clone().unwrap();
    let mut scope_b = pane_scope(b.window, &active(&b));
    let epoch = initial_a.compose.unwrap().viewport.unwrap().epoch;
    let request = pb::ComposeViewportRequest {
        id: 1, scope: Some(scope_a.clone()), position: 100_000, offset: -7.5, horizontal_offset: 137.25, epoch,
        ..Default::default()
    };
    let frame = viewport_step(&mut bridge, &mut ed, &mut a, request.clone());
    assert!(frame.viewport_reply.unwrap().accepted);
    let pane = active(&a);
    let shown = pane.compose.unwrap();
    assert!(shown.source_start > 0 && shown.source_start <= request.position && shown.source_end >= request.position);
    assert_eq!(shown.viewport.as_ref().unwrap().position, 100_000);
    assert_eq!(shown.viewport.as_ref().unwrap().offset, -7.5);
    assert_eq!(shown.viewport.as_ref().unwrap().horizontal_offset, 137.25);
    assert_eq!(shown.cursors.iter().find(|c| c.primary).unwrap().position, 0);
    assert_eq!(shown.text_context.as_ref().unwrap(), &text_context,
        "scrolling to another source window cannot renumber the input-method context or selection");
    assert_eq!(active(&b).compose.as_ref().unwrap().viewport.as_ref().unwrap().horizontal_offset, 0.0);
    assert_eq!(active(&b).compose.unwrap().viewport.unwrap().position, 0);
    assert_eq!(ed.active_window().focused_view().viewport.top_byte(), 0, "a graphical scroll cannot move the terminal");

    // A scroll captured before another client inserts a multibyte prefix is
    // still meaningful: its source marker and its queued byte coordinate
    // must use the same left-gravity edit mapping.
    let typed = rich_accepted(&mut bridge, &mut ed, &mut b, &mut scope_b, pb::ComposeInput {
        id: 1, kind: 20, payload: pb::Paste { text: "é".into() }.encode_to_vec(), ..Default::default()
    });
    assert!(typed.input_reply.unwrap().accepted);
    assert_eq!(active(&b).compose.unwrap().viewport.unwrap().position, 0, "inserting exactly at the top does not hide the inserted text");
    let mut queued = request.clone(); queued.id = 2; queued.position += 128;
    let frame = viewport_step(&mut bridge, &mut ed, &mut a, queued.clone());
    assert!(frame.viewport_reply.unwrap().accepted);
    let current = active(&a);
    let rebased = current.compose.as_ref().unwrap().viewport.as_ref().unwrap();
    assert_eq!(rebased.position, 100_130);
    assert_eq!(rebased.offset, -7.5);
    assert_eq!(rebased.horizontal_offset, 137.25);
    assert_eq!(rebased.sequence, 2);
    assert_eq!(ed.active_window().focused_view().viewport.top_byte(), 0);
    assert_eq!(current.compose.as_ref().unwrap().cursors.iter().find(|c| c.primary).unwrap().position, 2);

    // A duplicate is acknowledged without reapplying a different payload.
    queued.position = 3; queued.offset = -99.0; queued.horizontal_offset = 999.0;
    assert!(viewport_step(&mut bridge, &mut ed, &mut a, queued).viewport_reply.unwrap().accepted);
    assert_eq!(active(&a).compose.unwrap().viewport.unwrap(), *rebased);

    let mut invalid = request;
    invalid.scope = Some(pane_scope(a.window, &current));
    invalid.id = 3; invalid.epoch += 1;
    assert!(!viewport_step(&mut bridge, &mut ed, &mut a, invalid.clone()).viewport_reply.unwrap().accepted);
    invalid.id = 4; invalid.epoch = epoch; invalid.position = 1;
    assert!(!viewport_step(&mut bridge, &mut ed, &mut a, invalid.clone()).viewport_reply.unwrap().accepted,
        "an interior UTF-8 byte cannot become a source anchor");
    invalid.id = 5; invalid.position = 10; invalid.scope.as_mut().unwrap().buffer_revision += 1;
    assert!(!viewport_step(&mut bridge, &mut ed, &mut a, invalid).viewport_reply.unwrap().accepted,
        "a future source revision cannot become a source anchor");
    assert_eq!(active(&a).compose.unwrap().viewport.unwrap(), *rebased);
    assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes(), "scrolling and dirty edits do not save implicitly");
    drop(a);
    settle_bridge(&mut bridge, &mut ed);
    rich_accepted(&mut bridge, &mut ed, &mut b, &mut scope_b, pb::ComposeInput {
        id: 2, kind: 17, payload: pb::Key { key: "Escape".into(), ..Default::default() }.encode_to_vec(), ..Default::default()
    });
}

fn rich_accepted(bridge:&mut SceneBridge,ed:&mut fresh::app::Editor,app:&mut App,
    scope:&mut pb::ComposeScope,input:pb::ComposeInput)->pb::Frame {
    for _ in 0..10 {
        let frame=rich_step(bridge,ed,app,scope,input.clone());
        if frame.input_reply.as_ref().is_some_and(|r|r.accepted) { return frame; }
        if let Some(reply)=frame.input_reply.as_ref().filter(|reply|reply.cancelled) { panic!("input {} was cancelled: {}",input.id,reply.error); }
    }
    panic!("source input did not settle against the latest decoration revision");
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
    rich_reply(bridge, ed, app, scope, request)
}

fn rich_reply(bridge: &mut SceneBridge, ed: &mut fresh::app::Editor, app: &mut App,
    scope: &mut pb::ComposeScope, request: u64) -> pb::Frame {
    for _ in 0..10 {
        let frame=app.frame().expect("a reply to the revisioned input");
        for update in &frame.panes {
            if let Some(pane) = &update.pane {
                if pane.leaf == scope.leaf && pane.compose.is_some() { *scope = pane_scope(scope.window_id, pane); }
            }
        }
        if let Some(compose)=frame.input_reply.as_ref().and_then(|r|r.compose.as_ref()) {
            scope.buffer_revision=compose.buffer_revision; scope.projection_revision=compose.projection_revision;
            scope.selection_revision=compose.selection_revision;
        } else if frame.input_reply.as_ref().is_some_and(|reply|reply.accepted) {
            if let Some(pane)=app.panes.iter().find(|pane|pane.leaf==scope.leaf&&pane.compose.is_some()) {
                *scope=pane_scope(app.window,pane);
            }
        }
        app.send(26,pb::Applied { seq:frame.seq });
        if frame.input_reply.as_ref().is_some_and(|reply|reply.id==request) { return frame; }
        settle_bridge(bridge,ed);
    }
    panic!("input reply was not delivered");
}

fn rich_input_intents_rebase_and_cancel_when_the_view_changes(root: &std::path::Path, work: &std::path::Path) {
    let original = "# Input\n\nalpha café omega\n\n![picture](x.png)\n\n".to_string()
        + &(0..3000).map(|i| format!("Paragraph {i} with several words and **emphasis**.\n\n")).collect::<String>();
    let file = work.join("input-intents.md");
    let other = work.join("input-intents-other.md");
    std::fs::write(&file, &original).unwrap();
    std::fs::write(&other, "Other file\n").unwrap();
    let mut ed = build_editor(COLS, ROWS, &[other.clone(), file.clone()], true).unwrap();
    for _ in 0..12 { apply_step(&mut ed, &json!({})); }
    apply_step(&mut ed, &json!({"action":"markdownToggleCompose"}));
    for _ in 0..50 {
        apply_step(&mut ed, &json!({}));
        std::thread::sleep(Duration::from_millis(10));
    }
    let path = root.join("input-intents.sock");
    let mut bridge = SceneBridge::bind(&path, &ed).unwrap();
    let mut app = App::connect(&path);
    app.send(16, pb::ClientHello { version: 1, rich_compose: true, ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    ready_scene(&mut bridge, &mut ed, &mut app);
    let initial = app.panes.iter().find(|pane| pane.compose.is_some()).unwrap().clone();
    let mut scope = pane_scope(app.window, &initial);
    let captured_revision = scope.buffer_revision;
    let epoch = initial.compose.as_ref().unwrap().viewport.as_ref().unwrap().epoch;
    let point = original.find("café").unwrap() as u64;
    let image = original.find("![picture]").unwrap() as u64;
    let image_end = image + "![picture](x.png)".len() as u64;
    let pointer = |id, position, affinity, attachment_start| pb::ComposeInput {
        id, position: Some(position), affinity, attachment_start,
        source_revision: Some(captured_revision), buffer_epoch: epoch,
        click_count: 1, ..Default::default()
    };
    viewport_step(&mut bridge, &mut ed, &mut app, pb::ComposeViewportRequest {
        id: 1, scope: Some(scope.clone()), position: 100_000, epoch, ..Default::default()
    });
    assert!(app.panes.iter().find_map(|pane| pane.compose.as_ref()).unwrap().source_start > point,
        "the captured click is outside the current visible source window");
    // Another editor input changes the document while the graphical click is
    // queued. Only Fresh's coordinate history can rebase its captured source.
    apply_step(&mut ed, &json!({"type":"é"}));
    let clicked = rich_step(&mut bridge, &mut ed, &mut app, &mut scope, pointer(1, point, 0, None));
    assert!(clicked.input_reply.unwrap().accepted);
    assert_eq!(ed.active_window().active_cursors().primary().position as u64, point + 2);
    apply_step(&mut ed, &json!({"type":"X"}));
    assert!(rich_step(&mut bridge, &mut ed, &mut app, &mut scope, pointer(2, point, 1, None)).input_reply.unwrap().accepted);
    assert_eq!(ed.active_window().active_cursors().primary().position as u64, point + 2, "upstream stays before an insertion at the captured byte");
    assert!(rich_step(&mut bridge, &mut ed, &mut app, &mut scope, pointer(3, point, 0, None)).input_reply.unwrap().accepted);
    assert_eq!(ed.active_window().active_cursors().primary().position as u64, point + 3, "downstream stays with the original text after the insertion");
    assert!(rich_step(&mut bridge, &mut ed, &mut app, &mut scope, pointer(4, image_end, 1, Some(image))).input_reply.unwrap().accepted);
    let cursor = ed.active_window().active_cursors().primary();
    assert_eq!(cursor.anchor, Some(image as usize + 3));
    assert_eq!(cursor.position, image_end as usize + 3);
    let before_delete = ed.active_window().active_state().buffer.len();
    apply_step(&mut ed, &json!({"action":"delete_forward"}));
    assert_eq!(ed.active_window().active_state().buffer.len(), before_delete - (image_end - image) as usize);
    let rejected = rich_step(&mut bridge, &mut ed, &mut app, &mut scope, pointer(5, image_end, 1, Some(image))).input_reply.unwrap();
    assert!(rejected.cancelled && !rejected.accepted && !rejected.needs_layout, "{rejected:?}");
    assert!(rejected.error.contains("image"));
    let action = |id| pb::ComposeInput { id, kind: 21, buffer_epoch: epoch,
        payload: pb::Action { name: "move_down".into(), ..Default::default() }.encode_to_vec(), ..Default::default() };
    let waiting = rich_step(&mut bridge, &mut ed, &mut app, &mut scope, action(6)).input_reply.unwrap();
    assert!(waiting.needs_layout && !waiting.accepted && !waiting.cancelled);
    // A presentation toggle cancels a waiting command without another client
    // request. Source mode must never inherit its deferred measured motion.
    apply_step(&mut ed, &json!({"action":"markdownToggleCompose"}));
    for _ in 0..20 {
        if ed.active_window().focused_view().view_mode == fresh::state::ViewMode::Source { break; }
        apply_step(&mut ed, &json!({}));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ed.active_window().focused_view().view_mode, fresh::state::ViewMode::Source);
    let (mut cols, mut rows) = (COLS, ROWS);
    assert!(bridge.poll(&mut ed, &mut cols, &mut rows).frame_owed, "view changes terminate pending input without a client request");
    // Delay sending that outcome, as frame backpressure could. A legal retry
    // of the earlier needs-layout reply races the unsent cancellation.
    let mut retry = action(6); retry.scope = Some(scope.clone());
    app.send(27, retry);
    std::thread::sleep(Duration::from_millis(20));
    bridge.poll(&mut ed, &mut cols, &mut rows);
    assert!(bridge.is_busy(), "the cancellation/retry race must not disconnect the client");
    bridge.push_scene(&mut ed, COLS, ROWS);
    bridge.maintain();
    let cancelled = rich_reply(&mut bridge, &mut ed, &mut app, &mut scope, 6).input_reply.unwrap();
    assert!(cancelled.cancelled && !cancelled.accepted);
    assert!(!cancelled.error.is_empty());
    let duplicate = rich_step(&mut bridge, &mut ed, &mut app, &mut scope, action(6)).input_reply.unwrap();
    assert_eq!(duplicate, cancelled, "a retry racing terminal cancellation receives the same outcome");

    apply_step(&mut ed, &json!({"action":"markdownToggleCompose"}));
    apply_step(&mut ed, &json!({"action":"save"}));
    settle_bridge(&mut bridge, &mut ed);
    // Reopening the Compose projection uses the same buffer lifetime. Refresh
    // its scope from a plain no-op before beginning another deferred action.
    rich_accepted(&mut bridge, &mut ed, &mut app, &mut scope, pb::ComposeInput {
        id: 7, kind: 17, payload: pb::Key { key: "Escape".into(), ..Default::default() }.encode_to_vec(), ..Default::default()
    });
    let waiting = rich_step(&mut bridge, &mut ed, &mut app, &mut scope, action(8)).input_reply.unwrap();
    assert!(waiting.needs_layout && !waiting.accepted);
    apply_step(&mut ed, &json!({"action":"close_tab"}));
    settle_bridge(&mut bridge, &mut ed);
    let closed = rich_reply(&mut bridge, &mut ed, &mut app, &mut scope, 8).input_reply.unwrap();
    assert!(closed.cancelled && !closed.accepted);
    assert_ne!(ed.active_window().active_buffer().0 as u64, scope.buffer);
    assert_eq!(ed.active_window().active_state().buffer.file_path().unwrap().canonicalize().unwrap(), other.canonicalize().unwrap());
    let next = pb::ComposeInput { id: 9, kind: 17,
        payload: pb::Key { key: "Q".into(), ..Default::default() }.encode_to_vec(), ..Default::default() };
    app.send(27, next);
    settle_bridge(&mut bridge, &mut ed);
    assert!(rich_reply(&mut bridge, &mut ed, &mut app, &mut scope, 9).input_reply.unwrap().accepted,
        "a cancelled id is consumed and cannot strand later input");
    assert_eq!(ed.active_window().active_cursors().primary().position, 1, "the deferred down action did not leak into the other file");
    assert_eq!(std::fs::read(&other).unwrap(), b"Other file\n", "new dirty input was not saved implicitly");
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
    let scene = ready_scene(&mut bridge, &mut ed, &mut app);
    let pane = scene.panes.iter().find_map(|p| p.pane.as_ref().filter(|p| p.compose.is_some())).unwrap();
    let mut scope = pane_scope(scene.window_id.unwrap(), pane);
    let stop = |source: usize, x: f64, affinity: u32| pb::ComposeStop { source: source as u64, x, affinity };
    let row = |number: u64, y: f64, height: f64, start: usize, end: usize, points: Vec<(usize,f64)>| {
        let first = stop(points.first().unwrap().0, points.first().unwrap().1, 0);
        let last = stop(points.last().unwrap().0, points.last().unwrap().1, 1);
        let mut stops: Vec<_> = points.into_iter().map(|(s,x)| stop(s,x,0)).collect();
        stops.push(last.clone());
        pb::ComposeLine { number, y, height, source_start: start as u64, source_end: end as u64,
            start: Some(first), end: Some(last), stops, cell: None }
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
    let fresh = ready_scene(&mut bridge, &mut ed, &mut malformed);
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
    let scene = ready_scene(&mut bridge, &mut ed, &mut rich);
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
    let mut response_scope = initial.clone();
    let at = original.find("café").unwrap();
    rich.send(27, pb::ComposeInput { id: 1, scope: Some(initial.clone()), position: Some(at as u64), ..Default::default() });
    settle_bridge(&mut bridge, &mut ed);
    let placed = rich_reply(&mut bridge, &mut ed, &mut rich, &mut response_scope, 1);
    assert!(placed.input_reply.unwrap().accepted);
    assert_eq!(ed.active_window().active_cursors().primary().position, at);
    rich.send(26, pb::Applied { seq: placed.seq });
    let before_edit = scope(rich.panes.iter().find(|p|p.compose.is_some()).unwrap());
    let key = pb::ComposeInput { id: 2, scope: Some(before_edit.clone()), kind: 17,
        payload: pb::Key { key: "X".into(), ..Default::default() }.encode_to_vec(), ..Default::default() };
    let mut obsolete_cursor = key.clone();
    obsolete_cursor.scope = Some(initial);
    rich.send(27, obsolete_cursor);
    settle_bridge(&mut bridge, &mut ed);
    let stale_cursor = rich_reply(&mut bridge, &mut ed, &mut rich, &mut response_scope, 2);
    assert!(!stale_cursor.input_reply.unwrap().accepted, "cursor-only changes invalidate pending input too");
    rich.send(26, pb::Applied { seq: stale_cursor.seq });
    rich.send(27, key.clone());
    settle_bridge(&mut bridge, &mut ed);
    let typed = rich_reply(&mut bridge, &mut ed, &mut rich, &mut response_scope, 2);
    assert!(typed.input_reply.unwrap().accepted);
    rich.send(26, pb::Applied { seq: typed.seq });
    let source = |ed: &fresh::app::Editor| {
        let state = ed.active_window().active_state();
        String::from_utf8(state.buffer.slice_bytes(0..state.buffer.len())).unwrap()
    };
    assert_eq!(source(&ed), original.replacen("café", "Xcafé", 1));
    // Replaying an accepted request cannot type the character twice.
    rich.send(27, key);
    settle_bridge(&mut bridge, &mut ed);
    let duplicate = rich_reply(&mut bridge, &mut ed, &mut rich, &mut response_scope, 2);
    assert!(duplicate.input_reply.unwrap().accepted);
    assert_eq!(source(&ed), original.replacen("café", "Xcafé", 1));
    rich.send(26, pb::Applied { seq: duplicate.seq });
    let undo = pb::ComposeInput { id: 3, scope: Some(before_edit), kind: 21,
        payload: pb::Action { name: "undo".into(), args_json: String::new() }.encode_to_vec(), ..Default::default() };
    rich.send(27, undo.clone());
    settle_bridge(&mut bridge, &mut ed);
    let stale = rich_reply(&mut bridge, &mut ed, &mut rich, &mut response_scope, 3);
    assert!(!stale.input_reply.unwrap().accepted, "obsolete source geometry is refused");
    assert_eq!(source(&ed), original.replacen("café", "Xcafé", 1));
    rich.send(26, pb::Applied { seq: stale.seq });
    assert!(rich_accepted(&mut bridge, &mut ed, &mut rich, &mut response_scope, undo).input_reply.unwrap().accepted);
    assert_eq!(source(&ed), original, "Fresh's normal undo restores the exact CRLF source");
    assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes(), "presentation never saved or rewrote the source");
}
