use rostra_client_db::{Database, DbError, PayloadAllocation};
use rostra_core::event::EventContentRaw;
use rostra_core::event::content_kind::EventContentKind;
use snafu::ResultExt as _;

use crate::error::{PostResult, StorageSnafu};
use crate::payload_writer::PayloadWriter;

/// Serialized local content with capacity retained until the bytes are
/// released.
pub(crate) struct EncodedPayload {
    /// Encoded bytes must drop before their allocation charge.
    pub(crate) raw: EventContentRaw,
    /// Absent only for disabled admission.
    pub(crate) capacity: Option<PayloadAllocation>,
}

impl EncodedPayload {
    /// Preserve disabled serialization and budget configured output/conversion.
    pub(crate) fn encode<C: EventContentKind>(db: &Database, content: &C) -> PostResult<Self> {
        let Some(allocation) = db
            .reserve_payload_allocation(0)
            .map_err(|reason| DbError::PayloadAdmissionPaused { reason })
            .context(StorageSnafu)?
        else {
            return Ok(Self {
                raw: content.serialize_cbor()?,
                capacity: None,
            });
        };
        let mut writer = PayloadWriter::new(allocation);
        let result = content.serialize_cbor_to_writer(&mut writer);
        if let Some(reason) = writer.pause {
            return Err(DbError::PayloadAdmissionPaused { reason }).context(StorageSnafu);
        }
        result?;
        let capacity = db
            .reserve_payload_allocation(writer.bytes.len() as u64)
            .map_err(|reason| DbError::PayloadAdmissionPaused { reason })
            .context(StorageSnafu)?;
        let raw = EventContentRaw::new(std::mem::take(&mut writer.bytes));
        Ok(Self { raw, capacity })
    }
}
