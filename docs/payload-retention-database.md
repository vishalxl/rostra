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
The approved runtime policy is to trust the system clock, including startup.
It adds no acknowledgement, clock authority, or high-water latch. Forward jumps
can expire grace; unknown origins and timestamps in the future remain protected.
The internal demand primitive samples a fixed trusted timestamp inside each
writer transaction; no production worker invokes it yet.

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
transactional counterpart is used by the internal pressure operation and
requires matching promoted reverse ownership and both forward mappings.
Neither API authorizes pruning or converts selection into a quota request.

## Shared admission and acquisition integration

`PayloadAdmissionConfig` validates explicit nonzero logical database/common-author
caps, optional full-`RostraId` author overrides, and independent in-flight
count/byte limits. Counts are bounded by 4096. Author caps are strict ceilings,
not reserved shares; a ceiling above the database cap does not permit borrowing.
There is no universal GiB default. `experimental_low_water` computes floor(90%)
without overflow; the private driver's experimental hysteresis uses it.
The arithmetic alone is not a worker or an automatic deletion rule.

**Production admission remains disabled by construction.** The shared ledger owns
an empty optional configuration, and only disposable unit tests can populate it.
There is no public setter, production runtime configuration, Client worker or
service activation. The private test-installed driver described below deliberately
precedes complete client integration.

`PayloadAccount::disabled` creates an account-scoped ledger without filesystem
work. `MultiClient::new_with_payload_accounts` installs explicitly listed handles
before publication; duplicate identities are rejected. Raw HTTP consults this
immutable registry before parsing, and the later database attaches the same ledger.
Unlisted unverified identities do not grow the registry. Lazy open/build/eviction
is serialized across manager clones, and only one database can attach each account
ledger at a time. Reattachment invalidates old logical reservations without
releasing still-owned buffer bytes; lease IDs are never reset. Account identity
must match the storing `RostraId`.

LRU eviction removes a strong client-cache entry, not discoverability of live
request/task ownership. Reload reuses a surviving client, or its shared database
and surviving endpoint if only those remain; it does not reopen or reattach live
storage. A retained, cancellation-safe shared join future waits for actual old-task
termination before DB-only reconstruction or removal of its retired record.
The manager retains completion before task startup, with unique abort ownership
transferred from partial construction into the client. A constructor error or
panic cannot let retry bypass old-task joining, even for workers holding no DB.
The public builder retains consuming `Database` ownership; only a crate-private
constructor accepts shared storage.
Retired database keepers are atomically unwrapped and closed under the
load lock before a fresh open can occur. A weak-manager reaper checks at one-second
intervals, visiting at most 32 records and checking a 10-ms cooperative time budget
between records. File closure runs on a blocking worker; an indivisible database
close can exceed that time. The ordered
cursor prevents externally held entries from starving later cleanup.
Thus `max_clients` bounds strong LRU entries, not all externally retained live
resources; released retired DBs may remain briefly until the reaper runs.
This reaper only closes storage handles; it is not quota pruning or payload GC.

Cold initialization is manager-owned: cancelling one verified request does not
abandon the open/attach-to-publication interval or lose storage discoverability.
It finishes publication or failure; a still-waiting caller receives the result,
but a cancelled sole caller receives nothing. A task failure reaches its live
waiter as a distinct initialization-task error. Failed-build storage remains
discoverable until reuse or quiescent cleanup. While sleeping, the cleanup task
holds weak references to the registry/load lock and strong cursor/running-flag
metadata; that metadata owns no clients or databases and cannot keep a dropped
manager or its storage alive.

These are ownership foundations, **not runtime policy configuration**: every
constructible account is still disabled. No live configuration setter exists.
The approved runtime configuration is startup-only; changing mode, budgets or
policy requires orderly shutdown and fresh construction after old workers,
acquisitions and guards have quiesced. This checkpoint neither implements nor
performs a service restart.

