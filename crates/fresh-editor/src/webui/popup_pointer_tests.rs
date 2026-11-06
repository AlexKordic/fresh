use super::*;
use crate::view::{
    popup::{Popup, PopupListItem, PopupPosition},
    scene::ScenePopup,
};
use prost::Message;
use std::sync::Arc;

#[path = "../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

fn editor() -> (Editor, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.clipboard.use_osc52 = false;
    config.clipboard.use_system_clipboard = false;
    let mut editor = Editor::for_test(
        config,
        80,
        24,
        Some(root.path().into()),
        crate::config_io::DirectoryContext::for_testing(root.path()),
        crate::view::color_support::ColorCapability::TrueColor,
        Arc::new(crate::model::filesystem::StdFileSystem),
        None,
        None,
        false,
        false,
        false,
    )
    .unwrap();
    let cursor = editor.active_cursors().primary_id();
    editor
        .replace_native_texts(vec![(
            0..0,
            "# Heading\n\nSource under the popup\n\nalp".into(),
            cursor,
        )])
        .unwrap();
    editor.active_window_mut().focused_view_mut().view_mode = crate::state::ViewMode::PageView;
    (editor, root)
}

fn frame(editor: &mut Editor) -> pb::Frame {
    let mut probe = super::super::WireProbe::new(editor);
    let (_, bytes) = probe.next(editor, 80, 24).unwrap();
    pb::ServerHello::decode(bytes.as_slice())
        .unwrap()
        .scene
        .unwrap()
}

fn show(editor: &mut Editor) -> ScenePopup {
    let theme = crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
    let mut items: Vec<_> = (0..30)
        .map(|i| PopupListItem::new(format!("row-{i}"), format!("ROW-{i}")))
        .collect();
    items[0].disabled = true;
    let mut popup = Popup::list(items, &theme);
    popup.position = PopupPosition::BelowCursor;
    popup.width = 25;
    popup.max_height = 7;
    editor.active_state_mut().popups.show(popup);
    let _ = frame(editor);
    editor.popups_view().remove(0)
}

fn request(popup: &ScenePopup, kind: pb::MouseKind, column: i32, row: i32) -> Request {
    let request = pb::PopupPointer {
        instance: popup.instance,
        version: popup.version,
        column,
        row,
        mouse: Some(pb::Mouse {
            kind: kind as i32,
            n: 1,
            count: 1,
            ..Default::default()
        }),
    };
    Request::parse(&request.encode_to_vec()).unwrap()
}

#[test]
fn native_scene_keeps_the_underlay_and_carries_the_retained_popup_separately() {
    let (mut editor, _root) = editor();
    let before = frame(&mut editor);
    let popup = show(&mut editor);
    let after = frame(&mut editor);
    let grid = after.popups.unwrap().popups.remove(0).grid.unwrap();
    assert!(grid.rows.iter().any(|row| row.text.contains("ROW-0")));
    for y in popup.rect.y..popup.rect.y + popup.rect.h {
        let before = before
            .rows
            .iter()
            .find(|row| row.index == u32::from(y))
            .unwrap();
        let after = after
            .rows
            .iter()
            .find(|row| row.index == u32::from(y))
            .unwrap();
        assert_eq!(
            after.text, before.text,
            "the old footprint must retain its underlying row"
        );
    }
    let terminal = super::super::render_tui_cells(&mut editor, 80, 24);
    assert!(
        terminal.contains("ROW-0"),
        "ordinary terminal paint still includes the popup"
    );
}

#[test]
fn projected_popup_click_reuses_disabled_rows_and_confirm_without_moving_the_document_caret() {
    let (mut editor, _root) = editor();
    let popup = show(&mut editor);
    let cursor = editor.active_cursors().primary().position;
    let x = i32::from(popup.content_rect.x - popup.rect.x) + 2;
    let y = i32::from(popup.content_rect.y - popup.rect.y);
    let mut capture = None;
    assert!(request(&popup, pb::MouseKind::Down, x, y).apply(&mut editor, &mut capture));
    assert!(request(&popup, pb::MouseKind::Up, x, y).apply(&mut editor, &mut capture));
    assert!(
        editor.active_state().popups.is_visible(),
        "a disabled row cannot confirm"
    );
    let _ = frame(&mut editor);
    let current = editor.popups_view().remove(0);
    assert!(request(&current, pb::MouseKind::Down, x, y + 1).apply(&mut editor, &mut capture));
    assert!(request(&current, pb::MouseKind::Up, x, y + 1).apply(&mut editor, &mut capture));
    assert!(
        !editor.active_state().popups.is_visible(),
        "the real row handler confirms the foreground list"
    );
    assert_eq!(editor.active_cursors().primary().position, cursor);
    assert!(!request(&current, pb::MouseKind::Up, x, y + 1).apply(&mut editor, &mut capture));
    assert_eq!(editor.active_cursors().primary().position, cursor);
}

