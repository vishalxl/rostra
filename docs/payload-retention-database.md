# Payload retention database foundation

This checkpoint implements durable source metadata only. It does not enable
quota eviction, admission, hash deletion, a client worker, or production quotas.
The storing account's `RostraId`, not its rotating transport key, remains the
approved identity for subsequent candidate indexing.

## Source contract

Schema 27 adds two built-in tables, inaccessible through extension transactions:

- `events_retention_origins`: first header's
  `min(author_timestamp, local_receipt)` and optional first successful
  materialization time, keyed by short event ID.
- `events_quota_pruned`: original author/global quota reason and local decision
  time, keyed by short event ID. There is deliberately no production writer yet.

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
static scoring remain in the pure core policy; this checkpoint performs neither.

## Next checkpoints

**2b — lifecycle and accounting:** provide checked transactional
Processed→quota-Pruned and Missing→quota-Pruned APIs, with safe-kind and
local-author protection. Dematerialize social post/reply/reaction/edit
projections, maintain exact reference/usage/queue state and notifications, and
write the immutable quota decision atomically. The existing
`prune_event_content_tx` still is not a safe processed-eviction API. Add global
logical usage, unique stored-byte accounting, and a bounded zero-reference GC
queue that rechecks all references, including Missing, before deletion.

**2c — indexing:** add author/global static-key candidate indexes, grace-expiry
metadata, full policy bytes plus holder `RostraId` generations, and bounded,
resumable backfill/rebuild. Fail closed until the active generation is complete.
Keep candidate updates transactional with the lifecycle. Unknown legacy origins
need an explicit conservative initialization strategy, not synthetic replay time.

Only after those foundations may phase 3 add admission and worker integration.
Logical quota release may reclaim no physical bytes when another reference
survives. Headers and index overhead remain outside logical payload accounting.

## Verification

`retention_tests` covers clamped age, historical payload grace, duplicates and
clock rollback, aborted ingestion, schema-26 cutover, late legacy payloads,
repeated total replay, shared-content survival, missing-payload terminality,
Deleted precedence, materialization-feed preservation, and a malformed stash
that can be repaired and retried after reopen. Tests use in-memory or disposable
temporary databases, never the user's live data.
