use rostra_core::id::RostraId;
use rostra_core::{ExternalEventId, ShortEventId};
use scraper::{Html, Selector};

use super::{RoMode, UiState};

fn control(document: &Html, selector: &str, label: &str) {
    let control = document
        .select(&Selector::parse(selector).unwrap())
        .next()
        .unwrap_or_else(|| panic!("missing control {selector}"));
    assert_eq!(control.value().attr("title"), Some(label));
    assert_eq!(control.value().attr("aria-label"), Some(label));
}

fn shortcut_submit(document: &Html, button_selector: &str, title: &str, textarea_selector: &str) {
    let button = document
        .select(&Selector::parse(button_selector).unwrap())
        .next()
        .unwrap_or_else(|| panic!("missing button {button_selector}"));
    assert_eq!(button.value().attr("title"), Some(title));

    let textarea = document
        .select(&Selector::parse(textarea_selector).unwrap())
        .next()
        .unwrap_or_else(|| panic!("missing textarea {textarea_selector}"));
    assert!(textarea.value().attr("x-on:keyup.enter.ctrl").is_some());
}

#[test]
fn post_composer_icon_controls_have_tooltips_and_accessible_names() {
    let self_id = RostraId::from_bytes([42; 32]);
    let inline_reply = Html::parse_fragment(
        &UiState::render_inline_reply_form(
            ExternalEventId::new(self_id, ShortEventId::from_bytes([43; 16])),
            ShortEventId::from_bytes([44; 16]),
            self_id,
            RoMode::Rw,
        )
        .into_string(),
    );
    for (selector, label) in [
        (".m-inlineReply__helpButton", "Formatting help"),
        (".m-inlineReply__emojiButton", "Insert emoji"),
        (".m-inlineReply__attachButton", "Attach media"),
        (".m-inlineReply__cancelButton", "Cancel"),
    ] {
        control(&inline_reply, selector, label);
    }
    shortcut_submit(
        &inline_reply,
        ".m-inlineReply__previewButton",
        "Preview reply (Ctrl+Enter)",
        ".m-inlineReply__content",
    );

    let news_post = Html::parse_fragment(
        &UiState::news_post_form_inner(RoMode::Rw, Some(self_id), false).into_string(),
    );
    for (selector, label) in [
        (".m-newPostForm__helpButton", "Formatting help"),
        (".m-newPostForm__emojiButton", "Insert emoji"),
    ] {
        control(&news_post, selector, label);
    }
    shortcut_submit(
        &news_post,
        ".m-newPostForm__previewButton",
        "Preview post (Ctrl+Enter)",
        ".m-newPostForm__content",
    );

    let new_post = Html::parse_fragment(
        &UiState::new_post_form_inner(RoMode::Rw, Some(self_id), false).into_string(),
    );
    for (selector, label) in [
        (".m-newPostForm__helpButton", "Formatting help"),
        (".m-newPostForm__emojiButton", "Insert emoji"),
        (".m-newPostForm__attachButton", "Attach media"),
    ] {
        control(&new_post, selector, label);
    }
    shortcut_submit(
        &new_post,
        ".m-newPostForm__previewButton",
        "Preview post (Ctrl+Enter)",
        ".m-newPostForm__content",
    );
}