#[test]
fn hover_keeps_the_pointer_target_but_component_scroll_rejects_old_rows() {
    let (mut editor, _root) = editor();
    let popup = show(&mut editor);
    let x = i32::from(popup.content_rect.x - popup.rect.x) + 2;
    let y = i32::from(popup.content_rect.y - popup.rect.y) + 1;
    let mut capture = None;
    assert!(request(&popup, pb::MouseKind::Moved, x, y).apply(&mut editor, &mut capture));
    let _ = frame(&mut editor);
    let hovered = editor.popups_view().remove(0);
    assert_eq!(
        hovered.version, popup.version,
        "a click immediately queued after hover must remain usable"
    );
    assert!(request(&hovered, pb::MouseKind::ScrollDown, x, y).apply(&mut editor, &mut capture));
    let _ = frame(&mut editor);
    let scrolled = editor.popups_view().remove(0);
    assert_ne!(scrolled.version, popup.version);
    assert_ne!(
        scrolled.grid.unwrap().cells,
        popup.grid.as_ref().unwrap().cells
    );
    let cursor = editor.active_cursors().primary().position;
    assert!(!request(&popup, pb::MouseKind::Down, x, y).apply(&mut editor, &mut capture));
    assert!(editor.active_state().popups.is_visible());
    assert_eq!(editor.active_cursors().primary().position, cursor);
}

#[test]
fn popup_pointer_parser_rejects_duplicates_wrong_types_and_missing_owners() {
    assert!(Request::parse(&[]).is_err());
    assert!(Request::parse(&[0x08, 1, 0x08, 1]).is_err());
    assert!(Request::parse(&[0x0a, 1, 1]).is_err());
    let (mut editor, _root) = editor();
    let popup = show(&mut editor);
    let parsed = request(&popup, pb::MouseKind::Drag, -3, -4);
    assert_eq!((parsed.column, parsed.row), (-3, -4));
    assert!(
        !parsed.apply(&mut editor, &mut None),
        "a drag without a press cannot acquire a popup"
    );
}

#[test]
fn retiring_a_projected_source_owner_cancels_its_retained_capture_before_any_more_motion() {
    let (mut editor, _root) = editor();
    let theme = crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
    let mut model = Popup::text(vec!["select this documentation".into()], &theme);
    model.focused = true;
    model.position = PopupPosition::BelowCursor;
    editor.active_state_mut().popups.show(model);
    let _ = frame(&mut editor);
    let popup = editor.popups_view().remove(0);
    let x = i32::from(popup.content_rect.x - popup.rect.x) + 2;
    let y = i32::from(popup.content_rect.y - popup.rect.y);
    let mut capture = None;
    assert!(request(&popup, pb::MouseKind::Down, x, y).apply(&mut editor, &mut capture));
    assert!(editor.shell_ui.as_ref().unwrap().captured().is_some());
    let cursor = editor.active_cursors().primary_id();
    editor
        .replace_native_texts(vec![(0..0, "Changed owner revision\n".into(), cursor)])
        .unwrap();
    let current = editor.popups_view();
    reconcile(&mut editor, &current, &mut capture);
    assert!(capture.is_none());
    let ui = editor.shell_ui.as_ref().unwrap();
    assert!(ui.captured().is_none());
    assert!(ui.drag_autoscroll().is_none());
    let caret = editor.active_cursors().primary().position;
    assert!(!request(&popup, pb::MouseKind::Up, x, y).apply(&mut editor, &mut capture));
    assert_eq!(editor.active_cursors().primary().position, caret);
}

