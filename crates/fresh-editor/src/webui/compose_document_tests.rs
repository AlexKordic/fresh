use super::*;
use prost::Message as ProstMessage;

#[path = "../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

fn owner() -> pb::ComposeIndexOwner {
    pb::ComposeIndexOwner {
        window: 1,
        leaf: 2,
        buffer: 3,
        epoch: 4,
        revision: 5,
        authority: 6,
        document: 7,
        length: 1 << 20,
    }
}
fn subscribe(session: &mut Session, id: u64) {
    session.handle(
        Request::parse(
            &pb::ComposeDocumentRequest {
                id,
                owner: Some(owner()),
                selection_revision: Some(0),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .unwrap(),
    );
}
fn ack(session: &mut Session, id: u64, sequence: u64) {
    session.handle(
        Request::parse(
            &pb::ComposeDocumentRequest {
                id,
                applied_sequence: Some(sequence),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .unwrap(),
    );
}
fn next(session: &mut Session) -> Option<Chunk> {
    Session::next_chunk(session.read.as_mut()?)
}

#[test]
fn binary_chunks_roundtrip_and_render_applied_cannot_release_snapshot_pacing() {
    let mut session = Session::default();
    subscribe(&mut session, 1);
    let bytes: Vec<_> = (0..(CHUNK * 3 + 17)).map(|n| (n % 256) as u8).collect();
    let read = session.read.as_mut().unwrap();
    read.length = bytes.len();
    read.blob = Some(bytes.clone());
    let mut received = Vec::new();
    let mut sequence = 0;
    while let Some(chunk) = next(&mut session) {
        assert_eq!(chunk.selection, 0);
        assert_eq!(chunk.sequence, sequence);
        assert_eq!(chunk.length, bytes.len());
        let mut encoded = Vec::new();
        chunk.encode(&mut encoded);
        assert!(encoded.len() <= MAX_PAYLOAD);
        let decoded = pb::ComposeDocumentChunk::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.owner, Some(owner()));
        assert!(decoded.data.len() <= CHUNK);
        received.extend_from_slice(&decoded.data);
        assert_eq!(decoded.done, received.len() == bytes.len());
        assert!(
            next(&mut session).is_none(),
            "only a kind-32 acknowledgment advances this stream"
        );
        ack(&mut session, 1, sequence);
        sequence += 1;
    }
    assert_eq!(received, bytes);
    assert_eq!(sequence, 4);
    assert!(session.read.is_none());
}

#[test]
fn snapshot_budget_is_shared_and_cancellation_or_final_chunk_releases_retained_storage() {
    let budget = Budget::default();
    let mut a = Session::new(budget.clone());
    let mut b = Session::new(budget.clone());
    let mut c = Session::new(budget.clone());
    subscribe(&mut a, 1);
    subscribe(&mut b, 1);
    subscribe(&mut c, 1);
    let busy = next(&mut c).unwrap();
    assert!(busy.done && busy.data.is_empty() && busy.error.contains("busy"));
    assert_eq!(budget.0.load(Ordering::Relaxed), 2);
    a.handle(
        Request::parse(
            &pb::ComposeDocumentRequest {
                id: 1,
                cancel: true,
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .unwrap(),
    );
    subscribe(&mut c, 2);
    assert!(c.read.as_ref().unwrap().error.is_none());
    let read = c.read.as_mut().unwrap();
    read.length = 3;
    read.blob = Some(vec![1, 2, 3]);
    assert!(next(&mut c).unwrap().done);
    assert!(c.read.as_ref().unwrap().blob.is_none());
    assert_eq!(
        budget.0.load(Ordering::Relaxed),
        1,
        "the last copied chunk no longer retains the encoded snapshot"
    );
    drop(b);
    assert_eq!(budget.0.load(Ordering::Relaxed), 0);

    // A stalled formatter or reader cannot reserve either shared permit
    // indefinitely. Expiration discards the whole value and releases storage.
    subscribe(&mut a, 2);
    a.expire(a.read.as_ref().unwrap().started + PREPARE_TIMEOUT);
    assert_eq!(budget.0.load(Ordering::Relaxed), 0);
    assert!(next(&mut a).unwrap().error.contains("formatting timed out"));
    subscribe(&mut a, 3);
    let read = a.read.as_mut().unwrap();
    read.length = CHUNK * 2;
    read.blob = Some(vec![1; CHUNK * 2]);
    assert!(!next(&mut a).unwrap().done);
    a.expire(a.read.as_ref().unwrap().waiting.unwrap().1 + ACK_TIMEOUT);
    assert_eq!(budget.0.load(Ordering::Relaxed), 0);
    let terminal = next(&mut a).unwrap();
    assert!(terminal.done && terminal.data.is_empty() && terminal.error.contains("acknowledgment"));
}

#[test]
fn changed_owner_can_retire_one_unacknowledged_chunk_and_stale_ids_do_not_revive_it() {
    let mut session = Session::default();
    subscribe(&mut session, 1);
    let read = session.read.as_mut().unwrap();
    read.length = CHUNK * 2;
    read.blob = Some(vec![1; CHUNK * 2]);
    assert!(!next(&mut session).unwrap().done);
    session.read.as_mut().unwrap().fail("source changed".into());
    let terminal = next(&mut session).unwrap();
    assert!(terminal.done);
    assert_eq!(terminal.sequence, 1);
    assert_eq!(terminal.error, "source changed");
    assert_eq!(terminal.length, 0);
    assert!(terminal.data.is_empty());
    ack(&mut session, 1, 0);
    assert!(next(&mut session).is_none());
    ack(&mut session, 1, 1);
    assert!(session.read.is_none());
    subscribe(&mut session, 1);
    assert!(session.read.is_none());
    subscribe(&mut session, 2);
    assert!(session.read.as_ref().unwrap().error.is_none());
}

#[test]
fn source_admission_and_request_ownership_are_explicit_without_changing_source() {
    let mut session = Session::default();
    let mut huge = owner();
    huge.length = MAX_SOURCE as u64 + 1;
    session.handle(
        Request::parse(
            &pb::ComposeDocumentRequest {
                id: 1,
                owner: Some(huge),
                selection_revision: Some(0),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .unwrap(),
    );
    let rejected = next(&mut session).unwrap();
    assert!(rejected.done && rejected.data.is_empty());
    assert!(rejected.error.contains("source remains editable"));
    for request in [
        pb::ComposeDocumentRequest {
            id: 1,
            owner: Some(owner()),
            ..Default::default()
        },
        pb::ComposeDocumentRequest {
            id: 1,
            selection_revision: Some(0),
            ..Default::default()
        },
        pb::ComposeDocumentRequest {
            id: 1,
            cancel: true,
            selection_revision: Some(0),
            ..Default::default()
        },
    ] {
        assert!(Request::parse(&request.encode_to_vec()).is_err());
    }
    let ack = pb::ComposeDocumentRequest {
        id: 1,
        applied_sequence: Some(0),
        ..Default::default()
    }
    .encode_to_vec();
    assert_eq!(ack, vec![8, 1, 32, 0]);
    assert!(Request::parse(&ack).is_ok());
    let mut duplicate = pb::ComposeDocumentRequest {
        id: 1,
        owner: Some(owner()),
        selection_revision: Some(0),
        ..Default::default()
    }
    .encode_to_vec();
    duplicate.extend_from_slice(&[40, 0]);
    assert!(Request::parse(&duplicate).is_err());
}
