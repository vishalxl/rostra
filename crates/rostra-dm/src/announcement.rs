use crate::{Error, PublicKey};

/// Local sending duration assigned only when a new epoch is generated.
pub const DM_KEY_SEND_WINDOW: u64 = 7 * 24 * 60 * 60;
/// Local receiving grace assigned only when a new epoch is generated.
pub const DM_KEY_RECEIVE_GRACE: u64 = 28 * 24 * 60 * 60;
/// Independent wire sanity bound, not a local erasure guarantee.
pub const DM_MAX_ADVERTISED_KEY_LIFETIME: u64 = 365 * 24 * 60 * 60;
/// Future publication tolerance; never added to a key deadline.
pub const DM_FUTURE_ANNOUNCEMENT_SKEW: u64 = 5 * 60;

/// Public epoch key with immutable absolute Unix-second deadlines.
#[derive(Clone, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub struct EpochPublic {
    /// Canonical contributory X25519 public key.
    pub public_key: [u8; 32],
    /// Inclusive sending start.
    pub send_from: u64,
    /// Exclusive sending end.
    pub send_until: u64,
    /// Exclusive live decryption end.
    pub decrypt_until: u64,
}

impl EpochPublic {
    /// Check timeless wire constraints without changing signed deadlines.
    pub fn validate(&self) -> Result<(), Error> {
        if self.send_from >= self.send_until
            || self.send_until > self.decrypt_until
            || self.decrypt_until > i64::MAX as u64
            || self.decrypt_until - self.send_from > DM_MAX_ADVERTISED_KEY_LIFETIME
        {
            return Err(Error::Invalid);
        }
        PublicKey::from_bytes(self.public_key)?;
        Ok(())
    }

    /// Check eligibility of the selected newest announcement, without fallback.
    pub fn eligible(&self, now: u64, published_at: u64) -> bool {
        published_at <= now.saturating_add(DM_FUTURE_ANNOUNCEMENT_SKEW)
            && self.send_from <= now
            && now < self.send_until
    }
}

/// Account-signed installation announcement or permanent retirement.
#[derive(Clone, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub struct Announcement {
    /// Stable random installation ID; re-enrollment uses a new ID.
    pub device_id: [u8; 16],
    /// `None` permanently retires this ID regardless of arrival order.
    pub epoch: Option<EpochPublic>,
}

impl Announcement {
    /// Encode the version-one fixed-width announcement.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = vec![1, u8::from(self.epoch.is_some())];
        out.extend_from_slice(&self.device_id);
        if let Some(epoch) = &self.epoch {
            epoch.validate()?;
            out.extend_from_slice(&epoch.public_key);
            out.extend_from_slice(&epoch.send_from.to_be_bytes());
            out.extend_from_slice(&epoch.send_until.to_be_bytes());
            out.extend_from_slice(&epoch.decrypt_until.to_be_bytes());
        }
        Ok(out)
    }

    /// Decode canonical bytes and prohibit advance publication.
    pub fn decode(bytes: &[u8], published_at: u64) -> Result<Self, Error> {
        if !matches!(bytes.len(), 18 | 74) || bytes[0] != 1 {
            return Err(Error::Invalid);
        }
        let epoch = match (bytes[1], bytes.len()) {
            (0, 18) => None,
            (1, 74) => {
                let epoch = EpochPublic {
                    public_key: bytes[18..50].try_into().expect("fixed range"),
                    send_from: u64::from_be_bytes(bytes[50..58].try_into().expect("fixed range")),
                    send_until: u64::from_be_bytes(bytes[58..66].try_into().expect("fixed range")),
                    decrypt_until: u64::from_be_bytes(
                        bytes[66..74].try_into().expect("fixed range"),
                    ),
                };
                epoch.validate()?;
                if published_at < epoch.send_from || published_at > i64::MAX as u64 {
                    return Err(Error::Invalid);
                }
                Some(epoch)
            }
            _ => return Err(Error::Invalid),
        };
        Ok(Self {
            device_id: bytes[2..18].try_into().expect("fixed range"),
            epoch,
        })
    }
}
