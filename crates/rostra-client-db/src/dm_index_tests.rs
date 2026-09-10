use rostra_core::ShortEventId;
use rostra_core::id::RostraIdSecretKey;
use rostra_dm::Announcement;

use super::dm_index::{interval_prefixes, point_prefixes};
use crate::Database;

#[test]
fn dm_dyadic_cover_exhaustive_and_boundary_points() {
    for start in 0..64 {
        for end in 0..64 {
            let cover = interval_prefixes(start, end);
            assert!(cover.len() <= 128);
            for now in 0..=64 {
                let matches = point_prefixes(now)
                    .filter(|prefix| cover.contains(prefix))
                    .count();
                assert_eq!(matches, usize::from(start <= now && now < end));
            }
        }
    }
    for (start, end) in [
        (0, u64::MAX),
        (1, u64::MAX),
        (u64::MAX - 1, u64::MAX),
        (1 << 63, u64::MAX),
        (0, 1 << 63),
        (u64::MAX, u64::MAX),
    ] {
        let cover = interval_prefixes(start, end);
        assert!(cover.len() <= 128);
        for now in [0, 1, start, start.saturating_sub(1), end - 1, end, u64::MAX] {
            assert_eq!(
                point_prefixes(now)
                    .filter(|prefix| cover.contains(prefix))
                    .count(),
                usize::from(start <= now && now < end),
            );
        }
    }
    assert_eq!(point_prefixes(u64::MAX).count(), 65);
    assert_eq!(point_prefixes(u64::MAX).next(), Some((0, 0)));
    assert_eq!(point_prefixes(u64::MAX).last(), Some((64, u64::MAX)));
}

#[tokio::test(flavor = "multi_thread")]
async fn dm_interval_index_matches_reference_with_adversarial_populations() -> anyhow::Result<()> {
    let account = RostraIdSecretKey::generate().id();
    let other = RostraIdSecretKey::generate().id();
    let db = Database::new_in_memory(account).await?;
    let local = db.dm_maintain_local(1000).await?;
    let public = local.epoch.as_ref().unwrap().public_key;
    db.write_with(|tx| {
        db.dm_apply_announcement_tx(account, &local, 1000, ShortEventId::ZERO, tx)?;
        for n in 0u64..2000 {
            let mut device = [0; 16];
            device[..8].copy_from_slice(&n.to_be_bytes());
            let start = n * 37 % 2100;
            let announcement = Announcement {
                device_id: device,
                epoch: Some(rostra_dm::EpochPublic {
                    public_key: public, // Distinct devices deliberately share a key.
                    send_from: start,
                    send_until: start + 100,
                    decrypt_until: start + 200,
                }),
            };
            db.dm_apply_announcement_tx(
                account,
                &announcement,
                start + n % 450,
                ShortEventId::ZERO,
                tx,
            )?;
            // Cross-account rows cannot enter the point query.
            db.dm_apply_announcement_tx(
                other,
                &announcement,
                start + n % 450,
                ShortEventId::MAX,
                tx,
            )?;
            if n % 5 == 0 {
                db.dm_apply_announcement_tx(
                    account,
                    &Announcement {
                        device_id: device,
                        epoch: None,
                    },
                    0,
                    ShortEventId::ZERO,
                    tx,
                )?;
            }
            // An older, broadly eligible announcement must not become fallback.
            let mut older = announcement;
            older.epoch.as_mut().unwrap().send_from = 0;
            older.epoch.as_mut().unwrap().send_until = 3000;
            db.dm_apply_announcement_tx(account, &older, 0, ShortEventId::ZERO, tx)?;
        }
        Ok(())
    })
    .await?;
    for replay in [false, true] {
        if replay {
            db.write_with(|tx| Database::prepare_total_migration(tx, 32))
                .await?;
            db.write_with(|tx| db.reprocess_migration_stash(tx)).await?;
        }
        for now in (0..2400).step_by(37).chain([u64::MAX, 1000, 0]) {
            let expected = db
                .read_with(|tx| {
                    let mut ranked = Vec::new();
                    for row in tx
                        .open_table(&crate::ids_dm_devices::TABLE)?
                        .range((account, [0; 16])..=(account, [255; 16]))?
                    {
                        let (key, state) = row?;
                        let (_, device) = key.value_try()?;
                        let state = state.value_try()?;
                        if device != local.device_id
                            && let Some((time, event, epoch)) = state.latest()
                            && epoch.eligible(now, time)
                        {
                            ranked.push((time, event, device));
                        }
                    }
                    ranked.sort_by_key(|rank| std::cmp::Reverse(*rank));
                    ranked.truncate(8);
                    Ok(ranked)
                })
                .await?;
            let actual = db.dm_destinations(account, now).await;
            if expected.is_empty() {
                assert!(matches!(
                    actual,
                    Err(crate::DbError::DmRecipientUnavailable)
                ));
            } else {
                let actual = actual?;
                let actual = actual
                    .selected
                    .iter()
                    .map(|((_, device), state)| {
                        let (time, event, _) = state.latest().unwrap();
                        (time, event, *device)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "time={now}, replay={replay}");
            }
        }
    }
    Ok(())
}
