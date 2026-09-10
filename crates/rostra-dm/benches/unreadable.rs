use rostra_core::id::RostraId;
use rostra_dm::{MessageBody, PublicKey, decrypt, encrypt};

fn main() {
    divan::main();
}

/// Measure complete failed native discovery, including profile validation.
#[divan::bench(args = [1, 5, 8])]
fn unreadable(bencher: divan::Bencher, keys: usize) {
    let destination = age::x25519::Identity::generate();
    let identities: Vec<_> = (0..keys)
        .map(|_| age::x25519::Identity::generate())
        .collect();
    let body = MessageBody::new(
        RostraId::from_bytes([1; 32]),
        RostraId::from_bytes([2; 32]),
        "x".repeat(16384),
    )
    .unwrap();
    let frame = encrypt(
        &body,
        &[PublicKey::parse(&destination.to_public().to_string()).unwrap()],
    )
    .unwrap();
    bencher.bench_local(|| {
        assert!(
            decrypt(
                divan::black_box(&frame),
                &identities,
                body.sender(),
                body.recipient()
            )
            .is_err()
        );
    });
}
