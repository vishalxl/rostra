use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tokio::sync::Barrier;
use tower_sessions_core::Session;
use tower_sessions_core::session::{Id, Record};
use tower_sessions_core::session_store::{self, SessionStore};

use super::RedbSessionStore;

#[derive(Debug, Clone)]
struct CollisionStore {
    inner: RedbSessionStore,
    occupied_id: Arc<Mutex<Option<Id>>>,
}

#[async_trait]
impl SessionStore for CollisionStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        *self.occupied_id.lock().unwrap() = Some(record.id);
        self.inner.save(record).await?;
        self.inner.create(record).await
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        self.inner.save(record).await
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        self.inner.load(session_id).await
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        self.inner.delete(session_id).await
    }
}

fn store() -> (TempDir, RedbSessionStore) {
    let temp_dir = tempfile::tempdir().unwrap();
    let database = redb::Database::create(temp_dir.path().join("sessions.redb")).unwrap();
    let database = Arc::new(redb_bincode::Database::from(database));
    let store = RedbSessionStore::new(database).unwrap();
    (temp_dir, store)
}

fn record(id: Id, value: usize) -> Record {
    Record {
        id,
        data: [("value".to_owned(), json!(value))].into(),
        expiry_date: OffsetDateTime::now_utc() + Duration::hours(1),
    }
}

async fn assert_stored(store: &RedbSessionStore, expected: &Record) {
    let stored = store.load(&expected.id).await.unwrap().unwrap();
    assert_eq!(stored.id, expected.id);
    assert_eq!(stored.data, expected.data);
}

#[tokio::test]
async fn create_preserves_an_unoccupied_id() {
    let (_temp_dir, store) = store();
    let id = Id(1);
    let mut record = record(id, 10);

    store.create(&mut record).await.unwrap();

    assert_eq!(record.id, id);
    assert_stored(&store, &record).await;
}

#[tokio::test]
async fn create_regenerates_an_occupied_id_without_overwriting() {
    let (_temp_dir, store) = store();
    let shared_id = Id(1);
    let mut first = record(shared_id, 10);
    let mut second = record(shared_id, 20);

    store.create(&mut first).await.unwrap();
    store.create(&mut second).await.unwrap();

    assert_eq!(first.id, shared_id);
    assert_ne!(second.id, shared_id);
    assert_stored(&store, &first).await;
    assert_stored(&store, &second).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_creates_with_the_same_id_keep_every_record() {
    const RECORD_COUNT: usize = 16;

    let (_temp_dir, store) = store();
    let shared_id = Id(1);
    let barrier = Arc::new(Barrier::new(RECORD_COUNT));
    let tasks = (0..RECORD_COUNT)
        .map(|value| {
            let store = store.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let mut record = record(shared_id, value);
                barrier.wait().await;
                store.create(&mut record).await.unwrap();
                record
            })
        })
        .collect::<Vec<_>>();

    let mut records = Vec::new();
    for task in tasks {
        records.push(task.await.unwrap());
    }

    assert_eq!(
        records
            .iter()
            .map(|record| record.id)
            .collect::<HashSet<_>>()
            .len(),
        RECORD_COUNT
    );
    for record in records {
        assert_stored(&store, &record).await;
    }
}

#[tokio::test]
async fn save_still_overwrites_an_existing_record() {
    let (_temp_dir, store) = store();
    let id = Id(1);
    let mut initial = record(id, 10);
    let replacement = record(id, 20);

    store.create(&mut initial).await.unwrap();
    store.save(&replacement).await.unwrap();

    assert_stored(&store, &replacement).await;
}

#[tokio::test]
async fn session_uses_the_id_selected_during_create() {
    let (_temp_dir, inner) = store();
    let occupied_id = Arc::new(Mutex::new(None));
    let store = Arc::new(CollisionStore {
        inner,
        occupied_id: occupied_id.clone(),
    });
    let session = Session::new(None, store.clone(), None);

    session.insert("value", 10).await.unwrap();
    session.save().await.unwrap();

    let occupied_id = occupied_id.lock().unwrap().unwrap();
    let selected_id = session.id().unwrap();
    assert_ne!(selected_id, occupied_id);
    assert_eq!(
        store
            .load(&selected_id)
            .await
            .unwrap()
            .unwrap()
            .data
            .get("value"),
        Some(&json!(10))
    );
}
