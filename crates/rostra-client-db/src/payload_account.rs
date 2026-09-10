use std::sync::Arc;

use rostra_core::id::RostraId;

use crate::payload_reservation::AdmissionLedger;
use crate::{
    Database, PayloadAdmissionPause, PayloadAllocation, PayloadRetentionConfig, RetentionGeneration,
};

/// Startup attachment failures, distinct from temporary acquisition pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, snafu::Snafu)]
pub enum PayloadAccountAttachError {
    /// The ledger belongs to a different storing account.
    #[snafu(display("Payload account {account} does not match database {database}"))]
    IdentityMismatch {
        /// Identity recorded by the database.
        database: RostraId,
        /// Identity recorded by the proposed account ledger.
        account: RostraId,
    },
    /// This database already has immutable account ownership.
    #[snafu(display("Database already has a payload account attached"))]
    DatabaseAlreadyAttached,
    /// Attachment cannot replace an independently configured ledger.
    #[snafu(display("Database payload admission is already configured"))]
    DatabaseConfigured,
    /// The original database ledger still owns acquisitions or buffers.
    #[snafu(display("Database payload admission still owns capacity"))]
    DatabaseBusy,
    /// Another database still owns this account ledger.
    #[snafu(display("Payload account is already attached to another database"))]
    AccountAlreadyAttached,
    /// Startup allowances must be valid before any account is published.
    #[snafu(display("Invalid payload retention startup allowances"))]
    InvalidConfiguration,
}

/// Account-scoped acquisition ownership, available before opening its database.
///
/// Constructing this handle performs no filesystem work. Clones share the same
/// ledger, including provisional HTTP allocations made before verification.
#[derive(Debug, Clone)]
pub struct PayloadAccount {
    /// Storing account, independent of transport endpoint identity.
    id: RostraId,
    /// Shared logical reservations and provisional buffer ownership.
    ledger: Arc<AdmissionLedger>,
    /// Startup-only policy, independent of any disposable database index.
    config: PayloadRetentionConfig,
}

impl PayloadAccount {
    /// Construct a disabled account ledger without creating or loading storage.
    pub fn disabled(id: RostraId) -> Self {
        Self {
            id,
            ledger: Arc::default(),
            config: PayloadRetentionConfig::Disabled,
        }
    }

    /// Validate and install explicit policy before exposing an account or body.
    ///
    /// This performs no filesystem work. Tight allocation limits are valid but
    /// can refuse HTTP's five provisional slots or read/conversion overlap; no
    /// path borrows uncharged capacity to make progress.
    pub fn configured(
        id: RostraId,
        config: PayloadRetentionConfig,
    ) -> Result<Self, PayloadAccountAttachError> {
        let account = Self {
            config,
            ..Self::disabled(id)
        };
        match &account.config {
            PayloadRetentionConfig::Disabled => {}
            PayloadRetentionConfig::Enforce {
                policy,
                admission,
                limits,
            } => {
                crate::payload_runtime::PayloadRuntime::new(
                    RetentionGeneration::new(*policy, id),
                    admission.identity(),
                    *limits,
                )
                .ok_or(PayloadAccountAttachError::InvalidConfiguration)?;
                account.ledger.state.lock().unwrap().config = Some(admission.clone());
            }
            PayloadRetentionConfig::DryRun {
                admission, limits, ..
            } => {
                crate::payload_dry_run::DryRun::new(admission.clone(), *limits)
                    .ok_or(PayloadAccountAttachError::InvalidConfiguration)?;
            }
        }
        Ok(account)
    }

    /// Return the storing identity to which this ledger can be attached.
    pub fn id(&self) -> RostraId {
        self.id
    }

    /// Reserve provisional capacity before reading an unverified request body.
    ///
    /// A disabled ledger returns `Ok(None)` without charging capacity. When
    /// Enforce admission is configured, capacity exhaustion returns a temporary
    /// admission pause; successful ownership must remain alive through the
    /// covered bytes.
    pub fn reserve_payload_allocation(
        &self,
        bytes: u64,
    ) -> Result<Option<PayloadAllocation>, PayloadAdmissionPause> {
        PayloadAllocation::reserve(&self.ledger, bytes)
    }
}

impl Database {
    /// Attach account ownership before publishing a newly opened database.
    ///
    /// The supplied account must match this database's storing identity. This
    /// rejects a database that already has an account attachment, or whose
    /// original/default ledger is configured or owns acquisitions or buffers.
    /// It also rejects a supplied account still attached to another database.
    ///
    /// After a previous attached database is dropped, reattaching its account
    /// invalidates old logical reservations without releasing outstanding
    /// buffer or provisional charges. Those charges stay until their owners
    /// drop; lease IDs are never reset. Provisional allocations made before
    /// attachment may bind to new logical reservations after attachment.
    ///
    /// Failures are structural [`PayloadAccountAttachError`] values, not
    /// transient acquisition pressure. Exclusive database access is required:
    /// attach before publishing the database to client operations.
    pub fn attach_payload_account(
        &mut self,
        account: &PayloadAccount,
    ) -> Result<(), PayloadAccountAttachError> {
        if self.self_id != account.id {
            return Err(PayloadAccountAttachError::IdentityMismatch {
                database: self.self_id,
                account: account.id,
            });
        }
        if self.payload_account_owner.is_some() {
            return Err(PayloadAccountAttachError::DatabaseAlreadyAttached);
        }
        let state = self.payload_admission.state.lock().unwrap();
        if state.config.is_some() {
            return Err(PayloadAccountAttachError::DatabaseConfigured);
        }
        if !state.events.is_empty() || state.buffers != 0 {
            return Err(PayloadAccountAttachError::DatabaseBusy);
        }
        drop(state);
        let runtime = match &account.config {
            PayloadRetentionConfig::Disabled => None,
            PayloadRetentionConfig::Enforce {
                policy,
                admission,
                limits,
            } => Some(
                crate::payload_runtime::PayloadRuntime::new(
                    RetentionGeneration::new(*policy, account.id),
                    admission.identity(),
                    *limits,
                )
                .ok_or(PayloadAccountAttachError::InvalidConfiguration)?,
            ),
            PayloadRetentionConfig::DryRun {
                policy,
                admission,
                limits,
            } => Some(
                crate::payload_runtime::PayloadRuntime::new_dry_run(
                    self,
                    RetentionGeneration::new(*policy, account.id),
                    admission.clone(),
                    *limits,
                )
                .map_err(|_| PayloadAccountAttachError::InvalidConfiguration)?,
            ),
        };
        let mut demands = account.ledger.demands.lock().unwrap();
        let mut state = account.ledger.state.lock().unwrap();
        if state.database_owner.upgrade().is_some() {
            return Err(PayloadAccountAttachError::AccountAlreadyAttached);
        }
        let owner = Arc::new(());
        state.database_owner = Arc::downgrade(&owner);
        // Logical leases belong to the previous database attachment. Provisional
        // and outstanding buffer bytes remain charged until their owners drop.
        account.ledger.invalidate_pressure();
        state.events.clear();
        demands.clear();
        self.payload_account_owner = Some(owner);
        self.payload_admission = account.ledger.clone();
        self.payload_runtime = runtime;
        Ok(())
    }
}
