//! File Commander's scene socket: a session's scene for the macOS app's
//! native editor, as protobuf messages in a type-length-value stream
//! ([`super::wire`]) over an owner-only Unix socket.
//!
//! A client says hello (`ClientHello`, with the protocol version) and gets
//! the whole scene (`ServerHello`), then a `Frame` whenever something it
//! shows changed: the grid as rows it lacks and copies of rows it has, and
//! each region only when it changed. It reports each frame it applied
//! (`Applied`); at most two go out unapplied, and one that catches up gets
//! the newest scene. Its input — keys, mouse, resizes, clicks — is applied to
//! the editor as it arrives. The socket carries no HTTP and no WebSocket, and
//! a connection ends at EOF; browsers keep the JSON bridge over TCP.

use super::wire::{self, kind, ClientScene, EncodedScene, Field, Reader, Timing, WireError};
use super::{
    apply_action, apply_kbedit, apply_key, apply_mouse, apply_paste, apply_resize_want,
    apply_settings, apply_widget, BridgePoll, GridFrame, GridHasher, KeyInput, MouseInput,
    SceneFor, SceneModel, SceneSource, SceneTimes, SettingsClick, WS_OUTBUF_CAP,
};
use crate::app::Editor;
use anyhow::Result;
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The socket's buffers each way: macOS gives a Unix socket 8 KiB, a
/// fraction of one frame.
const SOCKET_BUFFER: libc::c_int = 4 << 20;

/// Frames an app may have been sent but not yet applied (`Applied`). Past
/// this, newer frames are held back, and once it catches up it gets one
/// frame with the newest scene: an app that falls behind skips to the
/// present instead of working through every frame in between, each one
/// older than the last by the time it is shown.
const FRAMES_AHEAD: u64 = 2;

/// How long an app with frames held back may go without reporting before
/// frames go out regardless, so a lost `Applied` can't freeze its view.
const APPLIED_TIMEOUT: Duration = Duration::from_millis(500);

/// The process's CPU time so far, user and system, in milliseconds.
fn process_cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the struct it is given.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0.0;
    }
    // SAFETY: zeroed, then filled by a successful getrusage.
    let usage = unsafe { usage.assume_init() };
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// File Commander's scene socket, bound at the path a session's handshake
/// names (`fc_scene`).
pub struct SceneBridge {
    listener: UnixListener,
    path: PathBuf,
    clients: Vec<SceneClient>,
    /// The renderer and the clipboard mirror.
    source: SceneSource,
    /// The grid's row hashes, kept between frames.
    grid: GridHasher,
    /// The last push's scene, encoded once for every client.
    encoded: EncodedScene,
    rich_encoded: EncodedScene,
    compose: super::compose::ComposeCache,
    /// Allocations of the last push, for the next frame's telemetry.
    last_push_allocs: Option<(u64, u64)>,
}

/// One connected app.
struct SceneClient {
    stream: UnixStream,
    /// Bytes read and not yet a whole message.
    inbuf: Vec<u8>,
    /// Bytes to send, from `out_start`; written without blocking.
    outbuf: Vec<u8>,
    out_start: usize,
    /// Its hello arrived, so it is owed a `ServerHello`; then that went out.
    greeted: bool,
    hello_sent: bool,
    /// Frames carry timing.
    telemetry: bool,
    rich_compose: bool,
    last_input: u64,
    input_reply: Option<super::compose::InputReply>,
    /// What it has of the scene.
    scene: ClientScene,
    /// The last frame sent, and the last it applied.
    seq: u64,
    applied: u64,
    /// A frame was held back from it.
    held: bool,
    sent_at: Instant,
    /// Messages received from it, every kind: a frame's `ack`.
    received: u64,
    /// The grid size it wants.
    want: Option<(u16, u16)>,
    /// A message's payload, built before its header.
    body: Vec<u8>,
}

