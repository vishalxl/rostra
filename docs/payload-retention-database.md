# Payload retention database foundation

This checkpoint implements durable source metadata, checked lifecycle counters,
global/unique-byte accounting, checked quota transitions and a bounded quota-only
collector, policy-generation candidate indexes, and a production-disabled shared
admission/reservation boundary. Explicit callers can
dematerialize an eligible Processed remote SocialPost or permanently decline a Missing payload and nominate its hash
atomically. It does not enable automatic eviction/admission, a client worker,
or production quotas.
The storing account's `RostraId`, not its rotating transport key, remains the
identity for candidate indexing.

## Source contract

Schema 27 adds two built-in tables, inaccessible through extension transactions:

- `events_retention_origins`: first header's
  `min(author_timestamp, local_receipt)` and optional first successful
  materialization time, keyed by short event ID.
- `events_quota_pruned`: original author/global quota reason and local decision
  time, keyed by short event ID, written only by the checked quota transition.

New header insertion and successful materialization update their origins in the
same transaction as lifecycle and projections. Duplicates do not refresh them.
An empty, non-Deleted payload materializes at header insertion. Invalid or
terminal-state payload delivery does not start grace.

Total migration stashes and restores both tables from schema 27 onward.
Envelope replay applies quota decisions before any ordinary content processing.
Signed deletion remains stronger, without losing the original quota reason.
Shared stored bytes therefore cannot resurrect a quota-pruned event; its
reference and missing-queue entry are not restored. Materialization-feed rows
remain immutable and resolve to removed markers.

Preparation and replay remain separate atomic transactions. Missing required
retention stash tables or malformed source values fail closed. Replay retries
decode the retained source and never manufacture new local timestamps.

## Conservative clock and migration semantics

Schema 26 upgrades incrementally without scanning historical events. Earlier
total migrations likewise leave origins unknown. An absent origin row means
**ineligible**, not “old enough.” Late materialization of an event whose header
predates tracking does not manufacture a partial origin. A subsequent bounded
migration policy must explicitly handle these events before making them
eligible; this checkpoint does not choose that policy.

Stored times are observations, not evidence of a reliable clock. A backwards
clock does not rewrite an existing origin. Future eligibility must reject an
unknown origin, a clock before the origin, or an unreliable clock. Forward jumps
cannot be detected from these timestamps alone. Grace expiration arithmetic and
static scoring remain in the pure core policy. The checked transition applies
that policy's grace check to Processed content. A Missing event has no
materialization grace yet, but still requires a known, nonfuture header origin.
`RetentionClock::Trusted` is an explicit caller assertion, not database proof.
Phase 3 must provide a concrete trust policy, including handling forward jumps,
before making that assertion in a runtime worker.

## Accounting and quota-only physical reclamation

Schema 28 adds disposable `content_accounting_state`,
`content_accounting_authors`, `content_accounting_hashes` and `content_quota_gc`
tables under already-reserved built-in names. Logical current usage sums
Processed event lengths, once per event. Unique stored-byte usage sums actual
content-store value lengths, once per hash, including unreferenced stored bytes.
Per-author usage transitions and RC increments/decrements now use checked
arithmetic; missing decrement ownership fails rather than fabricating one.

Envelope and content reducers capture their affected event/parent contributions
and store lengths, then apply accounting changes in the same transaction.
Rollback also rolls back totals and after-commit notifications. Hash accounting
independently derives expected references from Processed and Missing events.

All databases start accounting unready, including new empty databases. An
explicit `rebuild_payload_accounting` call visits at most the requested number
of rows, capped at 4096. Its durable cursor scans events, stored content,
expected and actual RC, and both author indexes. Ordinary ingestion adjusts
contributions already covered by each cursor; later keys are counted by a later
scan. Batches can interleave with ingestion, abort, and resume after reopen.
No final full scan or unbounded key collection hides behind readiness.
Malformed rows, mismatching RC/current usage and missing Processed bytes fail
closed. `get_payload_usage` returns `None` until the rebuild is complete.
Accounting readiness does **not** make a future policy generation usable.

The bounded collector accepts only the internal quota-nomination queue and
rechecks readiness, expected/actual RC and the historical replay guard before
removing bytes. Checked quota releases nominate transactionally. Signed deletion,
invalidation and legacy oversized pruning do not nominate general garbage.
Blocked nominations are consumed, but schema 29's disposable
`content_quota_hashes` provenance survives consumption. A later final reference
release requeues only a previously quota-released hash. Thus a shared Missing
reference's eventual signed deletion can complete quota-owned reclamation
without authorizing collection of unrelated legacy bytes.

