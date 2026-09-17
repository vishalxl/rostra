use rostra_core::event::content_kind::{self, EventContentKind as _};
use rostra_core::event::{Event, EventExt as _, EventKind, VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraIdSecretKey, ToShort as _};
use rostra_core::{ExternalEventId, ShortEventId, Timestamp};
use rostra_util_error::BoxedErrorResult;

use crate::social::{EventPaginationCursor, SocialPostIndexStream, paginate_social_post_index_rev};
use crate::tables::{SocialPostsReactionsRecord, SocialPostsRepliesRecord};
use crate::{Database, DbError, social_posts_reactions, social_posts_replies};

fn social_post(
    secret: RostraIdSecretKey,
    timestamp: i64,
    parent_prev: Option<rostra_core::EventId>,
    replaced: Option<rostra_core::EventId>,
    content: content_kind::SocialPost,
) -> VerifiedEventContent {
    let content = content
        .serialize_cbor()
        .expect("social post must serialize");
    let event = Event::builder_raw_content()
        .author(secret.id())
        .kind(EventKind::SOCIAL_POST)
        .timestamp(time::OffsetDateTime::from_unix_timestamp(timestamp).expect("valid timestamp"))
        .content(&content)
        .maybe_parent_prev(parent_prev.map(Into::into))
        .maybe_delete(replaced.map(Into::into))
        .build()
        .signed_by(secret);
    let event = VerifiedEvent::verify_signed(secret.id(), event).expect("event must verify");
    VerifiedEventContent::assume_verified(event, content)
}

fn stream(entries: &[(u64, u8)]) -> SocialPostIndexStream<'static> {
    let entries = entries
        .iter()
        .map(|(ts, id)| Ok((Timestamp::from(*ts), ShortEventId::from_bytes([*id; 16]))))
        .collect::<Vec<Result<_, DbError>>>();
    Box::new(entries.into_iter())
}

#[test]
fn metadata_merge_stops_loading_when_page_is_full() -> BoxedErrorResult<()> {
    let streams = vec![
        stream(&[(9, 1), (6, 1), (3, 1)]),
        stream(&[(8, 2), (5, 2), (2, 2)]),
        stream(&[(7, 3), (4, 3), (1, 3)]),
    ];
    let mut loads = 0;
    let records = paginate_social_post_index_rev(streams, 2, |ts, event_id| {
        loads += 1;
        Ok(Some((ts, event_id)))
    })?;

    assert_eq!(loads, 2);
    assert_eq!(
        records,
        vec![
            (Timestamp::from(9), ShortEventId::from_bytes([1; 16])),
            (Timestamp::from(8), ShortEventId::from_bytes([2; 16])),
        ]
    );
    Ok(())
}

#[test]
fn metadata_merge_skips_unreadable_candidates_and_zero_limit_loads_none() -> BoxedErrorResult<()> {
    let mut loads = Vec::new();
    let records = paginate_social_post_index_rev(
        vec![stream(&[(9, 1), (7, 1)]), stream(&[(8, 2), (6, 2)])],
        2,
        |ts, event_id| {
            loads.push((ts, event_id));
            Ok((ts != Timestamp::from(9) && ts != Timestamp::from(7)).then_some((ts, event_id)))
        },
    )?;
    assert_eq!(records.len(), 2);
    assert_eq!(
        loads.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
        vec![
            Timestamp::from(9),
            Timestamp::from(8),
            Timestamp::from(7),
            Timestamp::from(6)
        ]
    );

    let mut zero_limit_loads = 0;
    let records = paginate_social_post_index_rev(vec![stream(&[(9, 1)])], 0, |_, _| {
        zero_limit_loads += 1;
        Ok(Some(()))
    })?;
    assert!(records.is_empty());
    assert_eq!(zero_limit_loads, 0);
    Ok(())
}

