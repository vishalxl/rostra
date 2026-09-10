use std::str::FromStr as _;

use crate::Error;

/// A canonical contributory native age public key, safe to give to encryption.
///
/// Account authorization does not establish private-key possession or exclusive
/// ownership. Equal keys on different devices remain independently wrapped
/// slots.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// Canonical raw Montgomery coordinate.
    bytes: [u8; 32],
    /// Library recipient, constructed only after public-input validation.
    recipient: age::x25519::Recipient,
}

impl PublicKey {
    /// Validate a canonical field element and reject non-contributory inputs.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, Error> {
        let mut modulus = [0xff; 32];
        modulus[0] = 0xed;
        modulus[31] = 0x7f;
        if bytes.iter().rev().cmp(modulus.iter().rev()) != std::cmp::Ordering::Less
            || x25519_dalek::x25519([0x42; 32], bytes) == [0; 32]
        {
            return Err(Error::Invalid);
        }
        let encoded = bech32::encode::<bech32::Bech32>(bech32::Hrp::parse_unchecked("age"), &bytes)
            .map_err(|_| Error::Invalid)?;
        let recipient = age::x25519::Recipient::from_str(&encoded).map_err(|_| Error::Invalid)?;
        Ok(Self { bytes, recipient })
    }

    /// Parse the canonical lowercase native age recipient representation.
    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.len() != 62 {
            return Err(Error::Invalid);
        }
        let (hrp, bytes) = bech32::decode(value).map_err(|_| Error::Invalid)?;
        if hrp != bech32::Hrp::parse_unchecked("age") {
            return Err(Error::Invalid);
        }
        let key = Self::from_bytes(bytes.try_into().map_err(|_| Error::Invalid)?)?;
        if key.recipient.to_string() != value {
            return Err(Error::Invalid);
        }
        Ok(key)
    }

    /// Return the canonical public bytes for announcement encoding.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.bytes
    }

    pub(crate) fn recipient(&self) -> age::x25519::Recipient {
        self.recipient.clone()
    }
}
