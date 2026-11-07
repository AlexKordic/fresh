//! What File Commander's socket carries, decoded with the types generated
//! from `scene.proto`: a client's scene built from the messages it got
//! (`Client`), as File Commander builds it, and that scene as text for the
//! golden files (`Client::dump`).

use crate::scene_proto as pb;
use prost::Message as _;

/// A client's scene, from a `ServerHello` and the `Frame`s after it, applied
/// as File Commander applies them.
#[derive(Default)]
pub struct Client {
    width: u32,
    height: u32,
    window_id: u64,
    cursor: Option<pb::Cursor>,
    theme: Option<pb::Theme>,
    grid: Vec<pb::GridRow>,
    panes: Vec<pb::Pane>,
    menus: Vec<pb::MenuEntry>,
    status: Option<pb::StatusView>,
    file_explorer: Option<pb::FileExplorerView>,
    separators: Vec<pb::Separator>,
    popups: Vec<pb::ScenePopup>,
    palette: Option<pb::PaletteView>,
    trust_dialog: Option<pb::TrustDialogView>,
    context_menu: Option<pb::ContextMenuView>,
    keybinding_editor: Option<pb::KeybindingEditorView>,
    settings: Option<pb::SettingsView>,
    /// Rows the last frame sent in full, and rows it copied.
    pub rows_sent: usize,
    pub rows_copied: usize,
}

impl Client {
    /// Apply one message off the wire: its type and payload.
    pub fn receive(&mut self, kind: u64, payload: &[u8]) {
        match kind {
            1 => {
                let hello = pb::ServerHello::decode(payload).expect("a ServerHello");
                assert_eq!(hello.version, 1);
                *self = Client::default();
                self.apply(hello.scene.expect("the hello's scene"));
            }
            2 => self.apply(pb::Frame::decode(payload).expect("a Frame")),
            other => panic!("unexpected message type {other}"),
        }
    }

    fn apply(&mut self, frame: pb::Frame) {
        if let Some(w) = frame.width {
            self.width = w;
        }
        if let Some(h) = frame.height {
            self.height = h;
        }
        if let Some(id) = frame.window_id {
            self.window_id = id;
        }
        if frame.cursor.is_some() {
            self.cursor = frame.cursor;
        }
        if frame.theme.is_some() {
            self.theme = frame.theme;
        }
        let previous = self.grid.clone();
        if let Some(n) = frame.grid_len {
            self.grid.resize(n as usize, pb::GridRow::default());
        }
        self.rows_copied = 0;
        for run in frame.copies.chunks(3) {
            let [to, from, count] = [run[0] as usize, run[1] as usize, run[2] as usize];
            for k in 0..count {
                if let (Some(row), Some(old)) = (self.grid.get_mut(to + k), previous.get(from + k))
                {
                    *row = old.clone();
                    self.rows_copied += 1;
                }
            }
        }
        self.rows_sent = frame.rows.len();
        for row in frame.rows {
            let i = row.index as usize;
            if i < self.grid.len() {
                self.grid[i] = row;
            }
        }
        if let Some(n) = frame.pane_len {
            self.panes.resize(n as usize, pb::Pane::default());
        }
        for update in frame.panes {
            let i = update.index as usize;
            if i < self.panes.len() {
                self.panes[i] = update.pane.unwrap_or_default();
            }
        }
        if let Some(u) = frame.menus {
            self.menus = u.menus;
        }
        if let Some(u) = frame.status_bar {
            self.status = u.value;
        }
        if let Some(u) = frame.file_explorer {
            self.file_explorer = u.value;
        }
        if let Some(u) = frame.separators {
            self.separators = u.separators;
        }
        if let Some(u) = frame.popups {
            self.popups = u.popups;
        }
        if let Some(u) = frame.palette {
            self.palette = u.value;
        }
        if let Some(u) = frame.trust_dialog {
            self.trust_dialog = u.value;
        }
        if let Some(u) = frame.context_menu {
            self.context_menu = u.value;
        }
        if let Some(u) = frame.keybinding_editor {
            self.keybinding_editor = u.value;
        }
        if let Some(u) = frame.settings {
            self.settings = u.value;
        }
    }
}

impl Client {
    /// The scene as text, a line per part, for golden files: each grid row as
    /// its text and its runs (byte length:style), everything else as the
    /// decoded message prints.
    pub fn dump(&self) -> String {
        let mut out = format!(
            "size {}x{} window {}\n",
            self.width, self.height, self.window_id
        );
        out.push_str(&format!(
            "cursor {:?}\ntheme {:?}\n",
            self.cursor, self.theme
        ));
        for (i, row) in self.grid.iter().enumerate() {
            let runs: Vec<String> = row
                .run_bytes
                .iter()
                .zip(&row.run_styles)
                .map(|(n, style)| format!("{n}:{style:x}"))
                .collect();
            out.push_str(&format!("grid[{i}] {:?} {}\n", row.text, runs.join(" ")));
        }
        for (i, pane) in self.panes.iter().enumerate() {
            out.push_str(&format!("pane[{i}] {pane:?}\n"));
        }
        for (i, menu) in self.menus.iter().enumerate() {
            out.push_str(&format!("menu[{i}] {menu:?}\n"));
        }
        for (i, separator) in self.separators.iter().enumerate() {
            out.push_str(&format!("separator[{i}] {separator:?}\n"));
        }
        for (i, popup) in self.popups.iter().enumerate() {
            out.push_str(&format!("popup[{i}] {popup:?}\n"));
        }
        out.push_str(&format!("status {:?}\n", self.status));
        out.push_str(&format!("fileExplorer {:?}\n", self.file_explorer));
        out.push_str(&format!("palette {:?}\n", self.palette));
        out.push_str(&format!("trustDialog {:?}\n", self.trust_dialog));
        out.push_str(&format!("contextMenu {:?}\n", self.context_menu));
        out.push_str(&format!("keybindingEditor {:?}\n", self.keybinding_editor));
        out.push_str(&format!("settings {:?}\n", self.settings));
        out
    }
}
