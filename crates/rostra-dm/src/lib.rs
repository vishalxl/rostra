//! The version-one native-X25519 age envelope profile for direct messages.
//!
//! This module accepts only a bounded, eight-slot native age file. Event
//! authentication, key lifetime enforcement, durable history, and session
//! access control belong to the integrating client, not this encryption
//! boundary.

mod announcement;
mod body;
mod device;
mod envelope;
mod epoch;
mod public_key;

pub use announcement::{
    Announcement, DM_FUTURE_ANNOUNCEMENT_SKEW, DM_KEY_RECEIVE_GRACE, DM_KEY_SEND_WINDOW,
    DM_MAX_ADVERTISED_KEY_LIFETIME, EpochPublic,
};
pub use body::MessageBody;
pub use device::DeviceState;
pub use envelope::{decrypt, encrypt, validate_frame};
pub use epoch::LocalEpoch;
pub use public_key::PublicKey;

/// Number of native cryptographic slots, including fresh throwaway recipients.
pub const SLOT_COUNT: usize = 8;
/// Maximum UTF-8 text length in bytes, independent of metadata.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Accepted version-one text buckets; changing these requires wire
/// compatibility.
pub const TEXT_BUCKETS: [usize; 5] = [1024, 2048, 4096, 8192, 16384];
/// Fixed external overhead, including the four-byte age file length.
pub const FRAME_OVERHEAD: usize = 2048;
/// Upper bound on one externally visible frame.
pub const MAX_FRAME_BYTES: usize = FRAME_OVERHEAD + MAX_TEXT_BYTES;
/// Upper bound on the native header including its MAC.
pub const MAX_HEADER_BYTES: usize = 1536;
/// Maximum identities accepted in one bounded decryption operation.
///
/// Callers with more retained keys must schedule additional bounded operations;
/// they must not silently discard still-live keys.
pub const MAX_TRIAL_KEYS: usize = 8;

/// Controlled rejection of invalid inputs without secret-bearing diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A malformed, unsupported, or out-of-bounds encoding.
    Invalid,
    /// No supplied identity could authenticate and open the complete envelope.
    Unreadable,
    /// The encryption library failed to produce the expected complete profile.
    Encryption,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid direct message",
            Self::Unreadable => "unreadable direct message",
            Self::Encryption => "direct message encryption failed",
        })
    }
}

impl std::error::Error for Error {}

/// Find the unique smallest bucket that holds this text.
pub fn text_bucket(length: usize) -> Result<usize, Error> {
    TEXT_BUCKETS
        .into_iter()
        .find(|bucket| length <= *bucket)
        .ok_or(Error::Invalid)
}

#[cfg(test)]
mod tests;