For a nominated hash, any retained local-authored or non-SocialPost header
blocks collection, even if that historical reference is terminal and has
released RC. This per-hash, header-derived collision guard is not an event
reference and is not charged as current logical usage. Headers never disappear,
so the guard has no release operation under the current conservative kind
policy. It can prevent physical reclamation indefinitely; the guard does not
create a general pinning collector or alter unrelated hashes' retention.
Missing and surviving Processed references independently block collection.

Canonical SocialPost replacement lineage already survives total replay
independently of payload bytes. The real-collector deleted-edit regression
therefore needs no extra edit-source pin. Earlier concern that deleting those
bytes necessarily loses edit lineage was incorrect.

Successful physical removal, exact unique-byte decrement and nomination
consumption are atomic. The result reports actual unique removed bytes,
separately from logical releases. No production raw `content_store.remove`
exists outside this collector. General legacy garbage remains outside its
scope; bounded maintenance does not bound total retained bytes or database-file
size.

Total replay resets disposable accounting and the nomination queue, preserves
retention origins/quota decisions and canonical lineage as before, and requires
another explicit accounting rebuild. Existing replay may omit unreferenced
Deleted bytes; totals describe the actual rebuilt store rather than promising
byte-for-byte garbage preservation. Header-derived collision guards are rebuilt
even when their protected historical bytes are no longer stored.

Schema 29 also adds a disposable `content_quota_recovery` cursor.
`rebuild_quota_payload_nominations` scans at most the requested number of
authoritative quota rows (maximum 4096), validates terminal state and remote
SocialPost provenance, and reconstructs hash provenance and pending nominations
atomically with cursor advancement. Reopen resumes; replay starts it again.
New quota transitions nominate regardless of the cursor. Later reference
releases requeue already-scanned hashes; unscanned rows nominate when visited.
Call this recovery explicitly after upgrade/replay even when accounting is
already ready. It can interleave with accounting rebuild; collection still
requires accounting readiness. No automatic maintenance is enabled.

## Checked transition API

`prune_quota_payload(QuotaPruneRequest)` rechecks the expected Processed/Missing
target, nonempty remote SocialPost kind, immutable origins, active-policy grace
and explicit clock trust under the writer lock. It fails on unready accounting.
The expected target prevents a stale admission decision from evicting a payload
that materialized concurrently. Duplicate/terminal transitions change nothing;
the original quota reason/time is never refreshed. Unknown historical origins
remain ineligible; no migration policy is invented.

Processed pruning uses the ordinary strict social-post projection reversion,
not the author-deletion state transition. Reply/reaction contributions, timeline
and receipt indexes, news ranks and mentions are removed as applicable.
Canonical edit/deletion lineage and append-only materialization occurrences
remain; feed scans resolve pruned occurrences as Removed. Missing rejection
releases its reference and retry entry but frees zero logical current bytes.
The before/after accounting owner wraps both paths, and any error rolls back
projections, lifecycle, quota metadata, nominations and notifications together.
`quota_pruned_subscribe` is a lossy after-commit invalidation signal carrying the
event identity, not a fabricated content arrival or author deletion.

## Bounded policy-generation indexes

Schema **30** adds disposable `content_retention_state`, reverse ownership,
global/author ordered candidates, and grace scheduling tables. The generation
stores all 28 versioned policy bytes and the database holder account `RostraId`.
The database rejects reopening under another account; a mismatched requested
generation yields no candidates. No iroh key enters the API.

`configure_retention_index(policy)` immediately invalidates a different generation
without scanning rows. `rebuild_retention_index(limit)` first removes old reverse
rows and their owned forwards in bounded batches, then scans retained event
headers with a durable exclusive cursor. Repeating configuration for the same
policy resumes progress. During cleanup lifecycle upserts are skipped; during
backfill they idempotently refresh every affected event regardless of cursor.
Thus deletions, materialization, duplicates and quota transitions cannot leave
stale or mixed-generation membership. Each batch commits rows and progress
together and resumes across reopen. Total replay resets the generation; explicitly
configure and rebuild again. No origins are invented for historical events.

Only nonempty Processed remote SocialPosts with complete immutable origins enter
the grace schedule. Its deadline is the maximum of the header origin and checked
first-materialization-plus-grace; unrepresentable deadlines remain protected.
`promote_retention_grace(clock, limit)` moves due rows into both static ordered
indexes without rescoring. Its `ready` flag describes backfill, not grace-queue
exhaustion. A worker requiring the minimum among all currently due events must
finish due promotion at its chosen trusted timestamp before selecting: a trusted,
ready promotion call with fewer visits than its limit has exhausted that due
prefix. No trusted clock means no promotion. Keys use the full
event ID reconstructed from the retained verified header, not its shortened
database key, and compare in the core policy's canonical 48-byte order.