async fn assert_index_pagination(reaction: bool) -> BoxedErrorResult<()> {
    let parent_secret = RostraIdSecretKey::from_bytes([41; 32]);
    let parent_author = parent_secret.id();
    let db = Database::new_in_memory(parent_author).await?;
    let original = social_post(
        parent_secret,
        10,
        None,
        None,
        content_kind::SocialPost::new_text("original".to_owned(), None, Default::default()),
    );
    let edited = social_post(
        parent_secret,
        20,
        Some(original.event_id()),
        Some(original.event_id()),
        content_kind::SocialPost::new_text("edited".to_owned(), None, Default::default()),
    );
    db.try_process_event_with_content(&original).await?;
    db.try_process_event_with_content(&edited).await?;

    let targets = [original.event_id(), edited.event_id()];
    let timestamps = [40, 60, 50, 50];
    let mut expected = Vec::new();
    for (index, timestamp) in timestamps.into_iter().enumerate() {
        let secret = RostraIdSecretKey::from_bytes([50 + index as u8; 32]);
        let reply_to = ExternalEventId::new(parent_author, targets[index % targets.len()]);
        let content = if reaction {
            content_kind::SocialPost::new("👍".to_owned(), Some(reply_to), Default::default())
        } else {
            content_kind::SocialPost::new_text(
                format!("comment {index}"),
                Some(reply_to),
                Default::default(),
            )
        };
        let post = social_post(secret, timestamp, None, None, content);
        expected.push((post.timestamp(), post.event_id().to_short()));
        db.try_process_event_with_content(&post).await?;
    }
    expected.sort_by_key(|key| std::cmp::Reverse(*key));

    let missing_newest = ShortEventId::from_bytes([0xfe; 16]);
    let missing_middle = ShortEventId::from_bytes([0xfd; 16]);
    let original_id = original.event_id().to_short();
    let edited_id = edited.event_id().to_short();
    db.write_with(|tx| {
        if reaction {
            let mut table = tx.open_table(&social_posts_reactions::TABLE)?;
            table.insert(
                &(edited_id, Timestamp::from(70), missing_newest),
                &SocialPostsReactionsRecord,
            )?;
            table.insert(
                &(original_id, Timestamp::from(55), missing_middle),
                &SocialPostsReactionsRecord,
            )?;
        } else {
            let mut table = tx.open_table(&social_posts_replies::TABLE)?;
            table.insert(
                &(edited_id, Timestamp::from(70), missing_newest),
                &SocialPostsRepliesRecord,
            )?;
            table.insert(
                &(original_id, Timestamp::from(55), missing_middle),
                &SocialPostsRepliesRecord,
            )?;
        }
        Ok(())
    })
    .await?;

    let first = if reaction {
        db.paginate_social_post_reactions_rev(edited_id, None, 2)
            .await
    } else {
        db.paginate_social_post_comments_rev(edited_id, None, 2)
            .await
    };
    assert_eq!(
        first
            .0
            .iter()
            .map(|record| (record.ts, record.event_id))
            .collect::<Vec<_>>(),
        expected[..2]
    );
    assert_eq!(
        first.1,
        Some(EventPaginationCursor {
            ts: expected[1].0,
            event_id: expected[1].1,
        })
    );

    let second = if reaction {
        db.paginate_social_post_reactions_rev(edited_id, first.1, 10)
            .await
    } else {
        db.paginate_social_post_comments_rev(edited_id, first.1, 10)
            .await
    };
    assert_eq!(
        second
            .0
            .iter()
            .map(|record| (record.ts, record.event_id))
            .collect::<Vec<_>>(),
        expected[2..]
    );
    assert_eq!(
        second.1,
        Some(EventPaginationCursor {
            ts: expected.last().expect("records").0,
            event_id: expected.last().expect("records").1,
        })
    );

    let exhausted = if reaction {
        db.paginate_social_post_reactions_rev(edited_id, second.1, 10)
            .await
    } else {
        db.paginate_social_post_comments_rev(edited_id, second.1, 10)
            .await
    };
    assert!(exhausted.0.is_empty());
    assert_eq!(exhausted.1, None);

    let zero_limit = if reaction {
        db.paginate_social_post_reactions_rev(edited_id, None, 0)
            .await
    } else {
        db.paginate_social_post_comments_rev(edited_id, None, 0)
            .await
    };
    assert!(zero_limit.0.is_empty());
    assert_eq!(zero_limit.1, None);

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn comment_pages_merge_historical_versions_with_exclusive_cursors() -> BoxedErrorResult<()> {
    assert_index_pagination(false).await
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn reaction_pages_merge_historical_versions_with_exclusive_cursors() -> BoxedErrorResult<()> {
    assert_index_pagination(true).await
}
