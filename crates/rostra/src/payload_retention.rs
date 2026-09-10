//! Explicit startup-only JSON input; never read by an HTTP request or worker.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::time::Duration;

use rostra_client_db::{
    DryRunLimits, PayloadAccount, PayloadAdmissionConfig, PayloadAdmissionLimits,
    PayloadRetentionConfig, PayloadRuntimeLimits,
};
use rostra_core::id::RostraId;
use rostra_core::retention::RetentionPolicy;
use serde::Deserialize;

/// One explicit account; omitted identities retain Disabled behavior.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    /// Stable storing identity, never an iroh transport key.
    id: RostraId,
    /// No implicit mode or byte budgets in a listed configuration.
    retention: Mode,
}

/// Distinct enforcing policy and observation-only forecast inputs.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
enum Mode {
    Disabled,
    DryRun {
        policy: Policy,
        admission: Admission,
        snapshot: Snapshot,
    },
    Enforce {
        policy: Policy,
        admission: Admission,
        worker: Worker,
    },
}

/// Version-one experimental static-score and grace parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    size_floor: u32,
    tau_seconds: u32,
    alpha_q16: u32,
    beta_q16: u32,
    max_bonus: u32,
    grace_seconds: u32,
}

impl Policy {
    fn build(self) -> io::Result<RetentionPolicy> {
        RetentionPolicy::new(
            self.size_floor,
            self.tau_seconds,
            self.alpha_q16,
            self.beta_q16,
            self.max_bonus,
            self.grace_seconds,
        )
        .ok_or_else(|| invalid("invalid static retention policy"))
    }
}

/// Logical ceilings are separate from actual acquisition capacity.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    database_bytes: NonZeroU64,
    author_bytes: NonZeroU64,
    overrides: BTreeMap<RostraId, NonZeroU64>,
    in_flight_count: NonZeroUsize,
    in_flight_bytes: NonZeroU64,
}

impl Admission {
    fn build(self) -> io::Result<PayloadAdmissionConfig> {
        PayloadAdmissionConfig::new(PayloadAdmissionLimits {
            database_bytes: self.database_bytes,
            author_bytes: self.author_bytes,
            overrides: self.overrides,
            in_flight_count: self.in_flight_count,
            in_flight_bytes: self.in_flight_bytes,
        })
        .ok_or_else(|| invalid("invalid admission limits"))
    }
}

/// Explicit per-turn mutation limits, not automatic machine-size tuning.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Worker {
    operations: NonZeroUsize,
    bytes: u64,
    gc_bytes: u64,
    time_ms: u64,
}

/// Bounded whole-source observation; no extrapolation above these bounds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    events: NonZeroUsize,
    authors: NonZeroUsize,
    logical_bytes: u64,
    time_ms: u64,
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Validate every account before constructing the manager or exposing HTTP.
pub(crate) async fn read(path: Option<&Path>) -> io::Result<Vec<PayloadAccount>> {
    let Some(path) = path else {
        return Ok(vec![]);
    };
    parse(&tokio::fs::read(path).await?)
}

pub(crate) fn parse(bytes: &[u8]) -> io::Result<Vec<PayloadAccount>> {
    let input: Vec<Account> = serde_json::from_slice(bytes).map_err(invalid)?;
    let mut seen = BTreeSet::new();
    input
        .into_iter()
        .map(|account| {
            if !seen.insert(account.id) {
                return Err(invalid("duplicate payload retention account"));
            }
            let config = match account.retention {
                Mode::Disabled => PayloadRetentionConfig::Disabled,
                Mode::DryRun {
                    policy,
                    admission,
                    snapshot,
                } => PayloadRetentionConfig::DryRun {
                    policy: policy.build()?,
                    admission: admission.build()?,
                    limits: DryRunLimits {
                        events: snapshot.events,
                        authors: snapshot.authors,
                        logical_bytes: snapshot.logical_bytes,
                        time: Duration::from_millis(snapshot.time_ms),
                    },
                },
                Mode::Enforce {
                    policy,
                    admission,
                    worker,
                } => PayloadRetentionConfig::Enforce {
                    policy: policy.build()?,
                    admission: admission.build()?,
                    limits: PayloadRuntimeLimits {
                        operations: worker.operations,
                        bytes: worker.bytes,
                        gc_bytes: worker.gc_bytes,
                        time: Duration::from_millis(worker.time_ms),
                    },
                },
            };
            PayloadAccount::configured(account.id, config).map_err(invalid)
        })
        .collect()
}
