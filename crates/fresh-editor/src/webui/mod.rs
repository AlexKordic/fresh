//! Local HTTP bridge that hosts the **real** editor for the web UI (no mocks).
//!
//! The frontend renders the real editor by tapping the **actual render
//! pipeline**: we run `Editor::render` once into an in-memory cell buffer, then
//! read the geometry the pipeline already aggregated for the frame
//! (the frame's tree + `ChromeLayout`) and slice the rendered cells. Nothing
//! about layout, highlighting, tabs, scrollbars, or split borders is
//! re-implemented — we only re-target the final drawing:
//!
//!   - **buffer interiors** (text inside each split pane) are emitted as the
//!     real, syntax-highlighted **cells** the pipeline drew in each pane's
//!     `content_rect`;
//!   - **chrome** (menu bar, status bar, tabs, scrollbars, split borders, the
//!     file-explorer pane) is emitted as **semantic regions** (role + rect, with
//!     thumb/orientation as needed) so the frontend draws real UI elements.
//!
//! Two hosts, one [`WebBridge`] (single-threaded either way — the editor is not
//! `Send`, so everything stays on the thread that owns it):
//!
//!   - [`run`] builds its own editor and serves only browsers. This is the
//!     `webui_server` example, the Playwright suite and the scene-parity
//!     harness — and the only place `POST /reset` may rebuild the editor.
//!   - `fresh --web` runs the **session daemon** (`crate::server::EditorServer`)
//!     with the bridge hosted inside it, so browsers and `fresh -a` terminals
//!     are two transports onto ONE editor: type in either and the other sees
//!     it, close either and the session lives on. The daemon owns the tick and
//!     the grid size (the element-wise min of the browsers' viewports and the
//!     primary terminal's), and chrome suppression is scoped to the browser's
//!     render — the terminals still get chrome drawn into their cells.
//!
//! Transport:
//!
//! **WebSocket push (`GET /ws`)** is the browser's live channel
//! (docs/internal/web-ui.md §3.1). Hand-rolled RFC 6455, matching the
//! hand-rolled HTTP below. On connect the client receives
//! `{"type":"hello","seq":0,"scene":<full scene>}`; afterwards the server
//! pushes `{"type":"frame","seq":N,"changed":{<path>:<value>,...}}` frames
//! ONLY when something changed, where each path replaces its value wholesale:
//! `"w"`, `"h"`, `"theme"`, `"clipboard"`, `"regions.<key>"` — except panes,
//! which diff one level deeper as `"regions.panes.<index>"` plus
//! `"regions.panes.len"` when the pane count changes (panes carry the bulk of
//! the bytes; typing resends only the changed pane). Client→server input is
//! JSON text frames, tagged `{"type":"key"|"mouse"|"action"|"widget"|
//! "settings"|"kbedit"|"paste"|"resize"}` — each carrying the same fields as
//! the HTTP POST bodies below.
//!
//! Session model: MANY WebSocket clients, ALL mirroring the one single-threaded
//! editor (shared-view). Every `/ws` upgrade JOINS the client set — a second
//! tab, another device, or the page you load after a server restart all connect
//! and stay live; none is bounced (the old first-come-`409` model locked out
//! the newcomer, so a stale/half-open background tab could hold the single slot
//! until a server restart — the exact "can't load the page" failure this
//! replaced). The scene is built once per tick and pushed to each client as its
//! own region diff; input is accepted from all of them and applied to the one
//! editor (for one user across tabs/devices the interleaving is the point;
//! independent per-client cursors/viewports are the deeper §3.7 work, still
//! PLANNED). Because there is one editor and one grid but clients have
//! different window sizes, the render is fit to the element-wise MIN of every
//! client's viewport (`effective_size`) so it fits all of them — bigger windows
//! letterbox. A liveness heartbeat keeps this honest: the server pings every
//! `WS_PING_INTERVAL` and reaps a client gone silent past `WS_LIVENESS_TIMEOUT`
//! (browsers auto-pong, so a live-but-idle client stays), so a half-open peer —
//! a tab killed without a clean close, a slept laptop — can't linger and, in
//! particular, can't pin the shared grid to a dead small window's size. The
//! `Origin` header, when
//! present, must have the same host as the request's `Host` header (i.e. the
//! request is same-origin) AND that host must be a loopback/LAN literal or our
//! bind address, or the upgrade is rejected with `403 Forbidden`. The
//! same-origin half blocks a plain cross-origin page; the Host-allowlist half
//! blocks DNS rebinding, where an attacker-controlled name resolves to us so
//! `Origin` and `Host` agree (see `origin_host_matches` / `host_is_allowed`).
//! Comparing against `Host` (rather than the bind address) is what lets a
//! wildcard bind like `--web 0.0.0.0:8137` work when reached as
//! `127.0.0.1:8137`; non-browser tools send no Origin and are accepted.
//!
//! **HTTP routes** all keep working (full-scene responses); curl, the Playwright
//! harness and the parity routes depend on them. A mutation made over HTTP
//! reaches a connected WebSocket client as a pushed diff on the next tick.
//! State-mutating `POST`s are gated by the SAME same-origin/Host check as the
//! `/ws` upgrade (in `serve_request`), so a cross-origin browser page can't
//! drive the editor over HTTP — but non-browser callers (curl, the parity
//! harness, the Playwright request API) send no `Origin` and pass:
//!   - `GET /`        → serves the page assembled from `web-ui/` (see `INDEX_HTML`)
//!   - `GET /favicon.ico` → 204
//!   - `GET /state`   → `{ w, h, regions, theme, clipboard }` from the real render
//!   - `POST /key`    → runs the real `Editor::handle_key`, returns `/state`
//!   - `POST /paste`  → `{text}` → the editor's bracketed-paste path, returns `/state`
//!   - `POST /resize` → `{cols, rows}` → `Editor::resize`, returns `/state`
//!   - `POST /mouse` `/action` `/widget` `/settings` `/kbedit` → same pattern
//!   - `POST /step` `/reset` → parity-harness routes (no clipboard attach)

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
// The browser sends a chord, never a keyboard layout's shifted codepoint, so
// the web path builds plain key presses (`Event::key`) with no layout char.
use fresh_input_parser::Event;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use serde_json::{json, Value};

use crate::app::Editor;
use crate::view::scene::Rgb;

mod wire;
#[cfg(unix)]
mod scene_bridge;
#[cfg(unix)]
pub use scene_bridge::SceneBridge;
use crate::config;
use crate::config_io::DirectoryContext;
use crate::model::filesystem::{FileSystem, StdFileSystem};

/// Default terminal size the bridge boots / resets to (cols, rows). One source
/// so `run()`, the `/reset` route, and the daemon's pre-client grid (a browser
/// may be the first thing to attach, before any terminal reports a size) can't
/// drift apart.
pub const DEFAULT_SIZE: (u16, u16) = (140, 44);

/// The web-UI frontend served at `GET /`, embedded at compile time from the
/// page assembled by `build.rs` out of the split sources under `web-ui/`
/// (`shell.html` + `css/*.css` + `js/*.js`, concatenated in filename order).
/// The assembled page is the *only* source — there is no on-disk fallback,
/// so `fresh --web` (and the example bridge) is fully self-contained and
/// behaves identically wherever the binary runs. Editing the frontend
/// therefore requires a rebuild.
const INDEX_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/webui-index.html"));

/// Who a scene is gathered for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SceneFor {
    /// A browser: pane interiors as cells; it draws the chrome from the model.
    Browser,
    /// File Commander's app (`SceneBridge`): the whole frame as cells, chrome
    /// included, and no menu row while no menu is open — the app has Fresh's
    /// menus in its own menu bar.
    App,
}

/// Where part of building a scene went, for telemetry: rendering the editor
/// into cells, and hashing the grid's rows (`GridFrame`; a row's runs are
/// built when it is sent, in the diff).
#[derive(Clone, Copy, Debug, Default)]
struct SceneTimes {
    render_ms: f64,
    grid_ms: f64,
}

/// Cap on the clipboard text exposed in the scene (`ClipboardSync`). Anything
/// larger is truncated at a char boundary — a copy that big is better served
/// by a future dedicated fetch than by riding along on every scene response.
const CLIPBOARD_TEXT_CAP: usize = 1 << 20; // 1 MiB

/// Outbound OS-clipboard mirror (docs/internal/web-ui.md §3.5).
///
/// The editor's copy actions run server-side and land in its internal
/// clipboard (plus OSC 52/arboard for the TUI, which can't reach the
/// browser's clipboard). The bridge exposes that text in the scene as
/// `"clipboard": {"seq": N, "text": "..."}` where `seq` increments whenever
/// the copied text changed since the last scene build; the frontend writes
/// the text to `navigator.clipboard` when it sees a new `seq` (inside the
/// user-activation window of the very keypress/click that did the copy).
///
/// State lives here in the bridge — a hash of the last-seen text — so no new
/// core-editor state is needed; the core only gained the read-only
/// `Editor::clipboard_text()` accessor.
struct ClipboardSync {
    seq: u64,
    last_hash: u64,
}

impl ClipboardSync {
    /// Seed from the editor's current clipboard so the first scene after boot
    /// doesn't replay pre-existing content into the browser's clipboard.
    fn new(editor: &Editor) -> Self {
        Self {
            seq: 0,
            last_hash: Self::hash(editor.clipboard_text()),
        }
    }

    fn hash(text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        h.finish()
    }

    /// The clipboard's copy number and its text (capped), bumping the number
    /// if the editor's clipboard text changed. The text is never logged.
    fn current<'e>(&mut self, editor: &'e Editor) -> (u64, &'e str) {
        let text = editor.clipboard_text();
        let h = Self::hash(text);
        if h != self.last_hash {
            self.last_hash = h;
            self.seq += 1;
        }
        let mut end = text.len().min(CLIPBOARD_TEXT_CAP);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (self.seq, &text[..end])
    }

    /// Attach the clipboard object to a scene (see `current`).
    fn attach(&mut self, editor: &Editor, scene: &mut Value) {
        let (seq, text) = self.current(editor);
        scene["clipboard"] = json!({ "seq": seq, "text": text });
    }
}

/// Construct a fresh editor exactly as the web bridge does: real plugin runtime
/// enabled, init.ts loaded, chrome drawn as a semantic model (not cells). Shared
/// by `run()`, the `/reset` route (scenario isolation) and the parity test
/// runner so all three drive an identical editor.
/// Build the editor the bridge serves.
///
/// `fire_ready` is the startup lifecycle's second hook. A SERVED session wants
/// it — it is where the Orchestrator opens its dock and the welcome screen
/// opens itself. The harness entry points (`POST /reset`, the scene-parity
/// runner) want it OFF: both exist to hand a test a known editor, and the
/// hook's effects arrive asynchronously from the plugin thread, so firing it
/// would let a dock or a welcome tab land on the buffer a tick or two after
/// the reset returned — exactly the race the reset exists to remove.
pub fn build_editor(cols: u16, rows: u16, files: &[PathBuf], fire_ready: bool) -> Result<Editor> {
    let dir_context = DirectoryContext::from_system()?;
    let working_dir = std::env::current_dir().unwrap_or_default();
    let mut cfg = config::Config::load_with_layers(&dir_context, &working_dir);
    // Cell-level (TUI) animations would stream as bursts of frame diffs over
    // the bridge; the web frontend implements its own CSS-level motion
    // instead, so they are forced off regardless of the user's config.
    cfg.editor.animations = false;
    let fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(StdFileSystem);

    let mut editor = Editor::with_working_dir(
        cfg,
        cols,
        rows,
        Some(working_dir),
        dir_context,
        true, // plugins_enabled: load the real plugin runtime (git, orchestrator,
        // env manager, …) so the web UI is as full-featured as the TUI.
        crate::view::color_support::ColorCapability::TrueColor,
        fs,
    )?;
    // Mirror the TUI boot: load the user's init.ts and fire the plugins-loaded
    // lifecycle hook. Plugin loads run on the plugin thread and arrive via the
    // AsyncBridge, which `editor_tick` (run on every poll) drains — so by the
    // time the UI settles the plugin menus/commands are present.
    editor.load_init_script_async(true);
    editor.fire_plugins_loaded_hook();

    // We render chrome (menu, dropdown, command palette) as native HTML from the
    // semantic model, so tell the pipeline to compute chrome *layout* but not draw
    // it into the cells — the cell buffer carries pane interiors only, with no
    // chrome to hide. See docs/internal/web-ui.md.
    editor.suppress_chrome_cells = true;
    for f in files {
        if let Err(e) = editor.open_file(f) {
            eprintln!("open_file {f:?} failed: {e}");
        }
    }
    // The second startup lifecycle hook, in the order main.rs and the daemon
    // fire them: `plugins_loaded` once the registry and init.ts are in, then
    // `ready` once the startup files are open. `ready` is the "we have
    // finished starting up" signal — it is where the Orchestrator opens its
    // dock and the welcome screen opens itself — so a bridge that fired only
    // the first served a session with neither, while a directly-launched
    // editor and `fresh -a` had both. Opening the files first is what lets a
    // plugin branching on "is a real file open?" see them rather than race
    // them.
    if fire_ready {
        editor.process_pending_file_opens();
        editor.fire_ready_hook();
    }
    Ok(editor)
}

/// Apply one parity-scenario step to the editor: a key, a mouse event at a cell,
/// an action by name, a literal string to type, or a tick. Shared by the web
/// `/step` route and the Rust parity runner so both drive identical input.
pub fn apply_step(editor: &mut Editor, step: &Value) {
    if let Some(s) = step.get("type").and_then(|t| t.as_str()) {
        for ch in s.chars() {
            let key = ch.to_string();
            apply_key(
                editor,
                KeyInput {
                    key: &key,
                    ..KeyInput::default()
                },
            );
        }
    } else if step.get("key").is_some() {
        apply_key(editor, KeyInput::from_json(step));
    } else if step.get("kind").is_some() {
        if let Some(input) = MouseInput::from_json(step) {
            apply_mouse(editor, input);
        }
    } else if let Some(name) = step.get("action").and_then(|a| a.as_str()) {
        // Optional `args` object for parameterised actions
        // (e.g. {"action": "menu_open", "args": {"name": "File"}}).
        apply_action(editor, name, &action_args(step));
    }
    // Drain async work / step animations. The `bool` (needs-render) is moot —
    // the bridge re-renders the scene on every request — but surface a real
    // tick error rather than swallowing it.
    if let Err(e) = crate::app::editor_tick(editor, || Ok(())) {
        eprintln!("[webui] editor_tick error: {e}");
    }
}

/// Build the semantic scene (the same model the web frontend renders). Public so
/// the parity runner can assert on the identical scene the browser sees.
pub fn scene_value(editor: &mut Editor, cols: u16, rows: u16) -> Value {
    scene_json(editor, cols, rows)
}

/// File Commander's socket encoding of the scene, for tests: the messages a
/// client gets — a `ServerHello`, then `Frame`s — and the allocations
/// encoding the last one took (`alloc-stats` builds).
pub struct WireProbe {
    source: SceneSource,
    grid: GridHasher,
    encoded: wire::EncodedScene,
    client: wire::ClientScene,
    body: Vec<u8>,
    seq: u64,
    /// Allocations and bytes of encoding the last message: the scene's parts
    /// once, then the client's frame (rendering and the views not counted).
    pub last_encode_allocs: Option<(u64, u64)>,
}

