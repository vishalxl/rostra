use rostra_client_db::{Database, DbError, DbResult, PayloadBuffer, PayloadIngestOutcome};
use rostra_core::event::VerifiedEventContent;

/// A winning download whose buffer remains charged through database ingestion.
pub(crate) struct AcquiredPayload {
    /// Drop bytes before releasing the guard, including error/cancellation
    /// paths.
    pub(crate) content: VerifiedEventContent,
    /// One attempt's pre-read capacity; absent only when admission is disabled.
    pub(crate) buffer: Option<PayloadBuffer>,
}

impl AcquiredPayload {
    pub(crate) async fn ingest_dm(
        self,
        db: &Database,
        body: &rostra_dm::MessageBody,
        permit: &rostra_client_db::dm::SendPermit,
    ) -> DbResult<()> {
        db.dm_commit_outgoing(&self.content, body, permit, self.buffer.as_ref())
            .await
    }

    /// Apply ordinary validation while preserving temporary capacity refusals.
    pub(crate) async fn ingest(self, db: &Database) -> DbResult<()> {
        match db
            .try_process_admitted_event_content(&self.content, self.buffer.as_ref())
            .await?
        {
            PayloadIngestOutcome::Deferred(reason) => {
                Err(DbError::PayloadAdmissionPaused { reason })
            }
            _ => Ok(()),
        }
    }
}
