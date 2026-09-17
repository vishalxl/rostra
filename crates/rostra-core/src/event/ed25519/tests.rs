use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};

use super::*;

const MALFORMED_VERIFYING_KEY: [u8; 32] = [2; 32];

#[test]
fn raw_signature_verification_accepts_valid_signature() {
    let signing_key = SigningKey::from_bytes(&[7; 32]);
    let bytes = b"signed event bytes";
    let signature = signing_key.sign(bytes).into();
    let author = signing_key.verifying_key().into();

    assert!(Event::verify_signature_raw(bytes, signature, author).is_ok());
}

#[test]
fn raw_signature_verification_rejects_wrong_signature() {
    let signing_key = SigningKey::from_bytes(&[7; 32]);
    let signature = signing_key.sign(b"other event bytes").into();
    let author = signing_key.verifying_key().into();

    assert!(Event::verify_signature_raw(b"signed event bytes", signature, author).is_err());
}

#[test]
fn raw_signature_verification_rejects_malformed_author() {
    assert!(VerifyingKey::try_from(MALFORMED_VERIFYING_KEY.as_slice()).is_err());

    let result = Event::verify_signature_raw(
        b"signed event bytes",
        EventSignature::from_bytes([0; 64]),
        RostraId::from_bytes(MALFORMED_VERIFYING_KEY),
    );

    assert!(result.is_err());
}

#[cfg(feature = "bincode")]
#[test]
fn received_event_verification_rejects_decoded_malformed_author() {
    use crate::bincode::STD_BINCODE_CONFIG;
    use crate::event::{EventKind, SignedEvent, VerifiedEvent, VerifiedEventError};

    assert!(VerifyingKey::try_from(MALFORMED_VERIFYING_KEY.as_slice()).is_err());

    let signed = SignedEvent::unverified(
        Event::builder_raw_content()
            .author(RostraId::from_bytes(MALFORMED_VERIFYING_KEY))
            .kind(EventKind::RAW)
            .build(),
        EventSignature::from_bytes([0; 64]),
    );
    let encoded =
        bincode::encode_to_vec(signed, STD_BINCODE_CONFIG).expect("encoding must succeed");
    let (decoded, bytes_read): (SignedEvent, _) =
        bincode::decode_from_slice(&encoded, STD_BINCODE_CONFIG).expect("decoding must succeed");

    assert_eq!(bytes_read, encoded.len());
    assert!(matches!(
        VerifiedEvent::verify_received_as_is(decoded),
        Err(VerifiedEventError::SignatureInvalid { .. })
    ));
}
