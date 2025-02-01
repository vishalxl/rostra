use std::collections::HashMap;

use bincode::{Decode, Encode};
use rostra_core::event::{content_kind, EventExt as _};
use rostra_core::{ExternalEventId, RostraId, ShortEventId, Timestamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::Database;
use crate::event::EventContentState;
use crate::{
    events, events_content, social_posts, social_posts_by_time, social_posts_reply, LOG_TARGET,
};

#[derive(
    Encode, Decode, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct EventPaginationCursor {
    ts: Timestamp,
    event_id: ShortEventId,
}

pub struct SocialPostRecord<C> {
    pub ts: Timestamp,
    pub event_id: ShortEventId,
    pub author: RostraId,
    pub reply_to: Option<ExternalEventId>,
    pub content: C,
    pub reply_count: u64,
}

impl Database {
    pub async fn paginate_social_posts_rev(
        &self,
        upper_bound: Option<EventPaginationCursor>,
        limit: usize,
    ) -> Vec<SocialPostRecord<content_kind::SocialPost>> {
        let upper_bound = upper_bound
            .map(|b| (b.ts, b.event_id))
            .unwrap_or((Timestamp::MAX, ShortEventId::MAX));
        self.read_with(|tx| {
            let events_table = tx.open_table(&events::TABLE)?;
            let social_posts_table = tx.open_table(&social_posts::TABLE)?;
            let social_posts_by_time_table = tx.open_table(&social_posts_by_time::TABLE)?;
            let events_content_table = tx.open_table(&events_content::TABLE)?;

            let mut ret = vec![];

            for event in social_posts_by_time_table
                .range(&(Timestamp::ZERO, ShortEventId::ZERO)..&upper_bound)?
                .rev()
            {
                let (k, _) = event?;
                let (ts, event_id) = k.value();


                let Some(content_state) =
                    Database::get_event_content_tx(event_id, &events_content_table)?
                else {
                    warn!(target: LOG_TARGET, %event_id, "Missing content for a post with social_post_record?!");
                    continue;
                };
                let EventContentState::Present(content) = content_state else {
                    continue;
                };

                let Ok(social_post) = content.deserialize_cbor::<content_kind::SocialPost>() else {
                    debug!(target: LOG_TARGET, %event_id, "Content invalid");
                    continue;
                };

                let Some(event) = Database::get_event_tx(event_id, &events_table)? else {
                    warn!(target: LOG_TARGET, %event_id, "Missing event for a post with social_post_record?!");
                    continue;
                };

                let social_post_record =
                    Database::get_social_post_tx(event_id, &social_posts_table)?.unwrap_or_default()
                ;

                ret.push(SocialPostRecord {
                    ts,
                    author: event.author(),
                    event_id,
                    reply_count: social_post_record.reply_count,
                    reply_to: social_post.reply_to,
                    content: social_post,
                });

                if limit <= ret.len() {
                    break;
                }
            }

            Ok(ret)
        })
        .await
        .expect("Storage error")
    }

    pub async fn get_posts_by_id(
        &self,
        post_ids: impl Iterator<Item = ShortEventId>,
    ) -> HashMap<ShortEventId, SocialPostRecord<content_kind::SocialPost>> {
        self.read_with(|tx| {
            let events_table = tx.open_table(&events::TABLE)?;
            let social_posts_table = tx.open_table(&social_posts::TABLE)?;
            let events_content_table = tx.open_table(&events_content::TABLE)?;

            let mut ret = HashMap::new();

            for event_id in post_ids {
                let Some(content_state) =
                    Database::get_event_content_tx(event_id, &events_content_table)?
                else {
                    warn!(target: LOG_TARGET, %event_id, "Missing content for a post with social_post_record?!");
                    continue;
                };
                let EventContentState::Present(content) = content_state else {
                    continue;
                };

                let Ok(social_post) = content.deserialize_cbor::<content_kind::SocialPost>() else {
                    debug!(target: LOG_TARGET, %event_id, "Content invalid");
                    continue;
                };

                let Some(event) = Database::get_event_tx(event_id, &events_table)? else {
                    warn!(target: LOG_TARGET, %event_id, "Missing event for a post with social_post_record?!");
                    continue;
                };

                let social_post_record =
                    Database::get_social_post_tx(event_id, &social_posts_table)?.unwrap_or_default();

                ret.insert(event_id, SocialPostRecord {
                    ts: event.timestamp(),
                    author: event.author(),
                    event_id,
                    reply_count: social_post_record.reply_count,
                    reply_to: social_post.reply_to,
                    content: social_post,
                });
            }

            Ok(ret)
        })
        .await
        .expect("Storage error")
    }

    pub async fn paginate_social_post_comments_rev(
        &self,
        post_event_id: ShortEventId,
        upper_bound: Option<EventPaginationCursor>,
        limit: usize,
    ) -> Vec<SocialPostRecord<content_kind::SocialPost>> {
        let upper_bound = upper_bound
            .map(|b| (post_event_id, b.ts, b.event_id))
            .unwrap_or((post_event_id, Timestamp::MAX, ShortEventId::MAX));
        self.read_with(|tx| {
            let events_table = tx.open_table(&events::TABLE)?;
            let social_posts_tbl = tx.open_table(&social_posts::TABLE)?;
            let social_post_reply_tbl = tx.open_table(&social_posts_reply::TABLE)?;
            let events_content_table = tx.open_table(&events_content::TABLE)?;

            let mut ret = vec![];

            for event in social_post_reply_tbl
                .range(&(post_event_id, Timestamp::ZERO, ShortEventId::ZERO)..&upper_bound)?
                .rev()
            {
                let (k, _) = event?;
                let (_, ts, event_id) = k.value();


                let social_post_record = Database::get_social_post_tx(event_id, &social_posts_tbl)?.unwrap_or_default();

                let Some(content_state) =
                    Database::get_event_content_tx(event_id, &events_content_table)?
                else {
                    warn!(target: LOG_TARGET, %event_id, "Missing content for a post with social_post_record?!");
                    continue;
                };
                let EventContentState::Present(content) = content_state else {
                    debug!(target: LOG_TARGET, %event_id, "Skipping comment without content present");
                    continue;
                };

                let Ok(social_post) = content.deserialize_cbor::<content_kind::SocialPost>() else {
                    debug!(target: LOG_TARGET, %event_id, "Skpping comment with invalid content");
                    continue;
                };

                let Some(event) = Database::get_event_tx(event_id, &events_table)? else {
                    warn!(target: LOG_TARGET, %event_id, "Missing event for a post with social_post_record?!");
                    continue;
                };

                ret.push(SocialPostRecord {
                    ts,
                    author: event.author(),
                    event_id,
                    reply_to: social_post.reply_to,
                    reply_count: social_post_record.reply_count,
                    content: social_post,
                });

                if limit <= ret.len() {
                    break;
                }
            }

            Ok(ret)
        })
        .await
        .expect("Storage error")
    }
}
