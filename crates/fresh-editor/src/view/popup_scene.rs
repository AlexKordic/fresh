//! Explicit source anchors for frontends that measure Compose in pixels.
//!
//! Cell rectangles remain the retained UI's geometry. These annotations allow a
//! frontend to place a source-relative popup without deriving placement from its
//! kind, a rounded terminal caret, or a neighbouring pane's cursor.

use super::ScenePopup;
use crate::app::Editor;
use crate::state::ViewMode;
use crate::view::popup::{Popup, PopupPosition};
use serde::Serialize;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PopupSourceAnchor {
    pub window_id: u64,
    pub leaf: usize,
    pub buffer: usize,
    pub buffer_epoch: usize,
    pub buffer_revision: u64,
    /// The caret's source byte supplies the vertical anchor and line height.
    pub caret: usize,
    /// The source byte supplying x; a completion uses its word's start.
    pub horizontal: usize,
    /// 1: over, 2: below (flip above if needed), 3: above. All clamp to frame.
    pub placement: u32,
}

impl Editor {
    pub(super) fn popup_presentation(&self, index: usize) -> &[fresh_ui::Item] {
        self.shell_ui.as_ref().map_or(&[], |ui| {
            ui.spec()
                .items_for(&crate::view::shell::popup::popup_key(index))
        })
    }

    pub(super) fn popup_source_anchor(&self, popup: &Popup) -> Option<PopupSourceAnchor> {
        let placement = match popup.position {
            PopupPosition::AtCursor => 1,
            PopupPosition::BelowCursor => 2,
            PopupPosition::AboveCursor => 3,
            _ => return None,
        };
        let window = self.active_window();
        let leaf = window.effective_active_split();
        let buffer = window.pane_buffer(leaf)?;
        let (_, views) = window.buffers.splits()?;
        let view = views.get(&leaf)?.buffer_state(buffer)?;
        if view.view_mode != ViewMode::PageView {
            return None;
        }
        let state = window.buffer_state(buffer)?;
        let caret = view.cursors.primary().position;
        let horizontal = match crate::view::shell::popup::CaretAnchor::for_kind(popup.kind) {
            crate::view::shell::popup::CaretAnchor::Caret => caret,
            crate::view::shell::popup::CaretAnchor::CompletionWord => {
                crate::primitives::word_navigation::find_completion_word_start(&state.buffer, caret)
            }
        };
        Some(PopupSourceAnchor {
            window_id: window.id.0,
            leaf: leaf.0 .0,
            buffer: buffer.0,
            buffer_epoch: state.marker_list.instance_id(),
            buffer_revision: state.buffer.version(),
            caret,
            horizontal,
            placement,
        })
    }
}

