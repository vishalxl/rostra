//! Disposable ordinary-HTTP DM workflows. Run locally in a loopback-only
//! network namespace; SelfCI runs the suite in the Nix build sandbox.

mod common;

use std::time::Duration;

use common::{TestServer, UiDriver};
use reqwest::{Response, StatusCode, header};
use rostra_core::event::{VerifiedEvent, VerifiedEventContent};
use rostra_core::id::{RostraId, ToShort as _};
use scraper::{Html, Selector};

fn private_headers(response: &Response) {
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, private"
    );
    assert_eq!(response.headers()[header::PRAGMA], "no-cache");
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "identity");
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("script-src 'self' 'unsafe-eval'"));
    assert!(csp.contains("connect-src 'self'"));
}

fn token(page: &str) -> String {
    let document = Html::parse_document(page);
    document
        .select(&Selector::parse("input[name=csrf]").unwrap())
        .next()
        .unwrap()
        .value()
        .attr("value")
        .unwrap()
        .to_owned()
}

async fn settings_token(driver: &UiDriver) -> String {
    let response = driver.get("/settings/messages").await;
    assert_eq!(response.status(), StatusCode::OK);
    private_headers(&response);
    let page = response.text().await.unwrap();
    let document = Html::parse_document(&page);
    if document
        .select(&Selector::parse("input[name=csrf]").unwrap())
        .next()
        .is_some()
    {
        return token(&page);
    }
    let confirmation = document
        .select(&Selector::parse("a[href^='/settings/messages/retire/']").unwrap())
        .next()
        .unwrap()
        .value()
        .attr("href")
        .unwrap();
    let response = driver.get(confirmation).await;
    assert_eq!(response.status(), StatusCode::OK);
    private_headers(&response);
    token(&response.text().await.unwrap())
}