impl SceneBridge {
    /// Bind at `path`. Only the daemon binds it, while holding the session's
    /// startup lock, so a file already there is a socket left behind.
    pub fn bind(path: &Path, editor: &Editor) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            clients: Vec::new(),
            source: SceneSource::new(editor),
            grid: GridHasher::default(),
            encoded: EncodedScene::default(),
            rich_encoded: EncodedScene::default(),
            compose: Default::default(),
            last_push_allocs: None,
        })
    }

    pub fn socket_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// True while an app is connected: the host polls quickly, and the
    /// session doesn't shut down idle.
    pub fn is_busy(&self) -> bool {
        !self.clients.is_empty()
    }

    /// The grid size that fits every connected app (the element-wise
    /// minimum), `None` before one says.
    pub fn wanted_size(&self) -> Option<(u16, u16)> {
        self.clients
            .iter()
            .filter_map(|c| c.want)
            .reduce(|(ac, ar), (c, r)| (ac.min(c), ar.min(r)))
    }

    /// Read and apply what the apps sent, and accept new ones.
    /// The grid size is the host's, refit from `wanted_size`; `_cols` and
    /// `_rows` are there for the browser bridge's HTTP `/resize`.
    pub fn poll(&mut self, editor: &mut Editor, _cols: &mut u16, _rows: &mut u16) -> BridgePoll {
        let mut out = BridgePoll::default();
        let had_clients = !self.clients.is_empty();
        let mut i = 0;
        while i < self.clients.len() {
            match self.clients[i].read(editor, &mut out, &mut self.compose) {
                Ok(()) => i += 1,
                Err(reason) => {
                    eprintln!("[scene] client dropped: {reason}");
                    let mut client = self.clients.remove(i);
                    client.fail(&reason);
                    out.resize_dirty = true;
                }
            }
        }
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    tune(&stream);
                    self.clients.push(SceneClient::new(stream));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("[scene] accept error: {e}");
                    break;
                }
            }
        }
        // The native editor has its own menu bar, so its scenes hide Fresh's
        // menu row; with the last app gone, the terminals get it back.
        if had_clients && self.clients.is_empty() {
            let show = editor.config().editor.show_menu_bar;
            editor.active_window_mut().menu_bar_visible = show;
            out.mutated = true;
        }
        out
    }

    /// Build the scene once and send each app what it lacks: a `ServerHello`
    /// to one that just said hello, a `Frame` to the others. Returns true
    /// while an app is owed a frame it was held back from (it is behind);
    /// while none can take one, nothing is built.
    pub fn push_scene(&mut self, editor: &mut Editor, cols: u16, rows: u16) -> bool {
        let wants_hello = self.clients.iter().any(|c| c.greeted && !c.hello_sent);
        if !wants_hello && !self.clients.iter().any(|c| c.hello_sent && c.ready()) {
            for client in self.clients.iter_mut().filter(|c| c.hello_sent) {
                client.held = true;
            }
            return self.clients.iter().any(|c| c.held);
        }
        let allocs_before = crate::alloc_stats::counts();
        let started = Instant::now();
        let mut times = SceneTimes::default();
        let source = &mut self.source;
        let mut model = SceneModel::gather(editor, &mut source.renderer, cols, rows, SceneFor::App, &mut times);
        let grid_started = Instant::now();
        let grid = self.grid.frame(model.buf);
        times.grid_ms = grid_started.elapsed().as_secs_f64() * 1000.0;
        let rich = self.clients.iter().any(|c| c.greeted && c.rich_compose);
        if rich && model.aux_modal.is_none() && editor.menu_state().active_menu.is_none() {
            for pane in &mut model.panes {
                if let Some(buffer) = pane.buffer {
                    pane.compose = self.compose.gather(editor,
                        fresh_core::LeafId(fresh_core::SplitId(pane.leaf)),
                        crate::model::event::BufferId(buffer));
                }
            }
        }
        let build_ms = started.elapsed().as_secs_f64() * 1000.0;
        let encode_started = Instant::now();
        let (clip_seq, clip_text) = source.clip.current(editor);
        if rich {
            self.rich_encoded.encode(&model, wire::Clipboard { seq: clip_seq, text: clip_text });
            for pane in &mut model.panes { pane.compose = None; }
        }
        self.encoded.encode(&model, wire::Clipboard { seq: clip_seq, text: clip_text });
        let encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0;
        let base = Timing {
            build_ms,
            render_ms: times.render_ms,
            grid_ms: times.grid_ms,
            encode_ms,
            owed: editor.pending_wheel_lines(),
            top: editor.active_top_line() as u64,
            cpu_ms: process_cpu_ms(),
            prev_allocs: self.last_push_allocs.map(|a| a.0),
            prev_alloc_bytes: self.last_push_allocs.map(|a| a.1),
            ..Timing::default()
        };
        for client in &mut self.clients {
            if !client.greeted {
                continue;
            }
            let encoded = if client.rich_compose { &self.rich_encoded } else { &self.encoded };
            if !client.hello_sent {
                client.send_hello(encoded, &grid);
            } else if client.ready() {
                client.held = false;
                client.send_frame(encoded, &grid, base);
            } else {
                client.held = true;
            }
        }
        self.last_push_allocs = crate::alloc_stats::since(allocs_before);
        self.clients.iter().any(|c| c.held)
    }

    /// Send what is waiting, without blocking. Returns true when the set of
    /// apps shrank (one stopped reading for too long), so the host refits.
    pub fn maintain(&mut self) -> bool {
        let before = self.clients.len();
        self.clients.retain_mut(|c| match c.flush() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[scene] client dropped: {e}");
                false
            }
        });
        self.clients.len() != before
    }
}

