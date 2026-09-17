use std::collections::BTreeSet;

use rostra_core::event::PersonaTag;
use rostra_core::event::content_kind::PersonasTagsSelector;
use scraper::{Html, Selector};

use super::{follow_form_selector, prepare_follow_dialog_personas};
use crate::routes::fragment;

fn tags(values: &[&str]) -> BTreeSet<PersonaTag> {
    values
        .iter()
        .map(|value| PersonaTag::new(*value).unwrap())
        .collect()
}

fn rendered_persona_tags(
    available_tags: &BTreeSet<PersonaTag>,
    selected_tags: &BTreeSet<PersonaTag>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let document = Html::parse_fragment(
        &fragment::persona_tag_select("personas")
            .available_tags(available_tags)
            .selected_tags(selected_tags)
            .id("follow-persona-tags")
            .empty_label("none")
            .call()
            .into_string(),
    );
    let selector = Selector::parse(r#"input[name="personas"]"#).unwrap();
    let inputs = document.select(&selector);
    let available = inputs
        .clone()
        .map(|input| input.value().attr("value").unwrap().to_owned())
        .collect();
    let selected = inputs
        .filter(|input| input.value().attr("checked").is_some())
        .map(|input| input.value().attr("value").unwrap().to_owned())
        .collect();

    (available, selected)
}

#[test]
fn selected_custom_tags_render_and_round_trip_for_each_follow_type() {
    for (selector, expected_follow_type, removed_selector) in [
        (
            PersonasTagsSelector::Only {
                ids: tags(&["futuretag"]),
            },
            "follow_only",
            PersonasTagsSelector::Only {
                ids: BTreeSet::new(),
            },
        ),
        (
            PersonasTagsSelector::Except {
                ids: tags(&["futuretag"]),
            },
            "follow_all",
            PersonasTagsSelector::Except {
                ids: BTreeSet::new(),
            },
        ),
    ] {
        let mut available_tags: BTreeSet<_> = PersonaTag::defaults().into_iter().collect();
        let (follow_type, selected_tags) =
            prepare_follow_dialog_personas(Some(&selector), &mut available_tags);

        let (rendered_tags, checked_tags) = rendered_persona_tags(&available_tags, &selected_tags);
        assert_eq!(follow_type, expected_follow_type);
        assert!(rendered_tags.contains("futuretag"));
        assert_eq!(checked_tags, BTreeSet::from(["futuretag".to_owned()]));
        assert_eq!(
            follow_form_selector(follow_type, &checked_tags.into_iter().collect::<Vec<_>>()),
            selector
        );
        assert_eq!(follow_form_selector(follow_type, &[]), removed_selector);
    }
}

#[test]
fn selected_tags_merge_with_defaults_without_duplicates_and_can_be_removed() {
    let selector = PersonasTagsSelector::Only {
        ids: tags(&["personal", "discovered", "futuretag"]),
    };
    let mut available_tags = tags(&["discovered"]);
    available_tags.extend(PersonaTag::defaults());
    let (follow_type, selected_tags) =
        prepare_follow_dialog_personas(Some(&selector), &mut available_tags);

    let (rendered_tags, checked_tags) = rendered_persona_tags(&available_tags, &selected_tags);
    assert_eq!(
        rendered_tags,
        BTreeSet::from([
            "civic".to_owned(),
            "discovered".to_owned(),
            "futuretag".to_owned(),
            "personal".to_owned(),
            "professional".to_owned(),
        ])
    );
    assert_eq!(
        checked_tags,
        BTreeSet::from([
            "discovered".to_owned(),
            "futuretag".to_owned(),
            "personal".to_owned(),
        ])
    );
    let mut unchanged_submission: Vec<_> = checked_tags.iter().cloned().collect();
    unchanged_submission.push("futuretag".to_owned());
    assert_eq!(
        follow_form_selector(follow_type, &unchanged_submission),
        selector
    );
    assert_eq!(
        follow_form_selector(
            follow_type,
            &checked_tags
                .into_iter()
                .filter(|tag| tag != "futuretag")
                .collect::<Vec<_>>()
        ),
        PersonasTagsSelector::Only {
            ids: tags(&["personal", "discovered"]),
        }
    );
}

#[test]
fn new_follow_defaults_to_all_with_no_selected_personas() {
    let mut available_tags = tags(&["discovered"]);
    available_tags.extend(PersonaTag::defaults());
    let (follow_type, selected_tags) = prepare_follow_dialog_personas(None, &mut available_tags);

    let (_, checked_tags) = rendered_persona_tags(&available_tags, &selected_tags);
    assert_eq!(follow_type, "follow_all");
    assert!(checked_tags.is_empty());
    assert_eq!(
        follow_form_selector(follow_type, &[]),
        PersonasTagsSelector::Except {
            ids: BTreeSet::new(),
        }
    );
}