`reserve_payload` retains a verified envelope and returns Disabled, Unneeded,
Deferred or a unique logical `PayloadReservation`. Admission rechecks ready
accounting and current author/database logical usage plus pending reservations
inside the serialized writer boundary. Materialization rechecks capacity in its
own transaction. Temporary author/global pressure is not yet ranked against the
victim boundary and never produces a permanent quota rejection.

Each lease can acquire distinct `PayloadBuffer` guards, one per payload-sized
allocation. Four racing downloads plus their Vec-to-Arc conversions can charge
eight buffers but one
logical event. Both logical acquisitions and buffers have independent count
ceilings; buffers additionally have an aggregate byte ceiling. The guard API
reserves capacity, not memory: callers must acquire before allocation and hold it
through ingestion until those bytes are released. It does not measure allocator
overhead, transport/BAO working memory, envelopes, retained notification/caller
clones after acquisition completes, or DB file growth. It cannot retroactively bound already-allocated inputs to existing
APIs. Provisional `PayloadAllocation` guards share this buffer ledger before a
verified envelope exists. They grow before local serialization output allocation
and can bind to a logical reservation without releasing/reacquiring a buffer slot.
Failed binding retains its original charge; successful binding preserves the full
allocation capacity, including any excess over signed length.

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
that row exists. Client acquisition invokes reuse before networking, including when
admission is disabled. Logical usage charges both materialized events even if their bytes
share one stored hash.

### Acquisition-path audit

| Path | Logical boundary | Pre-read/capacity owner |
| --- | --- | --- |
| Missing retries / ancestor synchronization | `util::rpc::download_events_from_child` uses guarded cache ingestion | Shared-store/terminal check before logical reservation; two guards per racing peer cover read and conversion; winner owned through ingestion |
| Explicit direct fetch | `get_event_content_from_followers` uses the same cache owner | Same reuse, terminal, reservation and per-attempt contract |
| Pushed `FEED_EVENT` | Owned guarded ingestion | Reserve read and conversion before success/BAO read; terminal/reused payloads return AlreadyHave, temporary pressure uses existing DoesNotNeed response |
| Local publication / head merger | Guarded DB ingestion; empty merges need no payload room | Provisional CBOR writer charges before growth, separate conversion capacity, then binds surviving bytes to reservation; clear storage-capacity error |
| Omni remote publication | Outgoing content does not materialize in this DB | Same bounded serializer retains provisional capacity through outbound attempts; no extra payload-sized allocation for Arc clones |
| Raw signed web API (`routes/api.rs`) | Guarded owned ingestion after verification; disabled atomic ingestion preserved | Fixed 2-MiB HTTP body limit regardless of Content-Length; startup account ownership is available before lazy load, with loaded-DB fallback for unlisted accounts. When configured, the ledger supplies five conservative 2-MiB provisional slots for body, strings/scratch and Vec/Arc overlap before parse; one survives until ingestion |
| Hash-store reuse | Single-event reuse API uses the same gate | Capacity for owned copy and conversion before copying; pause without refetching shared bytes |
| Public direct DB ingestion | All three fallible ingestion variants and panic wrappers converge on the gate | External holders of already-allocated bytes own that memory; use the explicit pre-read guard API for controlled acquisition |
| Direct P2P connection/cache APIs | Cache owns database acquisition/ingestion | Removed unguarded convenience read; `get_event_content_with_guard` retains caller-owned guard through transport without reversing DB→P2P dependency |

This is still a **non-activatable checkpoint**. Low-level P2P callers and direct DB
callers that already own bytes remain responsible for their allocation policy.
A dummy transport guard is not a supported client acquisition path. HTTP's
conservative five-slot reservation can reject an otherwise small request under
tight limits; a two-slot read/conversion can likewise pause when only one slot
remains. Neither path bypasses limits to complete a conversion. Transport scratch,
allocator overhead, post-acquisition notification/caller clones and database file
growth remain outside this declared acquisition-capacity accounting.
Unverified raw publish paths must not create/open/compact databases: unloaded
accounts are loaded only after JSON, author, signature and content verification.
Their account ledger can now exist before database load, but no runtime policy can
be installed in this checkpoint. Runtime configuration must populate that immutable
startup ownership before activation. Ownership alone does not close the unloaded
preparse policy gap: the existing per-request HTTP body limit is still not an
aggregate capacity bound for disabled requests.

