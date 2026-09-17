use std::sync::{Arc, Mutex};

use super::*;

fn parse_opts(arguments: &[&str]) -> Result<Opts, clap::Error> {
    Opts::try_parse_from(["rostra-bot"].into_iter().chain(arguments.iter().copied()))
}

fn starts() -> Arc<Mutex<Vec<tokio::time::Instant>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn recorded_starts(starts: &Arc<Mutex<Vec<tokio::time::Instant>>>) -> Vec<tokio::time::Instant> {
    starts.lock().expect("starts mutex").clone()
}

#[test]
fn scrape_interval_defaults_to_thirty_minutes() {
    let opts = parse_opts(&[]).expect("default options parse");

    assert_eq!(opts.scrape_interval_minutes.minutes(), 30);
    assert_eq!(
        opts.scrape_interval_minutes.period(),
        Duration::from_secs(30 * 60)
    );
}

#[test]
fn scrape_interval_accepts_one_minute() {
    let opts = parse_opts(&["--scrape-interval-minutes", "1"]).expect("one minute parses");

    assert_eq!(opts.scrape_interval_minutes.minutes(), 1);
    assert_eq!(
        opts.scrape_interval_minutes.period(),
        Duration::from_secs(60)
    );
}

#[test]
fn scrape_interval_rejects_zero_with_the_option_name() {
    let error = parse_opts(&["--scrape-interval-minutes", "0"]).expect_err("zero is rejected");

    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    assert!(error.to_string().contains("--scrape-interval-minutes"));
}

#[test]
fn scrape_interval_arithmetic_ceiling_converts_to_seconds() {
    let minutes = u64::MAX / SECONDS_PER_MINUTE;

    let interval = ScrapeInterval::from_minutes(minutes).expect("arithmetic ceiling converts");

    assert_eq!(interval.minutes(), minutes);
    assert_eq!(
        interval.period(),
        Duration::from_secs(minutes * SECONDS_PER_MINUTE)
    );
}

#[test]
fn scrape_interval_rejects_minutes_that_overflow_seconds() {
    let arithmetic_ceiling = u64::MAX / SECONDS_PER_MINUTE;

    assert!(ScrapeInterval::from_minutes(arithmetic_ceiling + 1).is_err());
    assert!(ScrapeInterval::from_minutes(u64::MAX).is_err());
}

#[tokio::test(start_paused = true)]
async fn bot_loop_runs_first_cycle_immediately() {
    let started_at = tokio::time::Instant::now();
    let starts = starts();
    let task = tokio::spawn({
        let starts = Arc::clone(&starts);
        async move {
            run_bot_loop_with(Duration::from_secs(10), move || {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .expect("starts mutex")
                        .push(tokio::time::Instant::now());
                    Ok(())
                }
            })
            .await
        }
    });

    tokio::task::yield_now().await;

    assert_eq!(recorded_starts(&starts), [started_at]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn bot_loop_waits_until_the_first_periodic_deadline() {
    let starts = starts();
    let task = tokio::spawn({
        let starts = Arc::clone(&starts);
        async move {
            run_bot_loop_with(Duration::from_secs(10), move || {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .expect("starts mutex")
                        .push(tokio::time::Instant::now());
                    Ok(())
                }
            })
            .await
        }
    });

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert_eq!(recorded_starts(&starts).len(), 1);

    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(recorded_starts(&starts).len(), 2);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn bot_loop_keeps_the_original_periodic_deadlines() {
    let started_at = tokio::time::Instant::now();
    let starts = starts();
    let task = tokio::spawn({
        let starts = Arc::clone(&starts);
        async move {
            run_bot_loop_with(Duration::from_secs(10), move || {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .expect("starts mutex")
                        .push(tokio::time::Instant::now());
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok(())
                }
            })
            .await
        }
    });

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(8)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(8)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        recorded_starts(&starts),
        [
            started_at,
            started_at + Duration::from_secs(10),
            started_at + Duration::from_secs(20),
        ]
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn bot_loop_preserves_burst_catch_up_after_an_overrun() {
    let started_at = tokio::time::Instant::now();
    let starts = starts();
    let cycle = Arc::new(Mutex::new(0));
    let task = tokio::spawn({
        let starts = Arc::clone(&starts);
        let cycle = Arc::clone(&cycle);
        async move {
            run_bot_loop_with(Duration::from_secs(10), move || {
                let starts = Arc::clone(&starts);
                let cycle = Arc::clone(&cycle);
                async move {
                    starts
                        .lock()
                        .expect("starts mutex")
                        .push(tokio::time::Instant::now());
                    let cycle = {
                        let mut cycle = cycle.lock().expect("cycle mutex");
                        let current = *cycle;
                        *cycle += 1;
                        current
                    };
                    if cycle == 0 {
                        tokio::time::sleep(Duration::from_secs(25)).await;
                    }
                    Ok(())
                }
            })
            .await
        }
    });

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(25)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        recorded_starts(&starts),
        [
            started_at,
            started_at + Duration::from_secs(25),
            started_at + Duration::from_secs(25),
        ]
    );

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        recorded_starts(&starts),
        [
            started_at,
            started_at + Duration::from_secs(25),
            started_at + Duration::from_secs(25),
            started_at + Duration::from_secs(30),
        ]
    );
    task.abort();
}
