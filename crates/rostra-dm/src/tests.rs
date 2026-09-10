use base64::Engine as _;
use base64::prelude::BASE64_STANDARD_NO_PAD;
use rostra_core::id::RostraId;

use crate::{
    Error, FRAME_OVERHEAD, MAX_TEXT_BYTES, MessageBody, PublicKey, TEXT_BUCKETS, decrypt, encrypt,
    validate_frame,
};

#[test]
fn announcements_are_canonical_and_deadlines_are_not_extended() {
    use crate::{Announcement, DM_MAX_ADVERTISED_KEY_LIFETIME, EpochPublic};
    let identity = age::x25519::Identity::generate();
    let epoch = EpochPublic {
        public_key: key(&identity).to_bytes(),
        send_from: 1000,
        send_until: 2000,
        decrypt_until: 3000,
    };
    let announcement = Announcement {
        device_id: [9; 16],
        epoch: Some(epoch.clone()),
    };
    let bytes = announcement.encode().unwrap();
    assert_eq!(bytes.len(), 74);
    assert!(Announcement::decode(&bytes, 1000).unwrap() == announcement);
    assert!(Announcement::decode(&bytes, 999).is_err());
    for length in 0..bytes.len() {
        assert!(Announcement::decode(&bytes[..length], 1000).is_err());
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(Announcement::decode(&extra, 1000).is_err());
    assert!(!epoch.eligible(999, 1000));
    assert!(epoch.eligible(1000, 1300));
    assert!(!epoch.eligible(1000, 1301));
    assert!(epoch.eligible(1999, 1000));
    assert!(!epoch.eligible(2000, 1000));
    let retirement = Announcement {
        device_id: [9; 16],
        epoch: None,
    };
    assert!(Announcement::decode(&retirement.encode().unwrap(), 0).unwrap() == retirement);
    for (start, end, decrypt) in [
        (1000, 1000, 2000),
        (1000, 2000, 1999),
        (0, 1, DM_MAX_ADVERTISED_KEY_LIFETIME + 1),
        (u64::MAX - 2, u64::MAX - 1, u64::MAX),
    ] {
        let invalid = EpochPublic {
            send_from: start,
            send_until: end,
            decrypt_until: decrypt,
            ..epoch.clone()
        };
        assert!(invalid.validate().is_err());
    }
    let long = EpochPublic {
        decrypt_until: 1000 + DM_MAX_ADVERTISED_KEY_LIFETIME,
        ..epoch
    };
    assert!(long.validate().is_ok());
}

fn body(length: usize) -> MessageBody {
    MessageBody::new(
        RostraId::from_bytes([1; 32]),
        RostraId::from_bytes([2; 32]),
        "x".repeat(length),
    )
    .unwrap()
}

fn key(identity: &age::x25519::Identity) -> PublicKey {
    PublicKey::parse(&identity.to_public().to_string()).unwrap()
}

#[test]
fn device_reduction_is_order_independent_and_retirement_is_sticky() {
    use crate::{Announcement, DeviceState, EpochPublic};
    let identity = age::x25519::Identity::generate();
    let active = Announcement {
        device_id: [7; 16],
        epoch: Some(EpochPublic {
            public_key: key(&identity).to_bytes(),
            send_from: 1000,
            send_until: 2000,
            decrypt_until: 3000,
        }),
    };
    let retirement = Announcement {
        epoch: None,
        ..active.clone()
    };
    let low = rostra_core::ShortEventId::from_bytes([1; 16]);
    let high = rostra_core::ShortEventId::from_bytes([2; 16]);
    let mut a = DeviceState::default();
    a.apply(&active, 1000, low);
    a.apply(&active, 1000, high);
    let mut b = DeviceState::default();
    b.apply(&active, 1000, high);
    b.apply(&active, 1000, low);
    assert!(a == b);
    assert_eq!(a.latest().unwrap().1, high);
    let future = Announcement {
        epoch: Some(EpochPublic {
            send_from: 5000,
            send_until: 6000,
            decrypt_until: 7000,
            ..active.epoch.clone().unwrap()
        }),
        ..active.clone()
    };
    a.apply(&future, 5000, low);
    let (timestamp, _, epoch) = a.latest().unwrap();
    assert!(!epoch.eligible(1500, timestamp));
    assert!(epoch.eligible(5000, timestamp));
    assert!(!epoch.eligible(6000, timestamp));
    for retired_first in [false, true] {
        let mut state = DeviceState::default();
        if retired_first {
            state.apply(&retirement, 1, low);
        }
        state.apply(&active, 1000, high);
        state.apply(&future, 5000, high);
        if !retired_first {
            state.apply(&retirement, 1, low);
        }
        state.apply(&active, 9000, high);
        assert!(state.retired());
        assert!(state.latest().is_none());
    }
}

#[test]
fn local_epochs_keep_assigned_deadlines_and_reject_expired_trials() {
    let first = crate::LocalEpoch::generate(1000).unwrap();
    let second = crate::LocalEpoch::generate(1000).unwrap();
    assert_ne!(first.public().public_key, second.public().public_key);
    assert_eq!(first.public().send_until, 1000 + crate::DM_KEY_SEND_WINDOW);
    assert_eq!(
        first.public().decrypt_until,
        first.public().send_until + crate::DM_KEY_RECEIVE_GRACE
    );
    let encoded = zeroize::Zeroizing::new(
        bincode::encode_to_vec(&first, bincode::config::standard()).unwrap(),
    );
    let (restored, consumed): (crate::LocalEpoch, _) =
        bincode::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
    assert_eq!(consumed, encoded.len());
    assert!(restored.public() == first.public());
    assert!(
        restored
            .identity(restored.public().decrypt_until - 1)
            .is_ok()
    );
    assert!(matches!(
        restored.identity(restored.public().decrypt_until),
        Err(Error::Unreadable)
    ));
    // Backward wall-clock correction does not edit or extend assigned deadlines.
    assert!(restored.identity(999).is_ok());
    assert!(restored.public() == first.public());
    assert!(crate::LocalEpoch::generate(u64::MAX).is_err());
}

#[test]
fn every_bucket_roundtrips_with_eight_actual_slots() {
    let identity = age::x25519::Identity::generate();
    let destination = key(&identity);
    for length in [0, 1, 1024, 1025, 2048, 2049, 4096, 4097, 8192, 8193, 16384] {
        let message = body(length);
        let frame = encrypt(&message, std::slice::from_ref(&destination)).unwrap();
        let bucket = TEXT_BUCKETS.into_iter().find(|b| length <= *b).unwrap();
        assert_eq!(frame.len(), FRAME_OVERHEAD + bucket);
        let file = validate_frame(&frame).unwrap();
        assert_eq!(file.windows(10).filter(|w| *w == b"-> X25519 ").count(), 8);
        let decoded = decrypt(
            &frame,
            std::slice::from_ref(&identity),
            message.sender(),
            message.recipient(),
        )
        .unwrap();
        assert!(decoded == message);
    }
}

#[test]
fn every_real_slot_count_has_the_same_profile_and_all_selected_keys_decrypt() {
    let identities: Vec<_> = (0..8).map(|_| age::x25519::Identity::generate()).collect();
    let outsider = age::x25519::Identity::generate();
    let destinations: Vec<_> = identities.iter().map(key).collect();
    let message = body(1025);
    for real_count in 1..=8 {
        let frame = encrypt(&message, &destinations[..real_count]).unwrap();
        assert_eq!(frame.len(), FRAME_OVERHEAD + 2048);
        let file = validate_frame(&frame).unwrap();
        assert_eq!(file.windows(10).filter(|w| *w == b"-> X25519 ").count(), 8);
        for identity in &identities[..real_count] {
            let decoded = decrypt(
                &frame,
                std::slice::from_ref(identity),
                message.sender(),
                message.recipient(),
            )
            .unwrap();
            assert!(decoded == message);
        }
        for identity in identities[real_count..].iter().chain([&outsider]) {
            assert!(matches!(
                decrypt(
                    &frame,
                    std::slice::from_ref(identity),
                    message.sender(),
                    message.recipient(),
                ),
                Err(Error::Unreadable)
            ));
        }
    }
}

#[test]
fn every_selected_key_decrypts_and_duplicates_are_independent_slots() {
    let identities: Vec<_> = (0..8).map(|_| age::x25519::Identity::generate()).collect();
    let message = body(12);
    let destinations: Vec<_> = identities.iter().map(key).collect();
    let frame = encrypt(&message, &destinations).unwrap();
    for identity in &identities {
        assert!(
            decrypt(
                &frame,
                std::slice::from_ref(identity),
                message.sender(),
                message.sender()
            )
            .is_ok()
        );
    }
    let duplicate_frame = encrypt(&message, &vec![destinations[0].clone(); 8]).unwrap();
    assert!(
        decrypt(
            &duplicate_frame,
            &identities[..1],
            message.sender(),
            message.recipient()
        )
        .is_ok()
    );
    assert_ne!(frame, encrypt(&message, &destinations).unwrap());
}

#[test]
fn rejects_out_of_bounds_and_wrong_participants() {
    assert!(
        MessageBody::new(
            body(0).sender(),
            body(0).recipient(),
            "x".repeat(MAX_TEXT_BYTES + 1)
        )
        .is_err()
    );
    let identity = age::x25519::Identity::generate();
    let message = body(1);
    assert!(encrypt(&message, &[]).is_err());
    assert!(encrypt(&message, &vec![key(&identity); 9]).is_err());
    let frame = encrypt(&message, &[key(&identity)]).unwrap();
    let stranger = RostraId::from_bytes([3; 32]);
    assert!(
        decrypt(
            &frame,
            std::slice::from_ref(&identity),
            stranger,
            message.recipient()
        )
        .is_err()
    );
    assert!(
        decrypt(
            &frame,
            std::slice::from_ref(&identity),
            message.sender(),
            stranger
        )
        .is_err()
    );
    assert!(decrypt(&frame, &[], message.sender(), message.recipient()).is_err());
    assert!(
        decrypt(
            &frame,
            &vec![identity; 9],
            message.sender(),
            message.recipient()
        )
        .is_err()
    );
    let other = age::x25519::Identity::generate();
    assert!(matches!(
        decrypt(&frame, &[other], message.sender(), message.recipient()),
        Err(Error::Unreadable)
    ));
}

#[test]
fn noncontributory_and_noncanonical_public_keys_fail_without_encryption() {
    let mut one = [0; 32];
    one[0] = 1;
    let mut p = [0xff; 32];
    p[0] = 0xed;
    p[31] = 0x7f;
    let mut p_minus_one = p;
    p_minus_one[0] -= 1;
    let mut p_plus_one = p;
    p_plus_one[0] += 1;
    for bytes in [[0; 32], one, p, p_minus_one, p_plus_one, [0xff; 32]] {
        assert!(PublicKey::from_bytes(bytes).is_err());
    }
    // The two nontrivial canonical low-order Montgomery coordinates, also
    // covered by the upstream libsodium X25519 blocklist.
    for bytes in [
        [
            0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f,
            0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16,
            0x5f, 0x49, 0xb8, 0x00,
        ],
        [
            0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83,
            0xef, 0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd,
            0xd0, 0x9f, 0x11, 0x57,
        ],
    ] {
        assert!(PublicKey::from_bytes(bytes).is_err());
        let mut alias = bytes;
        alias[31] |= 0x80;
        assert!(PublicKey::from_bytes(alias).is_err());
    }
    let public = age::x25519::Identity::generate().to_public().to_string();
    assert!(PublicKey::parse(&public.to_uppercase()).is_err());
    assert!(PublicKey::parse("not a key").is_err());
}

fn structural_frame(native_count: usize, grease: &[u8]) -> Vec<u8> {
    let mut file = b"age-encryption.org/v1\n".to_vec();
    let key = BASE64_STANDARD_NO_PAD.encode([7; 32]);
    for _ in 0..native_count {
        file.extend_from_slice(format!("-> X25519 {key}\n{key}\n").as_bytes());
    }
    file.extend_from_slice(grease);
    file.extend_from_slice(format!("--- {key}\n").as_bytes());
    file.resize(file.len() + 16 + 85 + 1024 + 16, 0);
    let mut frame = (file.len() as u32).to_be_bytes().to_vec();
    frame.extend(file);
    frame.resize(FRAME_OVERHEAD + 1024, 0);
    frame
}

fn grease(body_length: usize) -> Vec<u8> {
    let mut stanza = b"-> abcdefgh-grease abcdefgh abcdefgh abcdefgh abcdefgh\n".to_vec();
    let encoded = BASE64_STANDARD_NO_PAD.encode(vec![8; body_length]);
    for chunk in encoded.as_bytes().chunks(64) {
        stanza.extend_from_slice(chunk);
        stanza.push(b'\n');
    }
    if encoded.len().is_multiple_of(64) {
        stanza.push(b'\n');
    }
    stanza
}

#[test]
fn every_grease_body_length_and_maximum_header_fit_without_retry() {
    let identity = age::x25519::Identity::generate();
    let sender = RostraId::from_bytes([1; 32]);
    let recipient = RostraId::from_bytes([2; 32]);
    assert!(validate_frame(&structural_frame(8, b"")).is_ok());
    for length in 0..100 {
        let stanza = grease(length);
        assert!(stanza.len() <= 190);
        let frame = structural_frame(8, &stanza);
        assert!(validate_frame(&frame).is_ok());
        let result = std::panic::catch_unwind(|| {
            decrypt(&frame, std::slice::from_ref(&identity), sender, recipient)
        });
        assert!(result.is_ok_and(|result| result.is_err()));

        // GREASE may precede native slots; both parsers must safely accept its
        // structure even though these synthetic stanzas do not authenticate.
        let mut reordered = frame.clone();
        let grease_start = 4 + 22 + 8 * 98;
        reordered[4 + 22..grease_start + stanza.len()].rotate_right(stanza.len());
        assert!(validate_frame(&reordered).is_ok());
        let result = std::panic::catch_unwind(|| {
            decrypt(
                &reordered,
                std::slice::from_ref(&identity),
                sender,
                recipient,
            )
        });
        assert!(result.is_ok_and(|result| result.is_err()));
    }
    let maximum = structural_frame(8, &grease(99));
    assert_eq!(
        u32::from_be_bytes(maximum[..4].try_into().unwrap()),
        1044 + 16 + 85 + 1024 + 16
    );
}

#[test]
fn profile_rejects_slot_count_and_grease_expansion() {
    for count in [0, 1, 7, 9, 12] {
        assert!(validate_frame(&structural_frame(count, &[])).is_err());
    }
    assert!(validate_frame(&structural_frame(8, &grease(100))).is_err());
    let mut twice = grease(0);
    twice.extend(grease(0));
    assert!(validate_frame(&structural_frame(8, &twice)).is_err());
    for malformed in [
        b"-> a-plugin\n\n".as_slice(),
        b"-> -grease\n\n".as_slice(),
        b"-> abcdefghi-grease\n\n".as_slice(),
        b"-> a-grease a b c d e\n\n".as_slice(),
        b"-> a-grease a  b\n\n".as_slice(),
        b"-> a-grease \x7f\n\n".as_slice(),
        b"-> a-grease\nYQ==\n".as_slice(),
        b"-> a-grease\n!!\n".as_slice(),
    ] {
        assert!(validate_frame(&structural_frame(8, malformed)).is_err());
    }
}

#[test]
fn authenticated_header_reordering_and_payload_truncation_are_rejected() {
    let identity = age::x25519::Identity::generate();
    let message = body(16);
    let mut frame = encrypt(&message, &[key(&identity)]).unwrap();
    let start = 4 + 22;
    let first = frame[start..start + 98].to_vec();
    let second = frame[start + 98..start + 196].to_vec();
    frame[start..start + 98].copy_from_slice(&second);
    frame[start + 98..start + 196].copy_from_slice(&first);
    assert!(validate_frame(&frame).is_ok());
    assert!(
        decrypt(
            &frame,
            std::slice::from_ref(&identity),
            message.sender(),
            message.recipient()
        )
        .is_err()
    );

    let mut frame = encrypt(&message, &[key(&identity)]).unwrap();
    let length = u32::from_be_bytes(frame[..4].try_into().unwrap());
    frame[..4].copy_from_slice(&(length - 1).to_be_bytes());
    frame[length as usize + 3] = 0;
    assert!(validate_frame(&frame).is_err());
}

#[test]
fn arbitrary_bounded_frames_do_not_panic() {
    use rand::RngCore as _;
    let mut rng = rand::rng();
    for _ in 0..500 {
        let mut bytes = vec![0; FRAME_OVERHEAD + 1024];
        rng.fill_bytes(&mut bytes);
        let _ = validate_frame(&bytes);
        bytes[..4].copy_from_slice(&2000u32.to_be_bytes());
        bytes[2004..].fill(0);
        let _ = validate_frame(&bytes);
    }
}

#[test]
fn corruption_truncation_mixed_headers_and_length_fail_closed() {
    let identity = age::x25519::Identity::generate();
    let message = body(1024);
    let frame = encrypt(&message, &[key(&identity)]).unwrap();
    let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    for offset in [0, 3, 4, 30, 100, 300, len + 3, frame.len() - 1] {
        let mut altered = frame.clone();
        altered[offset] ^= 0x80;
        assert!(
            decrypt(
                &altered,
                std::slice::from_ref(&identity),
                message.sender(),
                message.recipient()
            )
            .is_err()
        );
    }
    for end in [0, 3, 4, 100, frame.len() - 1] {
        assert!(validate_frame(&frame[..end]).is_err());
    }
    let mut mixed = frame.clone();
    let position = mixed.windows(6).position(|w| w == b"X25519").unwrap();
    mixed[position..position + 6].copy_from_slice(b"scrypt");
    assert!(validate_frame(&mixed).is_err());
}

#[test]
fn bounded_body_requires_canonical_padding_utf8_and_version() {
    let message = body(10);
    let encoded = message.encode().unwrap();
    for position in [0, 81, encoded.len() - 1] {
        let mut altered = encoded.clone();
        altered[position] ^= 0xff;
        assert!(MessageBody::decode(&altered, 1024).is_err());
    }
    let mut short_length = encoded.clone();
    short_length[84] = 0;
    assert!(MessageBody::decode(&short_length, 1024).is_err());
    let mut invalid_utf8 = encoded.clone();
    invalid_utf8[85] = 0xff;
    assert!(MessageBody::decode(&invalid_utf8, 1024).is_err());
    assert!(MessageBody::decode(&encoded[..100], 1024).is_err());
}