`retention_index_progress()` reports generation/backfill readiness independently
of accounting and nomination reconstruction. `select_retention_candidates`
accepts the expected generation, optional full author, explicit clock, exclusive
key cursor and row limit. It returns advisory full event IDs/keys and the last
visited key, counting rejected rows against the bound. All maintenance/selection
limits are 1..=4096. Selection visits indexes, never scans payloads. Even promoted
rows recheck lifecycle and original eligibility times, so a backwards clock cannot
bypass grace; an untrusted clock returns no candidates. Static keys are rescored
only for bounded selected-row validation, not periodically across retained data.
Clock trust itself is not inferred or latched by the database.

Pagination is a transaction-local snapshot, not a frozen victim list. Restart
from the beginning after policy changes and when considering newly promoted grace
rows. `retention_candidate_is_current` exposes advisory revalidation; its
transactional counterpart is available for the later pressure operation and
requires matching promoted reverse ownership and both forward mappings.
Neither API authorizes pruning or converts selection into a quota request.

## Shared admission foundation (phase 3a)

`PayloadAdmissionConfig` validates explicit nonzero logical database/common-author
caps, optional full-`RostraId` author overrides, and independent in-flight
count/byte limits. Counts are bounded by 4096. Author caps are strict ceilings,
not reserved shares; a ceiling above the database cap does not permit borrowing.
There is no universal GiB default. `experimental_low_water` computes floor(90%)
without overflow; this is an experimental starting point for future hysteresis,
not a worker or an automatic deletion rule.

**Production admission remains disabled by construction.** The database owns an
empty optional configuration, and only disposable unit tests can populate it.
There is no public setter, runtime config wiring, clock assertion, worker,
automatic quota decision, collection or service activation. This safety
checkpoint deliberately precedes complete client integration.

`reserve_payload` retains a verified envelope and returns Disabled, Unneeded,
Deferred or a unique logical `PayloadReservation`. Admission rechecks ready
accounting and current author/database logical usage plus pending reservations
inside the serialized writer boundary. Materialization rechecks capacity in its
own transaction. Temporary author/global pressure is not yet ranked against the
victim boundary and never produces a permanent quota rejection.

Each lease can acquire distinct `PayloadBuffer` guards, one per payload-sized
allocation/peer attempt. Four racing downloads charge four buffers but one
logical event. Both logical acquisitions and buffers have independent count
ceilings; buffers additionally have an aggregate byte ceiling. The guard API
reserves capacity, not memory: callers must acquire before allocation and hold it
through ingestion until those bytes are released. It does not measure allocator
overhead, transport/BAO working memory, envelopes, retained notification/caller
clones after acquisition completes, or DB file growth. It cannot retroactively bound already-allocated inputs to existing
APIs.

Dropping the acquisition cancels it unless a buffer still owns its lifetime.
Committed Processed/terminal transitions release the logical charge through the
existing accounting owner, but outstanding buffers remain charged until dropped.
Full event/lease/database identity prevents stale or cross-database guard reuse.
The bounded ledger supports one logical owner per event: another request receives
AlreadyReserved instead of making another logical claim. `payload_admission_usage`
reports these two kinds of ownership separately; `payload_admission_changed` is a
lossy notification, never readiness or a durable work queue.

All ordinary content reducers pass the same admission check before projections
or bytes change. `try_process_admitted_event_content` exposes Processed, Invalid,
Unchanged, or Deferred separately. Deferred
commits the header and retains/reinserts its existing Missing schedule, with no
quota decision or retry-attempt change. Ordinary signed-header effects, including
author deletion of another payload, still apply. The old fallible ingestion methods return
`DbError::PayloadAdmissionPaused { reason }` and roll back the whole transaction;
the old panic wrappers retain their documented panic-on-error contract. Local
publication and protected kinds are not exempt from logical caps. This error is
a clear storage-capacity refusal, not a database corruption or peer failure.

`try_materialize_stored_payload` handles one retained event, reserves before
copying hash-store bytes, then uses the same materialization gate. A deferred
shared-hash event gains its otherwise omitted Missing queue row, without scanning
historical events. Envelope ingestion under configured admission also ensures
that row exists. No client invokes the new reuse API yet; production scheduling
is unchanged. Logical usage charges both materialized events even if their bytes
share one stored hash.

### Acquisition-path audit and next caller obligations

