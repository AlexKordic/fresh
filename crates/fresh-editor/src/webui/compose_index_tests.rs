use super::*;
use crate::webui::compose::semantics::Entry;
use crate::webui::wire::Message as WireMessage;
use prost::Message;

#[path = "../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

fn owner() -> Owner {
    Owner {
        window: 11,
        leaf: 2,
        buffer: 3,
        epoch: 4,
        revision: 5,
        authority: 6,
        document: 7,
        length: 8_000_000,
    }
}

fn session(entries: Vec<Entry>) -> Session {
    let mut session = Session::default();
    session.handle(Request {
        id: 1,
        owner: Some(owner()),
        cancel: false,
        applied: None,
    });
    session.active.as_mut().unwrap().inventory = Some(Arc::new(Inventory {
        label_bytes: entries.iter().map(|e| e.label.len()).sum(),
        entries,
        error: None,
    }));
    session
}

fn next(session: &mut Session) -> Option<Chunk> {
    Session::next_chunk(session.active.as_mut()?)
}

fn ack(session: &mut Session, sequence: u64) {
    session.handle(Request {
        id: 1,
        owner: None,
        cancel: false,
        applied: Some(sequence),
    });
}

#[test]
fn chunked_unicode_names_roundtrip_in_full_and_require_an_independent_ack() {
    let label = "e\u{301}東京👩🏽‍💻".repeat(10000) + " tail";
    let mut session = session(vec![Entry {
        start: 5,
        end: 7_000_000,
        kind: 1,
        label: label.clone(),
    }]);
    let mut rebuilt = String::new();
    let mut count = 0;
    loop {
        let chunk = next(&mut session).unwrap();
        assert_eq!(chunk.sequence, count);
        assert_eq!(chunk.total_entries, 1);
        assert_eq!(chunk.label_bytes, label.len());
        let mut bytes = Vec::new();
        chunk.encode(&mut bytes);
        assert!(bytes.len() <= MAX_PAYLOAD);
        assert_eq!(chunk.encoded_len(), bytes.len());
        let decoded = pb::ComposeIndexChunk::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.owner.as_ref().unwrap().document, 7);
        for part in decoded.parts {
            assert_eq!(part.index, 0);
            assert_eq!((part.start, part.end, part.kind), (5, 7_000_000, 1));
            assert_eq!(part.label_offset as usize, rebuilt.len());
            assert!(part.label.len() <= PART_BYTES);
            assert_eq!(part.label_length as usize, label.len());
            rebuilt.push_str(&part.label);
            assert_eq!(part.label_done, rebuilt.len() == label.len());
        }
        assert!(
            next(&mut session).is_none(),
            "render-frame Applied must not release index pacing"
        );
        ack(&mut session, count);
        count += 1;
        if chunk.done {
            break;
        }
    }
    assert!(count > 1);
    assert_eq!(rebuilt.as_bytes(), label.as_bytes());
    assert!(
        session.active.is_none(),
        "a final acknowledgment releases the snapshot"
    );
}

#[test]
fn many_tiny_entries_have_bounded_payloads_and_empty_documents_finish() {
    let mut session = session(
        (0..2000)
            .map(|n| Entry {
                start: n * 2,
                end: n * 2 + 1,
                kind: 16,
                label: "L".into(),
            })
            .collect(),
    );
    let mut received = 0;
    while let Some(chunk) = next(&mut session) {
        assert!(chunk.parts.len() <= PARTS);
        let mut bytes = Vec::new();
        chunk.encode(&mut bytes);
        assert!(bytes.len() <= MAX_PAYLOAD);
        for part in &chunk.parts {
            assert_eq!(part.index, received);
            received += 1;
        }
        ack(&mut session, chunk.sequence);
    }
    assert_eq!(received, 2000);
    let mut empty = self::session(Vec::new());
    let chunk = next(&mut empty).unwrap();
    assert!(chunk.done && chunk.parts.is_empty() && chunk.error.is_empty());
    assert_eq!(
        (chunk.total_entries, chunk.label_bytes, chunk.sequence),
        (0, 0, 0)
    );
    ack(&mut empty, 0);
    assert!(empty.active.is_none());
}

#[test]
fn retirement_can_overtake_one_waiting_chunk_and_replay_cannot_revive_a_snapshot() {
    let mut session = session(vec![Entry {
        start: 1,
        end: 5,
        kind: 17,
        label: "x".repeat(100_000),
    }]);
    assert!(!next(&mut session).unwrap().done);
    session.active.as_mut().unwrap().error = Some("source changed".into());
    let retired = next(&mut session).unwrap();
    assert_eq!(retired.sequence, 1);
    assert!(retired.done);
    assert_eq!(retired.error, "source changed");
    assert!(retired.parts.is_empty());
    assert!(session.active.as_ref().unwrap().inventory.is_none());
    ack(&mut session, 0);
    assert!(next(&mut session).is_none());
    ack(&mut session, 1);
    assert!(session.active.is_none());
    session.handle(Request {
        id: 1,
        owner: Some(owner()),
        cancel: false,
        applied: None,
    });
    assert!(session.active.is_none());
    session.handle(Request {
        id: 2,
        owner: Some(owner()),
        cancel: false,
        applied: None,
    });
    session.handle(Request {
        id: 1,
        owner: None,
        cancel: true,
        applied: None,
    });
    assert_eq!(session.active.as_ref().unwrap().id, 2);
    session.handle(Request {
        id: 2,
        owner: None,
        cancel: true,
        applied: None,
    });
    assert!(session.active.is_none());
}

#[test]
fn requests_preserve_ack_zero_presence_and_reject_ambiguous_or_mistyped_fields() {
    let request = Request::parse(
        &pb::ComposeIndexRequest {
            id: 9,
            applied_sequence: Some(0),
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .unwrap();
    assert_eq!(request.applied, Some(0));
    assert!(request.owner.is_none());
    let bytes = pb::ComposeIndexRequest {
        id: 9,
        owner: Some(pb::ComposeIndexOwner {
            window: 11,
            leaf: 2,
            buffer: 3,
            epoch: 4,
            revision: 5,
            authority: 6,
            document: 7,
            length: 8_000_000,
        }),
        ..Default::default()
    }
    .encode_to_vec();
    assert_eq!(Request::parse(&bytes).unwrap().owner, Some(owner()));
    for bytes in [
        vec![],
        vec![8, 1],
        vec![8, 1, 24, 2],
        vec![8, 1, 24, 1, 32, 0],
        vec![8, 1, 8, 1, 32, 0],
        vec![8, 1, 34, 0],
        vec![8, 1, 18, 2, 32, 0],
        vec![8, 1, 18, 4, 32, 1, 32, 1],
    ] {
        assert!(Request::parse(&bytes).is_err(), "{bytes:?}");
    }
}
