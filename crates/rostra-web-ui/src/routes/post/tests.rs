use std::collections::BTreeSet;

use rostra_client_db::social::SocialPostRecord;
use rostra_client_db::{EventContentAvailability, QuotaPruneReason};
use rostra_core::event::SocialPost;
use rostra_core::id::RostraId;
use rostra_core::{ExternalEventId, ShortEventId, Timestamp};

use super::{
    UnavailablePostContent, fetch_post_response, find_own_reaction, is_own_reaction,
    post_title_markup, present_post_content_markup, requested_author_matches_event,
    unavailable_post_content_markup,
};

#[test]
fn retained_event_cannot_be_rendered_as_another_author() {
    let requested_author = RostraId::from_bytes([42; 32]);
    let actual_author = RostraId::from_bytes([43; 32]);

    assert!(requested_author_matches_event(requested_author, None));
    assert!(requested_author_matches_event(
        requested_author,
        Some(requested_author)
    ));
    assert!(!requested_author_matches_event(
        requested_author,
        Some(actual_author)
    ));
}

#[test]
fn own_reaction_to_an_older_post_version_remains_deletable_by_precise_id() {
    let current_user = RostraId::from_bytes([42; 32]);
    let post_author = RostraId::from_bytes([43; 32]);
    let older_post_version = ExternalEventId::new(post_author, ShortEventId::from_bytes([44; 16]));
    let reaction_event_id = ShortEventId::from_bytes([45; 16]);
    let reaction = SocialPostRecord {
        ts: Timestamp::ZERO,
        event_id: reaction_event_id,
        author: current_user,
        reply_to: Some(older_post_version),
        content: SocialPost::new("❤️".to_owned(), Some(older_post_version), BTreeSet::new()),
        reply_count: 0,
    };

    assert!(is_own_reaction(&reaction, current_user, reaction_event_id));
    assert!(
        find_own_reaction(
            std::slice::from_ref(&reaction),
            current_user,
            reaction_event_id
        )
        .is_some(),
        "selection from the displayed replacement chain must accept the older target"
    );
    assert!(!is_own_reaction(
        &reaction,
        RostraId::from_bytes([46; 32]),
        reaction_event_id
    ));
    assert!(!is_own_reaction(
        &reaction,
        current_user,
        ShortEventId::from_bytes([47; 16])
    ));

    let reply = SocialPostRecord {
        content: SocialPost::new_text(
            "not a reaction".to_owned(),
            Some(older_post_version),
            BTreeSet::new(),
        ),
        ..reaction
    };
    assert!(!is_own_reaction(&reply, current_user, reaction_event_id));
}

#[test]
fn unavailable_content_distinguishes_fetchable_and_terminal_states() {
    let cases = [
        (
            None,
            UnavailablePostContent::NotYetFetched,
            true,
            "has not been fetched yet",
        ),
        (
            Some(EventContentAvailability::Missing),
            UnavailablePostContent::NotYetFetched,
            true,
            "has not been fetched yet",
        ),
        (
            Some(EventContentAvailability::Pruned {
                quota_reason: Some(QuotaPruneReason::GlobalQuota),
            }),
            UnavailablePostContent::QuotaPruned(QuotaPruneReason::GlobalQuota),
            false,
            "configured storage limit",
        ),
        (
            Some(EventContentAvailability::Deleted),
            UnavailablePostContent::Deleted,
            false,
            "deleted by its author",
        ),
        (
            Some(EventContentAvailability::Invalid),
            UnavailablePostContent::Invalid,
            false,
            "invalid",
        ),
    ];

    for (availability, expected, fetchable, message) in cases {
        let unavailable = UnavailablePostContent::from_availability(availability);
        assert_eq!(unavailable, expected);
        assert_eq!(unavailable.can_fetch(), fetchable);
        assert!(
            unavailable_post_content_markup(Some("content"), maud::Markup::default(), unavailable)
                .into_string()
                .contains(message)
        );
    }
}

#[test]
fn news_title_markup_keeps_external_link_behavior() {
    let url = url::Url::parse("https://example.com/story").unwrap();
    let markup = post_title_markup(Some(&url), Some("Story title")).into_string();

    assert!(markup.starts_with("<h1 class=\"m-postView__linkHeader\">"));
    assert!(markup.contains(
        "<a href=\"https://example.com/story\" target=\"_blank\" rel=\"noopener noreferrer\">Story title</a>"
    ));

    let fallback = post_title_markup(Some(&url), None).into_string();
    assert!(fallback.contains(">https://example.com/story</a>"));
}

#[test]
fn news_title_is_inside_the_replaceable_post_content_container() {
    let url = url::Url::parse("https://example.com/story").unwrap();
    let markup = present_post_content_markup(
        Some("post-content-thread-event"),
        maud::html! {
            (post_title_markup(Some(&url), Some("Story title")))
            p { "Story summary" }
        },
    )
    .into_string();

    let container = markup.find("id=\"post-content-thread-event\"").unwrap();
    let title = markup
        .find("<h1 class=\"m-postView__linkHeader\">")
        .unwrap();
    let summary = markup.find("<p>Story summary</p>").unwrap();

    assert!(container < title);
    assert!(title < summary);
    assert!(markup.ends_with("<p>Story summary</p></div>"));

    let unavailable = unavailable_post_content_markup(
        Some("post-content-thread-event"),
        post_title_markup(Some(&url), Some("Story title")),
        UnavailablePostContent::NotYetFetched,
    )
    .into_string();
    let container = unavailable
        .find("id=\"post-content-thread-event\"")
        .unwrap();
    let title = unavailable
        .find("<h1 class=\"m-postView__linkHeader\">")
        .unwrap();
    let message = unavailable
        .find("Post content has not been fetched yet.")
        .unwrap();

    assert!(container < title);
    assert!(title < message);
}

#[tokio::test]
async fn ordinary_fetch_redirects_and_enhanced_fetch_replaces_complete_content() {
    let author = RostraId::from_bytes([42; 32]);
    let event_id = ShortEventId::from_bytes([43; 16]);
    let response = fetch_post_response(
        false,
        author,
        event_id,
        "content",
        Some(maud::html! {
            (post_title_markup(None, Some("Story title")))
            p { "successfully fetched" }
        }),
        UnavailablePostContent::NotYetFetched,
    );

    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap(),
        &super::post_url(author, event_id)
    );
    let enhanced = fetch_post_response(
        true,
        author,
        event_id,
        "content",
        Some(maud::html! {
            (post_title_markup(None, Some("Story title")))
            p { "successfully fetched" }
        }),
        UnavailablePostContent::NotYetFetched,
    );
    assert_eq!(enhanced.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(enhanced.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let container = body.find("id=\"content\"").unwrap();
    let title = body.find(">Story title</h1>").unwrap();
    let content = body.find("<p>successfully fetched</p>").unwrap();

    assert!(container < title);
    assert!(title < content);
    assert!(body.ends_with("<p>successfully fetched</p></div>"));
}