The payload race schedules at most four attempts. A peer is consumed only after
its read/conversion capacity is acquired; tight budgets wait for active reads and
then try the same pending peer. They must not repeatedly skip a later available
holder merely because an earlier unavailable holder owns the buffer slots.
Each cache connect/read attempt has the existing 30-second peer-operation deadline,
so a hanging first holder cannot retain tight-capacity slots indefinitely.
Configured raw HTTP body parsing also has a 30-second deadline before releasing
provisional capacity; unconfigured parsing retains ordinary disabled behavior.
FEED senders count AlreadyHave as acknowledged delivery, avoiding repeated
broadcasts after a peer accepted an earlier attempt. DoesNotNeed remains retryable
because it can mean temporary capacity pressure rather than permanent refusal.

Temporary pauses propagate as `DbError::PayloadAdmissionPaused`, not peer
failure or permanent pruning. Missing retries postpone their observed schedule
by 30 seconds with a compare-and-set, preserving attempt count/last-attempt time.
A 100-ms minimum pause bounds self-wakes and non-forward/saturated clocks; later
authors can proceed. Missing notifications register before peeking, with bounded
empty-queue recovery polling. Ancestor/head sync leaves durable Missing retries
instead of stopping its worker on temporary pressure.

Production activation still requires ranking-aware temporary versus permanent
admission, DryRun and complete Client/ingress integration. The private driver below
already composes generation/pressure/reducer atomicity, independent readiness,
hysteresis and bounded yielding batches.
The user-approved runtime clock assumption is to **trust the system clock**,
including startup: no acknowledgement workflow or omission of otherwise known
origins is required. Existing future/grace checks and protection for unknown legacy
origins remain. No production worker is activated.

## Next checkpoint

### Non-activatable pending-demand foundation

The database includes **crate-private, non-activatable** demand registration,
one-step preemption, and an internal maintenance/acquisition integration driver.
Only disposable configured tests install that driver; production acquisition
does not register demand and every production account remains Disabled. No
startup mode/budget API or live reconfiguration exists. The private test-installed
driver also performs bounded general pressure and quota-only collection.

A failed reservation can occur at `cap - 1` even though retained plus reserved
usage is below the cap. A metadata-only `PayloadDemand` expresses intent to make
room for that incoming event. Clones deduplicate by full event identity and share
one fixed 30-second walltime deadline; duplicates cannot refresh it. Count and
summed signed lengths independently reuse the explicit in-flight count and byte
limits as bounds on intent, without charging buffer capacity or promising logical
storage. A demand owns no payload or allocation guard. Cancelled, expired,
completed, newly reserved, reattached or policy-replaced demands cannot authorize
subsequent preemption. Unknown/future origins, protected incoming kinds and events
larger than either applicable ceiling cannot register preemption intent.

The internal step ranks live demands, rederives their Missing
ranks from durable headers/origins, and uses **one demand**, not an aggregate
sum, to check author pressure before database pressure. Before starting a plan,
any fitting demand returns `Fits` without eviction. An exhausted or byte-blocked
higher-ranked plan permits a lower-ranked author's feasible plan to proceed.
Once a plan starts eviction, its full event/lease identity keeps priority until
it reserves, cancels, expires or becomes invalid. Even if that partial plan becomes
blocked, another plan cannot spend the same released capacity before its owner
resolves. The original nonrenewable lifetime bounds this barrier; no additional
promise or buffer is retained. Otherwise the step considers
the lowest current candidate in the appropriate author/global index, and can
evict only a strictly lower-ranked candidate. It checks complete generation,
accounting, exhausted due prefix at the fixed timestamp, candidate ownership and
eligibility in the same writer transaction as the checked quota transition.
Promotion's `ready` flag still describes backfill only.

