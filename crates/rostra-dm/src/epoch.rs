use std::str::FromStr;

use age::secrecy::ExposeSecret as _;
use zeroize::Zeroize as _;

use crate::{DM_KEY_RECEIVE_GRACE, DM_KEY_SEND_WINDOW, EpochPublic, Error, PublicKey};

/// Persisted installation-local secret and the deadlines assigned at creation.
///
/// This is authoritative, non-replayable storage. Do not export it, derive it
/// from account recovery credentials, or retain cloned values across background
/// work. Live-memory cleanup does not erase database pages or backups.
#[derive(bincode::Encode, bincode::Decode)]
pub struct LocalEpoch {
    public: EpochPublic,
    secret: String,
}

impl Drop for LocalEpoch {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl LocalEpoch {
    /// Generate an independent random key and assign this installation's
    /// current defaults exactly once. The owner must atomically persist it
    /// before use.
    pub fn generate(now: u64) -> Result<Self, Error> {
        let send_until = now.checked_add(DM_KEY_SEND_WINDOW).ok_or(Error::Invalid)?;
        let decrypt_until = send_until
            .checked_add(DM_KEY_RECEIVE_GRACE)
            .ok_or(Error::Invalid)?;
        let identity = age::x25519::Identity::generate();
        let public = EpochPublic {
            public_key: PublicKey::parse(&identity.to_public().to_string())?.to_bytes(),
            send_from: now,
            send_until,
            decrypt_until,
        };
        public.validate()?;
        Ok(Self {
            public,
            secret: identity.to_string().expose_secret().to_owned(),
        })
    }

    /// Immutable public metadata; clock correction never edits these deadlines.
    pub fn public(&self) -> &EpochPublic {
        &self.public
    }

    /// Load a live identity for one bounded background operation.
    ///
    /// The owner must durably purge expired rows before calling this method and
    /// must drop the returned identity before the next scheduling boundary.
    pub fn identity(&self, now: u64) -> Result<age::x25519::Identity, Error> {
        self.public.validate()?;
        if self.public.decrypt_until <= now {
            return Err(Error::Unreadable);
        }
        let identity = age::x25519::Identity::from_str(&self.secret).map_err(|_| Error::Invalid)?;
        if PublicKey::parse(&identity.to_public().to_string())?.to_bytes() != self.public.public_key
        {
            return Err(Error::Invalid);
        }
        Ok(identity)
    }
}
