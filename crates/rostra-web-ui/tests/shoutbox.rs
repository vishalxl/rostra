mod common;

use common::TestServer;
use reqwest::{StatusCode, header};
use scraper::{Html, Selector};

async fn shoutbox_posts(server: &TestServer, id: rostra_core::id::RostraId) -> Vec<String> {
    server
        .client(id)
        .await
        .db()
        .paginate_shoutbox_posts_by_received_at_rev(None, 100)
        .await
        .0
        .into_iter()
        .map(|post| post.content.djot_content)
        .collect()
}

fn assert_complete_shoutbox_page(page: &str) {
    let document = Html::parse_document(page);
    assert!(page.starts_with("<!DOCTYPE html>"));
    assert_eq!(
        document
            .select(&Selector::parse("form[action='/shoutbox/post'][method='post']").unwrap())
            .count(),
        1
    );
    assert_eq!(
        document
            .select(&Selector::parse("textarea[name='content']").unwrap())
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_shoutbox_post_redirects_to_a_complete_landing_page() {
    let server = TestServer::start().await;
    let driver = server.driver();
    let (id, _) = driver.login_new_identity().await;

    let landing = driver.get("/shoutbox").await;
    assert_eq!(landing.status(), StatusCode::OK);
    assert_complete_shoutbox_page(&landing.text().await.unwrap());

    let before = shoutbox_posts(&server, id).await;
    let response = driver
        .post_form("/shoutbox/post", &[("content", "ordinary shout")])
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/shoutbox");

    let after = shoutbox_posts(&server, id).await;
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(after[0], "ordinary shout");

    let landing = driver.get("/shoutbox").await;
    assert_eq!(landing.status(), StatusCode::OK);
    let page = landing.text().await.unwrap();
    assert_complete_shoutbox_page(&page);
    assert!(page.contains("ordinary shout"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_shoutbox_validation_returns_a_complete_retry_page_without_publishing() {
    let server = TestServer::start().await;
    let driver = server.driver();
    let (id, _) = driver.login_new_identity().await;
    let before = shoutbox_posts(&server, id).await;

    for content in ["", " \n\t ", &"a".repeat(1001)] {
        let response = driver
            .post_form("/shoutbox/post", &[("content", content)])
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let page = response.text().await.unwrap();
        assert!(page.starts_with("<!DOCTYPE html>"));
        let document = Html::parse_document(&page);
        assert!(page.contains("Shouts must be between 1 and 1000 bytes."));
        assert_eq!(
            document
                .select(&Selector::parse("a[href='/shoutbox']").unwrap())
                .count(),
            1
        );
    }

    assert_eq!(shoutbox_posts(&server, id).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn enhanced_shoutbox_post_responses_remain_fragments() {
    let server = TestServer::start().await;
    let driver = server.driver();
    let (id, _) = driver.login_new_identity().await;

    let response = driver
        .ajax_post_form("/shoutbox/post", &[("content", "enhanced shout")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains(r#"id="shoutbox-posts""#));
    assert!(body.contains(r#"id="shoutbox-preview""#));
    assert!(body.contains(r#"id="ajax-scripts""#));
    assert!(body.contains("document.getElementById('shoutbox-input')"));
    assert!(!body.starts_with("<!DOCTYPE html>"));
    assert_eq!(shoutbox_posts(&server, id).await[0], "enhanced shout");

    let response = driver
        .ajax_post_form("/shoutbox/post", &[("content", " \n\t ")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains(r#"id="shoutbox-posts""#));
    assert!(body.contains(r#"id="shoutbox-preview""#));
    assert!(body.contains(r#"id="ajax-scripts""#));
    assert!(body.contains("Shoutout must be between 1 and 1000 characters"));
    assert!(!body.starts_with("<!DOCTYPE html>"));
    assert_eq!(shoutbox_posts(&server, id).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn readonly_and_unauthenticated_sessions_cannot_publish_shouts() {
    let server = TestServer::start().await;
    let writer = server.driver();
    let (id, _) = writer.login_new_identity().await;
    let reader = server.driver();
    reader.login_readonly(id).await;
    let before = shoutbox_posts(&server, id).await;

    let response = reader
        .post_form("/shoutbox/post", &[("content", "read-only shout")])
        .await;
    assert_ne!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(shoutbox_posts(&server, id).await, before);

    let visitor = server.driver();
    let response = visitor
        .post_form("/shoutbox/post", &[("content", "unauthenticated shout")])
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/unlock?redirect=%2Fshoutbox%2Fpost"
    );
    assert_eq!(shoutbox_posts(&server, id).await, before);
}
