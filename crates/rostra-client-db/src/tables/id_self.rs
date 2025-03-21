use bincode::{Decode, Encode};
use rostra_core::id::RostraId;

#[derive(Debug, Encode, Decode, Clone, Copy)]
pub struct IdSelfAccountRecord {
    pub rostra_id: RostraId,
    pub iroh_secret: [u8; 32],
}
