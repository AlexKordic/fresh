//! Independent protobuf decoding of popup ownership annotations.

#[path = "../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

use super::*;
use prost::Message as ProstMessage;

#[test]
fn popup_source_owner_and_duplicate_label_row_ids_survive_the_wire() {
    let view = ScenePopup {
        grid: None,
        instance: 41,
        version: 42,
        kind: "completion",
        title: Some("Suggestions".into()),
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
        scroll_offset: 1,
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
        content: PopupContentView::List {
            selected: 1,
            items: ["first", "second"]
                .into_iter()
                .map(|id| PopupItemView {
                    id: id.into(),
                    text: "same label".into(),
                    detail: None,
                    icon: None,
                    disabled: false,
                })
                .collect(),
        },
    };
    let mut bytes = Vec::new();
    view.encode(&mut bytes);
    let decoded = pb::ScenePopup::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.instance, 41);
    assert_eq!(decoded.version, 42);
    assert_eq!(decoded.rect.as_ref().unwrap().y, 3);
    assert_eq!(decoded.content_rect.as_ref().unwrap().y, 4);
    assert_eq!(decoded.scroll_offset, 1);
    let anchor = decoded.source_anchor.unwrap();
    assert_eq!((anchor.window_id, anchor.leaf, anchor.buffer), (4, 5, 6));
    assert_eq!((anchor.buffer_epoch, anchor.buffer_revision), (7, 8));
    assert_eq!(
        (anchor.caret, anchor.horizontal, anchor.placement),
        (30, 25, 2)
    );
    let Some(pb::popup_content::Content::List(list)) = decoded.content.unwrap().content else {
        panic!("list payload");
    };
    assert_eq!(list.selected, 1);
    assert_eq!(
        list.items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(list.items[0].text, list.items[1].text);
    // This byte fixture is also decoded by File Commander's SceneWireTests.
    let fixture = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(fixture, concat!(
        "0a0a636f6d706c6574696f6e120b53756767657374696f6e7322080802100318142008",
        "2a08080310041812200630013a2f0a2d0a130a0a73616d65206c6162656c2a056669727374",
        "0a140a0a73616d65206c6162656c2a067365636f6e6410014029482a521008041005180620072808301e38194002",
    ));
}
