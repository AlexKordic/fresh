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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCENE_CLIENT: AtomicU64 = AtomicU64::new(1);

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
    compose: super::compose::ComposeCache,
    assets: super::compose_assets::Service,
    viewport_clients: HashSet<u64>,
    /// Allocations of the last push, for the next frame's telemetry.
    last_push_allocs: Option<(u64, u64)>,
}

/// One connected app.
struct SceneClient {
    id: u64,
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
    measured_tables: bool,
    /// Measurements already delivered for each visible pane/table identity.
    table_measurements: HashSet<(u64, u64, u64, usize, usize, u64)>,
    last_input: u64,
    last_input_reply: Option<super::compose::InputReply>,
    pending_input_target: Option<super::compose::InputTarget>,
    last_measurement: Option<(u64, u64, u64, u64, bool)>,
    input_reply: Option<super::compose::InputReply>,
    input_scope: Option<super::compose::Scope>,
    navigation: super::compose_navigation::Session,
    rich_encoded: EncodedScene,
    asset_transfers: VecDeque<super::compose_assets::Transfer>,
    asset_finished: Vec<u64>,
    last_asset: u64,
    asset_targets: HashMap<u64, super::compose_assets::Target>,
    viewport_reply: Option<super::compose_viewport::Reply>,
    last_viewport_reply: Option<super::compose_viewport::Reply>,
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
            compose: Default::default(),
            assets: Default::default(),
            viewport_clients: HashSet::new(),
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
        let active = editor.active_session_id();
        for (&id, window) in &mut editor.windows {
            if id != active { window.compose_readiness.retain_demands(&[]); }
            out.frame_owed |= window.compose_readiness.expire(Instant::now());
        }
        // Release payloads sent in the previous push before accepting the
        // next requests. A client may pipeline another image with its Applied
        // ack; it must not receive a spurious busy reply for an already freed
        // transfer slot.
        for client in &mut self.clients {
            for id in client.asset_finished.drain(..) {
                self.assets.cancel(client.id, id);
                client.asset_targets.remove(&id);
            }
        }
        let had_clients = !self.clients.is_empty();
        let mut i = 0;
        while i < self.clients.len() {
            match self.clients[i].read(editor, &mut out, &mut self.compose, &mut self.assets) {
                Ok(()) => i += 1,
                Err(reason) => {
                    eprintln!("[scene] client dropped: {reason}");
                    let mut client = self.clients.remove(i);
                    client.fail(&reason);
                    out.resize_dirty = true;
                }
            }
        }
        let live: HashSet<_> = self.clients.iter().map(|client| client.id).collect();
        for client in &mut self.clients {
            if let Some(target) = client.pending_input_target.filter(|target| !target.valid(editor)) {
                client.cancel_input(target.id, "The pending editor action was cancelled because its file or view changed.");
                out.frame_owed = true;
            }
        }
        self.assets.retain_clients(&live);
        if live != self.viewport_clients {
            super::compose_viewport::retain(editor, &live);
            self.viewport_clients = live;
        }
        for client in &mut self.clients {
            let obsolete: Vec<_> = client.asset_targets.iter().filter(|(_, target)| !target.current(editor)).map(|(id, _)| *id).collect();
            for id in obsolete {
                self.assets.cancel(client.id, id);
                client.asset_targets.remove(&id);
                client.asset_transfers.retain(|transfer| transfer.id != id);
                client.asset_transfers.push_back(super::compose_assets::Transfer::failure(id, "Image source or authority changed; retry the load"));
            }
        }
        for (id, transfer) in self.assets.completed() {
            if let Some(client) = self.clients.iter_mut().find(|client| client.id == id) {
                if client.asset_targets.contains_key(&transfer.id) { client.asset_transfers.push_back(transfer); }
            }
        }
        out.frame_owed |= self.clients.iter().any(|client| !client.asset_transfers.is_empty() && client.ready());
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    tune(&stream);
                    self.clients.push(SceneClient::new(NEXT_SCENE_CLIENT.fetch_add(1, Ordering::Relaxed), stream));
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
        if !self.clients.iter().any(|client| client.greeted && client.rich_compose) {
            for window in editor.windows.values_mut() { window.compose_regions.clear(); window.compose_readiness.clear(); }
        }
        out
    }

    /// Build the scene once and send each app what it lacks: a `ServerHello`
    /// to one that just said hello, a `Frame` to the others. Returns true
    /// while an app is owed a frame it was held back from (it is behind);
    /// while none can take one, nothing is built.
    pub fn push_scene(&mut self, editor: &mut Editor, cols: u16, rows: u16) -> bool {
        super::compose_viewport::retain(editor, &self.clients.iter().map(|client| client.id).collect());
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
        let build_ms = started.elapsed().as_secs_f64() * 1000.0;
        let encode_started = Instant::now();
        let (clip_seq, clip_text) = source.clip.current(editor);
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
        let mut compose_regions = Vec::new();
        for client in &mut self.clients {
            if !client.greeted {
                continue;
            }
            // Text geometry and the requested source region belong to this
            // client. The common grid and chrome are still rendered once.
            let mut rich_encoded = std::mem::take(&mut client.rich_encoded);
            let mut timing = base;
            let mut table_measurements = HashSet::new();
            if client.rich_compose {
                let gather_started = Instant::now();
                if model.aux_modal.is_none() && editor.menu_state().active_menu.is_none() {
                    for pane in &mut model.panes {
                        if let Some(buffer) = pane.buffer {
                            let leaf = fresh_core::LeafId(fresh_core::SplitId(pane.leaf));
                            let buffer = crate::model::event::BufferId(buffer);
                            let viewport = super::compose_viewport::snapshot(editor, client.id, leaf, buffer);
                            let region = viewport.map(|v| v.position);
                            pane.compose = self.compose.gather_tables_at(editor, leaf, buffer, region, client.measured_tables);
                            if let Some(compose) = &mut pane.compose { compose.viewport = viewport; }
                            if let Some(compose) = &mut pane.compose {
                                for table in &mut compose.tables {
                                    let key = (model.window_id, pane.leaf as u64, compose.asset_document, table.start, table.end, table.measurement_id);
                                    table_measurements.insert(key);
                                    if client.table_measurements.contains(&key) { table.measurement = None; }
                                }
                            }
                            if let Some(compose) = &pane.compose {
                                compose_regions.push(crate::app::window::ComposeRegion {
                                    leaf, buffer, revision: compose.buffer_revision,
                                    source: compose.source_start..compose.source_end,
                                });
                            }
                        }
                    }
                }
                // A deferred motion can need source far from what this client
                // is reading. Supply it in the reply for geometry, without
                // replacing the visible viewport merely to measure a command.
                if let Some(scope) = client.input_scope.or(client.pending_input_target.map(|target| target.scope)) {
                    if scope.window == model.window_id {
                        let revision = editor.active_window().buffer_state(scope.buffer).map_or(0, |state| state.buffer.version());
                        if let Some(region) = client.navigation.region(scope.window, scope.leaf, scope.buffer, revision) {
                            let keep=client.navigation.context(&scope);
                            let measurement = self.compose.gather_tables_with_context(editor, scope.leaf, scope.buffer, Some(region), client.measured_tables, keep);
                            if let Some(compose) = &measurement {
                                compose_regions.push(crate::app::window::ComposeRegion { leaf: scope.leaf, buffer: scope.buffer,
                                    revision: compose.buffer_revision, source: compose.source_start..compose.source_end });
                                if let Some(target) = client.pending_input_target {
                                    let ready = compose.ready != Some(false);
                                    let fingerprint = (target.id, compose.buffer_revision, compose.projection_revision, compose.selection_revision, ready);
                                    if client.input_reply.is_none() && ready && client.last_measurement != Some(fingerprint) {
                                        client.input_reply = Some(super::compose::InputReply { id: target.id, needs_layout: true, ..Default::default() });
                                    }
                                    if let Some(reply) = client.input_reply.as_mut().filter(|reply| !reply.accepted && !reply.cancelled) {
                                        reply.compose = measurement;
                                        // A newly ready offscreen projection must retain the
                                        // requested focus. A later pixel request takes priority.
                                        if reply.need_region.is_none() && reply.need_y.is_none() { reply.need_region = Some(region); }
                                        client.last_measurement = Some(fingerprint);
                                    }
                                }
                            }
                        }
                    }
                }
                timing.build_ms += gather_started.elapsed().as_secs_f64() * 1000.0;
                let encode_started = Instant::now();
                let (seq, text) = source.clip.current(editor);
                rich_encoded.encode(&model, wire::Clipboard { seq, text });
                rich_encoded.table_reference_hashes(&mut model);
                timing.encode_ms += encode_started.elapsed().as_secs_f64() * 1000.0;
                timing.cpu_ms = process_cpu_ms();
                for pane in &mut model.panes { pane.compose = None; }
            }
            let encoded = if client.rich_compose { &rich_encoded } else { &self.encoded };
            let sent = if !client.hello_sent {
                client.send_hello(encoded, &grid);
                true
            } else if client.ready() {
                client.held = false;
                client.send_frame(encoded, &grid, timing);
                true
            } else {
                client.held = true;
                false
            };
            client.rich_encoded = rich_encoded;
            // A held scene was never delivered. Keep the previous bases until
            // a real frame/hello establishes the receiver's current pane set.
            if sent { client.table_measurements = table_measurements; }
        }
        compose_regions.sort_by_key(|r| (r.buffer.0, r.leaf.0.0, r.source.start, r.source.end));
        compose_regions.dedup();
        let window = editor.active_window_mut();
        window.compose_readiness.retain_demands(&compose_regions);
        if window.compose_regions != compose_regions {
            window.compose_regions = compose_regions;
            editor.request_plugin_render();
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
    fn new(id: u64, stream: UnixStream) -> Self {
        Self {
            id,
            stream,
            inbuf: Vec::new(),
            outbuf: Vec::new(),
            out_start: 0,
            greeted: false,
            hello_sent: false,
            telemetry: false,
            rich_compose: false,
            measured_tables: false,
            table_measurements: HashSet::new(),
            last_input: 0,
            last_input_reply: None,
            pending_input_target: None,
            last_measurement: None,
            input_reply: None,
            input_scope: None,
            navigation: Default::default(),
            rich_encoded: EncodedScene::default(),
            asset_transfers: VecDeque::new(),
            asset_finished: Vec::new(),
            last_asset: 0,
            asset_targets: HashMap::new(),
            viewport_reply: None,
            last_viewport_reply: None,
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
    fn read(&mut self, editor: &mut Editor, out: &mut BridgePoll, compose: &mut super::compose::ComposeCache,
        assets: &mut super::compose_assets::Service) -> Result<(), String> {
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
                    if let Err(e) = self.handle(k, payload, editor, out, compose, assets) {
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

    fn finish_input(&mut self, reply: super::compose::InputReply) {
        if reply.accepted || reply.cancelled {
            self.last_input = self.last_input.max(reply.id);
            self.last_input_reply = Some(reply.clone());
            self.pending_input_target = None;
            self.last_measurement = None;
        }
        self.input_reply = Some(reply);
    }

    fn cancel_input(&mut self, id: u64, error: &str) {
        self.navigation.clear();
        self.input_scope = None;
        self.finish_input(super::compose::InputReply { id, cancelled: true, error: error.into(), ..Default::default() });
    }

    fn handle(
        &mut self,
        k: u64,
        payload: &[u8],
        editor: &mut Editor,
        out: &mut BridgePoll,
        compose: &mut super::compose::ComposeCache,
        assets: &mut super::compose_assets::Service,
    ) -> Result<(), String> {
        let w = |e: WireError| e.to_string();
        if !self.greeted {
            if k != kind::CLIENT_HELLO {
                return Err(format!("expected a hello, got message type {k}"));
            }
            let (mut version, mut telemetry, mut rich_compose, mut measured_tables) = (0, false, false, false);
            for f in Reader::new(payload) {
                match f.map_err(w)? {
                    (1, v) => version = v.uint(),
                    (2, v) => telemetry = v.flag(),
                    (3, v) => rich_compose = v.flag(),
                    (4, v) => measured_tables = v.flag(),
                    _ => {}
                }
            }
            if version != u64::from(wire::VERSION) {
                return Err(format!("scene protocol {version}, expected {}", wire::VERSION));
            }
            self.greeted = true;
            self.telemetry = telemetry;
            self.rich_compose = rich_compose;
            if measured_tables && !rich_compose { return Err("measured tables require rich_compose".into()); }
            self.measured_tables = measured_tables;
            out.frame_owed = true;
            return Ok(());
        }
        match k {
            kind::COMPOSE_VIEWPORT_REQUEST => {
                if !self.rich_compose || !self.hello_sent { return Err("viewport requests require a rich scene hello".into()); }
                if self.viewport_reply.is_some() { return Err("viewport requests must wait for their reply".into()); }
                let request = super::compose_viewport::Request::parse(payload)?;
                let reply = if let Some(previous) = self.last_viewport_reply.filter(|reply| reply.id == request.id) {
                    previous
                } else {
                    if self.last_viewport_reply.is_some_and(|reply| reply.id > request.id) { return Err("viewport request ids must increase".into()); }
                    super::compose_viewport::Reply { id: request.id, accepted: super::compose_viewport::update(editor, self.id, &request) }
                };
                self.last_viewport_reply = Some(reply);
                self.viewport_reply = Some(reply);
                out.frame_owed = true;
            }
            kind::COMPOSE_ASSET => {
                if !self.rich_compose { return Err("image requests require rich_compose in hello".into()); }
                let request = super::compose_assets::Request::parse(payload).map_err(w)?;
                if request.cancel {
                    assets.cancel(self.id, request.id);
                    self.asset_transfers.retain(|transfer| transfer.id != request.id);
                    self.asset_targets.remove(&request.id);
                    return Ok(());
                }
                if request.id <= self.last_asset { return Err("image request ids must increase".into()); }
                self.last_asset = request.id;
                if self.asset_transfers.len() >= 16 { return Err("too many pending image replies".into()); }
                let resolved = (|| {
                    let scope = request.scope.as_ref().ok_or("image request requires a document")?;
                    if scope.window != editor.active_window().id.0 || editor.active_window().pane_buffer(scope.leaf) != Some(scope.buffer) {
                        return Err("image document is no longer displayed".to_string());
                    }
                    let viewport = super::compose_viewport::snapshot(editor, self.id, scope.leaf, scope.buffer);
                    let region = viewport.map(|v| v.position);
                    let pane = compose.gather_tables_with_context(editor, scope.leaf, scope.buffer, region, self.measured_tables, self.navigation.context(scope)).ok_or("image document is not composing")?;
                    if pane.buffer_revision != scope.revision { return Err("image source changed; request its current annotation".into()); }
                    if pane.asset_authority != request.authority { return Err("image filesystem authority changed".into()); }
                    if pane.asset_document != request.document { return Err("image document path changed".into()); }
                    let image = pane.images.iter().find(|image| image.start == request.start && image.end == request.end)
                        .ok_or("image annotation is no longer in this source region")?;
                    let window = editor.active_window();
                    let document = window.buffer_state(scope.buffer).and_then(|s| s.buffer.file_path()).ok_or("save the document before loading relative images")?;
                    let path = super::compose_assets::resolve(document, &image.target)?;
                    Ok((super::compose_assets::Target { scope: *scope, fs: window.authority().filesystem.clone(), document: document.to_path_buf(), instance: window.buffer_state(scope.buffer).unwrap().marker_list.instance_id() }, path))
                })();
                let result = resolved.map_err(|error| (error, false)).and_then(|(target, path)| {
                    assets.submit(self.id, request.id, target.fs.clone(), path, request.conditional)
                        .map_err(|error| (error, true))?;
                    self.asset_targets.insert(request.id, target);
                    Ok(())
                });
                if let Err((error, retryable)) = result {
                    self.asset_transfers.push_back(if retryable { super::compose_assets::Transfer::retryable_failure(request.id, error) }
                        else { super::compose_assets::Transfer::failure(request.id, error) });
                    out.frame_owed = true;
                }
            }
            kind::COMPOSE_INPUT => {
                if !self.rich_compose { return Err("rich input requires rich_compose in hello".into()); }
                let mut input = super::compose::Input::parse(payload).map_err(w)?;
                self.input_scope = input.scope;
                if input.id == 0 { return Err("rich input has no request id".into()); }
                if input.id > self.last_input.saturating_add(1) { return Err("rich input request ids must be sequential".into()); }
                if let Some(reply) = &self.input_reply {
                    // A terminal cancellation can race a legal retry of the
                    // earlier needs-layout reply. Keep the terminal outcome;
                    // do not drop the connection or run the action again.
                    if reply.id == input.id {
                        out.frame_owed = true;
                        return Ok(());
                    }
                    return Err("rich inputs must wait for their reply".into());
                }
                if input.id <= self.last_input {
                    self.input_reply = Some(self.last_input_reply.as_ref().filter(|reply| reply.id == input.id).cloned()
                        .unwrap_or(super::compose::InputReply { id: input.id, accepted: true, ..Default::default() }));
                    out.frame_owed = true;
                    return Ok(());
                }
                if input.cancel {
                    self.cancel_input(input.id, "The pending editor action was cancelled because its file or view changed.");
                    out.frame_owed = true;
                    return Ok(());
                }
                self.pending_input_target = input.scope.map(|scope| {
                    let epoch = if input.buffer_epoch != 0 { input.buffer_epoch } else {
                        editor.windows.get(&fresh_core::WindowId(scope.window)).and_then(|window| window.buffer_state(scope.buffer))
                            .map_or(0, |state| state.marker_list.instance_id())
                    };
                    super::compose::InputTarget { id: input.id, scope, epoch, pointer: input.position.is_some() }
                });
                if self.pending_input_target.is_some_and(|target| !target.valid(editor)) {
                    self.cancel_input(input.id, "The pending editor action was cancelled because its file or view changed.");
                    out.frame_owed = true;
                    return Ok(());
                }
                if input.replacement.is_some() || input.text_input {
                    if !super::compose::document_keyboard(editor) {
                        self.finish_input(super::compose::InputReply { id: input.id, needs_layout: true, ..Default::default() });
                        out.frame_owed = true;
                        return Ok(());
                    }
                    let cursors = if input.replacement.is_some() {
                        input.replacement_cursor.into_iter().chain(input.replacements.iter().map(|r| r.cursor)).collect()
                    } else { editor.active_window().active_cursors().iter().map(|(id, _)| id.0).collect::<Vec<_>>() };
                    let preserved_cursors = input.preserved_cursors(editor);
                    let result = input.replace(editor);
                    match result {
                        Ok(()) => {
                            out.mutated = true;
                            self.navigation.clear();
                            let edit = input.scope.and_then(|scope| {
                                let epoch = editor.active_window().buffer_state(scope.buffer)?.marker_list.instance_id();
                                let position = editor.active_window().active_cursors().primary().position;
                                let mut pane = compose.gather_tables_at(editor, scope.leaf, scope.buffer, Some(position), self.measured_tables)?;
                                pane.viewport = super::compose_viewport::snapshot(editor, self.id, scope.leaf, scope.buffer);
                                if let Some(viewport) = &pane.viewport { pane.top_byte = viewport.position; }
                                Some(super::compose::EditReceipt { window: scope.window, leaf: scope.leaf.0.0, buffer: scope.buffer.0, epoch, pane, cursors, preserved_cursors })
                            });
                            self.finish_input(super::compose::InputReply { id: input.id, accepted: true, edit, ..Default::default() });
                        }
                        Err(error) => self.cancel_input(input.id, &error),
                    }
                    out.frame_owed = true;
                    return Ok(());
                }
                if input.source_revision.is_some() {
                    match input.rebase_pointer(editor, compose) {
                        Ok(()) if input.pointer(editor) => {
                            out.mutated = true;
                            self.navigation.clear();
                            let carets = self.navigation.pointer(editor, &input);
                            self.finish_input(super::compose::InputReply { id: input.id, accepted: true, carets, ..Default::default() });
                        }
                        Ok(()) => self.cancel_input(input.id, "The clicked file is no longer available. Click again."),
                        Err(error) => self.cancel_input(input.id, &error),
                    }
                    out.frame_owed = true;
                    return Ok(());
                }
                if input.scope.is_some() && !super::compose::document_keyboard(editor) {
                    self.finish_input(super::compose::InputReply { id: input.id, needs_layout: true, ..Default::default() });
                    out.frame_owed = true;
                    return Ok(());
                }
                let mut accepted = false;
                let current = input.scope.as_ref().and_then(|scope| {
                    let viewport = (scope.window == editor.active_window().id.0).then(|| super::compose_viewport::snapshot(editor, self.id, scope.leaf, scope.buffer)).flatten();
                    let region = self.navigation.region(scope.window, scope.leaf, scope.buffer, scope.revision).or(viewport.map(|v| v.position));
                    compose.gather_tables_with_context(editor, scope.leaf, scope.buffer, region, self.measured_tables, self.navigation.context(scope))
                });
                if current.as_ref().is_some_and(|pane| pane.ready == Some(false)) {
                    self.finish_input(super::compose::InputReply { id: input.id, needs_layout: true, ..Default::default() });
                    out.frame_owed = true;
                    return Ok(());
                }
                let snapshot = current.filter(|pane| input.scope.as_ref().is_some_and(|scope| scope.matches(editor, pane)));
                if snapshot.is_none() && input.scope.is_some_and(|scope| compose.gather_at(editor, scope.leaf, scope.buffer, None).is_none()) {
                    self.cancel_input(input.id, "Rich Compose is no longer available in this view. Retry your action.");
                    out.frame_owed = true;
                    return Ok(());
                }
                let mut reply = super::compose::InputReply { id: input.id, accepted, ..Default::default() };
                if !accepted && (input.scope.is_none() || snapshot.is_some()) {
                    if let (Some(scope), Some(region)) = (&input.scope, input.region) {
                        if region > snapshot.as_ref().unwrap().document_len { return Err("source region is outside the document".into()); }
                        self.navigation.request_region(scope, region);
                        reply.needs_layout = true;
                        self.finish_input(reply);
                        out.frame_owed = true;
                        return Ok(());
                    }
                    if input.position.is_some() {
                        if let Some(start) = input.attachment_start {
                            if input.extend || !snapshot.as_ref().is_some_and(|pane| pane.images.iter().any(|image| (image.rendered || image.source_preview) && image.start == start && Some(image.end) == input.position)) {
                                return Err("image selection does not match its source annotation".into());
                            }
                        }
                        accepted = input.pointer(editor);
                        out.mutated |= accepted;
                        if accepted { reply.carets = self.navigation.pointer(editor, &input); }
                    } else if (kind::KEY..=kind::KB_EDIT_CLICK).contains(&input.kind) {
                        if let Some(scope) = &input.scope {
                            editor.focus_split(scope.leaf, scope.buffer);
                            self.navigation.begin(editor, &input, snapshot.as_ref().unwrap())?;
                        } else {
                            self.navigation.clear();
                        }
                        let applied = if let Some(action) = self.navigation.pending_action(input.id) {
                            out.mutated = true;
                            editor.handle_action(action).map_err(|e| e.to_string())
                        } else {
                            self.handle(input.kind, input.payload, editor, out, compose, assets)
                        };
                        // Always remove the per-input geometry, including a
                        // malformed nested payload or a failed action. An
                        // error must never leave it attached to terminal input.
                        reply = self.navigation.finish(editor, &input);
                        applied?;
                        accepted = reply.accepted;
                    } else {
                        return Err("unsupported input inside ComposeInput".into());
                    }
                }
                reply.accepted = accepted;
                self.finish_input(reply);
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
        if !changed && self.input_reply.is_none() && self.viewport_reply.is_none() && self.asset_transfers.is_empty() {
            return;
        }
        if let Some(transfer) = self.asset_transfers.front_mut() {
            let chunk = transfer.next();
            wire::field::msg(&mut self.body, 25, &chunk);
            if chunk.done {
                self.asset_finished.push(chunk.id);
                self.asset_transfers.pop_front();
            }
        }
        if let Some(reply) = self.input_reply.take() {
            wire::field::msg(&mut self.body, 24, &reply);
            self.input_scope = None;
        }
        if let Some(reply) = self.viewport_reply.take() { wire::field::msg(&mut self.body, 26, &reply); }
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
