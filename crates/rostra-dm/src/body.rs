use rostra_core::id::RostraId;
use zeroize::{Zeroize as _, Zeroizing};

use crate::{Error, text_bucket};

/// Fixed bytes preceding the bucket-padded UTF-8 text.
pub(crate) const BODY_METADATA_BYTES: usize = 1 + 32 + 32 + 16 + 4;

/// Authenticated logical message, before local history/provenance assignment.
///
/// No debug representation is provided: message text must not leak into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageBody {
    /// Account that must match the signed event author.
    sender: RostraId,
    /// Other conversation participant, named only inside encryption.
    recipient: RostraId,
    /// Random logical identifier, deduplicated in the sender's namespace.
    message_id: [u8; 16],
    /// Text retained as local plaintext after successful processing.
    text: String,
}

impl Drop for MessageBody {
    fn drop(&mut self) {
        self.text.zeroize();
    }
}

impl MessageBody {
    /// Return the sending account authenticated inside the body.
    pub fn sender(&self) -> RostraId {
        self.sender
    }

    /// Return the other named conversation participant.
    pub fn recipient(&self) -> RostraId {
        self.recipient
    }

    /// Return the stable logical ID assigned at creation or authenticated
    /// decode.
    pub fn message_id(&self) -> [u8; 16] {
        self.message_id
    }

    /// Borrow the immutable message text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Create a new logical send with an independently random identifier.
    pub fn new(sender: RostraId, recipient: RostraId, text: String) -> Result<Self, Error> {
        text_bucket(text.len())?;
        Ok(Self {
            sender,
            recipient,
            message_id: rand::random(),
            text,
        })
    }

    pub(crate) fn encode(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        let bucket = text_bucket(self.text.len())?;
        let mut result = Zeroizing::new(Vec::with_capacity(BODY_METADATA_BYTES + bucket));
        result.push(1);
        result.extend_from_slice(self.sender.as_slice());
        result.extend_from_slice(self.recipient.as_slice());
        result.extend_from_slice(&self.message_id);
        result.extend_from_slice(&(self.text.len() as u32).to_be_bytes());
        result.extend_from_slice(self.text.as_bytes());
        result.resize(BODY_METADATA_BYTES + bucket, 0);
        Ok(result)
    }

    pub(crate) fn decode(bytes: &[u8], bucket: usize) -> Result<Self, Error> {
        if bytes.len() != BODY_METADATA_BYTES + bucket || bytes[0] != 1 {
            return Err(Error::Invalid);
        }
        let length = u32::from_be_bytes(bytes[81..85].try_into().expect("fixed range")) as usize;
        if text_bucket(length)? != bucket
            || bytes[BODY_METADATA_BYTES + length..]
                .iter()
                .any(|b| *b != 0)
        {
            return Err(Error::Invalid);
        }
        let text = std::str::from_utf8(&bytes[BODY_METADATA_BYTES..BODY_METADATA_BYTES + length])
            .map_err(|_| Error::Invalid)?
            .to_owned();
        Ok(Self {
            sender: RostraId::from_bytes(bytes[1..33].try_into().expect("fixed range")),
            recipient: RostraId::from_bytes(bytes[33..65].try_into().expect("fixed range")),
            message_id: bytes[65..81].try_into().expect("fixed range"),
            text,
        })
    }

    /// Bind decrypted participants to the authenticated event and local
    /// account.
    pub fn validate_participants(&self, author: RostraId, local: RostraId) -> Result<(), Error> {
        if self.sender != author || (self.sender != local && self.recipient != local) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
