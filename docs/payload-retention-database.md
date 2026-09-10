# Payload retention database foundation

This checkpoint implements durable source metadata, checked lifecycle counters,
global/unique-byte accounting, checked quota transitions and a bounded quota-only
collector. Explicit callers can dematerialize an eligible Processed remote
SocialPost or permanently decline a Missing payload and nominate its hash
atomically. It does not enable automatic eviction/admission, a client worker,
or production quotas.
The storing account's `RostraId`, not its rotating transport key, remains the
approved identity for subsequent candidate indexing.

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

## Next checkpoints

**2c — indexing:** add author/global static-key candidate indexes, grace-expiry
metadata, full policy bytes plus holder `RostraId` generations, and bounded,
resumable backfill/rebuild. Fail closed until the active generation is complete.
Keep candidate updates transactional with the lifecycle. Unknown legacy origins
need an explicit conservative initialization strategy, not synthetic replay time.

Only after those foundations may phase 3 add admission and worker integration.
Logical quota release may reclaim no physical bytes when another reference
survives. Headers and index overhead remain outside logical payload accounting.

Phase 3 must call the checked transition, not the low-level
`prune_event_content_tx` ingestion helper, and integrate budget/reservation
rechecks and candidate maintenance in the same write transaction. The current
explicit transition does not select victims, validate quota pressure or establish
the active policy generation. Run accounting and nomination rebuilds separately;
their `PayloadMaintenance.ready` results describe their own operation, not
overall worker readiness.

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
