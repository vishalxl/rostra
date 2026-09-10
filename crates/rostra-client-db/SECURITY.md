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
The explicit quota transition rejects local-authored payloads, state-bearing
content kinds (such as follow/profile/vote), unknown kinds, unknown origins,
nonexpired materialization grace and untrusted clocks.
Missing admission rejection requires a known, nonfuture header origin but has
not yet started materialization grace. Clock trust is an explicit caller
assertion; the approved runtime assumption trusts the system wall clock, including
startup. No special acknowledgement or clock-monitor subsystem is required.
Unknown legacy origins and existing future/grace checks remain protected.
No automatic destructive worker is enabled.

The shared admission boundary remains production-disabled: its config
is `None`, with no production setter or worker. Adding an activation path is a
security/reliability revisit trigger, not routine configuration plumbing.
Configured admission rechecks ready accounting and author/database current usage
plus pending logical reservations inside the materialization writer transaction.
One full EventId has one logical acquisition owner; each racing payload attempt
requires its own buffer guard. Committed materialization/terminal changes release
logical ownership after commit, while buffers remain charged until their owners
drop. Cancellation drops unfinished ownership; rollback never publishes a
terminal release.

The buffer limit is declared capacity, not a whole-process memory measurement.
Existing APIs' already-allocated external content, transport/codec working memory,
allocator overhead and clones retained after acquisition remain outside its
guarantee. Client acquisition now reserves before reads, including a separate
full-payload Vec-to-Arc conversion charge, and retains the winner through ingestion.
Raw signed HTTP uses a pinned body limit and conservative simultaneous pre-parse
charges from an already-loaded client's ledger; local serialization charges output
growth before allocation. Unverified HTTP paths never create/open a database.
An unloaded account has no configured ledger here: activation must make its
pre-parse capacity policy available without DB creation, including lazy-load races.
The body limit alone does not bound aggregate memory for these disabled requests.
Low-level
P2P callers still own their allocation policy; supplying a dummy guard is not a
supported client admission path. Adding any acquisition path or changing buffer
representations is a capacity-audit trigger.

Production activation still requires ranking-aware admission, transactional
pressure/worker integration, bounded hysteresis/readiness work, dry-run modeling,
and independent review of the complete enabled runtime. Protecting local/state/
unknown content from eviction does not exempt it from admission caps. Runtime
clock policy trusts the system wall clock, per the operator decision; unknown
legacy origins and existing future/grace checks remain conservative.

Admission notifications are lossy: callers must register before checking work,
then recheck, and also wake for startup, configuration, accounting readiness and
bounded retry/grace deadlines. Deferred Missing work must not spin on its queue
row or treat temporary pressure as a peer failure. The current primitive tests
do not prove worker liveness, clock trust or complete pre-read memory bounds;
those remain explicit activation prerequisites.

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
are requeued on a later final reference release only when historical quota-hash
provenance authorizes that work.

The checked quota transition nominates atomically with dematerialization,
lifecycle, usage and reference changes, and emits invalidation only after commit.
It preserves canonical edit/deletion lineage and immutable feed rows, never
representing local pruning as author deletion. Errors abort every side effect.
General garbage from signed deletion, invalidation and legacy size pruning is
outside this collector's scope. Total replay drops disposable accounting and the nomination
queue/provenance/recovery cursor, then requires explicit rebuilds. Schema 29's
separate bounded quota-row scan reconstructs provenance and pending work;
concurrent quota transitions nominate directly, while final reference releases
requeue scanned hashes and unscanned rows nominate when visited. The recovery
cursor and reconstructed rows commit together and resume after reopen.
Accounting readiness alone does not mean nomination recovery is complete.
Existing replay may omit unreferenced Deleted bytes; rebuilt totals describe the
actual store. Canonical SocialPost replacement lineage survives independently of those bytes. Rebuilt
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