| Path | Database boundary already covered | Still required before activation |
| --- | --- | --- |
| Missing retries / ancestor synchronization | `util::rpc::download_events_from_child` ultimately uses guarded DB ingestion | Pre-read reservation, shared-store reuse first, typed pause propagation instead of retry failure |
| Explicit direct fetch | `get_event_content_from_followers` uses guarded DB ingestion | Same pre-read reservation and terminal checks |
| Pushed `FEED_EVENT` | `Client::store_event_with_content` uses guarded DB ingestion | Reserve before success response and BAO payload read; return existing suitable refusal semantics |
| Local publication / head merger | `try_process_event_with_content` is guarded; empty events need no payload room | Pre-allocation budgeting where possible and storage-full error mapping; never override caps |
| Raw signed web API (`routes/api.rs`) | Its direct `try_process_event_with_content` call is guarded | Bound request-body allocation before JSON/content verification, then transfer ownership to verified-event admission |
| Hash-store reuse | New single-event reuse API uses the same gate | Call before fetching and pause without re-fetching the existing shared bytes |
| Public direct DB ingestion | All three fallible ingestion variants and panic wrappers converge on the gate | External holders of already-allocated bytes own that memory; use the explicit pre-read guard API for controlled acquisition |
| Direct P2P connection/cache APIs | Any later DB materialization is guarded | Do not expose production acquisition through an unmetered `Connection::get_event_content`; cache currently races four peers |

Before adding runtime activation, all client acquisition owners must adopt the
buffer/typed-result contract. The raw signed HTTP request allocates its body before
it has a verified envelope: its pre-parse capacity must be bounded separately (or
with a provisional buffer lease), not falsely claimed as covered by the current
verified-envelope guard. Existing APIs' inline guards protect committed logical
growth only; they do not prove pre-read memory bounds.

A Deferred queue-front item must not be retried in a tight loop or converted to a
peer backoff failure. Register wakeups before checking work, recheck after a wake,
and use bounded retry/config/grace wakeups to tolerate lost or self-generated
signals. The later worker must supply ranking-aware temporary versus permanent
admission, generation/pressure/reducer atomicity, distinct readiness dimensions,
trusted-clock recovery, hysteresis, dry-run modeling and bounded yielding batches.
No production clock can be inferred from these APIs or test timestamps.

## Next checkpoint

Only after those foundations may phase 3b add complete client/worker integration.
Logical quota release may reclaim no physical bytes when another reference
survives. Headers and index overhead remain outside logical payload accounting.

Phase 3 must call the checked transition, not the low-level
`prune_event_content_tx` ingestion helper, and integrate budget/reservation
rechecks, active-generation/current-candidate validation and candidate maintenance
in the same write transaction. The current
explicit transition does not select victims, validate quota pressure or establish
the active policy generation. Run accounting and nomination rebuilds separately;
their `PayloadMaintenance.ready` results describe their own operation, not
overall worker readiness. Run generation backfill and bounded grace promotion
separately as well. Phase 3 must supply concrete clock-jump detection, trust and
recovery before asserting `Trusted`; immutable timestamps cannot prove clock
reliability. No durable high-water latch or reset policy is introduced here.

## Verification

`retention_tests` covers clamped age, historical payload grace, duplicates and
clock rollback, aborted ingestion, schema-26 cutover, late legacy payloads,
repeated total replay, shared-content survival, missing-payload terminality,
Deleted precedence, materialization-feed preservation, and a malformed stash
that can be repaired and retried after reopen. Tests use in-memory or disposable
temporary databases, never the user's live data.

`payload_accounting_tests` adds shared-hash/Missing ownership, historical
local/non-social collision guards, ingestion/backfill/reopen/replay interleaving,
transaction rollback, actual unique reclamation, absent production nominations,
readiness/corruption rejection, bounded limits and checked counter failures.
The deleted-edit lineage test now uses the real collector before reopen/replay.
`quota_pruning_tests` exercises the production transition, atomic rollback,
duplicate/concurrent decisions, terminal delivery/deletion, projection/feed/edit
preservation, shared Missing/protected hashes, and bounded recovery interrupted
by rollback/reopen/replay and concurrent reference release.

`retention_index_tests` covers canonical key ordering, full-ID ties, author/global
pages, independent readiness, interleaved rebuild/lifecycle updates, policy and
holder mismatch, stale advice, bounded grace promotion, protected/unknown/future
origins, rollback/untrusted clocks, abort/reopen and repeated total replay.

`payload_admission_tests` covers disabled behavior, named explicit configuration,
accounting readiness, author/common/override and database capacity including
reservations, independent buffer count/bytes, racing writers and peer buffers,
drop/cancellation wakeups, aborted ingestion, duplicate/foreign/stale guards,
committed terminal release and late delivery, Invalid separation, protected-kind
capacity refusal, configured shared-hash envelope scheduling and deferred
hash-store reuse without refetching.