/// Event-time fingerprint, not a security credential. A new lifetime, owner,
/// placement, source, content, scroll or keyboard policy invalidates old input.
/// `version` itself is excluded so refreshing a descriptor stays idempotent.
pub(super) fn version(popup: &ScenePopup, model: &Popup, presentation: &[fresh_ui::Item]) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    popup.instance.hash(&mut hash);
    popup.kind.hash(&mut hash);
    popup.title.hash(&mut hash);
    popup.description.hash(&mut hash);
    popup.rect.hash(&mut hash);
    popup.content_rect.hash(&mut hash);
    popup.scroll_offset.hash(&mut hash);
    popup.content.hash(&mut hash);
    popup.source_anchor.hash(&mut hash);
    model.transient.hash(&mut hash);
    model.focused.hash(&mut hash);
    // Labels can repeat while the underlying actions differ. An in-place
    // payload update must not let a delayed click invoke the new action.
    if let crate::view::popup::PopupContent::List { items, .. } = &model.content {
        for item in items {
            item.id.hash(&mut hash);
            item.data.hash(&mut hash);
        }
    }
    format!("{:?}", model.resolver).hash(&mut hash);
    // Scroll and hover can live in retained components rather than in Popup.
    // Hash their actual arranged picture too: checking only scroll_offset
    // would accept an old row after the component's window moved.
    format!("{presentation:?}").hash(&mut hash);
    hash.finish().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::scene::RectView;
    use crate::view::scene::{PopupContentView, PopupSpanView};

    fn editor() -> (Editor, tempfile::TempDir) {
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.clipboard.use_osc52 = false;
        config.clipboard.use_system_clipboard = false;
        let editor = Editor::for_test(
            config,
            80,
            24,
            Some(root.path().to_owned()),
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
        (editor, root)
    }

    #[test]
    fn source_popup_anchors_keep_bytes_placement_and_completion_word_ownership() {
        let (mut editor, _root) = editor();
        let text = "café greet";
        let cursor = editor.active_cursors().primary_id();
        editor
            .replace_native_texts(vec![(0..0, text.into(), cursor)])
            .unwrap();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        editor.active_cursors_mut().primary_mut().position = text.len();
        let theme =
            crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
        let mut popup = Popup::text(vec!["documentation".into()], &theme);
        for (position, placement) in [
            (PopupPosition::AtCursor, 1),
            (PopupPosition::BelowCursor, 2),
            (PopupPosition::AboveCursor, 3),
        ] {
            popup.position = position;
            let anchor = editor.popup_source_anchor(&popup).unwrap();
            assert_eq!(anchor.placement, placement);
            assert_eq!(anchor.caret, text.len());
            assert_eq!(anchor.horizontal, anchor.caret);
            assert_eq!(anchor.window_id, editor.active_window().id.0);
            assert_eq!(anchor.leaf, editor.effective_active_split().0 .0);
            assert_eq!(anchor.buffer, editor.active_buffer().0);
            assert_eq!(
                anchor.buffer_epoch,
                editor.active_state().marker_list.instance_id()
            );
            assert_eq!(
                anchor.buffer_revision,
                editor.active_state().buffer.version()
            );
        }
        popup.kind = crate::view::popup::PopupKind::Completion;
        let completion = editor.popup_source_anchor(&popup).unwrap();
        assert_eq!(completion.horizontal, text.find("greet").unwrap());
        assert_eq!(completion.caret, text.len());
        for position in [
            PopupPosition::Fixed { x: 3, y: 4 },
            PopupPosition::Centered,
            PopupPosition::CenteredOverlay {
                width_pct: 70,
                height_pct: 50,
            },
            PopupPosition::BottomRight,
            PopupPosition::AboveStatusBarAt(crate::view::ui::status_bar::StatusBarClickable::Lsp),
        ] {
            popup.position = position;
            assert!(editor.popup_source_anchor(&popup).is_none());
        }
        popup.position = PopupPosition::BelowCursor;
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::Source;
        assert!(editor.popup_source_anchor(&popup).is_none());
    }

    #[test]
    fn source_popup_anchor_follows_the_focused_view_of_a_shared_buffer() {
        let (mut editor, _root) = editor();
        let text = "first second";
        let cursor = editor.active_cursors().primary_id();
        editor
            .replace_native_texts(vec![(0..0, text.into(), cursor)])
            .unwrap();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        editor.active_cursors_mut().primary_mut().position = 1;
        let first = editor.effective_active_split();
        editor.split_pane_vertical();
        let second = editor.effective_active_split();
        assert_ne!(second, first);
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        editor.active_cursors_mut().primary_mut().position = text.len();
        let theme =
            crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
        let popup = Popup::text(vec!["documentation".into()], &theme);
        let second_anchor = editor.popup_source_anchor(&popup).unwrap();
        assert_eq!(second_anchor.leaf, second.0 .0);
        assert_eq!(second_anchor.caret, text.len());
        editor.focus_pane(first);
        let first_anchor = editor.popup_source_anchor(&popup).unwrap();
        assert_eq!(first_anchor.leaf, first.0 .0);
        assert_eq!(first_anchor.caret, 1);
        assert_eq!(first_anchor.buffer, second_anchor.buffer);
        assert_eq!(first_anchor.buffer_epoch, second_anchor.buffer_epoch);
    }

    #[cfg(feature = "web")]
    #[test]
    fn a_retained_popup_scroll_invalidates_its_version_without_changing_the_model_offset() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (mut editor, _root) = editor();
        let theme =
            crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
        let mut popup = Popup::text((0..50).map(|i| format!("Line {i}")).collect(), &theme);
        popup.max_height = 8;
        popup.position = PopupPosition::Fixed { x: 2, y: 2 };
        editor.active_state_mut().popups.show(popup);
        let _ = crate::webui::scene_value(&mut editor, 80, 24);
        let before = editor.popups_view().remove(0);
        assert_ne!(before.rect.h, 0);
        let _ = crate::webui::scene_value(&mut editor, 80, 24);
        assert_eq!(
            editor.popups_view()[0].version,
            before.version,
            "an unchanged frame keeps its target"
        );
        editor
            .handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: before.content_rect.x + 1,
                row: before.content_rect.y + 1,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        let _ = crate::webui::scene_value(&mut editor, 80, 24);
        let after = editor.popups_view().remove(0);
        assert_eq!(after.instance, before.instance);
        assert_eq!(
            after.scroll_offset, before.scroll_offset,
            "retained components own the real scroll"
        );
        assert_ne!(
            after.version, before.version,
            "a delayed pointer must not select a new visual row"
        );
    }

    fn popup() -> ScenePopup {
        ScenePopup {
            instance: 1,
            version: 0,
            source_anchor: Some(PopupSourceAnchor {
                window_id: 4,
                leaf: 5,
                buffer: 6,
                buffer_epoch: 7,
                buffer_revision: 8,
                caret: 30,
                horizontal: 25,
                placement: 2,
            }),
            kind: "hover",
            title: Some("Documentation".into()),
            description: None,
            rect: RectView {
                x: 2,
                y: 3,
                w: 20,
                h: 8,
            },
            content_rect: RectView {
                x: 3,
                y: 4,
                w: 18,
                h: 6,
            },
            scroll_offset: 0,
            content: PopupContentView::Lines {
                lines: vec![vec![PopupSpanView::plain("first".into())]],
            },
        }
    }

    #[test]
    fn popup_version_survives_reprojection_but_rejects_changed_owners_and_rows() {
        let theme =
            crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
        let mut model = Popup::text(vec!["first".into()], &theme);
        model.transient = true;
        let original = popup();
        let expected = version(&original, &model, &[]);
        assert_ne!(expected, 0);
        let mut projected = original.clone();
        projected.version = expected;
        assert_eq!(version(&projected, &model, &[]), expected);
        for change in [
            |p: &mut ScenePopup| p.instance += 1,
            |p: &mut ScenePopup| p.scroll_offset += 1,
            |p: &mut ScenePopup| p.rect.y += 1,
            |p: &mut ScenePopup| p.content_rect.h -= 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().window_id += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().buffer += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().buffer_epoch += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().buffer_revision += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().leaf += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().caret += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().horizontal += 1,
            |p: &mut ScenePopup| p.source_anchor.as_mut().unwrap().placement = 3,
            |p: &mut ScenePopup| p.source_anchor = None,
            |p: &mut ScenePopup| p.content = PopupContentView::Lines { lines: vec![] },
        ] {
            let mut changed = original.clone();
            change(&mut changed);
            assert_ne!(version(&changed, &model, &[]), expected);
        }
        model.transient = false;
        assert_ne!(version(&original, &model, &[]), expected);
        model.transient = true;
        model.focused = true;
        assert_ne!(version(&original, &model, &[]), expected);
    }

    #[test]
    fn equally_named_popup_rows_cannot_reuse_a_changed_action_target() {
        use crate::view::popup::{PopupContent, PopupListItem, PopupResolver};
        let theme =
            crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
        let mut model = Popup::list(
            vec![PopupListItem::new("first", "same label".into())],
            &theme,
        );
        let view = popup();
        let original = version(&view, &model, &[]);
        let PopupContent::List { items, .. } = &mut model.content else {
            unreachable!()
        };
        items[0].id = "other".into();
        assert_ne!(version(&view, &model, &[]), original);
        let PopupContent::List { items, .. } = &mut model.content else {
            unreachable!()
        };
        items[0].id = "first".into();
        items[0].data = Some("new payload".into());
        assert_ne!(version(&view, &model, &[]), original);
        let PopupContent::List { items, .. } = &mut model.content else {
            unreachable!()
        };
        items[0].data = None;
        assert_eq!(version(&view, &model, &[]), original);
        model.resolver = PopupResolver::Completion;
        assert_ne!(version(&view, &model, &[]), original);
    }
}