impl WireProbe {
    pub fn new(editor: &Editor) -> Self {
        Self {
            source: SceneSource::new(editor),
            grid: GridHasher::default(),
            encoded: wire::EncodedScene::default(),
            client: wire::ClientScene::default(),
            body: Vec::new(),
            seq: 0,
            last_encode_allocs: None,
        }
    }

    /// The next message for the editor's scene at `cols` × `rows`: its type
    /// and payload, `None` for a frame with nothing new.
    pub fn next(&mut self, editor: &mut Editor, cols: u16, rows: u16) -> Option<(u64, Vec<u8>)> {
        let source = &mut self.source;
        let model = SceneModel::gather(editor, &mut source.renderer, cols, rows, SceneFor::App, &mut SceneTimes::default());
        let (seq, text) = source.clip.current(editor);
        let before = crate::alloc_stats::counts();
        let grid = self.grid.frame(model.buf);
        self.encoded.encode(&model, wire::Clipboard { seq, text });
        self.body.clear();
        let message = if self.seq == 0 {
            self.client.write_hello(&mut self.body, &self.encoded, Some(&grid), None);
            Some(wire::kind::SERVER_HELLO)
        } else {
            self.client
                .write_frame(&mut self.body, self.seq, &self.encoded, Some(&grid), None)
                .then_some(wire::kind::FRAME)
        };
        self.last_encode_allocs = crate::alloc_stats::since(before);
        self.seq += 1;
        message.map(|kind| (kind, self.body.clone()))
    }
}

/// `scene.proto`, as File Commander's socket encoding writes it. For tests.
pub fn wire_schema() -> String {
    wire::schema()
}

