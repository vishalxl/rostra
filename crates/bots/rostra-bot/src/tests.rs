use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rostra_client_db::Database;
use rostra_core::Timestamp;
use rostra_core::id::RostraIdSecretKey;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, timeout};

use super::{
    ArticleAcknowledger, ArticlePublisher, BotDatabase, PublicationDelay, run_one_cycle_with,
};
use crate::publisher::{PublisherError, PublisherResult};
use crate::tables::Article;

fn article(id: &str) -> Article {
    let now = Timestamp::now();
    Article {
        id: id.to_owned(),
        title: format!("Article {id}"),
        url: Some(format!("https://example.com/{id}")),
        source_url: format!("https://example.com/comments/{id}"),
        score: 10,
        author: "author".to_owned(),
        scraped_at: now,
        source: "test".to_owned(),
        feed_title: None,
        feed_link: None,
        feed_subtitle: None,
        published_at: None,
    }
}

async fn open_bot_database(path: &Path, secret: RostraIdSecretKey) -> BotDatabase {
    let client_db = Arc::new(
        Database::open(path, secret.id())
            .await
            .expect("database should open"),
    );
    let db = BotDatabase::new(client_db);
    db.init_tables().await.expect("tables should initialize");
    db
}

struct ControlledPublisher {
    outcomes: Mutex<VecDeque<bool>>,
    attempts: Mutex<Vec<String>>,
    blocked_article: Option<String>,
    blocked: Notify,
    release: Notify,
}

impl ControlledPublisher {
    fn new(outcomes: impl IntoIterator<Item = bool>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            attempts: Mutex::new(Vec::new()),
            blocked_article: None,
            blocked: Notify::new(),
            release: Notify::new(),
        }
    }

    fn blocking(outcomes: impl IntoIterator<Item = bool>, article_id: &str) -> Self {
        Self {
            blocked_article: Some(article_id.to_owned()),
            ..Self::new(outcomes)
        }
    }
}

#[async_trait]
impl ArticlePublisher for ControlledPublisher {
    async fn publish_article(&self, article: &Article) -> PublisherResult<()> {
        self.attempts.lock().await.push(article.id.clone());
        if self.blocked_article.as_deref() == Some(&article.id) {
            self.blocked.notify_one();
            self.release.notified().await;
        }
        if self
            .outcomes
            .lock()
            .await
            .pop_front()
            .expect("each attempt should have an outcome")
        {
            Ok(())
        } else {
            Err(PublisherError::Test)
        }
    }
}

#[derive(Default)]
struct RecordingDelay {
    calls: Mutex<usize>,
    blocked_call: Option<usize>,
    blocked: Notify,
    release: Notify,
}

impl RecordingDelay {
    fn blocking(call: usize) -> Self {
        Self {
            blocked_call: Some(call),
            ..Self::default()
        }
    }
}

#[async_trait]
impl PublicationDelay for RecordingDelay {
    async fn wait(&self) {
        let mut calls = self.calls.lock().await;
        *calls += 1;
        let current = *calls;
        drop(calls);
        if self.blocked_call == Some(current) {
            self.blocked.notify_one();
            self.release.notified().await;
        }
    }
}

struct RecordingAcknowledger<'a> {
    db: &'a BotDatabase,
    fail: BTreeSet<String>,
    acknowledged: Mutex<Vec<String>>,
}

#[async_trait]
impl ArticleAcknowledger for RecordingAcknowledger<'_> {
    async fn mark_article_published(
        &self,
        article: &Article,
        published_at: Timestamp,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.acknowledged.lock().await.push(article.id.clone());
        if self.fail.contains(&article.id) {
            return Err(std::io::Error::other("injected acknowledgement failure").into());
        }
        self.db
            .mark_article_published(article, published_at)
            .await
            .map_err(|error| Box::new(error) as _)
    }
}

async fn queue(db: &BotDatabase, articles: &[Article]) {
    for article in articles {
        assert!(
            db.add_unpublished_article(article)
                .await
                .expect("article should queue")
        );
    }
}

