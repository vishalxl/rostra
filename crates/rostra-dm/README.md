# Native age envelope profile

This crate implements the cryptographic envelope only. It is not an activated
DM feature or a verified claim about the complete client. The approved
[implementation plan](../../docs/direct-messages/approved-plan.md) and
[independent design research](../../docs/direct-messages/design-security-research.md)
remain implementation inputs, not code verification.

## Version-one byte format

All lengths use unsigned fixed-width big-endian encoding. A frame contains a
four-byte age-file length, that exact complete native age v1 file, and zero
padding to `2048 + text_bucket` bytes. Accepted text buckets are 1024, 2048,
4096, 8192, and 16384 bytes. No other external sizes are accepted.

The encrypted plaintext is an 85-byte header followed by bucket-padded text:
version byte `1`, sender account's 32 raw bytes, recipient account's 32 raw
bytes, a random 16-byte logical message ID, four-byte actual UTF-8 text length,
text, and zeros to the smallest text bucket. Empty text fits the first bucket;
composer policy may separately reject it. Embedded NUL is valid UTF-8 text, not
a terminator. The maximum 16KiB text never spills to a 32KiB bucket.

The public length covers inner padding, so it discloses only the accepted bucket
and message-independent native-header variability. Neither frame nor encrypted
body carries a device ID, epoch ID, destination count, or receipt.

Every encryption supplies eight native X25519 recipients, shuffled uniformly,
with fresh throwaway identities supplying missing slots. The sending installation
does not require its own slot. Duplicate authorized public keys remain separate
slots with independent library encapsulation; there is no exclusive-key-ownership
claim. Selected-device eligibility and the 4+4 spillover policy are a client
responsibility, not a property of a bare recipient-key list.

## Bounded native profile

The parser admits exactly eight canonical X25519 stanzas and at most one GREASE
stanza. It admits no passphrase, SSH, plugin, PQ, arbitrary unknown, or mixed
recipient type. It validates structure before calling age, and never modifies a
finished header. Header order remains protected by the age MAC.

In pinned `age 0.12.1` / lockfile-selected `age-core 0.12.0`:

- version line: 22 bytes;
- each native stanza: 54-byte first line plus 44-byte body line = 98;
- header MAC line: 48;
- GREASE tag: 1–8 printable non-space ASCII characters plus `-grease`;
- GREASE arguments: 0–4 arguments, each 1–8 printable non-space ASCII characters;
- GREASE body: 0–99 bytes, base64 without padding, lines at most 64 characters
  with a terminating short line (possibly empty).

Thus the maximum GREASE first line is
`3 + 15 + 4*(1+8) + 1 = 55` bytes; the maximum body is
132 base64 characters plus three newlines = 135 bytes. The maximum header is
`22 + 8*98 + 190 + 48 = 1044` bytes, below the 1536-byte parser bound.
At most 16469 plaintext bytes fit one 65536-byte age payload chunk. With the
16-byte nonce and 16-byte final-chunk tag, maximum external overhead is
`4 + 1044 + 16 + 85 + 16 = 1165`, below 2048. Encryption is never retried to
obtain conveniently small random GREASE.

Public announcements use canonical 32-byte Montgomery coordinates less than
`2^255-19`; high-bit and field aliases are rejected. A public fixed-scalar
X25519 test rejects non-contributory coordinates before they reach age's
encryption API. This prevents the pinned native implementation's zero-DH panic,
without relying on panic catching or assuming signing proves key possession.

## Integration boundary

Before decryption the client must authenticate the complete signed event,
content hash and length, DM kind, zero aux-key, and non-singleton flag. It must
delete expired epoch identities before supplying keys. One call accepts at
most eight identities (at most 64 native DH trials on unreadable traffic);
remaining live keys require fair resumed batches, not deletion or permanent
classification after one failed batch. No event-time filtering is implied.

Decryption reads the exact expected plaintext and then reads through
authenticated EOF. It binds encrypted sender to signed author and requires the
local account to be a named participant before returning a body. There are no
callbacks, receipts, content fetches, or network operations in this crate.
The client must keep trials off public request paths and expose stored history
only to unlocked full-account sessions.