The demand arbitration mutex serializes cancellation and logical reservation
release against the synchronous destructive reducer; acquire it before the
ordinary admission-state mutex. DB writer serialization excludes reservation
additions. The step releases the state mutex before reducers reacquire it and
keeps arbitration until the reducer returns. Commit hooks execute after these
guards drop. Buffer-only releases need no demand arbitration. Cancellation
linearizes at demand removal: it either precedes the checked reduction or waits
for that reduction; it cannot retroactively undo a committed eviction.

Each call can prune at most one payload, caps visited candidate rows **across all
plans** at the supplied count (maximum 4096), refuses a candidate larger than the supplied
logical-byte allowance, and checks a cooperative deadline between bounded demand
and candidate visits. Individual DB operations/projection reduction remain
indivisible. The count-bounded demand pass and sorting remain bounded by the
configured intent count, independently of the candidate-row allowance.
`Continue` means a frontier advanced or another unvisited plan remains; yield
before another bounded turn. `NotReady`, `NoVictim`, and `Bounded` are not permission to spin:
the later scheduler must reconcile readiness or sleep until relevant changes.
It must not repeatedly retry protected overload or an unattainable byte budget.
The normal internal entry points sample fresh trusted walltime after acquiring
the writer and cancellation locks, and reuse that one timestamp for every check
in the transaction. A blocked/paused turn therefore cannot reuse expired
pre-lock authority. Clock injection is confined to internal test helpers.

Each demand carries one constant-size advisory cursor, not a retained victim list.
It skips only rejected rows, remembers exhaustion and the next victim's required
byte allowance, and never skips an eligible minimum merely because it is too large.
The cursor resets on author/global pressure-scope changes, backwards walltime,
the earliest skipped future row's eligibility time, or an index mutation revision.
Forward walltime before that deadline does not restart a long future prefix.
Generation/config replacement and reattachment invalidate the entire demand.
Lifecycle index changes, generation replacement, cleanup and grace promotion
invalidate scans before mutation without taking the demand mutex. Even an aborted
mutation may conservatively restart scans. Duplicate/no-op index refresh does not.
A successful reducer records partial-plan priority before commit releases the
writer; commit failure can conservatively retain that priority, but it never
supplies authority independent of current transactional checks.

`NoVictim` includes an advisory earliest expiry/eligibility walltime. A future
scheduler must also observe lifecycle, admission and readiness notifications,
register before checking, and use bounded recovery waits rather than trusting the
hint as a clock latch. Too-small time or byte allowances return `Bounded`, not
permission to retry the same work continuously. Under stable inputs and sufficient
per-operation time/byte allowance, bounded turns reach alternate plans and pass
finite future prefixes; continuous relevant mutation can restart that progress.
This is cursor/fair-plan support, not a proof of a complete worker's liveness.

Diagnostics now distinguish pending intent count/bytes from logical reservations
and owned acquisition buffers. Demand usage is an advisory live walltime snapshot,
not retained usage, unique store bytes, or physical allocation. This primitive
does not collect nominated bytes and does not implement general over-cap pressure,
90% low-water hysteresis, permanent ranked Missing rejection or DryRun.
The internal integration below adds bounded scheduling, general pressure/GC and
acquisition ownership while paused, not a complete enabled runtime. The full activation obligations in the
phase-3 handoff remain blockers for exposing enabled runtime modes.

### Non-activatable maintenance and acquisition integration

Private test construction can bind `PayloadRuntime` to the exact policy/holder
generation and immutable admission-config incarnation before exposing a disposable
database. There is no production constructor or setter. The ordinary
`prepare_payload_acquisition` method then exercises the real preparation path,
including shared-store reuse, while retaining only the verified header and one
metadata-only demand owner between attempts. Internal copy/conversion guards have
dropped before a paused attempt returns. Duplicate callers share the original
nonrenewable demand; cancellation drops ownership and expiry returns Deferred
without refreshing the intent. An additional monotonic 30-second wait bound
prevents a backwards system clock from extending an acquisition indefinitely.
Deferred remains a temporary outcome, not a permanent quota decision.