async fn run_with(
    max_articles: usize,
    db: &BotDatabase,
    publisher: &dyn ArticlePublisher,
    acknowledger: &dyn ArticleAcknowledger,
    delay: &dyn PublicationDelay,
) {
    run_one_cycle_with(0, 0, max_articles, db, &[], publisher, acknowledger, delay)
        .await
        .expect("cycle should complete");
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledges_success_before_starting_next_publication() {
    let secret = RostraIdSecretKey::generate();
    let client_db = Arc::new(
        Database::new_in_memory(secret.id())
            .await
            .expect("database should open"),
    );
    let db = Arc::new(BotDatabase::new(client_db));
    db.init_tables().await.expect("tables should initialize");
    queue(&db, &[article("a"), article("b")]).await;

    let publisher = Arc::new(ControlledPublisher::blocking([true, true], "b"));
    let delay = Arc::new(RecordingDelay::default());
    let task = {
        let db = db.clone();
        let publisher = publisher.clone();
        let delay = delay.clone();
        tokio::spawn(async move {
            run_with(
                2,
                db.as_ref(),
                publisher.as_ref(),
                db.as_ref(),
                delay.as_ref(),
            )
            .await;
        })
    };

    timeout(Duration::from_secs(5), publisher.blocked.notified())
        .await
        .expect("second publication should block");
    assert_eq!(
        publisher.attempts.lock().await.as_slice(),
        ["a".to_owned(), "b".to_owned()]
    );
    let queued = db
        .get_unpublished_articles()
        .await
        .expect("queue should read");
    assert_eq!(
        queued
            .iter()
            .map(|article| article.id.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledgement_survives_cancellation_during_delay_and_disk_reopen() {
    let temp = tempfile::tempdir().expect("temporary directory should open");
    let path = temp.path().join("bot.redb");
    let secret = RostraIdSecretKey::generate();
    let db = open_bot_database(&path, secret).await;
    queue(&db, &[article("a"), article("b")]).await;

    let publisher = ControlledPublisher::new([true, true]);
    let delay = Arc::new(RecordingDelay::blocking(1));
    let task = {
        let delay = delay.clone();
        tokio::spawn(async move {
            run_with(2, &db, &publisher, &db, delay.as_ref()).await;
        })
    };

    timeout(Duration::from_secs(5), delay.blocked.notified())
        .await
        .expect("post-publication delay should block");
    task.abort();
    let _ = task.await;

    let reopened = open_bot_database(&path, secret).await;
    let queued = reopened
        .get_unpublished_articles()
        .await
        .expect("queue should read");
    assert_eq!(
        queued
            .iter()
            .map(|article| article.id.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn continues_after_publication_and_acknowledgement_errors_with_pacing_and_limit() {
    let secret = RostraIdSecretKey::generate();
    let client_db = Arc::new(
        Database::new_in_memory(secret.id())
            .await
            .expect("database should open"),
    );
    let db = BotDatabase::new(client_db);
    db.init_tables().await.expect("tables should initialize");
    queue(
        &db,
        &[article("a"), article("b"), article("c"), article("d")],
    )
    .await;

    let publisher = ControlledPublisher::new([false, true, true]);
    let acknowledger = RecordingAcknowledger {
        db: &db,
        fail: BTreeSet::from(["b".to_owned()]),
        acknowledged: Mutex::new(Vec::new()),
    };
    let delay = RecordingDelay::default();
    run_with(3, &db, &publisher, &acknowledger, &delay).await;

    assert_eq!(
        publisher.attempts.lock().await.as_slice(),
        ["a".to_owned(), "b".to_owned(), "c".to_owned()]
    );
    assert_eq!(
        acknowledger.acknowledged.lock().await.as_slice(),
        ["b".to_owned(), "c".to_owned()]
    );
    assert_eq!(*delay.calls.lock().await, 3);
    let queued = db
        .get_unpublished_articles()
        .await
        .expect("queue should read");
    assert_eq!(
        queued
            .iter()
            .map(|article| article.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "d"]
    );
}
