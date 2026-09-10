use std::io::{Read as _, Write as _};

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD_NO_PAD;
use rand::seq::SliceRandom as _;
use rostra_core::id::RostraId;
use zeroize::Zeroizing;

use crate::body::BODY_METADATA_BYTES;
use crate::{
    Error, FRAME_OVERHEAD, MAX_HEADER_BYTES, MAX_TRIAL_KEYS, MessageBody, PublicKey, SLOT_COUNT,
    TEXT_BUCKETS, text_bucket,
};

/// Encrypt a message with exactly eight independently encapsulated native
/// slots.
///
/// The caller must select eligible devices and ensure at least one recipient
/// destination exists before invoking this function. Fewer than eight supplied
/// destinations are padded with freshly generated throwaway keys.
pub fn encrypt(body: &MessageBody, destinations: &[PublicKey]) -> Result<Vec<u8>, Error> {
    if destinations.is_empty() || destinations.len() > SLOT_COUNT {
        return Err(Error::Invalid);
    }
    let plaintext = body.encode()?;
    let bucket = text_bucket(body.text().len())?;
    let mut recipients: Vec<_> = destinations.iter().map(PublicKey::recipient).collect();
    while recipients.len() < SLOT_COUNT {
        // Identity uses a zeroizing StaticSecret; only its public key survives.
        recipients.push(age::x25519::Identity::generate().to_public());
    }
    recipients.shuffle(&mut rand::rng());
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
            .map_err(|_| Error::Encryption)?;
    let mut file = Vec::with_capacity(FRAME_OVERHEAD + bucket);
    let mut writer = encryptor
        .wrap_output(&mut file)
        .map_err(|_| Error::Encryption)?;
    writer
        .write_all(&plaintext)
        .map_err(|_| Error::Encryption)?;
    writer.finish().map_err(|_| Error::Encryption)?;
    if file.len() + 4 > FRAME_OVERHEAD + bucket {
        return Err(Error::Encryption);
    }
    let mut frame = Vec::with_capacity(FRAME_OVERHEAD + bucket);
    frame.extend_from_slice(&(file.len() as u32).to_be_bytes());
    frame.extend_from_slice(&file);
    frame.resize(FRAME_OVERHEAD + bucket, 0);
    validate_frame(&frame).map_err(|_| Error::Encryption)?;
    Ok(frame)
}

/// Validate the complete bounded native-only frame before expensive key trials.
///
/// This is structural validation, not signature or cryptographic
/// authentication. The returned slice excludes all outer padding and the fixed
/// length field.
pub fn validate_frame(frame: &[u8]) -> Result<&[u8], Error> {
    let bucket = frame
        .len()
        .checked_sub(FRAME_OVERHEAD)
        .ok_or(Error::Invalid)?;
    if !TEXT_BUCKETS.contains(&bucket) {
        return Err(Error::Invalid);
    }
    let length = u32::from_be_bytes(frame[..4].try_into().expect("bounded frame")) as usize;
    let end = 4usize.checked_add(length).ok_or(Error::Invalid)?;
    let file = frame.get(4..end).ok_or(Error::Invalid)?;
    if frame[end..].iter().any(|b| *b != 0) {
        return Err(Error::Invalid);
    }
    let header = validate_header(file)?;
    // Every allowed plaintext fits in one age 64KiB chunk, with a 16-byte
    // nonce and a single 16-byte authenticated final-chunk tag.
    if file.len() != header + 16 + BODY_METADATA_BYTES + bucket + 16 {
        return Err(Error::Invalid);
    }
    Ok(file)
}