impl Drop for SceneBridge {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

/// Raise the socket's buffers (`SOCKET_BUFFER`); best effort, as a smaller
/// buffer only makes it slower.
fn tune(stream: &UnixStream) {
    use std::os::unix::io::AsRawFd;
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: a live descriptor, and an option value of the size passed.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&SOCKET_BUFFER as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if result != 0 {
            eprintln!("[scene] socket buffer not raised: {}", std::io::Error::last_os_error());
        }
    }
}

impl SceneClient {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            inbuf: Vec::new(),
            outbuf: Vec::new(),
            out_start: 0,
            greeted: false,
            hello_sent: false,
            telemetry: false,
            rich_compose: false,
            last_input: 0,
            input_reply: None,
            scene: ClientScene::default(),
            seq: 0,
            applied: 0,
            held: false,
            sent_at: Instant::now(),
            received: 0,
            want: None,
            body: Vec::new(),
        }
    }

    /// Whether a frame may go out: at most `FRAMES_AHEAD` unapplied, unless
    /// the app has said nothing for `APPLIED_TIMEOUT`.
    fn ready(&self) -> bool {
        self.seq.saturating_sub(self.applied) < FRAMES_AHEAD || self.sent_at.elapsed() >= APPLIED_TIMEOUT
    }

    /// Read what arrived and apply each whole message.
    fn read(&mut self, editor: &mut Editor, out: &mut BridgePoll, compose: &mut super::compose::ComposeCache) -> Result<(), String> {
        let mut tmp = [0u8; 16384];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err("the app closed the connection".to_string()),
                Ok(n) => self.inbuf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        let inbuf = std::mem::take(&mut self.inbuf);
        let mut used = 0;
        let result = loop {
            match wire::read_message(&inbuf[used..]) {
                Ok(Some((k, payload, n))) => {
                    let payload = &inbuf[used..][payload];
                    used += n;
                    self.received += 1;
                    if let Err(e) = self.handle(k, payload, editor, out, compose) {
                        break Err(e);
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e.to_string()),
            }
        };
        self.inbuf = inbuf;
        self.inbuf.drain(..used);
        result
    }

    fn handle(
        &mut self,
        k: u64,
        payload: &[u8],
        editor: &mut Editor,
        out: &mut BridgePoll,
        compose: &mut super::compose::ComposeCache,
    ) -> Result<(), String> {
        let w = |e: WireError| e.to_string();
        if !self.greeted {
            if k != kind::CLIENT_HELLO {
                return Err(format!("expected a hello, got message type {k}"));
            }
            let (mut version, mut telemetry, mut rich_compose) = (0, false, false);
            for f in Reader::new(payload) {
                match f.map_err(w)? {
                    (1, v) => version = v.uint(),
                    (2, v) => telemetry = v.flag(),
                    (3, v) => rich_compose = v.flag(),
                    _ => {}
                }
            }
            if version != u64::from(wire::VERSION) {
                return Err(format!("scene protocol {version}, expected {}", wire::VERSION));
            }
            self.greeted = true;
            self.telemetry = telemetry;
            self.rich_compose = rich_compose;
            out.frame_owed = true;
            return Ok(());
        }
        match k {
            kind::COMPOSE_INPUT => {
                if !self.rich_compose { return Err("rich input requires rich_compose in hello".into()); }
                let input = super::compose::Input::parse(payload).map_err(w)?;
                if input.id == 0 { return Err("rich input has no request id".into()); }
                if input.id > self.last_input.saturating_add(1) { return Err("rich input request ids must be sequential".into()); }
                if self.input_reply.is_some() { return Err("rich inputs must wait for their reply".into()); }
                let mut accepted = input.id <= self.last_input;
                if !accepted && input.scope.as_ref().is_none_or(|scope| scope.matches(editor, compose)) {
                    if input.position.is_some() {
                        accepted = input.pointer(editor);
                        out.mutated |= accepted;
                    } else if (kind::KEY..=kind::KB_EDIT_CLICK).contains(&input.kind) {
                        if let Some(scope) = &input.scope { editor.focus_split(scope.leaf, scope.buffer); }
                        self.handle(input.kind, input.payload, editor, out, compose)?;
                        accepted = true;
                    } else {
                        return Err("unsupported input inside ComposeInput".into());
                    }
                }
                if accepted { self.last_input = self.last_input.max(input.id); }
                self.input_reply = Some(super::compose::InputReply { id: input.id, accepted });
                out.frame_owed = true;
            }
            kind::KEY => {
                let mut input = KeyInput::default();
                for f in Reader::new(payload) {
                    match f.map_err(w)? {
                        (1, v) => input.key = v.text().map_err(w)?,
                        (2, v) => input.code = Some(v.text().map_err(w)?).filter(|c| !c.is_empty()),
                        (3, v) => input.ctrl = v.flag(),
                        (4, v) => input.alt = v.flag(),
                        (5, v) => input.meta = v.flag(),
                        (6, v) => input.shift = v.flag(),
                        _ => {}
                    }
                }
                apply_key(editor, input);
                out.mutated = true;
            }
            kind::MOUSE => {
                if let Some(input) = mouse(payload).map_err(w)? {
                    apply_mouse(editor, input);
                }
                out.mutated = true;
            }
            kind::RESIZE => {
                let (mut c, mut r) = (None, None);
                for f in Reader::new(payload) {
                    match f.map_err(w)? {
                        (1, v) => c = Some(v.uint()),
                        (2, v) => r = Some(v.uint()),
                        _ => {}
                    }
                }
                if let Some(want) = apply_resize_want(self.want, c, r) {
                    self.want = Some(want);
                    out.resize_dirty = true;
                }
            }
            kind::PASTE => {
                for f in Reader::new(payload) {
                    if let (1, v) = f.map_err(w)? {
                        apply_paste(editor, v.text().map_err(w)?);
                    }
                }
                out.mutated = true;
            }
            kind::ACTION => {
                let (mut name, mut args) = ("", "");
                for f in Reader::new(payload) {
                    match f.map_err(w)? {
                        (1, v) => name = v.text().map_err(w)?,
                        (2, v) => args = v.text().map_err(w)?,
                        _ => {}
                    }
                }
                // The one JSON on this socket: a menu item's arguments, as the
                // keymap has them, handed back by the app.
                let args = if args.is_empty() {
                    Default::default()
                } else {
                    serde_json::from_str(args).map_err(|e| format!("action arguments: {e}"))?
                };
                apply_action(editor, name, &args);
                out.mutated = true;
            }
            kind::WIDGET => {
                let (mut surface, mut key) = ("", None);
                for f in Reader::new(payload) {
                    match f.map_err(w)? {
                        (1, v) => surface = v.text().map_err(w)?,
                        (2, v) => key = Some(v.text().map_err(w)?),
                        _ => {}
                    }
                }
                apply_widget(editor, surface, key);
                out.mutated = true;
            }
            kind::SETTINGS_CLICK => {
                let mut click = SettingsClick {
                    button: "cancel",
                    ..SettingsClick::default()
                };
                for f in Reader::new(payload) {
                    match f.map_err(w)? {
                        (1, v) => click.kind = v.text().map_err(w)?,
                        (2, v) => click.a = v.uint() as usize,
                        (3, v) => click.b = v.uint() as usize,
                        (4, v) => click.double = v.flag(),
                        (5, v) => click.button = v.text().map_err(w)?,
                        _ => {}
                    }
                }
                apply_settings(editor, click);
                out.mutated = true;
            }
            kind::KB_EDIT_CLICK => {
                let mut row = None;
                for f in Reader::new(payload) {
                    if let (1, v) = f.map_err(w)? {
                        row = Some(v.uint());
                    }
                }
                apply_kbedit(editor, row);
                out.mutated = true;
            }
            kind::CONFIGURE => {
                self.telemetry = false;
                for f in Reader::new(payload) {
                    if let (1, v) = f.map_err(w)? {
                        self.telemetry = v.flag();
                    }
                }
            }
            kind::APPLIED => {
                for f in Reader::new(payload) {
                    if let (1, v) = f.map_err(w)? {
                        self.applied = self.applied.max(v.uint());
                    }
                }
                if self.held && self.ready() {
                    out.frame_owed = true;
                }
            }
            kind::CLIENT_HELLO => return Err("a second hello".to_string()),
            other => return Err(format!("unknown message type {other}")),
        }
        Ok(())
    }

    fn send_hello(&mut self, scene: &EncodedScene, grid: &GridFrame<'_>) {
        self.body.clear();
        self.scene.write_hello(&mut self.body, scene, Some(grid), None);
        self.queue(kind::SERVER_HELLO);
        self.hello_sent = true;
        self.seq = 0;
        self.applied = 0;
        self.sent_at = Instant::now();
    }

    fn send_frame(&mut self, scene: &EncodedScene, grid: &GridFrame<'_>, base: Timing) {
        let started = Instant::now();
        self.body.clear();
        let changed = self.scene.write_frame(&mut self.body, self.seq + 1, scene, Some(grid), None);
        if !changed && self.input_reply.is_none() {
            return;
        }
        if let Some(reply) = self.input_reply.take() {
            wire::field::msg(&mut self.body, 24, &reply);
        }
        if self.telemetry {
            // Timing goes last, so it can say how long writing this frame took.
            let timing = Timing {
                ack: self.received,
                encode_ms: base.encode_ms + started.elapsed().as_secs_f64() * 1000.0,
                queued: (self.outbuf.len() - self.out_start) as u64,
                ahead: self.seq.saturating_sub(self.applied),
                ..base
            };
            wire::field::msg(&mut self.body, 23, &timing);
        }
        self.seq += 1;
        self.sent_at = Instant::now();
        self.queue(kind::FRAME);
    }

    /// Queue `body` as a message of type `k`, and send what the socket takes.
    fn queue(&mut self, k: u64) {
        wire::put_header(&mut self.outbuf, k, self.body.len());
        self.outbuf.extend_from_slice(&self.body);
        drop(self.flush());
    }

    /// Write what the socket takes now; the rest waits for the next pass. An
    /// app that stops reading past `WS_OUTBUF_CAP` is behind beyond recovery.
    fn flush(&mut self) -> std::io::Result<()> {
        if self.outbuf.len() - self.out_start > WS_OUTBUF_CAP {
            return Err(std::io::Error::other("the app stopped reading"));
        }
        while self.out_start < self.outbuf.len() {
            match self.stream.write(&self.outbuf[self.out_start..]) {
                Ok(0) => return Err(std::io::Error::from(ErrorKind::WriteZero)),
                Ok(n) => self.out_start += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.out_start == self.outbuf.len() {
            self.outbuf.clear();
            self.out_start = 0;
        } else if self.out_start > self.outbuf.len() / 2 {
            self.outbuf.drain(..self.out_start);
            self.out_start = 0;
        }
        Ok(())
    }

    /// Tell the app why it is being dropped, as far as the socket takes it.
    fn fail(&mut self, reason: &str) {
        self.body.clear();
        wire::field::text(&mut self.body, 1, reason);
        self.queue(kind::ERROR);
        drop(self.stream.flush());
    }
}

/// A `Mouse` message as the event it names; `None` for an unknown kind.
fn mouse(payload: &[u8]) -> Result<Option<MouseInput>, WireError> {
    let (mut kind, mut button) = (0, 0);
    let mut input = MouseInput {
        kind: MouseEventKind::Moved,
        col: 0,
        row: 0,
        n: 1,
        count: 1,
        modifiers: KeyModifiers::empty(),
    };
    let mut n = 1;
    for f in Reader::new(payload) {
        let (tag, v): (u32, Field<'_>) = f?;
        match tag {
            1 => kind = v.uint(),
            2 => button = v.uint(),
            3 => input.col = v.uint() as u16,
            4 => input.row = v.uint() as u16,
            5 => n = v.uint(),
            6 => input.count = v.uint(),
            7 if v.flag() => input.modifiers |= KeyModifiers::CONTROL,
            8 if v.flag() => input.modifiers |= KeyModifiers::ALT,
            9 if v.flag() => input.modifiers |= KeyModifiers::SHIFT,
            _ => {}
        }
    }
    let button = match button {
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        _ => MouseButton::Left,
    };
    input.kind = match kind {
        1 => MouseEventKind::Down(button),
        2 => MouseEventKind::Up(button),
        3 => MouseEventKind::Drag(button),
        4 => MouseEventKind::Moved,
        5 => MouseEventKind::ScrollUp,
        6 => MouseEventKind::ScrollDown,
        7 => MouseEventKind::ScrollLeft,
        8 => MouseEventKind::ScrollRight,
        _ => return Ok(None),
    };
    input.n = n.clamp(1, 10) as u16;
    Ok(Some(input))
}
