use std::sync::Arc;
use std::time::Duration;

use rostra_client_db::{Database, PayloadAccount, PayloadAccountAttachError};
use rostra_core::id::RostraIdSecretKey;
use tokio::sync::Notify;

use super::MultiClient;
use crate::Client;

/// One-shot gate at the open/attach-to-publication cancellation boundary.
#[derive(Default)]
pub(super) struct LoadPause {
    /// Reconstruction is waiting for the previous runtime's tasks.
    pub(super) waiting_teardown: Notify,
    /// Initialization reached the boundary with its database attached.
    pub(super) opened: Notify,
    /// The test permits manager-owned initialization to finish.
    pub(super) resume: Notify,
}

fn manager(accounts: Vec<PayloadAccount>) -> anyhow::Result<(tempfile::TempDir, MultiClient)> {
    let directory = tempfile::TempDir::new()?;
    let manager = MultiClient::new_with_payload_accounts(
        directory.path().to_path_buf(),
        1,
        false,
        Client::make_pkarr_client()?,
        accounts,
    )?;
    Ok((directory, manager))
}

async fn released<T>(weak: &std::sync::Weak<T>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retained_client_eviction_and_concurrent_reload_reuses_runtime() -> anyhow::Result<()> {
    let a = RostraIdSecretKey::generate().id();
    let b = RostraIdSecretKey::generate().id();
    let account = PayloadAccount::disabled(a);
    let (_directory, manager) = manager(vec![account.clone()])?;
    let first = manager.load(a).await?;
    let other = manager.load(b).await?;
    assert_eq!(manager.inner.read().await.len(), 1);
    assert!(!manager.inner.read().await.contains_key(&a));
    let clone = manager.clone();
    let (again, concurrent) = tokio::join!(manager.load(a), clone.load(a));
    let (again, concurrent) = (again?, concurrent?);
    assert!(Arc::ptr_eq(&first, &again));
    assert!(Arc::ptr_eq(&first, &concurrent));
    assert_eq!(manager.inner.read().await.len(), 1);
    let mut probe = Database::new_in_memory(a).await?;
    assert_eq!(
        probe.attach_payload_account(&account),
        Err(PayloadAccountAttachError::AccountAlreadyAttached)
    );
    assert!(Arc::ptr_eq(&other, &manager.load(b).await?));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn database_only_eviction_reuses_storage_then_reaps_and_reopens() -> anyhow::Result<()> {
    let a = RostraIdSecretKey::generate().id();
    let b = RostraIdSecretKey::generate().id();
    let account = PayloadAccount::disabled(a);
    let (_directory, manager) = manager(vec![account.clone()])?;
    let first = manager.load(a).await?;
    let old_client = Arc::downgrade(&first);
    let database = first.db().clone();
    let old_database = Arc::downgrade(&database);
    let other = manager.load(b).await?;
    drop(first);
    released(&old_client).await?;
    let clone = manager.clone();
    let (again, concurrent) = tokio::join!(manager.load(a), clone.load(a));
    let (again, concurrent) = (again?, concurrent?);
    assert!(Arc::ptr_eq(&again, &concurrent));
    assert!(Arc::ptr_eq(again.db(), &database));

    assert!(Arc::ptr_eq(&other, &manager.load(b).await?));
    drop(again);
    drop(concurrent);
    drop(database);
    // The weak-manager janitor closes released retired storage even with no new
    // load, rather than making the LRU eviction a permanent DB keeper.
    released(&old_database).await?;
    {
        // Weak upgrade failure can precede destructor completion. The same lock
        // used by cold loads establishes that file close and detachment finished.
        let _closed = manager.load_lock.lock().await;
        let mut probe = Database::new_in_memory(a).await?;
        probe.attach_payload_account(&account)?;
    }
    let reopened = manager.load(a).await?;
    assert!(old_database.upgrade().is_none());
    assert_eq!(reopened.rostra_id(), a);
    assert_eq!(manager.inner.read().await.len(), 1);

    let registry = Arc::downgrade(&manager.retired);
    drop(clone);
    drop(manager);
    released(&registry).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_loader_keeps_attachment_discoverable_until_publication() -> anyhow::Result<()> {
    let id = RostraIdSecretKey::generate().id();
    let account = PayloadAccount::disabled(id);
    let (_directory, manager) = manager(vec![account.clone()])?;
    let pause = Arc::new(LoadPause::default());
    *manager.load_pause.lock().unwrap() = Some(pause.clone());
    let caller_manager = manager.clone();
    let caller = tokio::spawn(async move { caller_manager.load(id).await });
    tokio::time::timeout(Duration::from_secs(10), pause.opened.notified()).await?;
    let mut probe = Database::new_in_memory(id).await?;
    assert_eq!(
        probe.attach_payload_account(&account),
        Err(PayloadAccountAttachError::AccountAlreadyAttached)
    );
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    let second_manager = manager.clone();
    let second = tokio::spawn(async move { second_manager.load(id).await });
    pause.resume.notify_one();
    let client = tokio::time::timeout(Duration::from_secs(10), second).await???;
    assert!(Arc::ptr_eq(&client, &manager.load(id).await?));
    assert_eq!(manager.inner.read().await.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retired_reaper_is_bounded_and_advances_past_live_owners() -> anyhow::Result<()> {
    let (_directory, manager) = manager(vec![])?;
    let _load = manager.load_lock.lock().await;
    let mut retired = manager.retired.write().await;
    for _ in 0..40 {
        let id = RostraIdSecretKey::generate().id();
        retired.insert(
            id,
            super::RetiredClient {
                client: Default::default(),
                database: Arc::new(Database::new_in_memory(id).await?),
                networking: Default::default(),
                tasks: None,
            },
        );
    }
    let kept_id = *retired.first_key_value().unwrap().0;
    let kept = retired[&kept_id].database.clone();
    let mut cursor = None;
    for _ in 0..80 {
        let before = retired.len();
        MultiClient::reap_retired(&mut retired, &mut cursor).await?;
        assert!(before - retired.len() <= 32);
        if retired.len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(retired.len(), 1);
    assert!(retired.contains_key(&kept_id));
    drop(kept);
    for _ in 0..2 {
        MultiClient::reap_retired(&mut retired, &mut cursor).await?;
    }
    assert!(retired.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn database_rebuild_waits_for_actual_old_task_termination() -> anyhow::Result<()> {
    let a = RostraIdSecretKey::generate().id();
    let b = RostraIdSecretKey::generate().id();
    let (_directory, manager) = manager(vec![PayloadAccount::disabled(a)])?;
    let first = manager.load(a).await?;
    let started = Arc::new(Notify::new());
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let task_started = started.clone();
    let task_finished = finished.clone();
    first
        .task_handles
        .push(n0_future::task::AbortOnDropHandle::new(tokio::spawn(
            async move {
                tokio::task::block_in_place(|| {
                    task_started.notify_one();
                    // Dropping the sender also releases this on test failure.
                    let _ = blocked.recv();
                });
                task_finished.store(true, std::sync::atomic::Ordering::Release);
            },
        )));
    started.notified().await;
    let _other = manager.load(b).await?;
    drop(first);
    // The remaining blocked task deliberately owns no DB Arc. The reaper must
    // preserve its completion state even once it could unwrap the database.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if manager
                .retired
                .read()
                .await
                .get(&a)
                .is_some_and(|entry| Arc::strong_count(&entry.database) == 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let pause = Arc::new(LoadPause::default());
    *manager.load_pause.lock().unwrap() = Some(pause.clone());
    let reload_manager = manager.clone();
    let reload = tokio::spawn(async move { reload_manager.load(a).await });
    tokio::time::timeout(Duration::from_secs(10), pause.waiting_teardown.notified()).await?;
    assert!(!finished.load(std::sync::atomic::Ordering::Acquire));
    assert!(!reload.is_finished());
    pause.resume.notify_one();
    drop(release);
    let rebuilt = tokio::time::timeout(Duration::from_secs(10), reload).await???;
    assert_eq!(rebuilt.rostra_id(), a);
    assert!(finished.load(std::sync::atomic::Ordering::Acquire));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retired_reaper_does_not_require_multithread_runtime() -> anyhow::Result<()> {
    let id = RostraIdSecretKey::generate().id();
    // Existing DB construction has its own multithread requirement; the reaper
    // must not introduce block_in_place on this current-thread executor.
    let database = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Database::new_in_memory(id))
    })
    .join()
    .expect("database construction thread")?;
    let mut retired = std::collections::BTreeMap::from([(
        id,
        super::RetiredClient {
            client: Default::default(),
            database: Arc::new(database),
            networking: Default::default(),
            tasks: None,
        },
    )]);
    MultiClient::reap_retired(&mut retired, &mut None).await?;
    assert!(retired.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_teardown_waiter_preserves_join_completion() -> anyhow::Result<()> {
    let tasks = crate::client::tasks::ClientTasks::default();
    let started = Arc::new(Notify::new());
    let task_started = started.clone();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    tasks.push(n0_future::task::AbortOnDropHandle::new(tokio::spawn(
        async move {
            tokio::task::block_in_place(|| {
                task_started.notify_one();
                let _ = blocked.recv();
            });
        },
    )));
    started.notified().await;
    tasks.abort();
    {
        let waiting = tasks.terminated();
        tokio::pin!(waiting);
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        // Drop a genuinely polled waiter while the aborted task still runs.
    }
    let waiting = tasks.terminated();
    tokio::pin!(waiting);
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    drop(release);
    tokio::time::timeout(Duration::from_secs(10), waiting).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn panicked_constructor_retry_waits_for_db_less_task_join() -> anyhow::Result<()> {
    use futures::FutureExt as _;

    let id = RostraIdSecretKey::generate().id();
    let (_directory, manager) = manager(vec![PayloadAccount::disabled(id)])?;
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_finished = finished.clone();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    *manager.build_hook.lock().unwrap() = Some(Box::new(move |client| {
        async move {
            let started = Arc::new(Notify::new());
            let task_started = started.clone();
            client
                .task_handles
                .push(n0_future::task::AbortOnDropHandle::new(tokio::spawn(
                    async move {
                        tokio::task::block_in_place(|| {
                            task_started.notify_one();
                            // No DB/Client ownership: only retained joins can protect
                            // retry from overlapping this still-running worker.
                            let _ = blocked.recv();
                        });
                        task_finished.store(true, std::sync::atomic::Ordering::Release);
                    },
                )));
            started.notified().await;
            panic!("injected constructor panic after task startup");
        }
        .boxed()
    }));
    let result = tokio::time::timeout(Duration::from_secs(10), manager.load(id)).await?;
    assert!(matches!(
        result,
        Err(super::MultiClientError::LoadTask { source }) if source.is_panic()
    ));
    assert!(!manager.inner.read().await.contains_key(&id));
    let pause = Arc::new(LoadPause::default());
    *manager.load_pause.lock().unwrap() = Some(pause.clone());
    let retry = manager.load_inner(id);
    tokio::pin!(retry);
    // Poll on this task so reaching the join and checking the next boundary
    // cannot race a concurrently scheduled retry.
    assert!(futures::poll!(retry.as_mut()).is_pending());
    assert!(pause.waiting_teardown.notified().now_or_never().is_some());
    assert!(!finished.load(std::sync::atomic::Ordering::Acquire));
    assert!(!manager.inner.read().await.contains_key(&id));
    // The preconstruction pause must not itself hide a bypassed join.
    assert!(pause.opened.notified().now_or_never().is_none());
    drop(release);
    let (client, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(retry, async {
            pause.opened.notified().await;
            assert!(finished.load(std::sync::atomic::Ordering::Acquire));
            pause.resume.notify_one();
        })
    })
    .await?;
    let client = client?;
    assert!(finished.load(std::sync::atomic::Ordering::Acquire));
    assert!(Arc::ptr_eq(&client, &manager.load(id).await?));
    Ok(())
}
