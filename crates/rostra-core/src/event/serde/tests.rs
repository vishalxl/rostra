use tracing::info;

use crate::event::{Event, EventAuxKey, EventKind, EventSignature, SignedEvent};
use crate::id::RostraId;
use crate::{MsgLen, ShortEventId, TimestampFixed};

#[test_log::test]
fn event_size() {
    let event = Event {
        version: 0,
        flags: 0,
        kind: EventKind::RAW,
        content_len: MsgLen(4),
        timestamp: TimestampFixed::from(1_735_000_000),
        key_aux: EventAuxKey::ZERO,
        author: RostraId::from_bytes([1; 32]),
        parent_prev: ShortEventId::from_bytes([2; 16]).into(),
        parent_aux: ShortEventId::from_bytes([3; 16]).into(),
        content_hash: blake3::hash(b"test").into(),
    };
    let event_signed = SignedEvent::unverified(event, EventSignature::from_bytes([4; 64]));

    let event_signed_serialized = serde_json::to_string(&event_signed).expect("Can't fail");

    info!(%event_signed_serialized, "event_signed_serialized");

    let event_signed_deserialized: SignedEvent =
        serde_json::from_str(&event_signed_serialized).expect("Can't fail");

    assert_eq!(event_signed, event_signed_deserialized);
}