/// Render the SAME editor the way the TUI would — chrome drawn into cells — and
/// return the joined cell text. Used by the parity test to assert the web's
/// semantic scene and the terminal's cell rendering agree for one editor state
/// (single source of truth). Temporarily clears `suppress_chrome_cells`, then
/// restores it so the caller's web mode is unaffected.
pub fn render_tui_cells(editor: &mut Editor, cols: u16, rows: u16) -> String {
    let prev = editor.suppress_chrome_cells;
    editor.suppress_chrome_cells = false;
    let mut renderer = SceneRenderer::new();
    let (buf, _) = renderer.render(editor, cols, rows);
    editor.suppress_chrome_cells = prev;
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

/// Tick cadence: rebuild/diff/push the scene at ~25 fps while the editor
/// reports activity (animations / LSP progress / pending timers — the same
/// hint the scene exposes as `regions.poll.active`), and at a relaxed idle
/// cadence otherwise so async events still land within a quarter second with
/// zero input — client or no client.
const TICK_ACTIVE: Duration = Duration::from_millis(40);
const TICK_IDLE: Duration = Duration::from_millis(250);
/// Event-loop sleep. Small while a WS client is connected so its input is
/// picked up within a few ms (input latency is the point of the push
/// transport; the nonblocking accept/read per iteration are near-free, so
/// idle CPU stays ~0% — measured, see docs/internal/web-ui.md §3.1). Longer
/// when nothing is connected.
const SLEEP_CONNECTED: Duration = Duration::from_millis(3);
const SLEEP_IDLE: Duration = Duration::from_millis(25);
/// How long a connection may take to finish sending its request head + body
/// before we drop it (browsers open speculative sockets that never send).
const HTTP_READ_DEADLINE: Duration = Duration::from_secs(10);
/// WebSocket liveness heartbeat. We ping the client every `WS_PING_INTERVAL`;
/// a live browser auto-pongs (any inbound frame refreshes the session's
/// last-heard-from clock), so a client that goes silent past
/// `WS_LIVENESS_TIMEOUT` (a few missed pings) is a half-open peer — a tab
/// killed without a clean close, a slept laptop, a dropped network — and is
/// reaped. Without this a dead connection is invisible (no clean FIN ever
/// arrives): it would linger in the client set forever and, worse, keep
/// constraining the shared grid to its (now dead) window's size via
/// `effective_size`, shrinking every live client until a server restart.
const WS_PING_INTERVAL: Duration = Duration::from_secs(15);
const WS_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
/// Cap on a buffered HTTP request (head + body). Pastes are the biggest
/// legitimate payload; anything larger is a runaway client.
const HTTP_REQUEST_CAP: usize = 8 << 20;

/// What one [`WebBridge::poll`] pass did, so the host loop knows what the
/// shared editor needs before it renders.
#[derive(Default)]
pub struct BridgePoll {
    /// Browser input (a WebSocket message or an HTTP route) reached the editor,
    /// so the host should render and push this pass instead of waiting for a
    /// tick deadline.
    pub mutated: bool,
    /// A client joined, left, or reported a new viewport, so the host should
    /// refit the shared grid — see [`WebBridge::wanted_size`].
    pub resize_dirty: bool,
    /// File Commander's app caught up after frames were held back from it, so
    /// the host should push the scene now even though the editor didn't change
    /// (`SceneBridge`; browsers are never held back).
    pub frame_owed: bool,
}

/// The browser-facing half of the web UI, decoupled from *who owns the editor*.
///
/// Two hosts drive this same struct, so the transports cannot drift:
///
///   - [`run`] — the standalone bridge (the `webui_server` example and the
///     Playwright / scene-parity harnesses). It builds and owns the editor it
///     serves; nothing else is watching.
///   - [`crate::server::EditorServer`] — the session daemon, which owns the
///     editor *and* the IPC socket. Hosting the bridge there is what makes
///     `fresh --web` a real session: the browser and any number of `fresh -a`
///     terminals attach to ONE editor, and either can be closed and reopened
///     without losing it (docs/internal/web-ui.md §3.7).
///
/// The host owns the editor's tick and picks the grid size; the bridge only
/// reports the size its browsers want ([`wanted_size`](Self::wanted_size)),
/// which the daemon mins against its attached terminals.
pub struct WebBridge {
    listener: TcpListener,
    /// Host part of the bind address — only the fallback for the WS-upgrade
    /// Origin check when a request carries no Host header (the check prefers
    /// the request's Host header; see `origin_host_matches`).
    bind_host: String,
    /// In-flight HTTP requests whose head/body hasn't fully arrived yet (reads
    /// are nonblocking so a slow client can never stall the host's loop).
    pending: Vec<PendingConn>,
    /// Every connected WebSocket client. All mirror the SAME editor
    /// (shared-view, see the module docs): input is accepted from all, the
    /// scene is built once per tick and pushed to each, and the rendered grid
    /// is fit to the smallest viewport so it fits every window (bigger windows
    /// letterbox). Each client keeps its own diff cache.
    ws: Vec<WsSession>,
    /// The clipboard mirror and the renderer, kept between scenes.
    source: SceneSource,
    /// Files the `/reset` parity route re-opens in the rebuilt editor.
    files: Vec<PathBuf>,
    /// Whether `POST /reset` may replace the editor wholesale. True only for
    /// the standalone bridge, which owns its editor. Under the daemon the
    /// editor carries session state — the recovery session, the restored
    /// workspace, attached terminals — that a silent replacement would drop on
    /// the floor, so the route is refused there instead.
    allow_reset: bool,
}

impl WebBridge {
    /// Bind the HTTP/WebSocket listener. `editor` seeds the outbound clipboard
    /// mirror so the first scene doesn't replay pre-existing content into the
    /// browser's clipboard; `files` / `allow_reset` configure the `/reset`
    /// parity route (see [`WebBridge`]).
    pub fn bind(addr: &str, editor: &Editor, files: &[PathBuf], allow_reset: bool) -> Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let bind_host = addr
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(addr)
            .to_string();
        Ok(Self {
            listener,
            bind_host,
            pending: Vec::new(),
            ws: Vec::new(),
            source: SceneSource::new(editor),
            files: files.to_vec(),
            allow_reset,
        })
    }

    /// True while a browser is connected or a request is mid-assembly. The
    /// host paces its loop by this — and the daemon also keeps its idle
    /// timeout from reaping a session someone is using from a browser.
    pub fn is_busy(&self) -> bool {
        !self.ws.is_empty() || !self.pending.is_empty()
    }

    /// The grid size that fits every connected browser: the element-wise MIN of
    /// their wanted (cols, rows), so each can see the whole grid (bigger
    /// windows letterbox). Clients that haven't reported a size yet
    /// (`want == None`) don't constrain it. `None` when none has reported — the
    /// caller then leaves the current size untouched (nothing to fit to).
    pub fn wanted_size(&self) -> Option<(u16, u16)> {
        let mut acc: Option<(u16, u16)> = None;
        for c in &self.ws {
            if let Some((wc, wr)) = c.want {
                acc = Some(match acc {
                    Some((ac, ar)) => (ac.min(wc), ar.min(wr)),
                    None => (wc, wr),
                });
            }
        }
        acc
    }

    /// Accept, read and apply everything the browsers sent this pass.
    ///
    /// Order matters and matches what the standalone loop always did: drain
    /// input from every existing client FIRST (which also detects clients that
    /// closed, e.g. a browser reload), then accept new connections, then serve
    /// the requests that completed, then apply the whole input batch in order.
    /// So a reload's disconnect and its reconnect land in the same pass.
    pub fn poll(&mut self, editor: &mut Editor, cols: &mut u16, rows: &mut u16) -> BridgePoll {
        let mut out = BridgePoll::default();

        // 1) Drain input from every client. `resize` messages update only the
        //    sending client's own wanted grid size (the effective size is the
        //    min across clients, refit by the host) and are NOT applied to the
        //    editor directly — every other message joins one shared input batch
        //    applied to the single editor. Read/parse errors drop just that
        //    client; the editor and the loop live on, and its reconnect gets a
        //    fresh hello. A dropped client may have been the size constraint,
        //    so flag a refit.
        let mut inputs: Vec<Value> = Vec::new();
        let mut i = 0;
        while i < self.ws.len() {
            match self.ws[i].drain_messages() {
                Ok(msgs) => {
                    for m in &msgs {
                        let v: Value = serde_json::from_str(m).unwrap_or_else(|_| json!({}));
                        let kind = v.get("type").and_then(|t| t.as_str());
                        if kind == Some("resize") {
                            if self.ws[i].note_resize(&v) {
                                out.resize_dirty = true;
                            }
                        } else {
                            inputs.push(v);
                        }
                    }
                    i += 1;
                }
                Err(e) => {
                    eprintln!("[webui] ws client disconnected: {e}");
                    self.ws.remove(i);
                    out.resize_dirty = true;
                }
            }
        }

        // 2) Accept new connections into the pending pool (nonblocking).
        loop {
            match self.listener.accept().map(|(stream, _)| stream) {
                Ok(stream) => {
                    // The pump relies on nonblocking reads; a socket we can't
                    // configure is dropped rather than risked stalling the
                    // editor loop. `tune` is folded in — it never fails on a
                    // live socket, and a dead one belongs on the floor anyway.
                    if stream.set_nonblocking(true).is_err() || stream.set_nodelay(true).is_err() {
                        continue;
                    }
                    self.pending.push(PendingConn {
                        stream,
                        buf: Vec::new(),
                        since: Instant::now(),
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("accept error: {e}");
                    break;
                }
            }
        }

        // 3) Pump pending connections; serve the complete ones. A `/ws` upgrade
        //    JOINS the client set (or gets 403); anything else runs through the
        //    HTTP routes, blocking only for its short localhost response write.
        //    An HTTP route that mutated the editor (input routes, /step,
        //    /reset) counts as input so connected clients get the resulting
        //    diff pushed this pass.
        let mut i = 0;
        while i < self.pending.len() {
            match pump_pending(&mut self.pending[i]) {
                Pump::NeedMore if self.pending[i].since.elapsed() <= HTTP_READ_DEADLINE => i += 1,
                Pump::NeedMore | Pump::Closed => {
                    self.pending.remove(i);
                }
                Pump::Ready(req) => {
                    let conn = self.pending.remove(i);
                    match serve_request(
                        conn.stream,
                        &req,
                        editor,
                        cols,
                        rows,
                        &self.files,
                        &mut self.source,
                        &self.bind_host,
                        self.allow_reset,
                    ) {
                        Ok(Served::WsClient(session)) => {
                            let mut session = *session;
                            // A new client mirrors the same editor. Seed its
                            // wanted size from the current effective grid so it
                            // doesn't momentarily shrink everyone to the default
                            // before its own `resize` lands; its real viewport
                            // arrives in the hello handler's resize a tick later.
                            session.want = Some((*cols, *rows));
                            self.ws.push(session);
                        }
                        Ok(Served::Http { mutated }) => out.mutated |= mutated,
                        Err(e) => eprintln!("conn error: {e}"),
                    }
                }
            }
        }

        // 4) Apply the whole (non-resize) input batch in order via the same
        //    dispatch the HTTP routes use — but do NOT render per message.
        //    Input from several mirrored clients interleaves here; for one user
        //    across tabs/devices that's the point, and the editor applies each
        //    message atomically regardless of origin.
        for v in &inputs {
            let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if apply_message(editor, kind, v, cols, rows) {
                out.mutated = true;
            } else {
                eprintln!("[webui] ignoring unknown ws message type {kind:?}");
            }
        }

        out
    }

    /// Build the scene ONCE and push each client its own region diff.
    ///
    /// Does NOT tick — the host owns that (the standalone loop ticks just
    /// before calling this; the daemon drains async work in its own loop).
    /// Each client's diff is against ITS OWN last-sent cache (they connected at
    /// different times), so one shared scene still yields correct per-client
    /// frames. A client whose push fails is dropped.
    pub fn push_scene(&mut self, editor: &mut Editor, cols: u16, rows: u16) {
        if self.ws.is_empty() {
            return;
        }
        let scene = build_scene(editor, &mut self.source, cols, rows);
        let mut i = 0;
        while i < self.ws.len() {
            if let Err(e) = self.ws[i].push_diff(&scene) {
                eprintln!("[webui] ws push failed, dropping client: {e}");
                self.ws.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Heartbeat + drain buffered outbound bytes for EACH client — a periodic
    /// ping (and the liveness check that reaps a silent half-open peer, so a
    /// dead small window can't pin everyone's grid to its size), leftover frame
    /// bytes for a slow-but-alive peer, and pongs queued while draining input.
    /// `flush` is nonblocking; a peer whose backlog blows past the cap is
    /// dropped here rather than allowed to stall the loop.
    ///
    /// Returns true when the client set shrank — reaping a client can free the
    /// size constraint, so the host should refit.
    pub fn maintain(&mut self) -> bool {
        let before = self.ws.len();
        let mut i = 0;
        while i < self.ws.len() {
            if let Err(e) = self.ws[i].heartbeat().and_then(|()| self.ws[i].flush()) {
                eprintln!("[webui] ws dropped: {e}");
                self.ws.remove(i);
            } else {
                i += 1;
            }
        }
        self.ws.len() != before
    }
}

/// The bridge a session hosts: browsers over TCP (`fresh --web`), or File
/// Commander's native editor over the session's Unix socket.
pub enum HostedBridge {
    Web(WebBridge),
    #[cfg(unix)]
    Scene(SceneBridge),
}

impl HostedBridge {
    /// The Unix socket File Commander's app connects to, for a scene bridge.
    pub fn socket_path(&self) -> Option<&Path> {
        match self {
            HostedBridge::Web(_) => None,
            #[cfg(unix)]
            HostedBridge::Scene(b) => b.socket_path(),
        }
    }

    pub fn is_busy(&self) -> bool {
        match self {
            HostedBridge::Web(b) => b.is_busy(),
            #[cfg(unix)]
            HostedBridge::Scene(b) => b.is_busy(),
        }
    }

    pub fn wanted_size(&self) -> Option<(u16, u16)> {
        match self {
            HostedBridge::Web(b) => b.wanted_size(),
            #[cfg(unix)]
            HostedBridge::Scene(b) => b.wanted_size(),
        }
    }

    pub fn poll(&mut self, editor: &mut Editor, cols: &mut u16, rows: &mut u16) -> BridgePoll {
        match self {
            HostedBridge::Web(b) => b.poll(editor, cols, rows),
            #[cfg(unix)]
            HostedBridge::Scene(b) => b.poll(editor, cols, rows),
        }
    }

    pub fn push_scene(&mut self, editor: &mut Editor, cols: u16, rows: u16) -> bool {
        match self {
            HostedBridge::Web(b) => {
                b.push_scene(editor, cols, rows);
                false
            }
            #[cfg(unix)]
            HostedBridge::Scene(b) => b.push_scene(editor, cols, rows),
        }
    }

    pub fn maintain(&mut self) -> bool {
        match self {
            HostedBridge::Web(b) => b.maintain(),
            #[cfg(unix)]
            HostedBridge::Scene(b) => b.maintain(),
        }
    }
}

/// Run the standalone bridge: build an editor, serve it to browsers, forever.
///
/// This is the editor-owning host (the `webui_server` example, the Playwright
/// and scene-parity harnesses). `fresh --web` does NOT come here — it runs the
/// session daemon, which hosts the same [`WebBridge`] alongside the IPC socket
/// so TUI clients can attach to the very same editor.
pub fn run(addr: &str, files: &[PathBuf]) -> Result<()> {
    let (mut cols, mut rows) = DEFAULT_SIZE;
    let mut editor = build_editor(cols, rows, files, true)?;

    // Bind the in-process control socket so a `fresh` run inside an embedded
    // terminal can forward opens *and* drive the command channel
    // (`ListCommands` / `RunCommand`) back to this editor — same as the TUI
    // path (main.rs). Web mode previously skipped this, so an agent workspace
    // got a `FRESH_CMD_TOKEN` with no socket to reach. Best-effort: on failure
    // the editor still runs, nested launches just open inline.
    if let Err(e) = crate::server::local_control::start() {
        eprintln!("[webui] local control socket unavailable: {e}");
    }

    let mut bridge = WebBridge::bind(addr, &editor, files, true)?;
    eprintln!(
        "fresh web bridge on http://{addr}  (real render pipeline, no mocks; WS push on /ws)"
    );

    let mut next_tick = Instant::now();

    loop {
        // 0) Drain nested-forward command requests (file/dir opens, and the
        //    `fresh --cmd` command channel) before anything else, exactly like
        //    the TUI loop (main.rs). `pump` is a cheap no-op until `start()`
        //    has bound the socket and never blocks. Its result is folded into
        //    `had_input` below so the queued work is applied and pushed in this
        //    same iteration rather than waiting for the idle tick.
        let control_changed = crate::server::local_control::pump(&mut editor);

        // 1) Everything the browsers sent: input, joins, HTTP routes.
        let poll = bridge.poll(&mut editor, &mut cols, &mut rows);

        // 2) Refit the shared grid to the smallest viewport whenever a client
        //    resized, joined, or left. Resizing the editor counts as input so
        //    the new fit is pushed to everyone this pass.
        let mut resized_editor = false;
        if poll.resize_dirty {
            resized_editor = refit(&mut editor, &bridge, &mut cols, &mut rows);
        }

        // 3) One tick per loop pass at most: immediately after input, else on
        //    the tick deadline. The editor keeps ticking at the idle cadence
        //    even with no client connected (async LSP/plugin/file events must
        //    not stall); with no clients we skip the scene build entirely —
        //    the diff caches belong to the connected sessions, and a reconnect
        //    starts over with a fresh hello anyway.
        let now = Instant::now();
        // `control_changed` (a `fresh --cmd` request applied above) forces the
        // tick + scene push this iteration so command effects show immediately.
        let had_input = poll.mutated || control_changed || resized_editor;
        if had_input || now >= next_tick {
            let needs_render = tick_only(&mut editor);
            let active_hint = poll_active(&editor);
            // Push only when something can have changed: input was applied
            // (over WS or an HTTP route), the tick reported needs-render (the
            // TUI's own redraw signal), or time-driven UI is in flight
            // (animations / LSP spinner — `poll_active`, refreshed on the fast
            // cadence below). An unchanged scene diffs to nothing anyway;
            // skipping the build keeps connected-but-idle sessions near 0% CPU.
            if had_input || needs_render || active_hint {
                bridge.push_scene(&mut editor, cols, rows);
            }
            let interval = if active_hint { TICK_ACTIVE } else { TICK_IDLE };
            next_tick = Instant::now() + interval;
        }

        // 4) Heartbeat / flush; a reap here shrank the constraint set, so refit.
        if bridge.maintain() {
            refit(&mut editor, &bridge, &mut cols, &mut rows);
        }

        // 5) Pace the loop without a busy spin. Stay snappy while any WS client
        //    is connected (input latency) or an HTTP request is mid-assembly
        //    (its bytes usually land one iteration after the accept); idle
        //    slowly otherwise — CPU stays near zero either way.
        std::thread::sleep(if bridge.is_busy() {
            SLEEP_CONNECTED
        } else {
            SLEEP_IDLE
        });
    }
}

/// Fit the standalone editor to the smallest connected viewport. Returns
/// whether the size actually moved. (The daemon has its own refit — it must
/// also fit the attached terminals, not just the browsers.)
fn refit(editor: &mut Editor, bridge: &WebBridge, cols: &mut u16, rows: &mut u16) -> bool {
    match bridge.wanted_size() {
        Some((ec, er)) if (ec, er) != (*cols, *rows) => {
            *cols = ec;
            *rows = er;
            editor.resize(*cols, *rows);
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// HTTP request assembly (nonblocking) + routing
// ---------------------------------------------------------------------------

/// A parsed HTTP request (head + full body). Header names are lowercased.
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A connection whose request bytes are still arriving.
struct PendingConn {
    stream: TcpStream,
    buf: Vec<u8>,
    since: Instant,
}

enum Pump {
    NeedMore,
    Closed,
    Ready(HttpRequest),
}

/// Read whatever is available (nonblocking) and try to parse a complete
/// request out of the buffer.
fn pump_pending(conn: &mut PendingConn) -> Pump {
    let mut tmp = [0u8; 16384];
    loop {
        match conn.stream.read(&mut tmp) {
            Ok(0) => return Pump::Closed,
            Ok(n) => {
                conn.buf.extend_from_slice(&tmp[..n]);
                if conn.buf.len() > HTTP_REQUEST_CAP {
                    return Pump::Closed;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Pump::Closed,
        }
    }
    match try_parse_request(&conn.buf) {
        Some(req) => Pump::Ready(req),
        None => Pump::NeedMore,
    }
}

/// Parse one HTTP request if `buf` holds the complete head and body.
fn try_parse_request(buf: &[u8]) -> Option<HttpRequest> {
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut it = lines.next()?.split_whitespace();
    let method = it.next()?.to_string();
    let path = it.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, val)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let val = val.trim().to_string();
            if name == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((name, val));
        }
    }
    // A declared body larger than the request cap is a runaway (or hostile)
    // client — reject it before any arithmetic. This guard is also what keeps
    // the `checked_add` below from being the only thing between us and a panic:
    // a `Content-Length: 18446744073709551615` header would otherwise make
    // `head_end + content_length` wrap (release) or overflow-panic (debug), and
    // the wrapped value slipped past the length check and sliced an inverted
    // range — one unauthenticated request that killed the single editor thread.
    if content_length > HTTP_REQUEST_CAP {
        return None;
    }
    let end = head_end.checked_add(content_length)?;
    if buf.len() < end {
        return None; // body still arriving (or never will — dropped on deadline)
    }
    let body = buf[head_end..end].to_vec();
    Some(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Outcome of serving one complete request.
enum Served {
    /// The request was a successful `/ws` upgrade — this is the new client.
    /// Boxed: a session is large, and every other request answers `Http`.
    WsClient(Box<WsSession>),
    /// A plain HTTP exchange; `mutated` = the route may have changed editor
    /// state (input routes, /step, /reset), so a connected WS client should
    /// get a diff pushed without waiting for the tick deadline.
    Http { mutated: bool },
}

/// Serve one complete request: a WS upgrade joins the client set; everything else
/// goes through the HTTP routes with `Connection: close`. State-mutating POSTs
/// are gated by the same-origin/Host guard so a foreign page can't drive the
/// editor over HTTP (see below).
#[allow(clippy::too_many_arguments)]
fn serve_request(
    mut stream: TcpStream,
    req: &HttpRequest,
    editor: &mut Editor,
    cols: &mut u16,
    rows: &mut u16,
    files: &[PathBuf],
    source: &mut SceneSource,
    bind_host: &str,
    allow_reset: bool,
) -> Result<Served> {
    // Serve the response blocking for simplicity — but bound the write. `GET /`
    // returns the whole embedded page (~200 KB), so a client that stops reading
    // (or advertises a tiny/zero window) would otherwise wedge `write_all` on
    // this one editor thread forever. A write deadline drops such a peer instead
    // (the WS handshake's own small write is well under it, and `/ws` then
    // switches back to nonblocking). The read side is already bounded by the
    // pending-pool `HTTP_READ_DEADLINE`.
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(HTTP_READ_DEADLINE))?;
    let wants_ws = req.method == "GET"
        && req.path == "/ws"
        && req
            .header("upgrade")
            .is_some_and(|u| u.to_ascii_lowercase().contains("websocket"));
    if wants_ws {
        return match upgrade_ws(stream, req, editor, *cols, *rows, source, bind_host)? {
            Some(session) => Ok(Served::WsClient(Box::new(session))),
            None => Ok(Served::Http { mutated: false }),
        };
    }
    // CSRF / DNS-rebinding guard for state-mutating requests. `upgrade_ws` checks
    // the `/ws` upgrade; the POST routes mutate the editor too, so a cross-origin
    // browser page (which sends its own `Origin`) must be rejected here as well —
    // otherwise the same-origin guard is trivially bypassed by choosing HTTP over
    // WS. It fires only when an `Origin` is present and mismatched/untrusted, so
    // non-browser callers (curl, the Playwright request API, the parity harness)
    // send no `Origin` and pass — the routes stay scriptable. GET stays open: the
    // same-origin policy already stops a foreign page from *reading* a response.
    if req.method == "POST" {
        if let Some(origin) = req.header("origin") {
            if !origin_host_matches(origin, req.header("host"), bind_host) {
                respond(
                    &mut stream,
                    "403 Forbidden",
                    "text/plain",
                    b"origin not allowed",
                )?;
                return Ok(Served::Http { mutated: false });
            }
        }
    }
    let mutated = handle_http(
        &mut stream,
        req,
        editor,
        cols,
        rows,
        files,
        source,
        allow_reset,
    )?;
    Ok(Served::Http { mutated })
}

/// The HTTP routes (full-scene responses). The mutating `POST` routes are
/// reachable only after `serve_request`'s same-origin guard, so a cross-origin
/// page can't drive them; non-browser callers (curl, the Playwright request
/// API, the parity harness `/step` `/reset`) send no `Origin` and pass. Returns
/// whether the route may have mutated editor state.
#[allow(clippy::too_many_arguments)]
fn handle_http(
    stream: &mut TcpStream,
    req: &HttpRequest,
    editor: &mut Editor,
    cols: &mut u16,
    rows: &mut u16,
    files: &[PathBuf],
    source: &mut SceneSource,
    allow_reset: bool,
) -> Result<bool> {
    let body_json = || serde_json::from_slice::<Value>(&req.body).unwrap_or_else(|_| json!({}));
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            respond(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes(),
            )?;
            Ok(false)
        }
        ("GET", "/favicon.ico") => {
            respond(stream, "204 No Content", "image/x-icon", b"")?;
            Ok(false)
        }
        ("GET", "/state") => {
            let s = tick_scene(editor, source, *cols, *rows).to_string();
            respond(stream, "200 OK", "application/json", s.as_bytes())?;
            Ok(false)
        }
        // Input routes: the route name IS the message kind, and `apply_message`
        // is the same dispatch the WebSocket transport uses — the two transports
        // cannot drift. Each returns the full post-tick scene, exactly as
        // before; a connected WS client gets the mutation pushed as a diff in
        // the same loop pass (the `true` below counts as input).
        (
            "POST",
            p @ ("/key" | "/paste" | "/mouse" | "/action" | "/widget" | "/settings" | "/kbedit"
            | "/resize"),
        ) => {
            apply_message(editor, &p[1..], &body_json(), cols, rows);
            let s = tick_scene(editor, source, *cols, *rows).to_string();
            respond(stream, "200 OK", "application/json", s.as_bytes())?;
            Ok(true)
        }
        // Parity-harness routes: apply one scenario step, and reset to a fresh
        // editor so each scenario runs in isolation (mirrors the Rust runner,
        // which builds a fresh editor per scenario).
        ("POST", "/step") => {
            let v = body_json();
            apply_step(editor, &v);
            let s = scene_json(editor, *cols, *rows).to_string();
            respond(stream, "200 OK", "application/json", s.as_bytes())?;
            Ok(true)
        }
        // Refused when the editor isn't ours to throw away: under the session
        // daemon it carries the recovery session, the restored workspace, and
        // any attached TUI clients, none of which survive a silent rebuild.
        ("POST", "/reset") if !allow_reset => {
            respond(
                stream,
                "409 Conflict",
                "text/plain",
                b"/reset is unavailable in a shared session",
            )?;
            Ok(false)
        }
        ("POST", "/reset") => {
            (*cols, *rows) = DEFAULT_SIZE;
            // No `ready`: a reset hands the harness a known editor, and the
            // hook's effects would arrive asynchronously after it returned.
            match build_editor(*cols, *rows, files, false) {
                Ok(e) => *editor = e,
                Err(err) => eprintln!("reset failed: {err}"),
            }
            let s = scene_json(editor, *cols, *rows).to_string();
            respond(stream, "200 OK", "application/json", s.as_bytes())?;
            Ok(true)
        }
        _ => {
            respond(stream, "404 Not Found", "text/plain", b"not found")?;
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Shared input dispatch (HTTP routes + WebSocket messages)
// ---------------------------------------------------------------------------

/// Apply one input message by kind — the single dispatch behind both the HTTP
/// POST routes (kind = route name) and the WS `{"type": kind, ...}` messages.
/// Returns false for unknown kinds. Does NOT render: callers decide when a
/// scene is built (per HTTP request, or once per WS input batch).
/// A key from a front end: the browser's name for it (`key`), the physical
/// key (`code`, read for the digit row), and the modifiers held.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KeyInput<'a> {
    pub(crate) key: &'a str,
    pub(crate) code: Option<&'a str>,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) meta: bool,
    pub(crate) shift: bool,
}

impl<'a> KeyInput<'a> {
    fn from_json(v: &'a Value) -> Self {
        let flag = |k: &str| v.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        Self {
            key: v.get("key").and_then(|k| k.as_str()).unwrap_or(""),
            code: v.get("code").and_then(|c| c.as_str()),
            ctrl: flag("ctrl"),
            alt: flag("alt"),
            meta: flag("meta"),
            shift: flag("shift"),
        }
    }
}

/// A mouse event from a front end, on a cell, repeated `n` times (wheel
/// notches), with the click count the front end counted (`count`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct MouseInput {
    pub(crate) kind: MouseEventKind,
    pub(crate) col: u16,
    pub(crate) row: u16,
    pub(crate) n: u16,
    pub(crate) count: u64,
    pub(crate) modifiers: KeyModifiers,
}

impl MouseInput {
    /// The event a JSON `mouse` message names, or `None` for an unknown kind.
    fn from_json(v: &Value) -> Option<Self> {
        let button = match v.get("button").and_then(|b| b.as_str()) {
            Some("right") => MouseButton::Right,
            Some("middle") => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        let kind = match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "down" => MouseEventKind::Down(button),
            "up" => MouseEventKind::Up(button),
            "drag" => MouseEventKind::Drag(button),
            "moved" => MouseEventKind::Moved,
            "scrollup" => MouseEventKind::ScrollUp,
            "scrolldown" => MouseEventKind::ScrollDown,
            "scrollleft" => MouseEventKind::ScrollLeft,
            "scrollright" => MouseEventKind::ScrollRight,
            _ => return None,
        };
        let flag = |k: &str| v.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        let mut modifiers = KeyModifiers::empty();
        if flag("ctrl") {
            modifiers |= KeyModifiers::CONTROL;
        }
        if flag("alt") {
            modifiers |= KeyModifiers::ALT;
        }
        if flag("shift") {
            modifiers |= KeyModifiers::SHIFT;
        }
        Some(Self {
            kind,
            col: v.get("col").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
            row: v.get("row").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
            n: v.get("n").and_then(|x| x.as_u64()).unwrap_or(1).clamp(1, 10) as u16,
            count: v.get("count").and_then(|x| x.as_u64()).unwrap_or(1),
            modifiers,
        })
    }
}

/// A click in the settings modal: what was hit (`kind`, the names of
/// `SettingsHit`'s variants and a few more), its indices, whether it was a
/// double click, and for an entry dialog's button, which one.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SettingsClick<'a> {
    pub(crate) kind: &'a str,
    pub(crate) a: usize,
    pub(crate) b: usize,
    pub(crate) double: bool,
    pub(crate) button: &'a str,
}

impl<'a> SettingsClick<'a> {
    fn from_json(v: &'a Value) -> Self {
        Self {
            kind: v.get("kind").and_then(|k| k.as_str()).unwrap_or(""),
            a: v.get("a").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
            b: v.get("b").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
            double: v.get("double").and_then(|x| x.as_bool()).unwrap_or(false),
            button: v.get("button").and_then(|x| x.as_str()).unwrap_or("cancel"),
        }
    }
}

/// Action arguments from a JSON message's `args` object.
fn action_args(v: &Value) -> HashMap<String, Value> {
    v.get("args")
        .and_then(|a| a.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn apply_message(
    editor: &mut Editor,
    kind: &str,
    v: &Value,
    cols: &mut u16,
    rows: &mut u16,
) -> bool {
    match kind {
        "key" => apply_key(editor, KeyInput::from_json(v)),
        "mouse" => {
            if let Some(input) = MouseInput::from_json(v) {
                apply_mouse(editor, input);
            }
        }
        "action" => {
            if let Some(name) = v.get("action").and_then(|a| a.as_str()) {
                apply_action(editor, name, &action_args(v));
            }
        }
        "paste" => {
            if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                apply_paste(editor, text);
            }
        }
        "widget" => apply_widget(
            editor,
            v.get("surface").and_then(|s| s.as_str()).unwrap_or(""),
            v.get("key").and_then(|k| k.as_str()),
        ),
        "settings" => apply_settings(editor, SettingsClick::from_json(v)),
        "kbedit" => apply_kbedit(editor, v.get("a").and_then(|x| x.as_u64())),
        "resize" => apply_resize(
            editor,
            v.get("cols").and_then(|x| x.as_u64()),
            v.get("rows").and_then(|x| x.as_u64()),
            cols,
            rows,
        ),
        _ => return false,
    }
    true
}

/// Run a named editor action (with optional args) through the real dispatch.
fn apply_action(editor: &mut Editor, name: &str, args: &HashMap<String, Value>) {
    if let Some(act) = crate::input::keybindings::Action::from_str(name, args) {
        if let Err(e) = editor.handle_action(act) {
            eprintln!("[webui] action error: {e}");
        }
    }
}

/// Inbound OS clipboard (docs/internal/web-ui.md §3.5/§4). The frontend's
/// document `paste` listener sends the clipboard text in ONE message — long
/// pastes never loop through per-char key events. Delivery reuses the exact
/// path a terminal bracketed paste takes (`Ev::Paste` in app/lifecycle.rs): a
/// focused floating panel / dock text field first, then the buffer / prompt /
/// terminal paste in `paste_text`.
fn apply_paste(editor: &mut Editor, text: &str) {
    if !editor.paste_bracketed_into_focused_panel(text) {
        editor.paste_text(text.to_string());
    }
}

/// Native plugin-widget interaction. For the overlay prompt toolbar, a
/// Toggle/Button click forwards the widget `key`; the editor flips the toggle
/// in-spec and fires the plugin's `widget_event` — the exact path a TUI
/// toolbar click takes.
///
/// **The `"panel"` surface is gone with the web's plugin panels.** It resolved
/// a click against the recorded hit list the scene shipped — by identity, with
/// the index as a tiebreaker — and placed a caret from a byte the browser had
/// measured. Both read `WidgetPanelState::hits`, which existed for a described
/// panel only to be sent here. A request naming it now falls through the
/// wildcard and does nothing, which is what it should do while there is no
/// panel on the page to have clicked. See
/// `docs/internal/retained-mode-ui.md` "The web".
fn apply_widget(editor: &mut Editor, surface: &str, key: Option<&str>) {
    if surface == "toolbar" {
        if let Some(key) = key {
            editor.toggle_overlay_toolbar_widget(key);
        }
    }
}

/// Native Settings interaction: the frontend sends the `SettingsHit` it
/// rendered (kind + indices); we run the SAME dispatch a TUI cell click would
/// (`dispatch_settings_hit`). Entry (add/edit) sub-dialog interactions take a
/// separate semantic path — the dialog is its own stacked state, not a
/// main-panel item.
fn apply_settings(editor: &mut Editor, click: SettingsClick<'_>) {
    let (a, bb, dbl, kind) = (click.a, click.b, click.double, click.kind);
    use crate::view::settings::SettingsHit as H;
    if kind == "entryItem" {
        editor.entry_dialog_select_item(a);
        return;
    }
    if kind == "entryButton" {
        editor.entry_dialog_activate_button(click.button);
        return;
    }
    // The category tree's chevron and section rows: their `SettingsHit`s were
    // the painter's rectangles and are gone with them, so these reach the
    // same three bodies the TUI's nodes do (`Editor::settings_*`) rather than
    // going round through `dispatch_settings_hit`.
    if kind == "categoryDisclosure" {
        editor.settings_toggle_category(a);
        return;
    }
    if kind == "categorySection" {
        editor.settings_jump_to_section(a, bb);
        return;
    }
    let hit = match kind {
        "category" => Some(H::Category(a)),
        "item" => Some(H::Item(a)),
        "controlToggle" => Some(H::ControlToggle(a)),
        "controlDropdown" => Some(H::ControlDropdown(a)),
        "controlDropdownOption" => Some(H::ControlDropdownOption(a, bb)),
        "controlDecrement" => Some(H::ControlDecrement(a)),
        "controlIncrement" => Some(H::ControlIncrement(a)),
        "controlText" => Some(H::ControlText(a)),
        "controlMapRow" => Some(H::ControlMapRow(a, bb)),
        "controlMapAddNew" => Some(H::ControlMapAddNew(a)),
        "controlTextListRow" => Some(H::ControlTextListRow(a, bb)),
        "controlTextListRemove" => Some(H::ControlTextListRemove(a, bb)),
        "controlDualListAvailable" => Some(H::ControlDualListAvailable(a, bb)),
        "controlDualListIncluded" => Some(H::ControlDualListIncluded(a, bb)),
        "controlDualListAdd" => Some(H::ControlDualListAdd(a)),
        "controlDualListRemove" => Some(H::ControlDualListRemove(a)),
        "controlDualListMoveUp" => Some(H::ControlDualListMoveUp(a)),
        "controlDualListMoveDown" => Some(H::ControlDualListMoveDown(a)),
        "controlInherit" => Some(H::ControlInherit(a)),
        "searchResult" => Some(H::SearchResult(a)),
        "save" => Some(H::SaveButton),
        "cancel" => Some(H::CancelButton),
        "reset" => Some(H::ResetButton),
        "layer" => Some(H::LayerButton),
        "edit" => Some(H::EditButton),
        "clearCategory" => Some(H::ClearCategoryButton),
        _ => None,
    };
    if let Some(hit) = hit {
        editor.dispatch_settings_hit(hit, dbl);
    }
}

/// Native keybinding-editor click: select the display row the frontend
/// rendered (same as a TUI row click). Other interactions are keyboard.
fn apply_kbedit(editor: &mut Editor, row: Option<u64>) {
    if let Some(row) = row {
        editor.kbedit_select_display_row(row as usize);
    }
}

/// `{cols, rows}` → the real `Editor::resize`, updating the bridge's size.
/// A client's wanted grid size after a resize to `cols` × `rows`, clamped as
/// the HTTP `/resize` route clamps; `None` when it didn't change. A resize
/// always sends both, but a partial one keeps the other from `current`.
fn apply_resize_want(current: Option<(u16, u16)>, cols: Option<u64>, rows: Option<u64>) -> Option<(u16, u16)> {
    let cur = current.unwrap_or(DEFAULT_SIZE);
    let next = (
        cols.map_or(cur.0, |c| (c as u16).clamp(20, 400)),
        rows.map_or(cur.1, |r| (r as u16).clamp(8, 200)),
    );
    (current != Some(next)).then_some(next)
}

fn apply_resize(
    editor: &mut Editor,
    new_cols: Option<u64>,
    new_rows: Option<u64>,
    cols: &mut u16,
    rows: &mut u16,
) {
    if let Some(c) = new_cols {
        *cols = (c as u16).clamp(20, 400);
    }
    if let Some(r) = new_rows {
        *rows = (r as u16).clamp(8, 200);
    }
    editor.resize(*cols, *rows);
}

// ---------------------------------------------------------------------------
// WebSocket: hand-rolled RFC 6455 (handshake, frame codec, session + diffs)
// ---------------------------------------------------------------------------

/// Sec-WebSocket-Accept = base64(SHA1(key + RFC 6455 GUID)).
fn ws_accept_key(key: &str) -> String {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64.encode(h.finalize())
}

/// The bare host of a `host[:port]` or `scheme://host[:port]/path` string,
/// unwrapping an IPv6 bracket (`[::1]:8080` → `::1`).
fn host_only(s: &str) -> &str {
    let rest = s.split("://").nth(1).unwrap_or(s);
    let hostport = rest.split('/').next().unwrap_or(rest);
    if let Some(h) = hostport.strip_prefix('[') {
        h.split(']').next().unwrap_or(h)
    } else {
        hostport
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(hostport)
    }
}

/// A `Host` value we'll accept for a WebSocket upgrade. Matching `Origin`
/// against `Host` alone is not enough: DNS rebinding makes a name the attacker
/// controls (`attacker.com`, its record flipped to `127.0.0.1` after first
/// load) resolve to us, so the browser sends `Host: attacker.com` *and*
/// `Origin: http://attacker.com` — the two agree and the same-origin check
/// passes. Requiring the host to be a literal IP, `localhost`, or the exact
/// address we bound to closes that hole: a rebound *name* is refused, while
/// every legitimate localhost/LAN reach (an IP, or `localhost`, including the
/// wildcard-bind case) still passes. A deployment that needs a real hostname is
/// the §3.7 story (real auth), not this dev-bridge guard.
fn host_is_allowed(host: &str, bind_host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.eq_ignore_ascii_case(bind_host)
}

/// True when the WebSocket upgrade is same-origin AND reached under a Host we
/// trust: the `Origin` header's host matches the host the browser connected to
/// (the `Host` header), and that host is a loopback/LAN literal or our bind
/// address (see `host_is_allowed` — the DNS-rebinding guard). A page on another
/// origin sends its own `Origin` while connecting to our `Host`, so the two
/// disagree and it's rejected.
///
/// Comparing against `Host` (not the bind address) is what makes wildcard
/// binds work: with `--web 0.0.0.0:8137` the browser reaches the server as
/// `127.0.0.1:8137` (or a LAN IP), and its `Origin` host matches that `Host`,
/// even though neither equals the literal bind host `0.0.0.0`. When no `Host`
/// header is present we fall back to the bind host. Non-browser tools send no
/// `Origin` and never reach this check.
fn origin_host_matches(origin: &str, host_header: Option<&str>, bind_host: &str) -> bool {
    let bind = host_only(bind_host);
    let target = host_only(host_header.unwrap_or(bind_host));
    host_is_allowed(target, bind) && host_only(origin).eq_ignore_ascii_case(target)
}

/// Handle a `/ws` upgrade request: enforce the Origin policy (403), then
/// handshake (101), switch the socket to nonblocking, and send the full-scene
/// hello. The returned session is added to the mirrored client set by the
/// CALLER (`run`); the upgrade itself never rejects a well-formed same-origin
/// request — every client joins (shared-view mirroring, see the module docs).
#[allow(clippy::too_many_arguments)]
fn upgrade_ws(
    mut stream: TcpStream,
    req: &HttpRequest,
    editor: &mut Editor,
    cols: u16,
    rows: u16,
    source: &mut SceneSource,
    bind_host: &str,
) -> Result<Option<WsSession>> {
    if let Some(origin) = req.header("origin") {
        if !origin_host_matches(origin, req.header("host"), bind_host) {
            respond(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                b"origin not allowed",
            )?;
            return Ok(None);
        }
    }
    let Some(key) = req.header("sec-websocket-key") else {
        respond(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"missing Sec-WebSocket-Key",
        )?;
        return Ok(None);
    };
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        ws_accept_key(key)
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()?;
    stream.set_nonblocking(true)?;
    let mut session = WsSession::new(stream);
    tick_only(editor);
    let scene = build_scene(editor, source, cols, rows);
    session.send_hello(&scene)?;
    Ok(Some(session))
}

/// One parsed WebSocket frame.
struct WsFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Cap on a single inbound frame / fragmented message (a paste is the biggest
/// legitimate payload).
const WS_PAYLOAD_CAP: usize = 16 << 20;

/// Parse one frame from the front of `buf`, returning it and the bytes
/// consumed. `None` = incomplete (wait for more bytes); `Err` = malformed.
fn ws_parse_frame(buf: &[u8]) -> Result<Option<(WsFrame, usize)>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0F;
    let masked = buf[1] & 0x80 != 0;
    let (mut len, mut off) = ((buf[1] & 0x7F) as u64, 2usize);
    if len == 126 {
        if buf.len() < 4 {
            return Ok(None);
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as u64;
        off = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return Ok(None);
        }
        len = u64::from_be_bytes(buf[2..10].try_into().unwrap());
        off = 10;
    }
    if len > WS_PAYLOAD_CAP as u64 {
        anyhow::bail!("ws frame too large ({len} bytes)");
    }
    let mask = if masked {
        if buf.len() < off + 4 {
            return Ok(None);
        }
        let k = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
        off += 4;
        Some(k)
    } else {
        None
    };
    let len = len as usize;
    if buf.len() < off + len {
        return Ok(None);
    }
    let mut payload = buf[off..off + len].to_vec();
    if let Some(k) = mask {
        // Client→server frames are always masked; unmask in place.
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= k[i % 4];
        }
    }
    Ok(Some((
        WsFrame {
            fin,
            opcode,
            payload,
        },
        off + len,
    )))
}

/// Encode one server→client frame (unmasked, 7/16/64-bit lengths — scenes are
/// ~41 KB, so 16-bit is the common case).
fn ws_encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 10);
    f.push(0x80 | (opcode & 0x0F));
    match payload.len() {
        n if n < 126 => f.push(n as u8),
        n if n <= 0xFFFF => {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(payload);
    f
}

/// Cap on a session's buffered-but-unsent outbound bytes. A peer that stops
/// draining its socket makes `outbuf` grow (frames are queued, never blocked
/// on); past this the client is behind beyond recovery and is dropped rather
/// than buffered without bound. Sized well above a single max frame so a brief
/// stall is absorbed, not a real one.
const WS_OUTBUF_CAP: usize = 8 << 20; // 8 MiB

/// THE WebSocket client, plus the diff cache backing the region-diff protocol
/// (see the module docs). The cache holds an 8-byte fingerprint of every unit
/// last sent — top-level scalars, each `regions.<key>` except panes, and each
/// pane by index (see `value_fingerprint`) — so change detection costs one
/// streaming hash per unit and no persistent per-frame allocation, rather than
/// keeping a full serialized copy (panes are tens of KB each). It is written
/// ONLY by this session's hello/push path — HTTP routes build their own full
/// scenes independently and never touch it.
struct WsSession {
    stream: TcpStream,
    /// Inbound bytes not yet parsed into complete frames.
    inbuf: Vec<u8>,
    /// Encoded outbound frames the socket hasn't accepted yet. Writes are
    /// always nonblocking (`flush`); a slow peer's backlog lands here instead
    /// of stalling the single editor loop.
    outbuf: Vec<u8>,
    /// Accumulated payload of an in-flight fragmented message.
    frag: Vec<u8>,
    frag_text: bool,
    seq: u64,
    top: HashMap<&'static str, u64>,
    regions: HashMap<String, u64>,
    panes: Vec<u64>,
    /// Liveness clock: the last time ANY inbound bytes arrived (input, a pong,
    /// a ping — all count). Refreshed in `drain_messages`; read by `heartbeat`
    /// to reap a peer that has gone silent past `WS_LIVENESS_TIMEOUT`.
    last_recv: Instant,
    /// When we last sent a heartbeat ping — throttles them to `WS_PING_INTERVAL`.
    last_ping: Instant,
    /// This client's wanted grid size (cols, rows), from its `resize` messages.
    /// The bridge fits the editor to the element-wise MIN across all clients so
    /// the render fits every viewport (`effective_size`); `None` until the
    /// client first reports (seeded to the current effective size on join so it
    /// doesn't transiently shrink the grid before its real viewport arrives).
    want: Option<(u16, u16)>,
}

/// Fingerprint a JSON value for change detection by streaming its serialization
/// through a hasher — no `String` is allocated, and the cache stores 8 bytes
/// instead of a full serialized copy. Serialization is deterministic here
/// (scene objects are built key-by-key), so an unchanged unit hashes the same.
/// A 64-bit SipHash collision (a changed unit that hashes identically, so its
/// frame is skipped) is astronomically unlikely and would self-correct on the
/// next real change or reconnect — an acceptable trade for a pure render cache.
fn value_fingerprint(v: &Value) -> u64 {
    use std::hash::Hasher;
    struct HashWriter(std::collections::hash_map::DefaultHasher);
    impl std::io::Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hw = HashWriter(std::collections::hash_map::DefaultHasher::new());
    // Writing to a hasher never fails, and Value serialization is infallible —
    // `drop` the (always-Ok) result without a `let _` (denied for must-use).
    drop(serde_json::to_writer(&mut hw, v));
    hw.0.finish()
}

/// Top-level scene keys diffed as single units ("regions" is handled per key).
const TOP_KEYS: [&str; 5] = ["w", "h", "theme", "clipboard", "windowId"];

impl WsSession {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            inbuf: Vec::new(),
            outbuf: Vec::new(),
            frag: Vec::new(),
            frag_text: false,
            seq: 0,
            top: HashMap::new(),
            regions: HashMap::new(),
            panes: Vec::new(),
            last_recv: Instant::now(),
            last_ping: Instant::now(),
            want: None,
        }
    }

    /// Record this client's wanted grid size from a `resize` message, applying
    /// the same clamps as the HTTP `/resize` route (`apply_resize`). Returns
    /// true if the wanted size changed — the caller then recomputes the
    /// effective (min-across-clients) size and resizes the shared editor.
    fn note_resize(&mut self, v: &Value) -> bool {
        let cols = v.get("cols").and_then(|x| x.as_u64());
        let rows = v.get("rows").and_then(|x| x.as_u64());
        match apply_resize_want(self.want, cols, rows) {
            Some(next) => {
                self.want = Some(next);
                true
            }
            None => false,
        }
    }

    /// Heartbeat, called once per loop pass. Reap the peer if it has gone
    /// silent past the liveness deadline (a half-open connection never sends a
    /// clean close, so this is the only thing that frees its slot), and
    /// otherwise ping on the interval so a live-but-idle browser keeps
    /// auto-ponging and stays counted as alive. The ping is enqueued, not
    /// written — the caller's `flush` sends it, so this never blocks.
    fn heartbeat(&mut self) -> std::io::Result<()> {
        let now = Instant::now();
        if now.duration_since(self.last_recv) > WS_LIVENESS_TIMEOUT {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "ws peer silent past liveness deadline (half-open); reaping",
            ));
        }
        if now.duration_since(self.last_ping) >= WS_PING_INTERVAL {
            self.last_ping = now;
            self.enqueue(0x9, b""); // ping (empty payload)
        }
        Ok(())
    }

    /// `{"type":"hello","seq":0,"scene":<full scene>}` — sent once per
    /// connection; seeds the diff cache and restarts `seq`.
    fn send_hello(&mut self, scene: &Value) -> std::io::Result<()> {
        self.seq = 0;
        self.diff(scene); // seed the cache; the hello carries the full scene
        let msg = json!({ "type": "hello", "seq": 0, "scene": scene }).to_string();
        self.enqueue(0x1, msg.as_bytes());
        self.flush()
    }

    /// Diff `scene` against the last-sent one and push a
    /// `{"type":"frame","seq":N,"changed":{...}}` if anything changed.
    fn push_diff(&mut self, scene: &Value) -> std::io::Result<()> {
        let changed = self.diff(scene);
        if changed.is_empty() {
            return self.flush(); // still drain any backlog from earlier frames
        }
        self.seq += 1;
        let msg = json!({ "type": "frame", "seq": self.seq, "changed": Value::Object(changed) })
            .to_string();
        self.enqueue(0x1, msg.as_bytes());
        self.flush()
    }

    /// Compute the changed-paths map and update the cache. A changed value
    /// replaces the previous one wholesale (null is a legal value — regions
    /// are frequently null).
    fn diff(&mut self, scene: &Value) -> serde_json::Map<String, Value> {
        let mut changed = serde_json::Map::new();
        for k in TOP_KEYS {
            let v = scene.get(k).cloned().unwrap_or(Value::Null);
            let h = value_fingerprint(&v);
            if self.top.get(k) != Some(&h) {
                self.top.insert(k, h);
                changed.insert(k.to_string(), v);
            }
        }
        let empty = serde_json::Map::new();
        let regions = scene
            .get("regions")
            .and_then(|r| r.as_object())
            .unwrap_or(&empty);
        for (k, v) in regions {
            if k == "panes" {
                continue; // diffed one level deeper below
            }
            let h = value_fingerprint(v);
            if self.regions.get(k) != Some(&h) {
                self.regions.insert(k.clone(), h);
                changed.insert(format!("regions.{k}"), v.clone());
            }
        }
        // Panes carry the bulk of the bytes — diff per pane so typing resends
        // only the changed one, plus a `len` entry when the count changes.
        let panes: &[Value] = regions
            .get("panes")
            .and_then(|p| p.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let new_hashes: Vec<u64> = panes.iter().map(value_fingerprint).collect();
        for (i, (p, h)) in panes.iter().zip(&new_hashes).enumerate() {
            if self.panes.get(i) != Some(h) {
                changed.insert(format!("regions.panes.{i}"), p.clone());
            }
        }
        if new_hashes.len() != self.panes.len() {
            changed.insert("regions.panes.len".to_string(), json!(new_hashes.len()));
        }
        self.panes = new_hashes;
        changed
    }

    /// Queue one encoded frame for sending. Never touches the socket — `flush`
    /// does, nonblocking — so enqueuing can't stall the editor loop.
    fn enqueue(&mut self, opcode: u8, payload: &[u8]) {
        self.outbuf.extend_from_slice(&ws_encode(opcode, payload));
    }

    /// Write as much of `outbuf` as the kernel will take right now (nonblocking)
    /// and keep the rest for the next loop pass. This is the only place the
    /// socket is written, and it never blocks: a slow/stalled peer just leaves
    /// bytes queued. A backlog past `WS_OUTBUF_CAP` means the peer isn't
    /// draining at all — error out so the caller drops the client instead of
    /// buffering unboundedly.
    fn flush(&mut self) -> std::io::Result<()> {
        if self.outbuf.len() > WS_OUTBUF_CAP {
            return Err(std::io::Error::other(
                "ws outbound backlog exceeded cap; peer not draining",
            ));
        }
        let mut sent = 0;
        while sent < self.outbuf.len() {
            match self.stream.write(&self.outbuf[sent..]) {
                Ok(0) => return Err(std::io::Error::from(ErrorKind::WriteZero)),
                Ok(n) => sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if sent > 0 {
            self.outbuf.drain(..sent);
        }
        Ok(())
    }

    /// Drain ALL pending inbound frames (nonblocking; `WouldBlock` ends the
    /// drain) into complete text messages — the input batch. Control frames
    /// are handled here (ping→pong, pong ignored, close→echo + `Err` so the
    /// caller drops the client); continuation frames are accumulated
    /// defensively.
    fn drain_messages(&mut self) -> std::io::Result<Vec<String>> {
        let mut tmp = [0u8; 16384];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err(std::io::Error::from(ErrorKind::ConnectionAborted)),
                Ok(n) => {
                    // Any inbound bytes — input, a pong, a ping — prove the peer
                    // is alive; refresh the liveness clock `heartbeat` reads.
                    self.last_recv = Instant::now();
                    self.inbuf.extend_from_slice(&tmp[..n]);
                    if self.inbuf.len() > WS_PAYLOAD_CAP * 2 {
                        return Err(std::io::Error::from(ErrorKind::InvalidData));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut out = Vec::new();
        loop {
            let (frame, used) = match ws_parse_frame(&self.inbuf) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(e) => return Err(std::io::Error::new(ErrorKind::InvalidData, e.to_string())),
            };
            self.inbuf.drain(..used);
            match frame.opcode {
                0x1 | 0x2 => {
                    if frame.fin {
                        if frame.opcode == 0x1 {
                            out.push(String::from_utf8_lossy(&frame.payload).into_owned());
                        } // binary frames are not part of the protocol; ignore
                    } else {
                        self.frag = frame.payload;
                        self.frag_text = frame.opcode == 0x1;
                    }
                }
                0x0 => {
                    self.frag.extend_from_slice(&frame.payload);
                    if self.frag.len() > WS_PAYLOAD_CAP {
                        return Err(std::io::Error::from(ErrorKind::InvalidData));
                    }
                    if frame.fin {
                        if self.frag_text {
                            out.push(String::from_utf8_lossy(&self.frag).into_owned());
                        }
                        self.frag = Vec::new();
                    }
                }
                0x8 => {
                    // Close: echo it (best effort — the peer may already be
                    // gone, and we're dropping the client either way) and
                    // report the disconnect.
                    self.enqueue(0x8, &frame.payload);
                    drop(self.flush());
                    return Err(std::io::Error::new(
                        ErrorKind::ConnectionAborted,
                        "client sent close",
                    ));
                }
                0x9 => self.enqueue(0xA, &frame.payload), // ping → queue a pong
                0xA => {}                                 // pong: ignore
                _ => {}                                   // unknown opcode: ignore
            }
        }
        Ok(out)
    }
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> Result<()> {
    // No CORS header: the frontend is served from this same origin, so it needs
    // none, and `Access-Control-Allow-Origin: *` would let any site the user
    // visits read `/state` (live buffer contents) cross-origin. Same-origin
    // policy is the protection here.
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// Renders the editor for scenes into one terminal kept between frames.
///
/// Building a `Terminal` and a `TestBackend` per frame allocated and cleared
/// three grids of cells and copied a fourth. Here the terminal's buffer is
/// reset and rendered into, and read in place: nothing is drawn to a backend.
pub(crate) struct SceneRenderer {
    terminal: Terminal<SceneBackend>,
}

impl SceneRenderer {
    pub(crate) fn new() -> Self {
        let backend = SceneBackend {
            size: ratatui::layout::Size::new(1, 1),
        };
        match Terminal::new(backend) {
            Ok(terminal) => Self { terminal },
            Err(never) => match never {},
        }
    }

    /// Run the real render pipeline at `cols` × `rows`: the rendered cells and
    /// the hardware-cursor cell. The cells are what a terminal shown this frame
    /// from scratch would hold (`as_drawn_from_scratch`), and a hidden cursor
    /// reads as the origin, as the per-frame `TestBackend` reported both.
    fn render(&mut self, editor: &mut Editor, cols: u16, rows: u16) -> (&Buffer, Option<(u16, u16)>) {
        self.terminal.backend_mut().size = ratatui::layout::Size::new(cols, rows);
        if let Err(never) = self.terminal.autoresize() {
            match never {}
        }
        self.terminal.current_buffer_mut().reset();
        editor.render(&mut self.terminal.get_frame());
        let cursor = editor.rendered_cursor().unwrap_or((0, 0));
        let buf = self.terminal.current_buffer_mut();
        as_drawn_from_scratch(buf);
        (buf, Some(cursor))
    }
}

/// Make a rendered buffer what a terminal shown it from scratch would hold,
/// which is what the scene carried while each frame got a new terminal: the
/// frame's diff against a blank screen never writes a cell marked `skip`, nor
/// the cells a wide character covers (except those of an emoji with VS16,
/// which it clears explicitly), so those stayed blank.
fn as_drawn_from_scratch(buf: &mut Buffer) {
    use unicode_width::UnicodeWidthStr;
    let mut to_skip = 0usize;
    // Cells before this index were written as the trailing cells of a VS16 emoji.
    let mut written_until = 0usize;
    for i in 0..buf.content.len() {
        let cell = &buf.content[i];
        let width = cell.symbol().width();
        let skip = cell.skip;
        if !skip && to_skip == 0 && width > 1 && cell.symbol().contains('\u{FE0F}') {
            written_until = i + width;
        }
        let written = !skip && (to_skip == 0 || i < written_until);
        to_skip = width.saturating_sub(1);
        if !written {
            buf.content[i] = ratatui::buffer::Cell::EMPTY;
        }
    }
}

/// A backend with a size and nothing else: `SceneRenderer` reads the cells
/// from the terminal's buffer, so nothing is ever drawn.
struct SceneBackend {
    size: ratatui::layout::Size,
}

impl ratatui::backend::Backend for SceneBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, _content: I) -> std::result::Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        Ok(())
    }

    fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::result::Result<Position, Self::Error> {
        Ok(Position::ORIGIN)
    }

    fn set_cursor_position<P: Into<Position>>(
        &mut self,
        _position: P,
    ) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn clear(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn clear_region(
        &mut self,
        _clear_type: ratatui::backend::ClearType,
    ) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn size(&self) -> std::result::Result<ratatui::layout::Size, Self::Error> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> std::result::Result<ratatui::backend::WindowSize, Self::Error> {
        Ok(ratatui::backend::WindowSize {
            columns_rows: self.size,
            pixels: ratatui::layout::Size::default(),
        })
    }

    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }
}

/// What building a scene keeps between frames: the clipboard mirror's state
/// and the renderer.
pub(crate) struct SceneSource {
    clip: ClipboardSync,
    renderer: SceneRenderer,
}

impl SceneSource {
    fn new(editor: &Editor) -> Self {
        Self {
            clip: ClipboardSync::new(editor),
            renderer: SceneRenderer::new(),
        }
    }
}

fn rect_json(r: Rect) -> Value {
    json!({ "x": r.x, "y": r.y, "w": r.width, "h": r.height })
}

/// Slice the rendered cells inside `r` into rows of styled runs.
/// The marks on one pane's vertical bar, as `[{row, color}]`.
///
/// Read off the bar's own item in the frame's display list — the marks the
/// bar's leaf bucketed onto the track layout gave it — so the web scrollbar
/// shows exactly the cells the terminal painted. Theme names are resolved
/// to hex here: the frontend has no theme-key resolver.
fn scrollbar_markers(editor: &Editor, leaf: crate::model::event::LeafId) -> Vec<ScrollbarMarker> {
    let key = crate::view::shell::splits::vscroll_key(leaf);
    let Some(ui) = editor.shell_ui.as_ref() else {
        return Vec::new();
    };
    let theme = editor.theme.read().unwrap();
    ui.spec()
        .items
        .iter()
        .filter(|i| i.key.as_ref() == Some(&key))
        .find_map(|i| match &i.draw {
            fresh_ui::Draw::Scrollbar { marks, .. } => Some(marks.clone()),
            _ => None,
        })
        .map(|marks| {
            marks
                .iter()
                .filter(|m| !m.full)
                .filter_map(|m| {
                    let style =
                        crate::app::shell_host::shell_theme::resolve(m.theme.as_str(), &theme);
                    let ratatui::style::Color::Rgb(r, g, b) = style.fg? else {
                        return None;
                    };
                    Some(ScrollbarMarker {
                        row: m.at,
                        color: Rgb(r, g, b),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The whole frame for File Commander's app (`SceneFor::App`): the rendered
/// cells and a hash of each row, which the app's diff (`wire::ClientScene`)
/// keeps to send only the rows the app lacks.
struct GridFrame<'a> {
    buf: &'a Buffer,
    hashes: &'a [u64],
}

/// Hashes a frame's grid rows into buffers kept between frames.
#[derive(Default)]
pub(crate) struct GridHasher {
    hashes: Vec<u64>,
    row: RowBytes,
}

/// A row's cells as bytes, hashed in one pass: hashing each cell's fields one
/// by one costs a millisecond for a large grid. The hash is foldhash's
/// quality variant with a fixed seed, several times faster than SipHash over
/// a row's couple of kilobytes; a collision would keep a changed row from
/// being sent, so it is a full-quality 64-bit hash, not the fastest one.
#[derive(Default)]
struct RowBytes(Vec<u8>);

impl std::hash::Hasher for RowBytes {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn finish(&self) -> u64 {
        use std::hash::BuildHasher;
        let mut hasher = foldhash::quality::FixedState::with_seed(0).build_hasher();
        hasher.write(&self.0);
        hasher.finish()
    }
}

impl GridHasher {
    fn frame<'a>(&'a mut self, buf: &'a Buffer) -> GridFrame<'a> {
        use std::hash::{Hash, Hasher};
        let area = buf.area;
        self.hashes.clear();
        for y in area.y..area.y.saturating_add(area.height) {
            self.row.0.clear();
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buf.cell(Position::new(x, y)) {
                    cell.symbol().hash(&mut self.row);
                    cell.fg.hash(&mut self.row);
                    cell.bg.hash(&mut self.row);
                    cell.modifier.hash(&mut self.row);
                }
            }
            self.hashes.push(self.row.finish());
        }
        GridFrame {
            buf,
            hashes: &self.hashes,
        }
    }
}

/// Slice the rendered cells inside `r` into rows of styled runs.
fn cells_json(buf: &Buffer, r: Rect) -> Value {
    let mut rows = Vec::with_capacity(r.height as usize);
    for y in r.y..r.y.saturating_add(r.height) {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_fg: Option<String> = None;
        let mut cur_bg: Option<String> = None;
        let mut cur_mods = Modifier::empty();
        let flush = |runs: &mut Vec<Value>,
                     text: &mut String,
                     fg: &Option<String>,
                     bg: &Option<String>,
                     m: Modifier| {
            if !text.is_empty() {
                runs.push(json!({
                    "t": text,
                    "fg": fg, "bg": bg,
                    "b": m.contains(Modifier::BOLD),
                    "i": m.contains(Modifier::ITALIC),
                    "u": m.contains(Modifier::UNDERLINED),
                    "r": m.contains(Modifier::REVERSED),
                }));
                text.clear();
            }
        };
        for x in r.x..r.x.saturating_add(r.width) {
            let Some(cell) = buf.cell(Position::new(x, y)) else {
                continue;
            };
            let fg = color_css(cell.fg);
            let bg = color_css(cell.bg);
            let m = cell.modifier;
            if !cur_text.is_empty() && (fg != cur_fg || bg != cur_bg || m != cur_mods) {
                flush(&mut runs, &mut cur_text, &cur_fg, &cur_bg, cur_mods);
            }
            cur_fg = fg;
            cur_bg = bg;
            cur_mods = m;
            cur_text.push_str(cell.symbol());
        }
        flush(&mut runs, &mut cur_text, &cur_fg, &cur_bg, cur_mods);
        rows.push(Value::Array(runs));
    }
    Value::Array(rows)
}

/// Advance the editor one "tick" (drain async LSP/plugin/file messages, fire
/// timers, step animations) exactly as the TUI event loop does, without
/// building a scene. Returns the tick's needs-render signal — the same bool
/// the TUI uses to decide whether to redraw (true after an error too, so a
/// failed tick can't wedge the display).
fn tick_only(editor: &mut Editor) -> bool {
    match crate::app::editor_tick(editor, || Ok(())) {
        Ok(needs_render) => needs_render,
        Err(e) => {
            eprintln!("[webui] editor_tick error: {e}");
            true
        }
    }
}

/// The scene's `regions.poll.active` hint, computed straight from the editor:
/// true while something is animating / an LSP spinner is live / a timer is
/// pending. Drives the server's tick cadence (and is still exposed in the
/// scene for the frontend/tests).
fn poll_active(editor: &Editor) -> bool {
    editor.active_window().animations.is_active()
        || editor.active_window().has_active_lsp_progress()
        || editor.next_periodic_redraw_deadline().is_some()
}

/// Build the browser-facing scene (no tick): `scene_json` plus the
/// outbound-clipboard mirror (`ClipboardSync`). The parity harness
/// (`scene_value`) uses `scene_json` directly and carries no clipboard. The
/// clipboard `seq` bumps on actual text changes (hash compare), so it cannot
/// double-increment no matter which path — `GET /state` or WS push — builds
/// the scene.
fn build_scene(editor: &mut Editor, source: &mut SceneSource, cols: u16, rows: u16) -> Value {
    let mut scene = scene_json_with(editor, &mut source.renderer, cols, rows);
    source.clip.attach(editor, &mut scene);
    scene
}

/// Tick, then build the browser-facing scene. Called per HTTP request.
fn tick_scene(editor: &mut Editor, source: &mut SceneSource, cols: u16, rows: u16) -> Value {
    tick_only(editor);
    build_scene(editor, source, cols, rows)
}

/// Run one render with chrome kept out of the cells.
///
/// The browser draws chrome (menu bar, tabs, status bar, scrollbars, explorer)
/// as native HTML from the semantic model, so the pane cells must carry buffer
/// interiors only. `build_editor` sets the flag permanently for the standalone
/// bridge, but the daemon shares its editor with TUI clients that *do* want
/// chrome drawn into the cells — so scope it to the scene's render and put the
/// host's value back, instead of leaving a mode flag flipped under them.
fn with_chrome_suppressed<T>(editor: &mut Editor, f: impl FnOnce(&mut Editor) -> T) -> T {
    let prev = editor.suppress_chrome_cells;
    editor.suppress_chrome_cells = true;
    let out = f(editor);
    editor.suppress_chrome_cells = prev;
    out
}

fn scene_json(editor: &mut Editor, cols: u16, rows: u16) -> Value {
    scene_json_with(editor, &mut SceneRenderer::new(), cols, rows)
}

/// The browser's scene, rendered with `renderer`.
fn scene_json_with(editor: &mut Editor, renderer: &mut SceneRenderer, cols: u16, rows: u16) -> Value {
    let model = SceneModel::gather(editor, renderer, cols, rows, SceneFor::Browser, &mut SceneTimes::default());
    scene_json_value(&model)
}

/// The colors a front end draws Fresh's chrome with, from the active theme
/// (named and indexed colors through the web palette, `color_rgb`).
pub(crate) struct SceneTheme {
    pub(crate) name: String,
    pub(crate) bg: Option<Rgb>,
    pub(crate) fg: Option<Rgb>,
    pub(crate) accent: Option<Rgb>,
    pub(crate) muted: Option<Rgb>,
    pub(crate) selection_bg: Option<Rgb>,
    pub(crate) menu_bg: Option<Rgb>,
    pub(crate) menu_fg: Option<Rgb>,
    pub(crate) menu_hi: Option<Rgb>,
    pub(crate) menu_highlight_fg: Option<Rgb>,
    pub(crate) popup_bg: Option<Rgb>,
    pub(crate) popup_fg: Option<Rgb>,
    pub(crate) border: Option<Rgb>,
    pub(crate) status_bg: Option<Rgb>,
    pub(crate) status_fg: Option<Rgb>,
    pub(crate) tab_active_bg: Option<Rgb>,
}

impl SceneTheme {
    fn of(editor: &Editor) -> Self {
        let t = editor.theme.read().unwrap();
        Self {
            name: t.name.clone(),
            bg: color_rgb(t.editor_bg),
            fg: color_rgb(t.editor_fg),
            accent: color_rgb(t.cursor),
            muted: color_rgb(t.line_number_fg),
            selection_bg: color_rgb(t.selection_bg),
            menu_bg: color_rgb(t.menu_bg),
            menu_fg: color_rgb(t.menu_fg),
            menu_hi: color_rgb(t.menu_highlight_bg),
            // The ink the theme itself chose for its highlight fill. The
            // frontend can only guess black or white from the fill; a theme
            // that names this beats the guess, so send it and let the guess be
            // the fallback for a theme that leaves it at terminal reset.
            menu_highlight_fg: color_rgb(t.menu_highlight_fg),
            popup_bg: color_rgb(t.popup_bg),
            popup_fg: color_rgb(t.popup_text_fg),
            border: color_rgb(t.popup_border_fg),
            status_bg: color_rgb(t.status_bar_bg),
            status_fg: color_rgb(t.status_bar_fg),
            tab_active_bg: color_rgb(t.tab_active_bg),
        }
    }
}

/// One pane: where its content and scrollbar are, its tab bar, and the
/// scrollbar's thumb and plugin marks.
pub(crate) struct PaneModel {
    pub(crate) leaf: usize,
    pub(crate) buffer: Option<usize>,
    pub(crate) content: Rect,
    /// The line-number gutter's width, at the content's left; 0 without one.
    pub(crate) gutter_width: u16,
    pub(crate) tab_bar: crate::view::scene::TabBarView,
    pub(crate) vscroll: Rect,
    pub(crate) thumb_start: usize,
    pub(crate) thumb_end: usize,
    pub(crate) markers: Vec<ScrollbarMarker>,
}

/// A plugin's mark on a pane's scrollbar: its row on the track and color.
pub(crate) struct ScrollbarMarker {
    pub(crate) row: u16,
    pub(crate) color: Rgb,
}

/// A line between panes.
pub(crate) struct SeparatorModel {
    pub(crate) vertical: bool,
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) len: u16,
}

/// Everything one scene carries, gathered once per build for the encoders to
/// read: the JSON one for browsers (`scene_json_value`) and the protobuf one
/// for File Commander's socket. The views are the core's (`view::scene`), the
/// cells the renderer's buffer, borrowed.
pub(crate) struct SceneModel<'r> {
    pub(crate) buf: &'r Buffer,
    pub(crate) cursor: Option<(u16, u16)>,
    pub(crate) window_id: u64,
    pub(crate) popups: Vec<crate::view::scene::ScenePopup>,
    pub(crate) menu: crate::view::scene::MenuView,
    pub(crate) menubar: Option<Rect>,
    pub(crate) panes: Vec<PaneModel>,
    pub(crate) separators: Vec<SeparatorModel>,
    pub(crate) file_explorer: Option<crate::view::scene::FileExplorerView>,
    pub(crate) status: Option<crate::view::scene::StatusView>,
    pub(crate) palette: Option<crate::view::scene::PaletteView>,
    pub(crate) file_browser: Option<crate::view::scene::FileBrowserView>,
    pub(crate) trust_dialog: Option<crate::view::scene::TrustDialogView>,
    pub(crate) context_menu: Option<crate::view::scene::ContextMenuView>,
    pub(crate) aux_modal: Option<crate::view::scene::AuxModalView>,
    pub(crate) keybinding_editor: Option<crate::view::scene::KeybindingEditorView>,
    pub(crate) settings: Option<crate::view::scene::SettingsView>,
    pub(crate) theme: SceneTheme,
    pub(crate) tree: crate::view::scene::TreeView,
    pub(crate) poll_active: bool,
}

impl<'r> SceneModel<'r> {
    /// Render the editor and gather the scene, in the order the scene was
    /// always built in.
    fn gather(
        editor: &mut Editor,
        renderer: &'r mut SceneRenderer,
        cols: u16,
        rows: u16,
        target: SceneFor,
        times: &mut SceneTimes,
    ) -> Self {
        // The app has Fresh's menus in its own menu bar and no use for the menu
        // row, so its frame starts at row 0. Only while no menu is open: a menu
        // opened from the keyboard (`menu_activate`) still shows itself, the way
        // a hidden bar auto-shows in the terminal.
        if target == SceneFor::App && editor.menu_state().active_menu.is_none() {
            let window = editor.active_window_mut();
            window.menu_bar_visible = false;
            window.menu_bar_auto_shown = false;
        }
        // The app gets the whole frame as cells, chrome included; a browser
        // gets pane interiors only and draws the chrome natively.
        let started = Instant::now();
        let (buf, cursor) = match target {
            SceneFor::App => renderer.render(editor, cols, rows),
            SceneFor::Browser => with_chrome_suppressed(editor, |e| renderer.render(e, cols, rows)),
        };
        times.render_ms = started.elapsed().as_secs_f64() * 1000.0;
        let (w, h) = (buf.area.width, buf.area.height);

        // Semantic popups (completion / hover / action / list / text) — derived once
        // in the core (`Editor::popups_view`) and rendered as native UI, not cells.
        let popups = editor.popups_view();
        // Semantic menu model — derived once in the core (`Editor::menu_view`) and
        // shared with the TUI renderer. See crates/fresh-editor/src/view/scene.rs.
        let menu = editor.menu_view();

        // --- per-window geometry, read off the frame's tree ---
        let content = editor.body_area().unwrap_or(Rect::new(0, 0, w, h));
        // The menu bar spans the FULL width at row 0 — exactly as the TUI draws it,
        // *above* any left dock (the dock/file-explorer carve the rows below). Using
        // `content.x` here would shift the whole menu right when a left dock opens.
        // Per-menu title x still comes from the tree's label rectangles (so
        // titles + their dropdowns align); only the container is full-width.
        let menubar = (content.y > 0).then(|| Rect::new(0, 0, w, content.y));

        // **The rectangles are the tree's here too, and so is the thumb.** The
        // web is a consumer of the same layout the TUI folds (D.3), so a pane's
        // content and its scrollbar come from the nodes under `content_key` /
        // `vscroll_key`, and the thumb from the bar's own facts. A pane the tree
        // has no rectangle for is dropped, which is what a zero-size entry meant.
        let panes = editor
            .window_panes()
            .iter()
            .filter_map(|(leaf, target)| {
                let content = editor.pane_content_rect(*leaf)?;
                let vscroll = editor
                    .pane_vscroll_rect(*leaf)
                    .unwrap_or(Rect::new(0, 0, 0, 0));
                // The thumb is the bar's own: its facts on the track the tree
                // gave it, the arithmetic the bar's leaf paints with.
                let (thumb_start, thumb_end) = editor
                    .bar_thumb(*leaf, fresh_ui::Axis::Vertical)
                    .map(|(s, e, _)| (s, e))
                    .unwrap_or((0, 0));
                // Tabs are derived once in the core (`Editor::tab_bar_view`).
                let tab_bar = editor.tab_bar_view(*leaf);
                // A pane showing a group tab has no buffer and no gutter:
                // its content is the group's grid, whose panels are panes
                // of their own in this list.
                let buffer = target.as_buffer();
                let gutter_width = buffer
                    .map_or(0, |b| editor.leaf_gutter_width(*leaf, b))
                    .min(content.width);
                Some(PaneModel {
                    leaf: leaf.0 .0,
                    buffer: buffer.map(|b| b.0),
                    content,
                    gutter_width,
                    tab_bar,
                    vscroll,
                    thumb_start,
                    thumb_end,
                    // Plugin scrollbar markers. A front end draws its own
                    // scrollbar from `vscroll`/`thumb*` rather than reading the
                    // painted cells, so markers must travel as data too.
                    markers: scrollbar_markers(editor, *leaf),
                })
            })
            .collect();

        let separators = editor
            .separator_rects()
            .iter()
            .map(|(_id, dir, x, y, len)| SeparatorModel {
                vertical: matches!(dir, crate::model::event::SplitDirection::Vertical),
                x: *x,
                y: *y,
                len: *len,
            })
            .collect();

        // Semantic file explorer, status bar, palette, file browser, dialogs
        // and modals — each derived once in the core and rendered natively.
        let file_explorer = editor.file_explorer_view();
        let status = editor.status_view();
        let palette = editor.palette_view();
        let file_browser = editor.file_browser_view();
        let trust_dialog = editor.trust_dialog_view();
        let context_menu = editor.context_menu_view();
        let aux_modal = editor.aux_modals_view();
        let keybinding_editor = editor.keybinding_editor_view();
        let settings = editor.settings_view();
        let theme = SceneTheme::of(editor);
        // The plugin panels, as the display list the tree produced for them
        // (`Editor::tree_view`).
        let tree = editor.tree_view();
        let poll_active = poll_active(editor);
        // The active window id lets the frontend tell a WORKSPACE SWITCH (dock
        // live-switch: everything changes at once) from an in-place layout change
        // (explorer toggle) — the former gets a hard cut, not layout motion.
        let window_id = editor.active_window.0;
        Self {
            buf,
            cursor,
            window_id,
            popups,
            menu,
            menubar,
            panes,
            separators,
            file_explorer,
            status,
            palette,
            file_browser,
            trust_dialog,
            context_menu,
            aux_modal,
            keybinding_editor,
            settings,
            theme,
            tree,
            poll_active,
        }
    }
}

/// The scene a browser gets, from the model: the JSON every web front end
/// reads (see the module docs).
fn scene_json_value(model: &SceneModel<'_>) -> Value {
    let buf = model.buf;
    let (w, h) = (buf.area.width, buf.area.height);
    let popups = serde_json::to_value(&model.popups).unwrap_or_else(|_| json!([]));
    let menu_view = serde_json::to_value(&model.menu).unwrap_or_else(|_| json!({}));
    let get = |k: &str| menu_view.get(k).cloned().unwrap_or(Value::Null);
    let menus = get("menus");
    let menu_open = get("menuOpen");
    let menu_highlight = get("menuHighlight");
    let submenu_path = get("submenuPath");
    let dropdown = get("dropdown");

    let panes: Vec<Value> = model
        .panes
        .iter()
        .map(|pane| {
            // Emit the line-number gutter as its own cell block, separate
            // from the buffer text, sliced at the renderer's real gutter
            // width — so the frontend keeps the gutter out of the text flow
            // (future native selection covers only code). `cells` is the
            // buffer text after the gutter; `gutter` is the line-number
            // column. When line numbers are off, gutterWidth is 0 and
            // `cells` is the whole pane.
            let (content, gw) = (pane.content, pane.gutter_width);
            let gutter_rect = Rect::new(content.x, content.y, gw, content.height);
            let text_rect = Rect::new(content.x + gw, content.y, content.width - gw, content.height);
            let markers: Vec<Value> = pane
                .markers
                .iter()
                .map(|m| json!({ "row": m.row, "color": m.color }))
                .collect();
            json!({
                "leaf": pane.leaf,
                "buffer": pane.buffer,
                "content": rect_json(content),
                "gutterWidth": gw,
                "gutter": if gw > 0 { cells_json(buf, gutter_rect) } else { Value::Null },
                "cells": cells_json(buf, text_rect),
                "tabBar": serde_json::to_value(pane.tab_bar.bar).unwrap_or(Value::Null),
                "tabs": serde_json::to_value(&pane.tab_bar.tabs).unwrap_or_else(|_| json!([])),
                "vscroll": rect_json(pane.vscroll),
                "thumbStart": pane.thumb_start,
                "thumbEnd": pane.thumb_end,
                // Colours are resolved here — the frontend has no theme-key
                // resolver.
                "vscrollMarkers": markers,
            })
        })
        .collect();

    let separators: Vec<Value> = model
        .separators
        .iter()
        .map(|s| json!({ "vertical": s.vertical, "x": s.x, "y": s.y, "len": s.len }))
        .collect();

    let file_explorer = serde_json::to_value(&model.file_explorer).unwrap_or(Value::Null);
    let statusbar = serde_json::to_value(&model.status).unwrap_or(Value::Null);
    let mut palette = serde_json::to_value(&model.palette).unwrap_or(Value::Null);
    // The overlay preview pane is real rendered buffer cells (a phantom-leaf
    // pipeline render into its content rect), not chrome — so slice them from
    // the buffer just like a pane interior and attach them to the palette. The
    // frontend draws these cells inside its native preview frame.
    if let Some(pv) = palette.get("previewRect").cloned() {
        let u = |k: &str| pv.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u16;
        let pr = Rect::new(u("x"), u("y"), u("w"), u("h"));
        if pr.width > 0 && pr.height > 0 {
            let cells = cells_json(buf, pr);
            if let Some(obj) = palette.as_object_mut() {
                obj.insert("previewCells".to_string(), cells);
            }
        }
    }
    // Semantic file browser for the OpenFile / SaveFileAs / SwitchProject
    // prompts (`Editor::file_browser_view`), attached to the palette because
    // it belongs to the active prompt.
    let file_browser = serde_json::to_value(&model.file_browser).unwrap_or(Value::Null);
    if !file_browser.is_null() {
        if let Some(obj) = palette.as_object_mut() {
            obj.insert("browser".to_string(), file_browser);
        }
    }
    let trust_dialog = serde_json::to_value(&model.trust_dialog).unwrap_or(Value::Null);
    let context_menu = serde_json::to_value(&model.context_menu).unwrap_or(Value::Null);
    let aux_modal = serde_json::to_value(&model.aux_modal).unwrap_or(Value::Null);
    let keybinding_editor = serde_json::to_value(&model.keybinding_editor).unwrap_or(Value::Null);
    let settings = serde_json::to_value(&model.settings).unwrap_or(Value::Null);

    // Theme-accurate chrome palette: the frontend's CSS variables (--bg, --fg,
    // --accent, …) are seeded from these so the native HTML chrome matches the
    // terminal instead of a fixed dark palette.
    let t = &model.theme;
    let theme = json!({
        "name": t.name,
        "bg": t.bg,
        "fg": t.fg,
        "accent": t.accent,
        "muted": t.muted,
        "selectionBg": t.selection_bg,
        "menuBg": t.menu_bg,
        "menuFg": t.menu_fg,
        "menuHi": t.menu_hi,
        "menuHighlightFg": t.menu_highlight_fg,
        "popupBg": t.popup_bg,
        "popupFg": t.popup_fg,
        "border": t.border,
        "statusBg": t.status_bg,
        "statusFg": t.status_fg,
        "tabActiveBg": t.tab_active_bg,
    });

    let regions = json!({
        "menubar": model.menubar.map(rect_json),
        "menus": menus,
        "menuOpen": menu_open,
        "menuHighlight": menu_highlight,
        "submenuPath": submenu_path,
        "dropdown": dropdown,
        "statusbar": statusbar,
        "fileExplorer": file_explorer,
        "panes": panes,
        "separators": separators,
        "popups": popups,
        // The plugin panels, as the display list the tree produced for them
        // (`Editor::tree_view`); the web folds it into DOM.
        "tree": serde_json::to_value(&model.tree).unwrap_or_else(|_| json!({})),
        "palette": palette,
        "trustDialog": trust_dialog,
        "contextMenu": context_menu,
        "auxModal": aux_modal,
        "keybindingEditor": keybinding_editor,
        "settings": settings,
        "cursor": model.cursor.map(|(x, y)| json!({ "x": x, "y": y })),
        // Pacing hint: when something is animating / an LSP spinner is live /
        // a timer is pending, the server ticks fast (TICK_ACTIVE), otherwise
        // idles (TICK_IDLE, just to pick up async LSP/file events). Kept in
        // the scene so tests (and any polling client) can observe it.
        "poll": json!({ "active": model.poll_active }),
    });
    json!({ "w": w, "h": h, "windowId": model.window_id, "regions": regions, "theme": theme })
}

/// The digit a browser's physical `code` names — `"Digit1"` → `'1'`. Every
/// other code (letters, punctuation, named keys) is left to the reported
/// character, which already matches what a terminal sends.
fn digit_row_char(code: &str) -> Option<char> {
    let rest = code.strip_prefix("Digit")?;
    let mut chars = rest.chars();
    let d = chars.next()?;
    (chars.next().is_none() && d.is_ascii_digit()).then_some(d)
}

/// Map a browser key to a crossterm key and run the real input path.
fn apply_key(editor: &mut Editor, input: KeyInput<'_>) {
    let KeyInput {
        key,
        ctrl,
        alt,
        meta,
        shift,
        ..
    } = input;

    let mut code = match key {
        "Enter" => KeyCode::Enter,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Tab" => KeyCode::Tab,
        "Escape" => KeyCode::Esc,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Insert" => KeyCode::Insert,
        f if f.len() > 1 && f.starts_with('F') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
            KeyCode::F(f[1..].parse().unwrap_or(1))
        }
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => return,
    };
    // A browser reports the SHIFTED character a key produces: Ctrl+Shift+1
    // arrives as `{key:"!", code:"Digit1"}`. Keybindings — and the kitty-
    // protocol events a terminal delivers — are written against the UNSHIFTED
    // key plus a SHIFT modifier (`set_bookmark` is Ctrl+Shift+0..9), so
    // forwarding '!' left all ten bookmark slots unreachable from a browser.
    // With CONTROL held the character is not text being typed, so the physical
    // `code` is the better witness, and it is what the terminal reports. Plain
    // typing is untouched (no CONTROL), and ALT is excluded so AltGr layouts
    // keep producing their own characters.
    let digit_row = match ctrl && !alt {
        true => input.code.and_then(digit_row_char),
        false => None,
    };
    if let Some(d) = digit_row {
        code = KeyCode::Char(d);
    }
    let mut mods = KeyModifiers::empty();
    if ctrl {
        mods |= KeyModifiers::CONTROL;
    }
    if alt {
        mods |= KeyModifiers::ALT;
    }
    if meta {
        mods |= KeyModifiers::SUPER;
    }
    // SHIFT handling mirrors what crossterm delivers to the TUI. Keep it on
    // named keys (Tab, arrows, …) and on ASCII *letters*: the browser sends the
    // uppercase form for a shifted letter, and `normalize_key` reconciles the
    // redundant SHIFT — which is exactly what makes Ctrl+Shift+<letter> bindings
    // fire (dropping SHIFT there left `Ctrl+Shift+N` resolving as plain
    // `Ctrl+N`). Do NOT add it for a shifted *symbol* like '!': the shift is
    // already baked into the character, so a bare symbol is what the terminal
    // sends (a spurious SHIFT would diverge and could break symbol input).
    let shift_is_meaningful = match code {
        // A digit recovered from the physical key above is the UNSHIFTED
        // character, so its SHIFT is real information (Ctrl+Shift+1 →
        // `set_bookmark`), unlike the '!' the browser reported.
        KeyCode::Char(_) if digit_row.is_some() => true,
        KeyCode::Char(c) => c.is_ascii_alphabetic(),
        _ => true,
    };
    if shift && shift_is_meaningful {
        mods |= KeyModifiers::SHIFT;
    }
    // A browser reports Shift+Tab as `{key:"Tab", shift:true}`, but crossterm
    // delivers real-terminal Shift+Tab as `KeyCode::BackTab` (carrying SHIFT),
    // and the editor's reverse-navigation paths (Settings dialog focus, popup
    // `select_prev`, `SwitchPane(Prev)`) match on `BackTab`. Fold so the web
    // event is identical to the terminal one — without it, Shift+Tab moved the
    // wrong way (or was a no-op) on the web.
    if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
        code = KeyCode::BackTab;
    }
    // Wave-animation dismissal parity with the TUI event loops (main.rs and
    // the daemon server loop): ANY key press dismisses the interactive wave
    // and is CONSUMED — it only stops the show, it doesn't also act on the
    // editor. `KeyEvent::new` sets kind=Press, which the dismissal requires.
    if editor.maybe_dismiss_wave_animation(&Event::key(KeyEvent::new(code, mods))) {
        return;
    }
    // Cmd chords from a macOS front end. Keymaps written for terminals can't
    // bind Super (no terminal sends it), so Fresh's own `macos-gui` keymap
    // answers them — Cmd+S saves, Cmd+Z undoes, Cmd+W closes the tab — while
    // the user's keymap keeps every other key, here and for the terminals
    // sharing this session.
    if mods.contains(KeyModifiers::SUPER) {
        if let Some(action) = super_chord_action(editor, code, mods) {
            if let Err(e) = editor.handle_action(action) {
                eprintln!("[webui] action error: {e}");
            }
            return;
        }
    }
    if let Err(e) = editor.handle_key(code, mods) {
        eprintln!("handle_key error: {e}");
    }
}

/// The action Fresh's `macos-gui` keymap binds to a Cmd chord in the current
/// key context, if any. The resolver is built once and rebuilt only when the
/// user's keymap choice or custom bindings change.
fn super_chord_action(
    editor: &mut Editor,
    code: KeyCode,
    mods: KeyModifiers,
) -> Option<crate::input::keybindings::Action> {
    use crate::input::keybindings::{Action, KeybindingResolver};
    use std::cell::RefCell;
    thread_local! {
        static RESOLVER: RefCell<Option<(String, KeybindingResolver)>> = const { RefCell::new(None) };
    }
    let context = editor.get_key_context();
    let config = editor.config();
    let stamp = format!(
        "{}:{}",
        config.active_keybinding_map.0,
        serde_json::to_string(&config.keybindings).unwrap_or_default()
    );
    // Bindings name the unshifted letter (`super+shift p`), as `handle_key` resolves them.
    let (code, mods) = crate::input::keybindings::normalize_key(code, mods);
    let event = KeyEvent::new(code, mods);
    RESOLVER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().is_none_or(|(s, _)| *s != stamp) {
            let mut gui = config.clone();
            gui.active_keybinding_map = config::KeybindingMapName("macos-gui".to_string());
            *slot = Some((stamp, KeybindingResolver::new(&gui)));
        }
        // `resolve` falls back from the key context to Normal and Global, as
        // the editor's own dispatch does (Cmd+Shift+P is a Global binding).
        let action = slot.as_ref().map(|(_, r)| r.resolve(&event, context.clone()))?;
        (!matches!(action, Action::None | Action::InsertChar(_))).then_some(action)
    })
}

/// Forward a browser mouse/wheel event to the real `Editor::handle_mouse` at
/// cell coordinates; the editor does all hit-testing (panes, tabs, scrollbars,
/// separators), exactly as the terminal/GUI frontends do.
fn apply_mouse(editor: &mut Editor, input: MouseInput) {
    let MouseInput {
        kind,
        col,
        row,
        n,
        count,
        modifiers: mods,
    } = input;
    // Wave-animation dismissal parity with the TUI event loops: ANY mouse
    // activity (move, click, wheel) dismisses the interactive wave and is
    // CONSUMED. One representative event is enough — the whole batch below
    // carries the same kind/cell.
    if editor.maybe_dismiss_wave_animation(&Event::Mouse(MouseEvent {
        kind,
        column: col,
        row,
        modifiers: mods,
    })) {
        return;
    }
    // Explicit multi-click count from the browser (`event.detail`). The editor
    // detects double/triple clicks itself (`detect_multi_click`) by comparing
    // wall-clock spacing and the exact cell of consecutive Downs — reliable in
    // a terminal, but jitter-sensitive across the HTTP hop and stricter than
    // the browser's own few-pixel slop (two clicks the browser counts as a
    // double can straddle a cell boundary). When the browser already counted,
    // prime the editor's OWN click-tracking state so its detection resolves
    // the same count deterministically: `detect_multi_click` sees a
    // just-now previous click at this cell and bumps `click_count` to the
    // browser's count (2 = double, ≥3 = triple; the editor's word/line
    // selection then runs through its normal, unmodified path).
    if count >= 2 && matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
        let w = editor.active_window_mut();
        w.previous_click_time = Some(std::time::Instant::now());
        w.previous_click_position = Some((col, row));
        w.click_count = (count - 1).min(2) as u8;
    }
    for _ in 0..n {
        let ev = MouseEvent {
            kind,
            column: col,
            row,
            modifiers: mods,
        };
        if let Err(e) = editor.handle_mouse(ev) {
            eprintln!("handle_mouse error: {e}");
            break;
        }
    }
}

/// The 16 ANSI colors (indices 0-15), shared by `color_css` (named-color arm)
/// and `indexed_rgb` so the same logical color resolves to one hex no matter
/// whether it arrives as a named `Color` or a `Color::Indexed`.
const ANSI16: [(u8, u8, u8); 16] = [
    (0, 0, 0),          // 0 black
    (0xcd, 0x31, 0x31), // 1 red
    (0x0d, 0xbc, 0x79), // 2 green
    (0xe5, 0xe5, 0x10), // 3 yellow
    (0x24, 0x72, 0xc8), // 4 blue
    (0xbc, 0x3f, 0xbc), // 5 magenta
    (0x11, 0xa8, 0xcd), // 6 cyan
    (0xe5, 0xe5, 0xe5), // 7 white / gray
    (0x66, 0x66, 0x66), // 8 bright black / dark gray
    (0xf1, 0x4c, 0x4c), // 9 bright red
    (0x23, 0xd1, 0x8b), // 10 bright green
    (0xf5, 0xf5, 0x43), // 11 bright yellow
    (0x3b, 0x8e, 0xea), // 12 bright blue
    (0xd6, 0x70, 0xd6), // 13 bright magenta
    (0x29, 0xb8, 0xdb), // 14 bright cyan
    (0xff, 0xff, 0xff), // 15 bright white
];

fn hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// ratatui Color → CSS hex (or None for the terminal default). Named colors map
/// to their ANSI index in `ANSI16` so they agree with `Color::Indexed`.
fn color_css(c: Color) -> Option<String> {
    color_rgb(c).map(|Rgb(r, g, b)| hex(r, g, b))
}

/// A color as RGB through the web palette (`ANSI16`, the xterm cube and
/// ramp), `None` for the terminal default.
fn color_rgb(c: Color) -> Option<Rgb> {
    let ansi = |i: usize| {
        let (r, g, b) = ANSI16[i];
        Rgb(r, g, b)
    };
    Some(match c {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => Rgb(r, g, b),
        Color::Black => ansi(0),
        Color::Red => ansi(1),
        Color::Green => ansi(2),
        Color::Yellow => ansi(3),
        Color::Blue => ansi(4),
        Color::Magenta => ansi(5),
        Color::Cyan => ansi(6),
        Color::Gray => ansi(7),
        Color::DarkGray => ansi(8),
        Color::LightRed => ansi(9),
        Color::LightGreen => ansi(10),
        Color::LightYellow => ansi(11),
        Color::LightBlue => ansi(12),
        Color::LightMagenta => ansi(13),
        Color::LightCyan => ansi(14),
        Color::White => ansi(15),
        Color::Indexed(i) => indexed_rgb(i),
    })
}

/// xterm-256 palette → hex.
fn indexed_rgb(i: u8) -> Rgb {
    let (r, g, b) = if i < 16 {
        ANSI16[i as usize]
    } else if i < 232 {
        let n = i - 16;
        let levels = [0u8, 95, 135, 175, 215, 255];
        (
            levels[(n / 36) as usize],
            levels[((n / 6) % 6) as usize],
            levels[(n % 6) as usize],
        )
    } else {
        let v = 8 + (i - 232) * 10;
        (v, v, v)
    };
    Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::{
        digit_row_char, host_only, origin_host_matches, try_parse_request, HTTP_REQUEST_CAP,
    };

    #[test]
    fn rendered_cells_read_as_a_terminal_drawn_from_scratch_holds_them() {
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::{Color, Style};
        let area = Rect::new(0, 0, 12, 3);
        let mut buf = Buffer::empty(area);
        // Wide characters under a line highlight, which also styles the
        // cells they cover; an emoji with VS16; a cell marked skip.
        buf.set_string(0, 0, "a日本b", Style::default());
        buf.set_style(Rect::new(0, 0, 12, 1), Style::default().bg(Color::Blue));
        buf.set_string(0, 1, "x\u{1F44D}\u{FE0F}y", Style::default());
        buf.set_style(Rect::new(0, 1, 12, 1), Style::default().bg(Color::Red));
        buf[(5, 2)].set_symbol("Z").set_skip(true);
        let mut terminal = TestBackend::new(12, 3);
        let updates = Buffer::empty(area).diff(&buf);
        match terminal.draw(updates.into_iter()) {
            Ok(()) => {}
            Err(never) => match never {},
        }
        let mut ours = buf.clone();
        super::as_drawn_from_scratch(&mut ours);
        assert_eq!(&ours, terminal.buffer());
    }

    #[test]
    fn digit_row_is_recovered_from_the_physical_key() {
        // Regression: a browser reports Ctrl+Shift+1 as `{key:"!",
        // code:"Digit1"}`, and the ten `set_bookmark` bindings are written
        // against the unshifted digit plus SHIFT, so the bridge reads the
        // digit off the physical key.
        assert_eq!(digit_row_char("Digit1"), Some('1'));
        assert_eq!(digit_row_char("Digit0"), Some('0'));
        assert_eq!(digit_row_char("Digit9"), Some('9'));
        // Everything else keeps the character the browser reported.
        assert_eq!(digit_row_char("KeyA"), None);
        assert_eq!(digit_row_char("Numpad1"), None);
        assert_eq!(digit_row_char("Digit"), None);
        assert_eq!(digit_row_char("Digit12"), None);
        assert_eq!(digit_row_char("Digitx"), None);
        assert_eq!(digit_row_char(""), None);
    }

    #[test]
    fn absurd_content_length_is_rejected_not_panicked() {
        // Regression: a `Content-Length` near usize::MAX used to make
        // `head_end + content_length` wrap (release) / overflow-panic (debug),
        // and the wrapped value sliced an inverted range — one unauthenticated
        // request that killed the single editor thread. It must now parse to
        // "incomplete" (None), never panic.
        let raw = b"POST /key HTTP/1.1\r\nContent-Length: 18446744073709551615\r\n\r\n";
        assert!(try_parse_request(raw).is_none());
        // A declared body just over the request cap is likewise refused up front.
        let over = format!(
            "POST /key HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            HTTP_REQUEST_CAP + 1
        );
        assert!(try_parse_request(over.as_bytes()).is_none());
    }

    #[test]
    fn well_formed_request_with_body_still_parses() {
        // A request whose declared body has fully arrived parses (the fix must
        // reject only absurd/oversize lengths, not normal ones).
        let raw = b"POST /action HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let req = try_parse_request(raw).expect("complete request parses");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/action");
        assert_eq!(req.body, b"hello");
        // A request whose body hasn't fully arrived yet is "incomplete", not an error.
        let partial = b"POST /action HTTP/1.1\r\nContent-Length: 5\r\n\r\nhel";
        assert!(try_parse_request(partial).is_none());
    }

    #[test]
    fn host_only_strips_scheme_port_and_brackets() {
        assert_eq!(host_only("http://127.0.0.1:8137/ws"), "127.0.0.1");
        assert_eq!(host_only("127.0.0.1:8137"), "127.0.0.1");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("http://[::1]:8137"), "::1");
        assert_eq!(host_only("[::1]:8137"), "::1");
        assert_eq!(host_only("0.0.0.0"), "0.0.0.0");
    }

    #[test]
    fn same_origin_upgrade_is_allowed_for_wildcard_bind() {
        // The regression: `--web 0.0.0.0:8137` reached via 127.0.0.1. The
        // browser's Origin host matches the Host it connected to, so the
        // upgrade must be allowed even though neither equals the bind host.
        assert!(origin_host_matches(
            "http://127.0.0.1:8137",
            Some("127.0.0.1:8137"),
            "0.0.0.0",
        ));
        // Same for a LAN IP served by the same wildcard bind.
        assert!(origin_host_matches(
            "http://192.168.1.5:8137",
            Some("192.168.1.5:8137"),
            "0.0.0.0",
        ));
        // ...and for localhost / IPv6 loopback.
        assert!(origin_host_matches(
            "http://localhost:8137",
            Some("localhost:8137"),
            "0.0.0.0",
        ));
        assert!(origin_host_matches(
            "http://[::1]:8137",
            Some("[::1]:8137"),
            "::",
        ));
    }

    #[test]
    fn cross_origin_upgrade_is_rejected() {
        // A page on another origin connects to our Host but carries its own
        // Origin — the mismatch is the guard that must still fire.
        assert!(!origin_host_matches(
            "http://evil.example.com",
            Some("127.0.0.1:8137"),
            "0.0.0.0",
        ));
        assert!(!origin_host_matches(
            "https://attacker.test:443",
            Some("127.0.0.1:8137"),
            "127.0.0.1",
        ));
    }

    #[test]
    fn dns_rebinding_upgrade_is_rejected() {
        // Rebinding: the attacker's page is served from `attacker.com`, whose
        // DNS record is flipped to 127.0.0.1 after first load. The browser then
        // connects to us sending Host AND Origin both `attacker.com`, so the
        // Origin-equals-Host check passes — but the Host is an untrusted name,
        // not a loopback/LAN literal, so the allowlist must reject it.
        assert!(!origin_host_matches(
            "http://attacker.com",
            Some("attacker.com:8137"),
            "0.0.0.0",
        ));
        assert!(!origin_host_matches(
            "http://attacker.com:8137",
            Some("attacker.com:8137"),
            "127.0.0.1",
        ));
        // A bare IP / localhost Host is still fine (the legitimate reach).
        assert!(origin_host_matches(
            "http://127.0.0.1:8137",
            Some("127.0.0.1:8137"),
            "0.0.0.0",
        ));
    }

    #[test]
    fn falls_back_to_bind_host_without_a_host_header() {
        assert!(origin_host_matches(
            "http://127.0.0.1:8137",
            None,
            "127.0.0.1",
        ));
        assert!(!origin_host_matches(
            "http://127.0.0.1:8137",
            None,
            "10.0.0.1",
        ));
    }
}
