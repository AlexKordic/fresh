use super::Message;
use crate::webui::compose::ComposeSpan;
use prost::Message as ProstMessage;

#[path = "../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

#[test]
fn link_destination_wire_preserves_unicode_and_distinguishes_an_empty_target_from_absence() {
    let mut span = ComposeSpan {
        start: 7,
        end: 20,
        role: 0,
        flags: 20,
        content_start: None,
        link_target: Some("docs/café.md".into()),
    };
    let mut bytes = Vec::new();
    span.encode(&mut bytes);
    assert_eq!(
        pb::ComposeSpan::decode(bytes.as_slice())
            .unwrap()
            .link_target
            .as_deref(),
        Some("docs/café.md")
    );
    // Protoc and FC's SceneWireTests independently check this fixture.
    assert_eq!(
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "080710142014320d646f63732f636166c3a92e6d64"
    );
    span.link_target = Some("".into());
    bytes.clear();
    span.encode(&mut bytes);
    assert_eq!(
        pb::ComposeSpan::decode(bytes.as_slice())
            .unwrap()
            .link_target
            .as_deref(),
        Some("")
    );
    span.link_target = None;
    bytes.clear();
    span.encode(&mut bytes);
    assert!(pb::ComposeSpan::decode(bytes.as_slice())
        .unwrap()
        .link_target
        .is_none());
}