The driver independently advances accounting, quota-nomination recovery, policy
index backfill and grace promotion in round-robin one-row transactions, even
without acquisitions. Each turn has explicit operation, logical-eviction-byte
and cooperative-time limits. The phase survives one-operation turns; the driver
finishes a bounded reconciliation cycle before sleeping rather than delaying
the demand stage behind repeated idle maintenance stages. A promotion prefix uses
one fixed walltime until drained. A later destructive demand step samples fresh
walltime under its writer/arbitration locks and can require a newer prefix drain;
the older promotion time never supplies pruning authority.

Both runner and waiter register notifications before checking work. Runner
continuation yields; blocked, exhausted, fitting and idle demand results wait
after reconciliation. A minimum 100-ms wait limits notification-churn retries;
one-second recovery waits also observe missed signals and future grace eligibility
without treating walltime hints as a latch. This is bounded polling of metadata,
not a new clock monitor. A candidate too large for the turn allowance cannot
cause immediate repeated pruning attempts or be skipped for a larger-ranked one.

`run` borrows the database and has a unique RAII runner owner. It spawns no task
and performs synchronous, indivisible DB transactions; dropping or aborting its
owning future releases exclusivity after the current transaction finishes. Tests
scope runner futures to acquisition completion and cancellation. Before production
activation, Client must own and join this runner through its existing retained
task-completion machinery. This checkpoint does not start it in Client or claim
that end-to-end Client teardown and every ingress path are already integrated.

The integration tests cover cap-minus-one preparation through preemption,
reservation and actual ingestion; independent single-operation maintenance;
deduplication/cancellation/expiry with zero paused buffer charges; shared-store
reuse; alternate-author progress behind an exhausted higher-ranked demand;
bounded continuation past a previously promoted future prefix;
and byte-blocked waiting with runner cancellation/exclusivity. Durable ranked
rejection (including sustained refetch suppression), DryRun, immutable enabled
startup policy and the complete enabled bypass/load/body/race/overload audit remain
activation blockers. General pressure and quota-only collection are integrated
only in this non-activatable driver; unique stored bytes may remain after logical
eviction, and no physical reclamation is claimed.

### General pressure, hysteresis and quota collection

The driver visits one accounted author and at most one candidate per pressure
operation. Retained logical bytes **plus logical reservations** trigger pressure
only above the applicable explicit high water. Once triggered, the scope remains
active until that sum reaches `floor(90% * high_water)` or below; whole-event
reductions can undershoot. Author pressure runs before global pressure. Global
pressure is first evaluated after the author pass, as in the pure simulator.
Protected or exhausted authors keep their author target but do not prevent later
authors or global work. No event-sized victim list or author-sized RAM map exists.

Schema **31** adds disposable `content_pressure_state` and
`content_pressure_authors` tables. A checked monotonically increasing runtime
incarnation owns the singleton global latch and per-author latch/frontier rows.
Dropping a runner cursor does not lose the same runtime's targets. A fresh runtime
does not inherit previous-config targets: it evaluates current startup pressure,
and a one-author-at-a-time pass removes stale/inactive rows or replaces active
ones. Accounting retains zero-usage author rows, so every old pressure row remains
reachable for this bounded cleanup. Total replay discards both tables; neither
table changes immutable origins or quota-decision authority.

A complete author pass is advice tied to logical-usage/reservation and candidate
mutation revisions, plus the earliest skipped author eligibility time. Lifecycle
logical changes, logical lease addition/drop/completion/reattachment, index
changes, reaching that eligibility time and backwards walltime invalidate the
pass before global selection. Aborted mutations may conservatively invalidate
advice. Revision saturation fails shut rather than reusing a sweep. Actual
duplicate no-op delivery and buffer-only churn do not invalidate logical advice.
Invalidation waits before restarting, preventing mutation storms from spinning;
finite stable inputs and adequate operation allowances permit progress. Sustained
relevant mutation can delay a complete author sweep; this is not an unconditional
liveness guarantee under arbitrary writes.

