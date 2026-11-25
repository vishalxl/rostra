use super::{button, timeline_destination_group};
use crate::routes::timeline::PendingCounts;

#[test]
fn shared_button_preserves_established_sizing_contract() {
    let stylesheet = include_str!("../../../assets/style.css");
    let shared_button_rule = stylesheet
        .split_once(".u-button {")
        .expect("shared button rule should exist")
        .1
        .split_once('}')
        .expect("shared button rule should be closed")
        .0;

    assert!(shared_button_rule.contains("min-width: 5rem;"));
    assert!(shared_button_rule.contains("max-width: 140px;"));
    assert!(shared_button_rule.contains("padding: 0.0rem .5rem;"));
    assert!(!shared_button_rule.contains("white-space"));

    let create_account_rule = stylesheet
        .split_once(".o-unlockScreen__generateButton {")
        .expect("create-account button rule should exist")
        .1
        .split_once('}')
        .expect("create-account button rule should be closed")
        .0;
    assert!(create_account_rule.contains("white-space: nowrap;"));
    assert!(create_account_rule.contains("max-width: unset;"));
}

#[test]
fn shared_button_supports_ordinary_form_actions_and_existing_icons() {
    let markup = button("m-example__submitButton", "Submit")
        .name("action")
        .value("cancel")
        .formaction("/post/link_dialog")
        .formnovalidate(true)
        .disabled_expression("loading")
        .icon_class("o-mediaList__closeButtonIcon")
        .call()
        .into_string();
    assert!(markup.contains("name=\"action\""));
    assert!(markup.contains("value=\"cancel\""));
    assert!(markup.contains("formaction=\"/post/link_dialog\""));
    assert!(markup.contains("formnovalidate"));
    assert!(markup.contains(":disabled=\"loading\""));
    assert!(markup.contains("class=\"o-mediaList__closeButtonIcon u-buttonIcon\""));
}

#[test]
fn javascript_requirement_preserves_button_icon_hook() {
    let markup = button("m-example__copyButton", "Copy")
        .button_type("button")
        .aria_label("Copy secret")
        .requires_js(true)
        .call()
        .into_string();
    let button_tag = markup
        .split_once('>')
        .expect("button should have an opening tag")
        .0;

    assert!(button_tag.contains("m-example__copyButton"));
    assert!(button_tag.contains("u-button"));
    assert!(button_tag.contains("u-requiresJs"));
    assert!(markup.contains("aria-label=\"Copy secret\""));
    assert!(markup.contains("class=\"m-example__copyButtonIcon u-buttonIcon\""));
}

#[test]
fn destination_group_renders_both_layouts_with_live_badges() {
    let counts = PendingCounts {
        followees: 2,
        network: 0,
        notifications: 120,
        shoutbox: 1,
        messages: 3,
    };
    let markup =
        timeline_destination_group("notifications", counts, true, Some("/profile/feed.atom"))
            .into_string();

    assert!(markup.contains("class=\"o-appNavGroup\""));
    assert!(markup.contains(
        "x-data=\"badgeCounts({ followees: 2, network: 0, notifications: 120, shoutbox: 1, messages: 3 })\""
    ));
    assert!(markup.contains("@badges:updated.window=\"onUpdate($event.detail)\""));
    assert!(markup.contains("class=\"o-appNav -desktop\""));
    assert!(markup.contains("class=\"o-appNavDisclosure\""));
    assert!(markup.contains("aria-current=\"page\""));
    assert!(markup.contains("href=\"/following\""));
    assert!(markup.contains("href=\"/profile/feed.atom\""));
    assert!(markup.contains("-pending"));
}

#[test]
fn destination_group_omits_live_state_and_feed_when_private() {
    let markup =
        timeline_destination_group("", PendingCounts::default(), false, None).into_string();

    assert!(markup.contains("class=\"o-appNavGroup\""));
    assert!(!markup.contains("x-data"));
    assert!(!markup.contains("@badges:updated.window"));
    assert!(!markup.contains("aria-current"));
    assert!(!markup.contains("href=\"/profile/feed.atom\""));
}
