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
        link_target_bytes: None,
        link_target_preview: None,
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

    span.link_target_bytes = Some(5000);
    span.link_target_preview = Some("docs/café.md".into());
    bytes.clear();
    span.encode(&mut bytes);
    let deferred = pb::ComposeSpan::decode(bytes.as_slice()).unwrap();
    assert!(deferred.link_target.is_none());
    assert_eq!(deferred.link_target_bytes, Some(5000));
    assert_eq!(
        deferred.link_target_preview.as_deref(),
        Some("docs/café.md")
    );
    assert_eq!(span.encoded_len(), bytes.len());
}
