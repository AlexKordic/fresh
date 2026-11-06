use super::*;
use crate::view::popup::{Popup, PopupListItem, PopupPosition};
use prost::Message;

#[path = "../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

fn fixture() -> (Editor, tempfile::TempDir, crate::view::scene::ScenePopup) {
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
        std::sync::Arc::new(crate::model::filesystem::StdFileSystem),
        None,
        None,
        false,
        false,
        false,
    )
    .unwrap();
    editor.active_window_mut().focused_view_mut().view_mode = crate::state::ViewMode::PageView;
    let theme = crate::view::theme::Theme::load_builtin(crate::view::theme::THEME_DARK).unwrap();
    let mut popup = Popup::list(vec![PopupListItem::new("one", "First".into())], &theme);
    popup.position = PopupPosition::BelowCursor;
    popup.focused = true;
    editor.active_state_mut().popups.show(popup);
    let _ = super::super::scene_value(&mut editor, 80, 24);
    let popup = editor.popups_view().remove(0);
    (editor, root, popup)
}

fn pointer(popup: &crate::view::scene::ScenePopup, kind: pb::MouseKind) -> Vec<u8> {
    pb::PopupPointer {
        instance: popup.instance,
        version: popup.version,
        column: i32::from(popup.content_rect.x - popup.rect.x) + 1,
        row: i32::from(popup.content_rect.y - popup.rect.y),
        mouse: Some(pb::Mouse {
            kind: kind as i32,
            n: 1,
            count: 1,
            ..Default::default()
        }),
    }
    .encode_to_vec()
}

#[test]
fn popup_client_requires_negotiation_and_an_issued_target_before_delivering_a_press() {
    let (mut editor, _root, popup) = fixture();
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut client = SceneClient::new(1, stream);
    let payload = pointer(&popup, pb::MouseKind::Down);
    assert!(client.apply_popup_pointer(&payload, &mut editor).is_err());
    assert!(client.popup_capture.is_none());
    client.native_popup_projection = true;
    assert!(!client.apply_popup_pointer(&payload, &mut editor).unwrap());
    assert!(client.popup_capture.is_none());
    client.popup_targets.insert((popup.instance, popup.version));
    assert!(client.apply_popup_pointer(&payload, &mut editor).unwrap());
    assert_eq!(
        client.popup_capture.as_ref().map(|held| held.instance),
        Some(popup.instance)
    );
    editor.active_state_mut().popups.clear();
    assert!(!client
        .apply_popup_pointer(&pointer(&popup, pb::MouseKind::Up), &mut editor)
        .unwrap());
    assert!(client.popup_capture.is_none());
}

#[test]
fn scoped_popup_client_returns_captured_source_and_cancels_stale_input_without_document_fallback() {
    let (mut editor, _root, popup) = fixture();
    let owner = popup.source_anchor.as_ref().unwrap();
    let scope = pb::ComposeScope {
        window_id: owner.window_id,
        leaf: owner.leaf as u64,
        buffer: owner.buffer as u64,
        buffer_revision: owner.buffer_revision,
        ..Default::default()
    };
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut client = SceneClient::new(1, stream);
    client.greeted = true;
    client.rich_compose = true;
    client.native_popup_projection = true;
    client.popup_targets.insert((popup.instance, popup.version));
    let mut compose = super::super::compose::ComposeCache::default();
    let mut assets = super::super::compose_assets::Service::default();
    let mut out = BridgePoll::default();
    let input = pb::ComposeInput {
        id: 1,
        scope: Some(scope),
        buffer_epoch: owner.buffer_epoch as u64,
        kind: kind::POPUP_POINTER as u32,
        payload: pointer(&popup, pb::MouseKind::Down),
        ..Default::default()
    };
    client
        .handle(
            kind::COMPOSE_INPUT,
            &input.encode_to_vec(),
            &mut editor,
            &mut out,
            &mut compose,
            &mut assets,
        )
        .unwrap();
    let reply = client.input_reply.as_ref().unwrap();
    assert!(reply.accepted && !reply.cancelled);
    let result = reply.result.as_ref().unwrap();
    assert_eq!(
        (result.window, result.leaf, result.buffer, result.epoch),
        (
            owner.window_id,
            owner.leaf,
            owner.buffer,
            owner.buffer_epoch
        )
    );
    assert!(out.frame_owed && out.mutated);
    client.input_reply = None; // The frame carrying this reply has been sent.
    let cursor = editor.active_cursors().primary().position;
    editor.active_state_mut().popups.clear();
    let release = pb::ComposeInput {
        id: 2,
        payload: pointer(&popup, pb::MouseKind::Up),
        ..input
    };
    client
        .handle(
            kind::COMPOSE_INPUT,
            &release.encode_to_vec(),
            &mut editor,
            &mut out,
            &mut compose,
            &mut assets,
        )
        .unwrap();
    let reply = client.input_reply.as_ref().unwrap();
    assert!(reply.cancelled && !reply.accepted);
    assert_eq!(client.last_input, 2);
    assert_eq!(editor.active_cursors().primary().position, cursor);
}