#[test]
fn wrapped_popup_text_selection_copies_the_same_rows_that_the_native_picture_displays() {
    let (mut editor, _root) = editor();
    let theme = crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
    let mut model = Popup::text(
        vec!["alpha bravo charlie delta echo foxtrot golf hotel india juliet".into()],
        &theme,
    );
    model.width = 22;
    model.max_height = 5;
    model.focused = true;
    model.position = PopupPosition::BelowCursor;
    editor.active_state_mut().popups.show(model);
    let _ = frame(&mut editor);
    let popup = editor.popups_view().remove(0);
    let grid = &popup.grid.as_ref().unwrap().cells;
    let row_text = |row: u16| {
        (popup.content_rect.x..popup.content_rect.x + popup.content_rect.w)
            .filter_map(|x| grid.cell((x, row)).map(|cell| cell.symbol()))
            .collect::<String>()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    let first = row_text(popup.content_rect.y);
    let second = row_text(popup.content_rect.y + 1);
    assert!(!first.is_empty() && !second.is_empty());
    let x = i32::from(popup.content_rect.x - popup.rect.x);
    let y = i32::from(popup.content_rect.y - popup.rect.y);
    let mut capture = None;
    assert!(request(&popup, pb::MouseKind::Down, x + 6, y).apply(&mut editor, &mut capture));
    editor.native_compose_pointer_allowed();
    let after_press = editor.popups_view().remove(0);
    assert_eq!(after_press.version, popup.version, "press changed target: before rect={:?} inner={:?} anchor={:?}; after rect={:?} inner={:?} anchor={:?}", popup.rect, popup.content_rect, popup.source_anchor, after_press.rect, after_press.content_rect, after_press.source_anchor);
    assert!(request(&popup, pb::MouseKind::Drag, x + 5, y + 1).apply(&mut editor, &mut capture));
    let _ = frame(&mut editor);
    let selected = editor.popups_view().remove(0);
    assert_eq!(
        selected.version, popup.version,
        "selection colors must not retire a captured pointer"
    );
    assert_ne!(
        selected.grid.as_ref().unwrap().cells,
        popup.grid.as_ref().unwrap().cells,
        "the retained picture must show the selection"
    );
    assert!(request(&popup, pb::MouseKind::Up, x + 5, y + 1).apply(&mut editor, &mut capture));
    super::super::apply_step(&mut editor, &serde_json::json!({"key":"c", "ctrl":true}));
    assert_eq!(
        editor.clipboard_text(),
        format!("{} {}", &first[6..], &second[..5]),
        "soft wraps do not insert source newlines into copied text"
    );
}

#[test]
fn styled_popup_selection_keeps_emoji_bytes_and_emphasis_and_link_lookup_uses_logical_bytes() {
    let (mut editor, _root) = editor();
    let theme = crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
    let emoji = "👩🏽‍💻";
    let mut model = Popup::markdown(&format!("a **{emoji}** b e\u{301} 東京 café"), &theme, None);
    model.width = 22;
    model.max_height = 5;
    model.focused = true;
    model.position = PopupPosition::BelowCursor;
    editor.active_state_mut().popups.show(model);
    let _ = frame(&mut editor);
    let popup = editor.popups_view().remove(0);
    let x = i32::from(popup.content_rect.x - popup.rect.x);
    let y = i32::from(popup.content_rect.y - popup.rect.y);
    let mut capture = None;
    assert!(request(&popup, pb::MouseKind::Down, x + 2, y).apply(&mut editor, &mut capture));
    assert!(request(&popup, pb::MouseKind::Drag, x + 4, y).apply(&mut editor, &mut capture));
    let _ = frame(&mut editor);
    let selected = editor.popups_view().remove(0);
    assert_eq!(selected.version, popup.version);
    assert_eq!(
        editor
            .active_state()
            .popups
            .top()
            .unwrap()
            .get_selected_text()
            .as_deref(),
        Some(emoji),
        "logical bytes must match the displayed grapheme before painting its selection"
    );
    let before = popup
        .grid
        .as_ref()
        .unwrap()
        .cells
        .content
        .iter()
        .find(|cell| cell.symbol() == emoji)
        .unwrap();
    let after = selected
        .grid
        .as_ref()
        .unwrap()
        .cells
        .content
        .iter()
        .find(|cell| cell.symbol() == emoji)
        .unwrap();
    assert_ne!(before.style(), after.style());
    assert!(after.modifier.contains(ratatui::style::Modifier::BOLD));
    assert!(request(&selected, pb::MouseKind::Up, x + 4, y).apply(&mut editor, &mut capture));
    super::super::apply_step(&mut editor, &serde_json::json!({"key":"c", "ctrl":true}));
    assert_eq!(editor.clipboard_text(), emoji);

    // No opener is invoked: test the source lookup with a multibyte label.
    let link = Popup::markdown(
        "prefix [café 東京](https://example.invalid/target) tail",
        &theme,
        None,
    );
    let crate::view::popup::PopupContent::Markdown(lines) = &link.content else {
        unreachable!()
    };
    let line = lines
        .iter()
        .position(|line| line.plain_text().contains("café"))
        .unwrap();
    let plain = lines[line].plain_text();
    let byte = plain.find("café").unwrap();
    assert_eq!(
        link.link_at_source_position(line, byte).as_deref(),
        Some("https://example.invalid/target")
    );
    assert!(link.link_at_source_position(line, byte - 1).is_none());
    assert!(link
        .link_at_source_position(line, byte + "café 東京".len())
        .is_none());
}
