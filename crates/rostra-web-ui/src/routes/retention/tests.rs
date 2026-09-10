use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use rostra_client_db::{
    DryRunLimits, DryRunProjection, DryRunReport, DryRunStatus, DryRunVictim,
    PayloadAdmissionUsage, PayloadUsage, QuotaPruneReason, RetentionGeneration,
};
use rostra_core::id::RostraId;
use rostra_core::retention::RetentionPolicy;
use rostra_core::{EventId, Timestamp};

#[test]
fn complete_and_incomplete_forecasts_present_distinct_units_and_no_prefix() {
    let author = RostraId::from_bytes([1; 32]);
    let event = EventId::from_bytes([2; 32]);
    let mut report = DryRunReport {
        as_of: Timestamp::from(42),
        generation: RetentionGeneration::new(RetentionPolicy::experimental(), author),
        limits: DryRunLimits {
            events: NonZeroUsize::new(10).unwrap(),
            authors: NonZeroUsize::new(2).unwrap(),
            logical_bytes: 1000,
            time: Duration::from_millis(10),
        },
        database_high_water: 100,
        status: DryRunStatus::Complete,
        visited: 2,
        observed_usage: Some(PayloadUsage {
            logical_current_bytes: 200,
            unique_stored_bytes: 100,
        }),
        observed_guarded_admission: PayloadAdmissionUsage::default(),
        projection: Some(DryRunProjection {
            author_high_waters: BTreeMap::from([(author, 100)]),
            victims: vec![(event, QuotaPruneReason::GlobalQuota)],
            victim_details: vec![DryRunVictim {
                event,
                author,
                bytes: 100,
                age_seconds: 20,
                distance_credit_ticks: 3i128 << 32,
            }],
            logical_victim_bytes: 100,
            logical_remaining_bytes: 100,
            protected_logical_bytes: 100,
            unmet_authors: 1,
            unmet_global: true,
        }),
    };
    let page = super::render_forecast(Some(&report)).into_string();
    assert!(page.contains(&event.to_string()));
    assert!(page.contains("Observed logical retained: 200"));
    assert!(page.contains("unique content store: 100"));
    assert!(page.contains("Unmet author targets: 1"));
    assert!(page.contains("Unique-store savings and GC backlog are not projected"));
    report.projection = None;
    report.status = DryRunStatus::EventLimit;
    let incomplete = super::render_forecast(Some(&report)).into_string();
    assert!(incomplete.contains("EventLimit"));
    assert!(incomplete.contains("No projection"));
    assert!(!incomplete.contains(&event.to_string()));
    assert!(!incomplete.contains("Proposed logical removal"));
}