The mutation-to-sweep invalidation contract is:

| Mutation | Invalidation owner |
| --- | --- |
| Header/contribution creation or changed retained logical bytes | before/after accounting owner, inside the writer |
| Logical lease addition | reservation insertion, inside the writer |
| Logical lease cancellation/drop | owner Drop under demand arbitration and state |
| Logical lease completion | after-commit removal under demand arbitration and state |
| Account reattachment | logical-ledger reset under both locks |
| Candidate membership or promotion | existing index revision |
| Duplicate no-op delivery; buffer-only acquire/drop | no logical revision change |

`runtime_pressure_revision_mutation_matrix_and_saturation` checks logical
mutations, rollback invalidation, lease lifecycle/reattachment, no-op negatives
and saturation-to-error without pruning.

The pressure step samples trusted walltime after taking the writer and demand
arbitration locks. In that same transaction it checks full generation, config
incarnation, accounting/due-prefix readiness, live demand barriers, current
reservations, scope pressure, exact candidate ownership and eligibility, then
calls the checked Processed quota reducer. A fitting live demand or the exact
active partial-plan event/lease prevents general eviction from spending room
already freed for acquisition. Unrelated nonfitting or infeasible intent does not
block general pressure. Arbitration remains held through the synchronous reducer;
the ordinary state lock is released before reducers reacquire it. Unknown legacy,
local, state-bearing, future/grace and true-minimum byte protections remain intact.

The seven-phase scheduler shares one operation allowance and cooperative deadline
across accounting, nominations, index, grace, demand, pressure and collection.
Demand and general eviction share a strict logical-byte allowance. Collection
has a separate strict allowance measuring **unique content-store value bytes
removed**, not logical bytes, decoded bytes, allocator memory or physical pages.
One DB operation and its validation/reducer are indivisible; no hard walltime,
whole-process memory or physical-reclaim bound is claimed.

Collection visits one quota nomination per operation using an exclusive hash
cursor. Oversized values remain nominated while later smaller hashes can progress;
there is no oversized-operation exception. A completed sweep sleeps before retry,
so retained oversized nominations cannot pin the frontier or cause a busy loop.
Earlier concurrent insertions and final-reference-release requeues are revisited
on the next bounded recovery sweep. An unchanged insufficient byte allowance can
leave oversized values unreclaimed indefinitely. The existing collector's checked
RC, historical local/non-social protection and quota-only provenance rules remain;
unrelated signed-delete, invalid and legacy garbage is not newly nominated.

Remaining checkpoints must add runtime configuration and retention-worker integration, including
the unloaded-account HTTP policy boundary above, before any activation path.
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
separately as well. The runtime will trust the system clock per the explicit user
decision; immutable timestamps cannot prove clock reliability. No clock-jump
acknowledgement, durable high-water latch or reset policy is required.

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

`payload_demand_tests` covers cap-minus-one pressure without aggregate-demand
eviction, author-first selection, true-minimum selection after due-prefix
draining, count/byte/time bounds, deduplication, cancellation, expiry and stale
owners. It also covers reservation-release arbitration through the reducer,
clock sampling inside the writer boundary, policy/config invalidation,
checked-reducer rollback with live intent preserved, and protected/no-victim
overload without pruning. These are disposable configured primitive tests, not
enabled-runtime liveness or acquisition-path coverage.
Bounded continuation tests cover alternate-author progress after exhausted higher
rank, a future prefix across forward walltime, rollback rewinding, earliest
eligibility retry, newly promoted minima before a saved frontier, and partial-plan
priority through cancellation, replacement ownership and expiry. They also pin
byte-blocked alternate plans and larger-budget retry, no-op duplicate refresh, and
conservative scan reset after an aborted lifecycle mutation.