Temporary application plaintext buffers use `Zeroizing`. Retained message text
is intentionally ordinary plaintext state; this does not promise forensic
erasure, elimination of compiler/runtime copies, or protection from readable
databases, swap, dumps, snapshots, or backups.

## Dependency evidence

On September 10, 2026, the sparse crates.io index reported `age 0.12.1` not
yanked, and the upstream latest-release API reported `v0.12.1`, published
July 14, 2026. The crates.io REST API returned HTTP 403; the independent sparse
registry check and Cargo resolution succeeded. Exact selected transitive
versions are recorded in the workspace lockfile.

The upstream RustSec entry `RUSTSEC-2024-0433` concerns plugin-name command
execution in older age releases and lists `>=0.11.1` as patched. This crate
additionally disables default features and never constructs plugin identities.
An advisory database check is dependency evidence, not a cryptographic audit.
The lockfile scan against advisory database commit
`b50980aad8b8f14f77e25a97b32dd94bf008b0af` found no advisories among newly added
DM dependencies. Workspace feature unification still includes existing
dependencies with warnings; this is not an advisory-clean runtime claim.
The workspace still has pre-existing quick-xml 0.38.4
`RUSTSEC-2026-0194`/`RUSTSEC-2026-0195` vulnerabilities and existing
bincode/fxhash/paste/proc-macro-error/lru warnings; this change does not fix or
waive them.

## Installation announcements and local epochs

Version-one device events use kind `0x0041`; encrypted messages use `0x0040`.
Both are non-singletons with zero aux keys. Announcement bytes are version `1`
(one byte), active `1` or retired `0` (one byte), and a random 16-byte device ID.
Active records append a canonical 32-byte X25519 public key and three big-endian
u64 Unix-second deadlines: `send_from`, `send_until`, `decrypt_until`. Exact
lengths are 74 bytes active and 18 retired; trailing bytes are invalid.

Require `send_from < send_until <= decrypt_until`, signed publication at or after
`send_from`, and representable Unix times. The independent v1 sanity bound on
total advertised lifetime is 365 days, **not** the local key-erasure schedule.
Local new epochs currently assign seven sending days plus 28 receiving-grace
days, deriving 35 days. Defaults affect only newly generated independent random
keys, never existing stored deadlines. Recovery credentials do not regenerate
these keys.

Reduce active announcements by `(signed timestamp, ShortEventId)` before
eligibility; expired/future newest keys never fall back to older announcements.
Allow at most five minutes of future publication skew, without extending any
key deadline. Retirement permanently dominates every active event for that ID.
Retirement stops future selection once learned, not past decryption: already
held private epochs retain their original receive-only deadlines. Re-enrollment
uses a new random ID and key.

The database persists installation identity, individual live epochs, latest
device states/tombstones and first-winner plaintext history as authoritative,
non-replayable rows. Total replay restores them before reducing ciphertext and
rebuilds only the receive queue. Live key deletion commits before queued trials.
Ordinary wall time is captured for lifecycle work; backward correction pauses
generation when the newest key is still future. Deleted keys are not restored
or recreated. There is no persisted global high-water clock or snapshot
antirollback guarantee.

## Trial-cost measurement

`cargo bench -p rostra-dm --bench unreadable -- --sample-count 100` measures
complete failed profile validation and decryption of a maximum-text frame.
On the development host, release-build medians on September 10, 2026 were
716.9 microseconds for one identity, 2.230 milliseconds for five, and 3.292
milliseconds for eight. These are host-local observations, not a throughput
guarantee. The unreadable file requires eight native DH trials per identity;
runtime admission/fair scheduling must account for that cost. The benchmark
does not justify public key hints, fewer slots, or discarding live keys.

Primary sources used to derive the profile:

```text
https://index.crates.io/3/a/age
https://api.github.com/repos/str4d/rage/releases/latest
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age/src/native/x25519.rs
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age/src/format.rs
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age-core/src/format.rs
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age/src/primitives/stream.rs
https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/age/RUSTSEC-2024-0433.md
```
