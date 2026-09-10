# Direct-message cryptographic boundary

Read the root security and engineering guidance before changing this crate.
The [README](README.md) owns the exact format and derived bounds; the
[approved plan](../../docs/direct-messages/approved-plan.md) owns product scope.
This crate is not by itself an end-to-end security guarantee.

Treat frames, announced keys, signed timestamps, and decrypted application
fields as hostile. An account signature authenticates authority, not honest key
generation, key possession, exclusive ownership, or a trustworthy clock.
Prevalidate canonical contributory public keys before native age encryption;
the pinned library can panic on non-contributory destination input.

The integrating caller must verify the event signature, author, full content
hash and length, DM kind, zero aux-key and non-singleton flag before trials.
It must delete expired live keys first and supply only retained live identities.
The crate checks strict bounded native-only structure before the general age
parser, authenticates through EOF, and verifies encrypted sender/participants.
Do not relax these checks or accept an authenticated plaintext prefix.

Native age identities, temporary plaintext, file keys, and retained history are
secrets. Never log, debug-format, export, or include them in errors. Temporary
application buffers and library secret types provide best-effort live-memory
cleanup, not forensic storage/memory erasure. Retained history remains plaintext;
database reads, swap, dumps, backups, old pages, and snapshots are outside that
cleanup promise.

Run trials in bounded background work, not externally observable request paths.
One call permits eight identities; unreadable traffic costs up to 64 native DH
trials. More live keys require fair resumed batches. Never interpret one batch's
failure as proof of another recipient, and never drop live keys to meet this
batch bound. Do not add receipts, decryption-result callbacks, remote content
fetches, or plaintext-triggered networking. UI access must separately require
an unlocked full-account session.

Exactly eight shuffled native slots, genuine fresh throwaway recipients, and
inner text padding are required for the qualified outsider envelope privacy
target. Public metadata, malicious authorized keys, insider membership/count
predicates, endpoint compromise, and voluntary replies prevent stronger global
or active-insider anonymity claims.

Changing age, age-core, their lockfile versions, native stanza encoding, GREASE,
chunking, metadata, buckets, or key parsing requires source reinspection and
re-derivation of the documented bounds. Tests must cover canonical/low-order
keys, every bucket and GREASE length, maximum header, malformed/mixed headers,
custom-parser/upstream-parser parity, authentication failure, full EOF, and
participant binding. Run the unreadable benchmark when changing trial behavior.
The required end-to-end Linked Specs CLAIM must remain unverified until its
separate independent verification passes against the integrated source.
