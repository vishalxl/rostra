use rostra_client_db::{Database, PayloadIngestOutcome};
use rostra_core::Timestamp;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};
use rostra_core::event::{Event, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::RostraIdSecretKey;

use crate::acquired_payload::AcquiredPayload;

fn content(author: RostraIdSecretKey) -> VerifiedEventContent {
    let raw = SocialPost::new_text("payload".to_owned(), None, Default::default())
        .serialize_cbor()
        .unwrap();
    let event = Event::builder_raw_content()
        .author(author.id())
        .timestamp(Timestamp::from(1).to_offset_date_time().unwrap())
        .kind(EventKind::SOCIAL_POST)
        .content(&raw)
        .build()
        .signed_by(author);
    VerifiedEventContent::verify(
        VerifiedEvent::verify_signed(author.id(), event).unwrap(),
        raw,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_with_outcome_preserves_duplicate_transaction_result() -> anyhow::Result<()> {
    let author = RostraIdSecretKey::generate();
    let db = Database::new_in_memory(author.id()).await?;
    let content = content(author);
    db.try_process_event(&content.event).await?;

    assert_eq!(
        AcquiredPayload {
            content: content.clone(),
            buffer: None,
        }
        .ingest_with_outcome(&db)
        .await?,
        PayloadIngestOutcome::Processed
    );
    assert_eq!(
        AcquiredPayload {
            content,
            buffer: None,
        }
        .ingest_with_outcome(&db)
        .await?,
        PayloadIngestOutcome::Unchanged
    );
    Ok(())
}