/// Try at most eight retained epoch identities and authenticate through EOF.
///
/// Call only after verifying the signed event, content hash, length, DM kind,
/// zero aux-key and non-singleton flag. The caller must first delete expired
/// identities. `Unreadable` is not evidence of another recipient: callers with
/// more keys must resume further chunks, without externally visible callbacks.
pub fn decrypt(
    frame: &[u8],
    identities: &[age::x25519::Identity],
    signed_author: RostraId,
    local_account: RostraId,
) -> Result<MessageBody, Error> {
    if identities.is_empty() || identities.len() > MAX_TRIAL_KEYS {
        return Err(Error::Invalid);
    }
    let file = validate_frame(frame)?;
    let bucket = frame.len() - FRAME_OVERHEAD;
    let decryptor = age::Decryptor::new(file).map_err(|_| Error::Invalid)?;
    let mut reader = decryptor
        .decrypt(identities.iter().map(|key| key as &dyn age::Identity))
        .map_err(|_| Error::Unreadable)?;
    let expected = BODY_METADATA_BYTES + bucket;
    let mut plaintext = Zeroizing::new(vec![0; expected]);
    reader
        .read_exact(&mut plaintext)
        .map_err(|_| Error::Unreadable)?;
    // A successful read of a prefix is insufficient: force authenticated EOF.
    let mut trailing = [0];
    if reader.read(&mut trailing).map_err(|_| Error::Unreadable)? != 0 {
        return Err(Error::Invalid);
    }
    let body = MessageBody::decode(&plaintext, bucket)?;
    body.validate_participants(signed_author, local_account)?;
    Ok(body)
}

fn line<'a>(file: &'a [u8], offset: &mut usize) -> Result<&'a [u8], Error> {
    let remaining = file
        .get(*offset..file.len().min(MAX_HEADER_BYTES))
        .ok_or(Error::Invalid)?;
    let length = remaining
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(Error::Invalid)?;
    if length > 64 {
        return Err(Error::Invalid);
    }
    let value = &remaining[..length];
    *offset += length + 1;
    Ok(value)
}

fn decode_base64(bytes: &[u8], expected: usize) -> Result<(), Error> {
    let value = BASE64_STANDARD_NO_PAD
        .decode(bytes)
        .map_err(|_| Error::Invalid)?;
    if value.len() != expected || BASE64_STANDARD_NO_PAD.encode(&value).as_bytes() != bytes {
        return Err(Error::Invalid);
    }
    Ok(())
}

fn validate_header(file: &[u8]) -> Result<usize, Error> {
    let mut offset = 0;
    if line(file, &mut offset)? != b"age-encryption.org/v1" {
        return Err(Error::Invalid);
    }
    let mut native = 0;
    let mut grease = false;
    loop {
        let start = line(file, &mut offset)?;
        if let Some(mac) = start.strip_prefix(b"--- ") {
            decode_base64(mac, 32)?;
            if native != SLOT_COUNT {
                return Err(Error::Invalid);
            }
            return Ok(offset);
        }
        if let Some(ephemeral) = start.strip_prefix(b"-> X25519 ") {
            native += 1;
            if native > SLOT_COUNT {
                return Err(Error::Invalid);
            }
            decode_base64(ephemeral, 32)?;
            decode_base64(line(file, &mut offset)?, 32)?;
        } else {
            if grease {
                return Err(Error::Invalid);
            }
            validate_grease_start(start)?;
            grease = true;
            let mut encoded = Vec::with_capacity(132);
            loop {
                let part = line(file, &mut offset)?;
                if encoded.len() + part.len() > 132 {
                    return Err(Error::Invalid);
                }
                encoded.extend_from_slice(part);
                if part.len() < 64 {
                    break;
                }
            }
            let decoded = BASE64_STANDARD_NO_PAD
                .decode(&encoded)
                .map_err(|_| Error::Invalid)?;
            if decoded.len() > 99 || BASE64_STANDARD_NO_PAD.encode(decoded).as_bytes() != encoded {
                return Err(Error::Invalid);
            }
        }
    }
}

fn validate_grease_start(start: &[u8]) -> Result<(), Error> {
    let fields: Vec<_> = start
        .strip_prefix(b"-> ")
        .ok_or(Error::Invalid)?
        .split(|b| *b == b' ')
        .collect();
    if fields.is_empty() || fields.len() > 5 {
        return Err(Error::Invalid);
    }
    let prefix = fields[0].strip_suffix(b"-grease").ok_or(Error::Invalid)?;
    for value in std::iter::once(prefix).chain(fields[1..].iter().copied()) {
        if value.is_empty() || value.len() > 8 || !value.iter().all(|b| (33..=126).contains(b)) {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}
