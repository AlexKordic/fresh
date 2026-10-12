//! What File Commander's socket carries, decoded with the types generated
//! from `scene.proto`, against the JSON scene of the same model: a client's
//! scene built from the messages it got (`Client`), compared region by
//! region (`compare`).

use crate::scene_proto as pb;
use prost::Message as _;
use serde_json::Value;

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
                if let (Some(row), Some(old)) = (self.grid.get_mut(to + k), previous.get(from + k)) {
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
        let mut out = format!("size {}x{} window {}\n", self.width, self.height, self.window_id);
        out.push_str(&format!("cursor {:?}\ntheme {:?}\n", self.cursor, self.theme));
        for (i, row) in self.grid.iter().enumerate() {
            let runs: Vec<String> =
                row.run_bytes.iter().zip(&row.run_styles).map(|(n, style)| format!("{n}:{style:x}")).collect();
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

/// The differences found, as `path: json …, protobuf …`.
struct Diffs(Vec<String>);

impl Diffs {
    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, path: &str, json: T, proto: T) {
        if json != proto {
            self.0.push(format!("{path}: json {json:?}, protobuf {proto:?}"));
        }
    }
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}
fn opt_u(v: &Value) -> Option<u64> {
    v.as_u64()
}
fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}
fn opt_s(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}
fn b(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}
fn rect(v: &Value) -> Option<(u64, u64, u64, u64)> {
    (!v.is_null()).then(|| (u(&v["x"]), u(&v["y"]), u(&v["w"]), u(&v["h"])))
}
fn prect(r: &Option<pb::Rect>) -> Option<(u64, u64, u64, u64)> {
    r.as_ref().map(|r| (u64::from(r.x), u64::from(r.y), u64::from(r.w), u64::from(r.h)))
}
fn css(c: Option<u32>) -> Option<String> {
    c.map(|c| format!("#{c:06x}"))
}
fn list(v: &Value) -> &[Value] {
    v.as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

/// One cell of the grid: its character, colors and flags.
type CellLook = (char, Option<String>, Option<String>, bool, bool, bool, bool);

fn json_cells(row: &Value) -> Vec<CellLook> {
    list(row)
        .iter()
        .flat_map(|run| {
            let look = (opt_s(&run["fg"]), opt_s(&run["bg"]), b(&run["b"]), b(&run["i"]), b(&run["u"]), b(&run["r"]));
            s(&run["t"])
                .chars()
                .map(move |c| (c, look.0.clone(), look.1.clone(), look.2, look.3, look.4, look.5))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn proto_cells(row: &pb::GridRow) -> Vec<CellLook> {
    let color = |c: u64| (c & 1 << 24 != 0).then(|| format!("#{:06x}", c & 0xff_ffff));
    let mut out = Vec::new();
    let mut at = 0usize;
    for (len, style) in row.run_bytes.iter().zip(&row.run_styles) {
        let text = &row.text[at..at + *len as usize];
        at += *len as usize;
        let flags = style >> 50;
        for c in text.chars() {
            out.push((
                c,
                color(style & 0x1ff_ffff),
                color(style >> 25 & 0x1ff_ffff),
                flags & 1 != 0,
                flags & 2 != 0,
                flags & 4 != 0,
                flags & 8 != 0,
            ));
        }
    }
    out
}

fn menu_items(d: &mut Diffs, path: &str, json: &Value, items: &[pb::MenuItem]) {
    d.eq(&format!("{path}.len"), list(json).len(), items.len());
    for (i, (j, p)) in list(json).iter().zip(items).enumerate() {
        let path = format!("{path}[{i}]");
        use pb::menu_item::Item;
        match (s(&j["kind"]).as_str(), &p.item) {
            ("action", Some(Item::Action(a))) => {
                d.eq(&format!("{path}.label"), s(&j["label"]), a.label.clone());
                d.eq(&format!("{path}.action"), s(&j["action"]), a.action.clone());
                d.eq(&format!("{path}.accel"), opt_s(&j["accel"]), a.accel.clone());
                d.eq(&format!("{path}.enabled"), b(&j["enabled"]), a.enabled);
                d.eq(&format!("{path}.checked"), j["checked"].as_bool(), a.checked);
                let args = (!a.args_json.is_empty())
                    .then(|| serde_json::from_str::<Value>(&a.args_json).expect("args_json is JSON"));
                d.eq(&format!("{path}.args"), j.get("args").cloned(), args);
            }
            ("sep", Some(Item::Separator(_))) => {}
            ("submenu", Some(Item::Submenu(m))) => {
                d.eq(&format!("{path}.label"), s(&j["label"]), m.label.clone());
                menu_items(d, &format!("{path}.items"), &j["items"], &m.items);
            }
            ("label", Some(Item::Label(l))) => d.eq(&format!("{path}.label"), s(&j["label"]), l.label.clone()),
            (kind, item) => d.0.push(format!("{path}: json kind {kind}, protobuf {item:?}")),
        }
    }
}

/// Every difference between the JSON scene and the client's.
pub fn compare(json: &Value, c: &Client) -> Vec<String> {
    let mut d = Diffs(Vec::new());
    let r = &json["regions"];
    d.eq("w", u(&json["w"]), u64::from(c.width));
    d.eq("h", u(&json["h"]), u64::from(c.height));
    d.eq("windowId", u(&json["windowId"]), c.window_id);
    let cursor = c.cursor.as_ref().map(|p| (u64::from(p.x), u64::from(p.y)));
    d.eq("cursor", Some((u(&r["cursor"]["x"]), u(&r["cursor"]["y"]))), cursor);

    // Theme.
    let t = &json["theme"];
    let th = c.theme.clone().unwrap_or_default();
    d.eq("theme.name", s(&t["name"]), th.name.clone());
    for (key, value) in [
        ("bg", th.bg),
        ("fg", th.fg),
        ("accent", th.accent),
        ("muted", th.muted),
        ("selectionBg", th.selection_bg),
        ("menuBg", th.menu_bg),
        ("menuFg", th.menu_fg),
        ("menuHi", th.menu_hi),
        ("menuHighlightFg", th.menu_highlight_fg),
        ("popupBg", th.popup_bg),
        ("popupFg", th.popup_fg),
        ("border", th.border),
        ("statusBg", th.status_bg),
        ("statusFg", th.status_fg),
        ("tabActiveBg", th.tab_active_bg),
    ] {
        d.eq(&format!("theme.{key}"), opt_s(&t[key]), css(value));
    }

    // The grid, cell by cell.
    let rows = list(&r["grid"]);
    d.eq("grid.len", rows.len(), c.grid.len());
    for (i, (j, p)) in rows.iter().zip(&c.grid).enumerate() {
        let (jc, pc) = (json_cells(j), proto_cells(p));
        if jc != pc {
            let at = jc.iter().zip(&pc).take_while(|(a, b)| a == b).count();
            d.0.push(format!(
                "grid[{i}] differs at cell {at}: json {:?}, protobuf {:?}",
                jc.get(at),
                pc.get(at)
            ));
        }
    }

    // Status bar.
    match (r["statusbar"].is_null(), &c.status) {
        (true, None) => {}
        (false, Some(st)) => {
            d.eq("statusbar.rect", rect(&r["statusbar"]["rect"]), prect(&st.rect));
            let segs = list(&r["statusbar"]["segments"]);
            d.eq("statusbar.segments.len", segs.len(), st.segments.len());
            for (i, (j, p)) in segs.iter().zip(&st.segments).enumerate() {
                let path = format!("statusbar.segments[{i}]");
                d.eq(&format!("{path}.name"), s(&j["name"]), p.name.clone());
                d.eq(&format!("{path}.key"), opt_s(&j["key"]), p.key.clone());
                d.eq(&format!("{path}.text"), s(&j["text"]), p.text.clone());
                d.eq(&format!("{path}.x"), u(&j["x"]), u64::from(p.x));
                d.eq(&format!("{path}.w"), u(&j["w"]), u64::from(p.w));
                d.eq(&format!("{path}.side"), s(&j["side"]), p.side.clone());
            }
        }
        (json_none, proto) => d.0.push(format!("statusbar: json null {json_none}, protobuf {proto:?}")),
    }

    // Menus.
    let menus = list(&r["menus"]);
    d.eq("menus.len", menus.len(), c.menus.len());
    for (i, (j, p)) in menus.iter().zip(&c.menus).enumerate() {
        let path = format!("menus[{i}]");
        d.eq(&format!("{path}.label"), s(&j["label"]), p.label.clone());
        d.eq(&format!("{path}.visible"), b(&j["visible"]), p.visible);
        d.eq(&format!("{path}.x"), opt_u(&j["x"]), p.x.map(u64::from));
        d.eq(&format!("{path}.w"), opt_u(&j["w"]), p.w.map(u64::from));
        menu_items(&mut d, &format!("{path}.items"), &j["items"], &p.items);
    }

    // Panes.
    let panes = list(&r["panes"]);
    d.eq("panes.len", panes.len(), c.panes.len());
    for (i, (j, p)) in panes.iter().zip(&c.panes).enumerate() {
        let path = format!("panes[{i}]");
        d.eq(&format!("{path}.leaf"), u(&j["leaf"]), p.leaf);
        d.eq(&format!("{path}.buffer"), opt_u(&j["buffer"]), p.buffer);
        d.eq(&format!("{path}.content"), rect(&j["content"]), prect(&p.content));
        d.eq(&format!("{path}.gutterWidth"), u(&j["gutterWidth"]), u64::from(p.gutter_width));
        d.eq(&format!("{path}.vscroll"), rect(&j["vscroll"]), prect(&p.vscroll));
        d.eq(&format!("{path}.thumbStart"), u(&j["thumbStart"]), p.thumb_start);
        d.eq(&format!("{path}.thumbEnd"), u(&j["thumbEnd"]), p.thumb_end);
        let tab_bar = p.tab_bar.clone().unwrap_or_default();
        d.eq(&format!("{path}.tabBar"), rect(&j["tabBar"]), prect(&tab_bar.bar));
        let tabs = list(&j["tabs"]);
        d.eq(&format!("{path}.tabs.len"), tabs.len(), tab_bar.tabs.len());
        for (k, (jt, pt)) in tabs.iter().zip(&tab_bar.tabs).enumerate() {
            let path = format!("{path}.tabs[{k}]");
            d.eq(&format!("{path}.bufferId"), opt_u(&jt["bufferId"]), pt.buffer_id);
            d.eq(&format!("{path}.label"), s(&jt["label"]), pt.label.clone());
            d.eq(&format!("{path}.active"), b(&jt["active"]), pt.active);
            d.eq(&format!("{path}.modified"), b(&jt["modified"]), pt.modified);
            d.eq(&format!("{path}.rect"), rect(&jt["rect"]), prect(&pt.rect));
            d.eq(&format!("{path}.closeRect"), rect(&jt["closeRect"]), prect(&pt.close_rect));
        }
        let marks = list(&j["vscrollMarkers"]);
        d.eq(&format!("{path}.markers.len"), marks.len(), p.markers.len());
        for (k, (jm, pm)) in marks.iter().zip(&p.markers).enumerate() {
            d.eq(&format!("{path}.markers[{k}].row"), u(&jm["row"]), u64::from(pm.row));
            d.eq(&format!("{path}.markers[{k}].color"), opt_s(&jm["color"]), css(Some(pm.color)));
        }
    }

    // Separators.
    let seps = list(&r["separators"]);
    d.eq("separators.len", seps.len(), c.separators.len());
    for (i, (j, p)) in seps.iter().zip(&c.separators).enumerate() {
        d.eq(
            &format!("separators[{i}]"),
            (b(&j["vertical"]), u(&j["x"]), u(&j["y"]), u(&j["len"])),
            (p.vertical, u64::from(p.x), u64::from(p.y), u64::from(p.len)),
        );
    }

    // Palette.
    match (r["palette"].is_null(), &c.palette) {
        (true, None) => {}
        (false, Some(p)) => {
            let j = &r["palette"];
            d.eq("palette.query", s(&j["query"]), p.query.clone());
            d.eq("palette.message", s(&j["message"]), p.message.clone());
            d.eq("palette.promptType", s(&j["promptType"]), p.prompt_type.clone());
            d.eq("palette.overlay", b(&j["overlay"]), p.overlay);
            d.eq("palette.title", s(&j["title"]), p.title.clone());
            d.eq("palette.status", s(&j["status"]), p.status.clone());
            d.eq("palette.selected", opt_u(&j["selected"]), p.selected);
            d.eq("palette.scrollStart", u(&j["scrollStart"]), p.scroll_start);
            d.eq("palette.visibleCount", u(&j["visibleCount"]), p.visible_count);
            d.eq("palette.total", u(&j["total"]), p.total);
            d.eq("palette.outerRect", rect(&j["outerRect"]), prect(&p.outer_rect));
            d.eq("palette.listRect", rect(&j["listRect"]), prect(&p.list_rect));
            let sugg = list(&j["suggestions"]);
            d.eq("palette.suggestions.len", sugg.len(), p.suggestions.len());
            for (i, (js, ps)) in sugg.iter().zip(&p.suggestions).enumerate() {
                let path = format!("palette.suggestions[{i}]");
                d.eq(&format!("{path}.text"), s(&js["text"]), ps.text.clone());
                d.eq(&format!("{path}.description"), opt_s(&js["description"]), ps.description.clone());
                d.eq(&format!("{path}.keybinding"), opt_s(&js["keybinding"]), ps.keybinding.clone());
                d.eq(&format!("{path}.disabled"), b(&js["disabled"]), ps.disabled);
            }
        }
        (json_none, proto) => d.0.push(format!("palette: json null {json_none}, protobuf {}", proto.is_some())),
    }

    // File explorer.
    match (r["fileExplorer"].is_null(), &c.file_explorer) {
        (true, None) => {}
        (false, Some(p)) => {
            let j = &r["fileExplorer"];
            d.eq("fileExplorer.rect", rect(&j["rect"]), prect(&p.rect));
            d.eq("fileExplorer.title", s(&j["title"]), p.title.clone());
            d.eq("fileExplorer.scrollOffset", u(&j["scrollOffset"]), p.scroll_offset);
            d.eq("fileExplorer.viewportHeight", u(&j["viewportHeight"]), p.viewport_height);
            d.eq("fileExplorer.selected", opt_u(&j["selected"]), p.selected);
            let vr: Vec<u64> = list(&j["viewportRows"]).iter().map(u).collect();
            d.eq("fileExplorer.viewportRows", vr, p.viewport_rows.clone());
            let rows: Vec<(String, u64, bool, bool)> = list(&j["rows"])
                .iter()
                .map(|r| (s(&r["name"]), u(&r["depth"]), b(&r["isDir"]), b(&r["expanded"])))
                .collect();
            let prows: Vec<(String, u64, bool, bool)> =
                p.rows.iter().map(|r| (r.name.clone(), r.depth, r.is_dir, r.expanded)).collect();
            d.eq("fileExplorer.rows", rows, prows);
        }
        (json_none, proto) => d.0.push(format!("fileExplorer: json null {json_none}, protobuf {}", proto.is_some())),
    }

    // Settings.
    match (r["settings"].is_null(), &c.settings) {
        (true, None) => {}
        (false, Some(p)) => {
            let j = &r["settings"];
            d.eq("settings.title", s(&j["title"]), p.title.clone());
            d.eq("settings.focus", s(&j["focus"]), p.focus.clone());
            d.eq("settings.targetLayer", s(&j["targetLayer"]), p.target_layer.clone());
            let cats: Vec<(u64, String, bool)> =
                list(&j["categories"]).iter().map(|c| (u(&c["index"]), s(&c["name"]), b(&c["selected"]))).collect();
            let pcats: Vec<(u64, String, bool)> =
                p.categories.iter().map(|c| (c.index, c.name.clone(), c.selected)).collect();
            d.eq("settings.categories", cats, pcats);
            let items = list(&j["items"]);
            d.eq("settings.items.len", items.len(), p.items.len());
            for (i, (ji, pi)) in items.iter().zip(&p.items).enumerate() {
                let path = format!("settings.items[{i}]");
                d.eq(&format!("{path}.path"), s(&ji["path"]), pi.path.clone());
                d.eq(&format!("{path}.name"), s(&ji["name"]), pi.name.clone());
                d.eq(&format!("{path}.description"), opt_s(&ji["description"]), pi.description.clone());
                d.eq(&format!("{path}.modified"), b(&ji["modified"]), pi.modified);
                d.eq(&format!("{path}.selected"), b(&ji["selected"]), pi.selected);
                use pb::setting_control::Control;
                let control = pi.control.as_ref().and_then(|c| c.control.as_ref());
                let jc = &ji["control"];
                match (s(&jc["kind"]).as_str(), control) {
                    ("toggle", Some(Control::Toggle(t))) => d.eq(&path, b(&jc["checked"]), t.checked),
                    ("number", Some(Control::Number(n))) => {
                        d.eq(&format!("{path}.value"), jc["value"].as_f64(), Some(n.value));
                        d.eq(&format!("{path}.min"), jc["min"].as_f64(), n.min);
                        d.eq(&format!("{path}.max"), jc["max"].as_f64(), n.max);
                    }
                    ("dropdown", Some(Control::Dropdown(x))) => {
                        d.eq(&format!("{path}.selected"), u(&jc["selected"]), x.selected);
                        let opts: Vec<String> = list(&jc["options"]).iter().map(s).collect();
                        d.eq(&format!("{path}.options"), opts, x.options.clone());
                    }
                    ("text", Some(Control::Text(x))) => d.eq(&path, s(&jc["value"]), x.value.clone()),
                    ("textList", Some(Control::TextList(x))) => {
                        let items: Vec<String> = list(&jc["items"]).iter().map(s).collect();
                        d.eq(&path, items, x.items.clone());
                    }
                    ("dualList", Some(Control::DualList(x))) => {
                        let inc: Vec<String> = list(&jc["included"]).iter().map(s).collect();
                        d.eq(&path, inc, x.included.clone());
                    }
                    ("map", Some(Control::Map(x))) => {
                        let entries: Vec<(String, String)> =
                            list(&jc["entries"]).iter().map(|e| (s(&e["key"]), s(&e["display"]))).collect();
                        let pentries: Vec<(String, String)> =
                            x.entries.iter().map(|e| (e.key.clone(), e.display.clone())).collect();
                        d.eq(&path, entries, pentries);
                    }
                    ("objectArray", Some(Control::ObjectArray(x))) => {
                        let entries: Vec<String> = list(&jc["entries"]).iter().map(s).collect();
                        d.eq(&path, entries, x.entries.clone());
                    }
                    ("json", Some(Control::Json(x))) => d.eq(&path, s(&jc["value"]), x.value.clone()),
                    ("complex", Some(Control::Complex(x))) => d.eq(&path, s(&jc["typeName"]), x.type_name.clone()),
                    (kind, control) => d.0.push(format!("{path}: json control {kind}, protobuf {control:?}")),
                }
            }
        }
        (json_none, proto) => d.0.push(format!("settings: json null {json_none}, protobuf {}", proto.is_some())),
    }

    // The rest, by presence and their main fields.
    let present = |k: &str| !r[k].is_null();
    d.eq("trustDialog", present("trustDialog"), c.trust_dialog.is_some());
    d.eq("contextMenu", present("contextMenu"), c.context_menu.is_some());
    if let Some(p) = &c.context_menu {
        let items: Vec<String> = list(&r["contextMenu"]["items"]).iter().map(s).collect();
        d.eq("contextMenu.items", items, p.items.clone());
    }
    d.eq("keybindingEditor", present("keybindingEditor"), c.keybinding_editor.is_some());
    let popups = list(&r["popups"]);
    d.eq("popups.len", popups.len(), c.popups.len());
    for (i, (j, p)) in popups.iter().zip(&c.popups).enumerate() {
        d.eq(&format!("popups[{i}].kind"), s(&j["kind"]), p.kind.clone());
        d.eq(&format!("popups[{i}].rect"), rect(&j["rect"]), prect(&p.rect));
    }
    d.0
}
