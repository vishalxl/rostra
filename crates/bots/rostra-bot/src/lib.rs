pub mod database;
pub mod dedup;
pub mod publisher;
pub mod scraper;
pub mod tables;

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rostra_core::Timestamp;
use tracing::{debug, error, info, warn};

use crate::database::BotDatabase;
use crate::publisher::{Publisher, PublisherResult};
use crate::scraper::Scraper;
use crate::tables::Article;

pub const PROJECT_NAME: &str = "rostra-bot";
pub const LOG_TARGET: &str = "rostra_bot::main";
pub const MAX_ARTICLE_AGE_SECS: u64 = 30 * 24 * 60 * 60; // ~1 month

#[async_trait]
trait ArticlePublisher: Send + Sync {
    async fn publish_article(&self, article: &Article) -> PublisherResult<()>;
}

#[async_trait]
impl ArticlePublisher for Publisher {
    async fn publish_article(&self, article: &Article) -> PublisherResult<()> {
        Publisher::publish_article(self, article).await
    }
}

#[async_trait]
trait ArticleAcknowledger: Send + Sync {
    async fn mark_article_published(
        &self,
        article: &Article,
        published_at: Timestamp,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
impl ArticleAcknowledger for BotDatabase {
    async fn mark_article_published(
        &self,
        article: &Article,
        published_at: Timestamp,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        BotDatabase::mark_article_published(self, article, published_at)
            .await
            .map_err(|error| Box::new(error) as _)
    }
}

#[async_trait]
trait PublicationDelay: Send + Sync {
    async fn wait(&self);
}

struct StandardPublicationDelay;

#[async_trait]
impl PublicationDelay for StandardPublicationDelay {
    async fn wait(&self) {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
}

pub fn get_min_score_for_article(
    hn_min_score: u32,
    lobsters_min_score: u32,
    article: &crate::tables::Article,
) -> u32 {
    if article.source == "hn" {
        hn_min_score
    } else if article.source == "lobsters" {
        lobsters_min_score
    } else {
        0
    }
}

pub async fn run_one_cycle(
    hn_min_score: u32,
    lobsters_min_score: u32,
    max_articles_per_run: usize,
    db: &BotDatabase,
    scrapers: &[Box<dyn Scraper + Send + Sync>],
    publisher: &Publisher,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_one_cycle_with(
        hn_min_score,
        lobsters_min_score,
        max_articles_per_run,
        db,
        scrapers,
        publisher,
        db,
        &StandardPublicationDelay,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_one_cycle_with(
    hn_min_score: u32,
    lobsters_min_score: u32,
    max_articles_per_run: usize,
    db: &BotDatabase,
    scrapers: &[Box<dyn Scraper + Send + Sync>],
    publisher: &dyn ArticlePublisher,
    acknowledger: &dyn ArticleAcknowledger,
    publication_delay: &dyn PublicationDelay,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(target: LOG_TARGET, "Starting scraping and publishing cycle");

    // Scrape articles from all sources
    let mut total_added = 0;
    for scraper in scrapers {
        match scraper.scrape_frontpage().await {
            Ok(articles) => {
                info!(target: LOG_TARGET, count = articles.len(), "Scraped articles from source");

                // Filter articles by age and per-source score, then add to database
                let now = Timestamp::now();
                let mut added_count = 0;
                for article in articles {
                    // Skip articles older than ~1 month (when publication date is known)
                    if let Some(published_at) = article.published_at {
                        if MAX_ARTICLE_AGE_SECS < now.secs_since(published_at) {
                            debug!(
                                target: LOG_TARGET,
                                article_id = %article.id,
                                title = %article.title,
                                "Skipping old article"
                            );
                            continue;
                        }
                    }

                    let min_score =
                        get_min_score_for_article(hn_min_score, lobsters_min_score, &article);
                    if min_score <= article.score {
                        match db.add_unpublished_article(&article).await {
                            Ok(true) => added_count += 1,
                            Ok(false) => {}
                            Err(e) => {
                                warn!(target: LOG_TARGET, error = %e, article_id = %article.id, "Failed to add article to database")
                            }
                        }
                    }
                }
                total_added += added_count;
            }
            Err(e) => {
                error!(target: LOG_TARGET, error = %e, "Failed to scrape frontpage");
            }
        }
    }
    info!(target: LOG_TARGET, added = total_added, "Added new articles to unpublished queue");

    // Publish unpublished articles
    match db.get_unpublished_articles().await {
        Ok(articles) => {
            // Filter out old articles already sitting in the queue.
            // This is a workaround for articles that were enqueued before the
            // scraping-time age filter was added, and can be removed once all
            // deployed databases have been drained of stale entries.
            let now = Timestamp::now();
            let mut articles_filtered = Vec::new();
            for article in articles {
                if let Some(published_at) = article.published_at {
                    if MAX_ARTICLE_AGE_SECS < now.secs_since(published_at) {
                        debug!(
                            target: LOG_TARGET,
                            article_id = %article.id,
                            title = %article.title,
                            "Discarding old article from unpublished queue"
                        );
                        if let Err(e) = db.remove_unpublished_article(&article.id).await {
                            warn!(target: LOG_TARGET, error = %e, article_id = %article.id, "Failed to remove old article from queue");
                        }
                        continue;
                    }
                }
                articles_filtered.push(article);
            }

            let articles_to_publish: Vec<_> = articles_filtered
                .into_iter()
                .take(max_articles_per_run)
                .collect();

            if !articles_to_publish.is_empty() {
                info!(target: LOG_TARGET, count = articles_to_publish.len(), "Publishing articles to Rostra");

                for article in &articles_to_publish {
                    let result = publisher.publish_article(article).await;
                    match result {
                        Ok(()) => {
                            // Publication and acknowledgement remain separate commits. A
                            // cancellation between them can still replay the article.
                            let published_at = Timestamp::from(
                                SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .expect("Time went backwards")
                                    .as_secs(),
                            );
                            if let Err(e) = acknowledger
                                .mark_article_published(article, published_at)
                                .await
                            {
                                error!(target: LOG_TARGET, error = %e, article_id = %article.id, "Failed to mark article as published in database");
                            }
                        }
                        Err(e) => {
                            error!(target: LOG_TARGET, error = %e, article_id = %article.id, "Failed to publish article");
                        }
                    }
                    publication_delay.wait().await;
                }
            } else {
                info!(target: LOG_TARGET, "No articles to publish");
            }
        }
        Err(e) => {
            error!(target: LOG_TARGET, error = %e, "Failed to get unpublished articles from database");
        }
    }

    // Show current queue status
    if let Ok(unpublished_count) = db.get_unpublished_count().await {
        info!(target: LOG_TARGET, queue_size = unpublished_count, "Articles in unpublished queue");
    }

    Ok(())
}

#[cfg(test)]
mod tests;