async fn replicate_device(server: &TestServer, source: RostraId, target: RostraId) {
    let source_client = server.client(source).await;
    let event_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let devices = source_client.db().dm_own_devices(None, 64).await.unwrap();
            if let Some((_, event, _)) = devices.iter().find_map(|(_, device)| device.latest()) {
                break event;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    replicate_event(server, source, target, event_id).await;
}

async fn replicate_event(
    server: &TestServer,
    source: RostraId,
    target: RostraId,
    event_id: rostra_core::ShortEventId,
) {
    let source_client = server.client(source).await;
    let target_client = server.client(target).await;
    let record = source_client.db().get_event(event_id).await.unwrap();
    let event = VerifiedEvent::verify_signed(source, record.signed).unwrap();
    let content = source_client
        .db()
        .get_event_content(event_id)
        .await
        .unwrap();
    target_client
        .db()
        .try_process_event_with_content(&VerifiedEventContent::verify(event, content).unwrap())
        .await
        .unwrap();
}

fn assert_conversation_panel_has_no_start_form(document: &Html) {
    let panel = document
        .select(&Selector::parse(".m-directMessages__conversationPanel").unwrap())
        .next()
        .expect("conversation panel");
    assert!(
        panel
            .select(&Selector::parse("form[action='/messages/open']").unwrap())
            .next()
            .is_none(),
        "conversation panel should only list existing conversations"
    );
    assert!(
        panel
            .select(&Selector::parse("a[href='/settings/messages']").unwrap())
            .next()
            .is_none(),
        "device administration belongs in Settings, not the conversation workspace"
    );
    assert!(
        panel
            .select(&Selector::parse("h1").unwrap())
            .next()
            .is_none(),
        "the top-level Messages tab already identifies the workspace"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn private_pages_require_this_sessions_secret_and_protect_all_responses() {
    let server = TestServer::start().await;
    let anonymous = server.driver();
    for path in ["/messages", "/settings/messages", "/messages/not-an-id"] {
        let response = anonymous.get(path).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        private_headers(&response);
        assert!(response.text().await.unwrap().contains("<html"));
    }
    let writer = server.driver();
    let (id, secret) = writer.login_new_identity().await;
    let observer = server.driver();
    observer.login_readonly(id).await;
    // The shared client is fully active because of the other session.
    for path in ["/messages", "/settings/messages"] {
        let response = observer.get(path).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        private_headers(&response);
    }
    let response = writer.get("/messages").await;
    assert_eq!(response.status(), StatusCode::OK);
    private_headers(&response);
    let page = response.text().await.unwrap();
    assert!(!page.contains(&secret.to_string()));
    let page = Html::parse_document(&page);
    let scripts = page
        .select(&Selector::parse("script[src]").unwrap())
        .filter_map(|script| script.value().attr("src"))
        .collect::<Vec<_>>();
    assert_eq!(
        scripts,
        [
            "/assets/libs/alpinejs-persist@3.14.3.js",
            "/assets/libs/alpine-ajax@0.12.6.js",
            "/assets/libs/alpinejs@3.14.3.js"
        ]
    );
    let response = writer.get("/messages/invalid").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    private_headers(&response);
    assert!(response.text().await.unwrap().contains("<html"));
    let response = writer.get("/messages/no/such/page").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    private_headers(&response);
    let csrf = settings_token(&writer).await;
    let before = server
        .client(id)
        .await
        .db()
        .dm_local_installation()
        .await
        .unwrap()
        .unwrap()
        .device_id;
    let response = observer
        .post_form(
            "/settings/messages",
            &[
                ("csrf", csrf.as_str()),
                ("action", "retire"),
                ("device", &data_encoding::HEXLOWER.encode(&before)),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    private_headers(&response);
    let other_writer = server.driver();
    other_writer
        .login_with_secret(&id.to_string(), &secret.to_string())
        .await;
    let other_csrf = settings_token(&other_writer).await;
    assert_ne!(csrf, other_csrf);
    let response = writer
        .post_form(
            "/settings/messages",
            &[
                ("csrf", other_csrf.as_str()),
                ("action", "retire"),
                ("device", &data_encoding::HEXLOWER.encode(&before)),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !server
            .client(id)
            .await
            .db()
            .dm_local_installation()
            .await
            .unwrap()
            .unwrap()
            .retired
    );
    writer.post_form("/unlock/logout", &[]).await;
    let response = writer.get("/messages").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    private_headers(&response);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ajax_send_clears_only_the_submitted_draft_instance() {
    let server = TestServer::start().await;
    let alice = server.driver();
    let bob = server.driver();
    let (alice_id, _) = alice.login_new_identity().await;
    let (bob_id, _) = bob.login_new_identity().await;
    replicate_device(&server, bob_id, alice_id).await;

    let path = format!("/messages/{}", bob_id.to_short());
    let page = alice.get(&path).await;
    assert_eq!(page.status(), StatusCode::OK);
    let csrf = token(&page.text().await.unwrap());
    let sent_draft_token = "a".repeat(64);
    let response = alice
        .ajax_post_form(
            &path,
            &[
                ("text", "sent with Alpine"),
                ("csrf", csrf.as_str()),
                ("draft_token", sent_draft_token.as_str()),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    private_headers(&response);
    let page = Html::parse_document(&response.text().await.unwrap());
    assert!(
        page.select(&Selector::parse("#direct-message-thread").unwrap())
            .next()
            .is_some()
    );
    let composer = page
        .select(&Selector::parse("form[x-init]").unwrap())
        .next()
        .expect("successful AJAX response draft clear");
    let clear = composer.value().attr("x-init").unwrap();
    assert!(clear.contains(&format!("draftToken === \"{sent_draft_token}\"")));
    assert!(clear.contains(&format!("direct-message-draft-token-{alice_id}-{bob_id}")));
    assert!(clear.contains("localStorage.getItem"));
    assert!(clear.contains("text = ''"));
    assert!(clear.contains("draftToken = "));

    let page = alice.get(&path).await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = Html::parse_document(&page.text().await.unwrap());
    assert!(
        page.select(&Selector::parse("form[x-init]").unwrap())
            .next()
            .is_none(),
        "ordinary navigation must not clear a draft"
    );
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_send_receive_retirement_and_reenrollment() {
    let server = TestServer::start().await;
    let alice = server.driver();
    let bob = server.driver();
    let (alice_id, alice_secret) = alice.login_new_identity().await;
    let (bob_id, bob_secret) = bob.login_new_identity().await;
    server
        .client(bob_id)
        .await
        .post_social_profile_update(bob_secret, "Bob Example".to_owned(), String::new(), None)
        .await
        .unwrap();
    let bob_profile_event = server
        .client(bob_id)
        .await
        .db()
        .get_social_profile(bob_id)
        .await
        .unwrap()
        .event_id;
    replicate_device(&server, alice_id, bob_id).await;
    replicate_device(&server, bob_id, alice_id).await;
    replicate_event(&server, bob_id, alice_id, bob_profile_event).await;
    let path = format!("/messages/{}", bob_id.to_short());
    let page = alice.get("/messages").await.text().await.unwrap();
    let document = Html::parse_document(&page);
    assert_conversation_panel_has_no_start_form(&document);
    let response = alice.get(&format!("/messages/open?peer={bob_id}")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], path);
    private_headers(&response);
    for peer in [
        "invalid".to_owned(),
        rostra_core::id::RostraIdSecretKey::generate()
            .id()
            .to_string(),
    ] {
        let response = alice.get(&format!("/messages/open?peer={peer}")).await;
        assert!(matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
        ));
        private_headers(&response);
        assert!(response.text().await.unwrap().contains("<html"));
    }
    let response = alice
        .get(&format!("/messages/{bob_id}?before_time=1"))
        .await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("{path}?before_time=1")
    );
    private_headers(&response);
    let response = alice.get(&path).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = response.text().await.unwrap();
    let document = Html::parse_document(&page);
    assert_conversation_panel_has_no_start_form(&document);
    let header = document
        .select(&Selector::parse(".m-directMessages__threadHeader").unwrap())
        .next()
        .expect("conversation header");
    assert_eq!(
        header
            .select(&Selector::parse("h1").unwrap())
            .next()
            .expect("recipient name")
            .text()
            .collect::<String>(),
        "Bob Example"
    );
    let recipient_link = header
        .select(&Selector::parse("h1 a").unwrap())
        .next()
        .expect("recipient profile link");
    assert_eq!(
        recipient_link.value().attr("href"),
        Some(format!("/profile/{}", bob_id.to_short()).as_str())
    );
    assert!(
        !header
            .text()
            .collect::<String>()
            .contains("Private conversation")
    );
    assert!(
        document
            .select(&Selector::parse(".m-directMessages__availabilityWarning").unwrap())
            .next()
            .is_none()
    );
    assert!(
        document
            .select(&Selector::parse("textarea[name=text]").unwrap())
            .next()
            .unwrap()
            .value()
            .attr("disabled")
            .is_none()
    );
    assert_eq!(
        document
            .select(&Selector::parse("textarea[name=text]").unwrap())
            .next()
            .expect("message composer")
            .value()
            .attr("aria-label"),
        Some("Message")
    );
    let composer = document
        .select(&Selector::parse("form[action^='/messages/']").unwrap())
        .find(|form| {
            form.select(&Selector::parse("textarea[name=text]").unwrap())
                .next()
                .is_some()
        })
        .expect("message composer form");
    let draft_state = composer
        .value()
        .attr("x-data")
        .expect("persisted draft state");
    assert!(draft_state.contains(&alice_id.to_string()));
    assert!(draft_state.contains(&bob_id.to_string()));
    assert_eq!(
        composer.value().attr("x-on:keyup.enter.ctrl"),
        Some(
            "if (!$event.repeat && !$event.isComposing && $event.keyCode !== 229) { $el.requestSubmit(); }"
        )
    );
    assert_eq!(
        composer
            .select(&Selector::parse("textarea[name=text]").unwrap())
            .next()
            .unwrap()
            .value()
            .attr("x-model"),
        Some("text")
    );
    assert_eq!(
        composer
            .select(&Selector::parse("textarea[name=text]").unwrap())
            .next()
            .unwrap()
            .value()
            .attr("@input"),
        Some(
            "draftToken = Array.from(crypto.getRandomValues(new Uint8Array(32)), byte => byte.toString(16).padStart(2, '0')).join('')"
        )
    );
    assert!(
        document
            .select(&Selector::parse(".m-directMessages__sendButton").unwrap())
            .next()
            .unwrap()
            .value()
            .attr("disabled")
            .is_none()
    );
    let csrf = token(&page);
    let text = "<img src=\"https://outsider.invalid/pixel\"> **not markup**\nsecond line";
    let response = alice
        .post_form(&path, &[("text", text), ("csrf", "wrong")])
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        server
            .client(alice_id)
            .await
            .db()
            .dm_history_with(bob_id, None, 64)
            .await
            .unwrap()
            .is_empty()
    );
    let oversized = "é".repeat(8193);
    let response = alice
        .post_form(
            &path,
            &[("text", oversized.as_str()), ("csrf", csrf.as_str())],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    private_headers(&response);
    let document = Html::parse_document(&response.text().await.unwrap());
    assert_eq!(
        document
            .select(&Selector::parse("textarea").unwrap())
            .next()
            .unwrap()
            .text()
            .collect::<String>(),
        oversized
    );
    let response = alice
        .post_form(&path, &[("text", text), ("csrf", csrf.as_str())])
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], path);
    private_headers(&response);
    let alice_client = server.client(alice_id).await;
    let bob_client = server.client(bob_id).await;
    let sent = alice_client
        .db()
        .dm_history_with(bob_id, None, 64)
        .await
        .unwrap();
    assert_eq!(sent.len(), 1);
    let event_id = sent[0].event_id;
    let event = VerifiedEvent::verify_signed(
        alice_id,
        alice_client.db().get_event(event_id).await.unwrap().signed,
    )
    .unwrap();
    let content = alice_client.db().get_event_content(event_id).await.unwrap();
    bob_client
        .db()
        .try_process_event_with_content(&VerifiedEventContent::verify(event, content).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let history = bob_client
                .db()
                .dm_history_with(alice_id, None, 64)
                .await
                .unwrap();
            if !history.is_empty() {
                assert_eq!(history[0].text, text);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let bob_path = format!("/messages/{}", alice_id.to_short());
    let bob_page = bob.get(&bob_path).await;
    assert_eq!(bob_page.status(), StatusCode::OK);
    let bob_page = Html::parse_document(&bob_page.text().await.unwrap());
    let bob_draft_state = bob_page
        .select(&Selector::parse("form[x-data]").unwrap())
        .next()
        .expect("Bob composer")
        .value()
        .attr("x-data")
        .expect("Bob draft state");
    assert!(bob_draft_state.contains(&bob_id.to_string()));
    assert!(bob_draft_state.contains(&alice_id.to_string()));
    assert_ne!(
        draft_state, bob_draft_state,
        "draft keys must distinguish the current account"
    );
    for (driver, path) in [(&alice, path.clone()), (&bob, bob_path)] {
        let response = driver.get(&path).await;
        private_headers(&response);
        let document = Html::parse_document(&response.text().await.unwrap());
        assert!(
            document
                .select(&Selector::parse("img, iframe, script:not([src])").unwrap())
                .next()
                .is_none()
        );
        assert!(
            document
                .select(&Selector::parse(".m-directMessages__text").unwrap())
                .any(|element| element.text().collect::<String>() == text)
        );
    }
    let conversations = alice.get("/messages").await.text().await.unwrap();
    let conversations = Html::parse_document(&conversations);
    let conversation = conversations
        .select(&Selector::parse(&format!("a[href='{path}']")).unwrap())
        .next()
        .expect("conversation");
    assert_eq!(
        conversation
            .select(&Selector::parse(".m-directMessages__conversationPeer").unwrap())
            .next()
            .expect("conversation display name")
            .text()
            .collect::<String>(),
        "Bob Example"
    );
    assert!(
        !conversation.text().collect::<String>().contains(text),
        "conversation list must not expose message previews"
    );
    let local = alice_client
        .db()
        .dm_local_installation()
        .await
        .unwrap()
        .unwrap()
        .device_id;
    let settings_csrf = settings_token(&alice).await;
    let before = alice_client.db().get_self_current_head().await;
    let confirmation = alice
        .get(&format!(
            "/settings/messages/retire/{}",
            data_encoding::HEXLOWER.encode(&local)
        ))
        .await;
    assert_eq!(confirmation.status(), StatusCode::OK);
    private_headers(&confirmation);
    let confirmation = Html::parse_document(&confirmation.text().await.unwrap());
    assert!(
        confirmation
            .root_element()
            .text()
            .collect::<String>()
            .contains("permanent")
    );
    assert!(
        confirmation
            .select(&Selector::parse("a[href='/settings/messages']").unwrap())
            .any(|link| link.text().collect::<String>() == "Cancel")
    );
    assert_eq!(alice_client.db().get_self_current_head().await, before);
    assert!(
        !alice_client
            .db()
            .dm_local_installation()
            .await
            .unwrap()
            .unwrap()
            .retired
    );
    for fields in [
        vec![("csrf", settings_csrf.as_str()), ("action", "unknown")],
        vec![("csrf", settings_csrf.as_str()), ("action", "retire")],
        vec![
            ("csrf", settings_csrf.as_str()),
            ("action", "retire"),
            ("device", "bad"),
        ],
        vec![
            ("csrf", settings_csrf.as_str()),
            ("action", "reenroll"),
            ("device", "unexpected"),
        ],
    ] {
        let response = alice.post_form("/settings/messages", &fields).await;
        assert!(response.status().is_client_error());
        private_headers(&response);
        assert!(response.text().await.unwrap().contains("<html"));
        assert_eq!(alice_client.db().get_self_current_head().await, before);
        let state = alice_client
            .db()
            .dm_local_installation()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.device_id, local);
        assert!(!state.retired);
    }
    let response = alice
        .post_form(
            "/settings/messages",
            &[
                ("csrf", settings_csrf.as_str()),
                ("action", "retire"),
                ("device", &data_encoding::HEXLOWER.encode(&local)),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(
        alice_client
            .db()
            .dm_local_installation()
            .await
            .unwrap()
            .unwrap()
            .retired
    );
    let response = alice
        .post_form(&path, &[("text", "not queued"), ("csrf", csrf.as_str())])
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let document = Html::parse_document(&response.text().await.unwrap());
    assert!(
        document
            .select(
                &Selector::parse(".m-directMessages__availabilityWarning[role=status]").unwrap()
            )
            .any(|warning| warning
                .text()
                .collect::<String>()
                .contains("No message will be queued."))
    );
    assert!(
        document
            .select(&Selector::parse("textarea[name=text][disabled]").unwrap())
            .any(|textarea| textarea.text().collect::<String>() == "not queued")
    );
    assert!(
        document
            .select(&Selector::parse(".m-directMessages__sendButton[disabled]").unwrap())
            .next()
            .is_some()
    );
    assert_eq!(
        alice_client
            .db()
            .dm_history_with(bob_id, None, 64)
            .await
            .unwrap()
            .len(),
        1
    );
    let response = alice
        .post_form(
            "/settings/messages",
            &[("csrf", settings_csrf.as_str()), ("action", "reenroll")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let replacement = alice_client
        .db()
        .dm_local_installation()
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.device_id, local);
    assert!(!replacement.retired);
    assert_eq!(
        alice_client
            .db()
            .dm_history_with(bob_id, None, 64)
            .await
            .unwrap()
            .len(),
        1
    );
    for index in 1..32 {
        alice_client
            .send_direct_message(alice_secret, bob_id, format!("page {index}"))
            .await
            .unwrap();
    }
    let response = alice.get(&path).await;
    let document = Html::parse_document(&response.text().await.unwrap());
    assert_eq!(
        document
            .select(&Selector::parse(".m-directMessages__message").unwrap())
            .count(),
        32
    );
    assert!(
        !document
            .select(&Selector::parse("a").unwrap())
            .any(|link| link.text().collect::<String>() == "Older messages")
    );
    alice_client
        .send_direct_message(alice_secret, bob_id, "one more".to_owned())
        .await
        .unwrap();
    let response = alice.get(&path).await;
    let document = Html::parse_document(&response.text().await.unwrap());
    assert_eq!(
        document
            .select(&Selector::parse(".m-directMessages__message").unwrap())
            .count(),
        32
    );
    let older = document
        .select(&Selector::parse("a").unwrap())
        .find(|link| link.text().collect::<String>() == "Older messages")
        .unwrap()
        .value()
        .attr("href")
        .unwrap();
    let response = alice.get(older).await;
    let document = Html::parse_document(&response.text().await.unwrap());
    assert_eq!(
        document
            .select(&Selector::parse(".m-directMessages__message").unwrap())
            .count(),
        1
    );
    assert!(
        !document
            .select(&Selector::parse("a").unwrap())
            .any(|link| link.text().collect::<String>() == "Older messages")
    );
    drop(alice_client);
    drop(bob_client);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_header_uses_unnamed_fallback_and_escapes_profile_names() {
    let server = TestServer::start().await;
    let alice = server.driver();
    let bob = server.driver();
    let charlie = server.driver();
    let (alice_id, _) = alice.login_new_identity().await;
    let (bob_id, bob_secret) = bob.login_new_identity().await;
    let (charlie_id, _) = charlie.login_new_identity().await;
    replicate_device(&server, bob_id, alice_id).await;
    replicate_device(&server, charlie_id, alice_id).await;

    let fallback = alice
        .get(&format!("/messages/{}", charlie_id.to_short()))
        .await;
    assert_eq!(fallback.status(), StatusCode::OK);
    private_headers(&fallback);
    let fallback = Html::parse_document(&fallback.text().await.unwrap());
    let fallback_header = fallback
        .select(&Selector::parse(".m-directMessages__threadHeader").unwrap())
        .next()
        .expect("conversation header");
    assert_eq!(
        fallback_header
            .select(&Selector::parse("h1").unwrap())
            .next()
            .expect("fallback identity")
            .text()
            .collect::<String>(),
        "Unnamed profile"
    );
    assert_eq!(
        fallback_header
            .select(&Selector::parse("h1 a").unwrap())
            .next()
            .expect("fallback profile link")
            .value()
            .attr("href"),
        Some(format!("/profile/{}", charlie_id.to_short()).as_str())
    );
    let charlie_draft = fallback
        .select(&Selector::parse("form[x-data]").unwrap())
        .next()
        .expect("Charlie composer")
        .value()
        .attr("x-data")
        .expect("Charlie draft key")
        .to_owned();
    assert!(charlie_draft.contains(&alice_id.to_string()));
    assert!(charlie_draft.contains(&charlie_id.to_string()));

    let malicious_name = "<img src=x onerror=alert(1)>".to_owned();
    server
        .client(bob_id)
        .await
        .post_social_profile_update(bob_secret, malicious_name.clone(), String::new(), None)
        .await
        .unwrap();
    let profile_event = server
        .client(bob_id)
        .await
        .db()
        .get_social_profile(bob_id)
        .await
        .unwrap()
        .event_id;
    replicate_event(&server, bob_id, alice_id, profile_event).await;

    let page = alice.get(&format!("/messages/{}", bob_id.to_short())).await;
    assert_eq!(page.status(), StatusCode::OK);
    private_headers(&page);
    let page = page.text().await.unwrap();
    assert!(!page.contains(&malicious_name));
    let document = Html::parse_document(&page);
    let bob_draft = document
        .select(&Selector::parse("form[x-data]").unwrap())
        .next()
        .expect("Bob composer")
        .value()
        .attr("x-data")
        .expect("Bob draft key");
    assert!(bob_draft.contains(&alice_id.to_string()));
    assert!(bob_draft.contains(&bob_id.to_string()));
    assert_ne!(bob_draft, charlie_draft);
    let header = document
        .select(&Selector::parse(".m-directMessages__threadHeader").unwrap())
        .next()
        .expect("conversation header");
    assert_eq!(
        header
            .select(&Selector::parse("h1").unwrap())
            .next()
            .expect("escaped recipient name")
            .text()
            .collect::<String>(),
        malicious_name
    );
    assert_eq!(
        header
            .select(&Selector::parse("h1 a").unwrap())
            .next()
            .expect("recipient profile link")
            .value()
            .attr("href"),
        Some(format!("/profile/{}", bob_id.to_short()).as_str())
    );
    assert!(
        header
            .select(&Selector::parse("img, script, iframe").unwrap())
            .next()
            .is_none(),
        "profile names must remain escaped text"
    );
    server.shutdown().await;
}
