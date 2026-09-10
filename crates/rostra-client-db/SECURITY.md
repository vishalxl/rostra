# Client database security and reliability

`rostra-client-db` is the persistent authoritative graph and projection store
for one local Rostra identity. The asynchronous client opens it at startup; one
in-process mutex serializes writes and post-commit publication. The database
file contains signed public events and payloads plus sensitive local identity
and network-key metadata. Filesystem access is trusted and must be limited to
the account running Rostra.

Peer envelopes and payloads are untrusted until normal ingestion verifies event
signatures, event identity, payload hash, and payload length. Retained migration
source rows are trusted as previously admitted data; total replay is not a
cryptographic audit. Caller extension tables are trusted in-process state
outside core replay invariants and remain the extension owner's compatibility
responsibility.

The SocialPost materialization feed stores event identities, not payload copies.
A scan therefore trusts retained events, lifecycle tables, replacement metadata,
and the content store as one coherent snapshot. Missing events, impossible
Missing/Invalid state, absent current bytes, invalid processed content, or
sequence gaps fail closed without returning a checkpoint. Deleted, pruned, or
replaced content is an expected `Removed` result.
For a replaced post, the scan validates coherent lifecycle before returning
`Removed`; if lifecycle claims it remains processed, retained bytes must also be
present and decodable. Replacement metadata does not mask corruption.

The public self-follow snapshot scans the complete encoded self-identity key
prefix through its exclusive lexicographic successor, then exactly decodes every
matching key and value. This prevents malformed self-owned rows with truncated
or trailing followee bytes from falling outside well-formed tuple bounds and
silently producing a partial snapshot. Rows under adjacent identity prefixes
remain outside the snapshot.

Schema 25 performs a forward-only total rebuild. Preparation atomically stashes
retained source and installs the current schema; replay and stash cleanup commit
atomically. Any replay error leaves the complete stash retryable, and code must
detect an existing stash before preparing another migration. Historical source
encodings require fixtures using the corresponding released layouts. A
malformed authoritative stash must fail closed; disposable metadata corruption
must not destroy authoritative source.

Schema 27 also preserves local retention origins and quota decisions as
authoritative source. Their stash tables are mandatory for a schema-27 replay;
missing tables or malformed values fail closed and retain the retryable stash.
Older events have unknown origins rather than fabricated migration-time grace.
No production quota mutation or destructive worker is enabled by this source
metadata foundation. A future worker must fail closed for unknown origins and
unreliable clocks rather than treating stored wall-clock readings as proof of
elapsed time.

Schema 28 adds disposable global logical/unique-byte accounting and a quota-only
physical collector. All databases start unready; totals remain unavailable and
collection fails closed until an explicit bounded rebuild finishes. The rebuild
derives expected RC (including Missing references) and current logical usage
from retained events, checks expected/actual RC and both author indexes, and
counts actual unique store values. It rejects mismatches and absent Processed
bytes. Cursor-covered ingestion deltas, cursor advancement and partial totals
commit transactionally, so batches can interleave with normal writes and resume
after interruption without exposing partial readiness.

For a quota-nominated hash, a retained local-authored or non-SocialPost header is
a conservative historical protection guard even after it releases event RC.
Under the current protected-kind policy the guard has no release operation.
It is not RC or current logical usage; it can prevent physical reclamation
indefinitely. Before removing any nominated bytes, the collector rechecks
readiness, expected/actual RC, zero references and the historical guard inside
the write transaction. Store removal, unique-byte decrement and nomination
consumption are atomic. Blocked nominations are consumed without removal and
must be nominated again by a later eligible release.

There is no production nominator or worker in this checkpoint. General garbage
from signed deletion, invalidation and legacy size pruning is outside this
collector's scope. Total replay drops disposable accounting and the nomination
queue, then requires another explicit rebuild. Existing replay may omit
unreferenced Deleted bytes; rebuilt totals describe the actual store. Canonical
SocialPost replacement lineage survives independently of those bytes. Rebuilt
historical guards protect surviving or reintroduced shared collisions, not a
promise that deleted bytes survive replay.

Maintenance limits bound visited rows, not payload bytes, aggregate retained
storage, index overhead or redb file allocation. Accounting readiness does not
prove readiness of a future policy candidate generation, and neither stored
wall-clock observations nor static ranking keys authorize quota pruning.

Replay runs before the database is published, suppresses incremental hooks and
materialization-feed emission, and refreshes current-state watches after commit.
Total migration preserves a feed from schema 26 or newer byte-for-byte; older
schemas and the version-26 cutover create an empty feed without reconstructing
historical occurrences. Replay does not retain an event graph
or per-event publication closures. It transiently owns decoded payload and
codec copies, then allocates the final follow/follower/WoT snapshot. The redb
backend also tracks dirty, allocated, and freed pages for the whole atomic
transaction, so total process memory is not constant in database size.

Operators must back up the database before upgrade and provision measured RAM
and temporary disk headroom for their database shape. There is no preflight or
safe fixed free-space multiplier. Once preparation commits schema 25, an older
binary cannot open the database; rollback means restoring the pre-upgrade
backup. Disk exhaustion or interruption may leave a roll-forward stash and
require retry with schema-25 code. Replay does not compact automatically.

Primary safeguards are atomic redb transactions, retry-marker persistence,
strict historical value decoding, content commitment verification,
identity-collision rejection, and historical-layout migration tests. Re-check
these safeguards whenever changing retained source formats, schema/version
handling, migration markers, decoding, transaction boundaries, or replay
publication.

The current redb-bincode range API validates encoded keys infallibly, and
migration value decoding does not impose a separate allocation limit on corrupt
length prefixes. Because the database file and local filesystem are trusted,
malformed authoritative keys or hostile encoded lengths are an accepted
non-adversarial corruption risk: open may panic or exhaust memory while the
committed stash remains on disk. Restore the pre-upgrade backup or use an
offline audited repair tool; repeated normal opens are not a corruption scrub.
