# Payload pruning: discussion draft

**Status: proposal, not an adopted specification.** The pure experimental
ranking policy, in-memory simulator, and durable database retention source
metadata are implemented; see the [implementation guide](payload-retention-policy.md)
and [database checkpoint](payload-retention-database.md). Admission/fetch ownership
and a private, test-installed driver now integrate bounded demand/general pressure,
hysteresis, conservative ranked durable Missing rejection and quota-only collection.
The private driver also supports bounded read-only whole-snapshot DryRun projections,
with a separate forecast config and Disabled acquisition. No production runtime
pruning or enabled quota configuration is available; complete Client/startup/ingress
integration remains unfinished.

## Recommendation

Implement **quota-driven, deterministic payload eviction**. Prefer retaining
small, recent payloads whose event IDs are close to the local storage node's ID.
Keep every signed event header. Use a bounded distance bonus so closeness never
amounts to permanent storage.

The proposed policy has three separate parts:

1. **Eligibility:** which payloads may safely be discarded?
2. **Ranking:** among eligible payloads, which are least worth retaining?
3. **Pressure:** how many bytes must this account or database give up?

Use an exponential age-decay formula. It reduces to a **static, indexable
eviction key**: no periodic full-database rescoring is necessary.

This gives opportunistic, decaying replication—not guaranteed distributed
storage. Different nodes tend to keep different old material, but nothing
ensures that the last copy survives.

The most important implementation work is not the formula: it is preserving
content lifecycle, projection, replay, and accounting invariants while actually
freeing bytes and preventing immediate re-download.

## 1. Scope and existing foundations

First version:

- Retain headers, signatures, DAG edges, and signed deletion instructions.
- Prune whole event payloads, not sections of payloads.
- Keep ordinary replication and verification intact.
- Introduce no DHT, placement protocol, availability claims, or replica counting.
- Start with explicitly classified expendable social content; protect
  state-bearing and unknown kinds until their replay semantics are reviewed.
- Default to dry-run until operators choose budgets and observe the effect.

The current code already provides much of the foundation:

| Existing facility | Relevance |
| --- | --- |
| `EventContentState::Pruned` | Local terminal decision, distinct from author's deletion |
| `IdsDataUsageRecord` | Per-author accounting, including current/missing/pruned/deleted/invalid buckets |
| `content_store` and `content_rc` | Hash-deduplicated bytes and event references |
| Header content hash, length, kind | Ranking/admission metadata without downloading payload bytes |
| Missing-content queue | Durable scheduling that pruning must remove or suppress |
| Database's local `RostraId` | Retention identity independent of transport keys |

Sources: [content lifecycle specification](../crates/rostra-client-db/specs/SPEC-event-content-lifecycle.md),
[database lifecycle guide](../crates/rostra-client-db/docs/content-lifecycle.md),
[database tables](../crates/rostra-client-db/src/tables.rs),
and [client construction](../crates/rostra-client/src/client.rs).
These describe current behavior; the sections below propose changes.

Disposable global logical/unique-byte accounting is implemented. No production
background pruning/GC worker is enabled; only the private test-installed driver
performs automatic pressure relief and quota-only collection.
The existing `prune_event_content_tx` helper is used for oversized payloads at
initial envelope ingestion, before ordinary projection processing. Reusing it
for already processed payloads requires the additional lifecycle work below.

**Do not compare the quota against `total_content_size`.** That total includes
payloads already missing, pruned, deleted, or invalid. Eviction cannot reduce
it. Use the retained/current bucket for logical payload storage, and separate
missing-payload admission reservations.

## 2. Distance and identity

Use the **storing account's `RostraId`**, not the event author's identity or
the storage device's iroh public key. For a client database, this is its local
Rostra identity; for fetching, it is the candidate holder's Rostra identity.

Transport keys may need to rotate for privacy. Such rotation must not change
the retention coordinate, rebuild eviction indexes, or reshuffle stored history.
Using `RostraId` also lets fetchers rank candidate accounts before resolving
their transport endpoints.

Devices serving the same Rostra identity deliberately share the same distance
preference. With identical inputs, policy, and budgets they retain the same
tail; different received content, timestamps, or budgets can still produce
different retained sets. We accept reduced within-account diversity: users are
not expected to operate large numbers of replicas, and account-level fetching
is simpler than enumerating and ranking all their devices.

Define two domain-separated 256-bit coordinates using the project's
cryptographic hash:

```text
event_coordinate  = H("rostra/retention/event/v1"  || full_event_id)
holder_coordinate = H("rostra/retention/holder/v1" || full_holder_rostra_id)
x = integer_big_endian(event_coordinate XOR holder_coordinate)
d = x / 2^256
```

Use the full event ID reconstructed from its retained signed header, not its
short database key. Domain strings, encoding, hash choice, and metric version
must be specified before implementing fetch ordering. Do not assume that
different identifier types already have interchangeable distributions.

This borrows Kademlia's XOR metric, not its routing or replication guarantees.
See Maymounkov and Mazières, *Kademlia: A Peer-to-peer Information System Based
on the XOR Metric* (2002), section 2.1.

Make the distance bonus bounded:

```text
B(d) = 1 / max(d, 1 / B_max)
```

Thus `1 <= B <= B_max`, including the zero-distance case. Closer nodes receive
a larger bonus. Beyond the cap, they receive no further storage preference.

## 3. Retention formula

For event `e` on node `n`, propose:

```text
s = max(verified_header.content_len, s0) / s0
age = now - t_eff

R(e, n, now) = exp(-age / tau) * s^(-alpha) * B(d)^beta
```

**Evict the lowest `R` first.** This is a relative utility score, not a literal
probability or a promise of how long an event will survive.

Parameters:

| Parameter | Meaning | Initial simulation value |
| --- | --- | --- |
| `s0` | Size floor; tiny payloads have the same size penalty | 1 KiB |
| `tau` | Age scale used to trade age against size and distance | 30 days |
| `alpha` | Preference for smaller payloads | 0.5 |
| `beta` | Strength of specialization between holder accounts | 1 |
| `B_max` | Maximum closeness bonus | 64 |

These are proposed experiment settings, not production defaults. The user's
storage budget should be explicit; there is no justified universal GiB quota.

At fixed size and distance, older content loses. At fixed age and distance,
larger content loses. At fixed age and size, farther content loses.
There is no fresh random eviction draw: the cryptographic coordinates provide
stable variation between holder accounts. Randomizing every sweep would create churn
and destroy useful predictability for fetchers.

### A static key instead of continuously changing scores

Take logarithms and multiply by `tau`:

```text
tau * ln R = -now + K
K = t_eff - tau * alpha * ln(s) + tau * beta * ln(B)
```

At a particular node, `-now` is common to every candidate. Therefore:

> **Keep high `K`; evict low `K`. `K` changes only when policy or source
> metadata changes, not when the clock advances.**

Treat `K` as a virtual timestamp: size makes an event virtually older, closeness
makes it virtually newer.

With the example settings:

| Change | Virtual-age effect |
| --- | --- |
| Double payload size, above the size floor | About 10.4 days older |
| Increase from 1 KiB to 1 MiB | About 104 days older |
| Distance 0.5 instead of approximately 1 | About 20.8 days younger |
| Distance 0.1 instead of approximately 1 | About 69.1 days younger |
| Distance at or below 1/64 | Maximum bonus: about 124.8 days |

A 100-day-old 1 KiB payload at distance 0.01 has virtual age about -24.8
days; a 10-day-old 1 KiB payload at distance 0.5 has virtual age about -10.8
days. The older, closer payload wins. Making that older payload 1 MiB adds
about 104 days of penalty, and it loses instead.

Do not calculate exponentials for every comparison. Store a signed,
fixed-resolution virtual timestamp, with an event-ID tie-breaker. Define
rounding, overflow handling, and the logarithm approximation as part of the
policy version. Changing parameters requires a bounded index rebuild;
do not mix differently scaled keys in a live index.

### Why this formula, and what it does not do

Exponential decay gives simple tradeoffs and an exact static ordering.
Power-law age decay could produce a longer tail, but would generally change
relative rankings over time and complicate indexing.

There is deliberately **no age-only expiration**. Below quota, keep everything
eligible that has already been admitted. During continuing ingestion, newer
material raises the effective retention cutoff and old material fades out.
An idle database does not delete its history simply because a timer fires.

Because only ranking matters, `tau` is not an actual retention lifetime:
its products with `alpha` and `beta` set the age tradeoffs. Capacity and traffic
determine the eventual cutoff.

### Choosing the age timestamp

Persist a pruning-specific timestamp at first accepted header receipt:

```text
t_eff = min(author_timestamp, first_local_header_receipt)
```

Never reset it on payload re-download or duplicate delivery. This preserves
the age of backfilled history while preventing future-dated headers from
buying arbitrarily long retention. It does not make author timestamps honest:
an author can still republish old content in a fresh event.

Use a separate, bounded grace period after first payload materialization
(initial experiment: 24 hours) so a newly fetched historical post is usable.
Grace is an eligibility rule, not a score bonus. Apply it only once, persist
its origin, and allow pressure admission control to reject content before it
consumes this protected allowance. Treat unreliable local clocks and missing
migration timestamps conservatively and document the migration policy.

## 4. What “distributed sharding” would mean

Suppose, only for analysis, that replicas have the same candidate set,
timestamps, parameters, and effective log-score cutoff `ell`.
Let `a = age/tau + alpha*ln(s)`. For `beta > 0`, retention requires:

```text
beta * ln B(d) >= ell + a
```

For uniformly distributed holder-account coordinates this yields:

- Everyone retains it when `ell + a <= 0`.
- A fraction approximately `exp(-(ell + a)/beta)` retains it when
  `0 < ell + a <= beta*ln(B_max)`.
- Nobody retains it when `ell + a > beta*ln(B_max)`.

With `m` independent holder accounts that actually received the payload, each
keeping it with probability `p`, the toy-model probability of at least one surviving
copy is `1 - (1-p)^m`. For example, `p=0.1` gives only about 41% with five
holder accounts, versus about 88% with twenty. Multiple devices belonging to
one account must not be counted as independent distance-based retention choices.
They may improve reachability without adding distinct retention preferences.

These are deductions from the proposed formula, **not availability estimates
for Rostra**. Real replicas have different candidate sets, clocks, budgets,
uptimes, and follow graphs; outages and policies are correlated.
The cap also creates a sharp final tail cutoff in the equal-budget model.
Simulate that explicitly rather than assuming a few nearby nodes keep content
forever.

Distance only distributes copies among nodes that already received them.
It does not place a payload on the globally closest nodes. Rarely replicated
accounts remain vulnerable regardless of score.

Do not claim last-copy safety. Users who need durable history need an archive
or backup outside this opportunistic policy. Protected local-authored content
is a sensible default, but not a remote durability guarantee.

## 5. Quotas, accounting, and hysteresis

### Start with one database as the enforcement boundary

Rostra clients have per-identity databases. Define “global” initially as
**all retained payloads in one database**, not all databases on a hosting
process. A host-wide budget would need a separate allocator to divide budgets
between clients; independent workers cannot each enforce the same host cap.

Use:

- `Q_a`: logical retained-payload cap for tracked author `a`, normally from a
  common default with optional overrides.
- `G`: global logical retained-payload cap for this database.
- Low-water targets, initially `0.9 * Q_a` and `0.9 * G`.

Account usage is the sum of retained payload lengths for that author's events,
including protected ones. A payload referenced by two events is charged twice
logically, even if physically stored once. This matches the event-level nature
of existing accounting and avoids cross-author cost-attribution complexity.

The caps are ceilings, not reserved shares. An inactive author does not reserve
disk. Initially keep per-author caps strict rather than introducing borrowing.

Worker selection:

1. Bring authors above their high-water cap down to their low-water target,
   selecting their lowest eligible `K`.
2. If the database is above `G`, bring it down to `0.9*G` using the global
   lowest eligible `K`.
3. Stop once each triggered target has been met; deleting an indivisible
   payload can undershoot a target by its size.

The global fallback can evict from an author below its individual cap. This
does not guarantee equal per-author allocation; it maximizes the chosen
retention preference within ceilings. If equal guaranteed shares are wanted,
that is a different policy worth deciding explicitly.

### Logical usage is not physical space

Track separately:

- Logical current bytes by author and database.
- Unique live bytes in `content_store`.
- Zero-reference bytes waiting for garbage collection.
- Actual database file size and filesystem free space.
- Missing/in-flight reserved bytes, protected bytes, and prunable bytes.

Dropping one event's reference might free no physical bytes. A shared hash
cannot be removed while another event still references it, including a Missing
event that wants its bytes. The same may happen with protected content.

A logical global cap conservatively bounds ordinary live payload bytes without
requiring hash-group eviction, but may waste deduplication savings. This is the
recommended initial tradeoff. Do not promise a physical GiB limit from it:
headers, projections, indexes, garbage, and database overhead remain.

Run zero-reference garbage collection before escalating physical pressure.
Logical eviction, removal from the content table, reusable database pages,
and filesystem shrinkage are distinct outcomes. The current database guide
already warns that replay does not compact the file. Treat compaction as
separate maintenance, not part of each pruning batch.

### Protected content and overload

Propose protecting:

- Local-authored payloads by default, with an explicit operator opt-in to prune.
- State-bearing payloads and unknown event kinds in the first version.
- Explicitly pinned payloads if pinning is introduced.
- Content in bounded first-materialization grace.

Protected bytes still count. If protected bytes alone exceed a limit, report
that the target is unattainable. Do not silently unpin data or loop forever.
Stop admitting background payloads that would worsen the situation; permit
local publication only under an explicit available-space policy, otherwise
return a clear storage-full error. Reserve enough space to commit cleanup.

Continuous ingestion must not outrun a background worker indefinitely.
Bound in-flight fetch concurrency and bytes, reserve admission capacity, and
pause new background payload fetches when the reserve is exhausted. A sudden
disk emergency may require rejecting new data even below logical quotas.
Header retention itself remains unbounded in this proposal and needs separate
abuse limits; payload pruning does not solve header floods.

## 6. Indexes and worker

Proposed additions, named conceptually rather than as final Rust APIs:

```text
prune_by_author[(author, K, event_id)]
prune_global[(K, event_id)]
grace_expiry[(eligible_after, event_id)]
retention_meta[event_id] = immutable age/grace origins, local prune reason
retention_policy = version, parameters, storage coordinate identity
```

Only eligible, actually retained event payloads enter the eviction indexes.
Keep empty payloads out: deleting them saves nothing. Grace expiry promotes
entries into both indexes without rescoring. Protected entries stay out.
Include a reverse key or sufficient metadata to remove exact index entries.

Missing content is handled by admission policy, not falsely counted as bytes
the eviction worker can free.

Maintain indexes in the same transaction as lifecycle/accounting changes.
An existing database needs a bounded backfill before pruning is enabled.
Startup verifies the index policy version and resumes incomplete backfill.

A single background task per database should:

- Wake on a committed usage change, quota/configuration change, grace expiry,
  and startup reconciliation.
- Use a coalesced notification; durable usage is authoritative.
- Process bounded batches, initially experimenting with 128 events or 8 MiB
  of logical eviction, plus a wall-time bound.
- Recheck state, protection, quota, and reference counts inside each write
  transaction. Handle duplicate wakeups and concurrent deletion idempotently.
- Commit and yield so ingestion and reads remain responsive.
- Stop and surface a local database error on invariant/storage failure.

Selecting the next indexed victim costs roughly a B-tree seek followed by
per-victim transactional work, not a scan of all payload bytes. Initial
index construction is linear enumeration plus index insertion; policy changes
have similar rebuild cost. This is a design expectation, not a measured bound
on redb latency or projection cleanup.

Garbage collection needs its own bounded zero-reference work queue, populated
when a reference count reaches zero. Recheck the count before removing bytes:
a new reference can arrive after enqueueing. Count **actual removed unique
bytes**, not just logical evictions, as physical reclaim progress.

## 7. Lifecycle correctness is a prerequisite

Existing terminal-state support is useful but is not proof that switching on
bulk pruning is safe.

For a selected retained event, one transaction must consistently:

1. Recheck eligibility and current state.
2. Establish local `Pruned` state with a persistent quota-policy reason.
3. Move usage from current to pruned, without changing historical total usage.
4. Release exactly one content-hash reference.
5. Remove candidate/fetch entries and enqueue zero-reference GC if appropriate.
6. Apply the chosen projection policy and notify readers after commit.

No raw `content_store.remove()` outside the reference-count/lifecycle boundary.
Signed `Deleted` intent is stronger than local pruning and must remain
monotone. Racing deletion and pruning must not double-decrement anything.

### Social projections versus durable identity state

For expendable social posts, recommend treating pruning as **local
dematerialization**: the body becomes unavailable and the locally derived
post visibility/count/index state is updated consistently. It must not look
like the author issued a deletion. Keep header-level replacement/deletion
lineage and stable materialization-feed history according to their contracts.

Do not assume existing deletion helpers can simply be reused unchanged:
reply/reaction counters, mentions, news indexes, edit resolution, and the
materialization feed need explicit prune tests.

For follow state, profiles, node announcements, votes, and generic singleton
state, discarding source bytes while retaining projections creates a
replay question. Cached values may work during ordinary operation but disappear
or change after rebuilding from retained inputs. Protect these kinds initially.
Pruning their old sources requires a separate analysis of winners,
supersession, canonical checkpoints, and replay behavior—not just “they are
small, so they probably survive.”

### Pruning decisions must survive replay

The current total-replay design preserves envelopes and available content,
then reconstructs derived tables. Local quota decisions and immutable pruning
timestamps must become explicitly preserved replay inputs; otherwise absent
pruned content can turn into Missing and be downloaded again. If a pruned
event's bytes remain in the hash store because another event references them,
replay can instead immediately resurrect its projections.

This requires an intentional implementation-time update to the lifecycle and
migration contracts. This draft does not change those governing records.
Add restart, total-replay, and backup/restore tests before enabling deletion.
Increasing a quota should not automatically requeue all historical prune
markers; provide a separately bounded backfill operation if wanted.

## 8. Admission and deliberate re-fetching

All acquisition paths must share policy: missing-content retries, ancestor
sync, pushed/feed payloads, direct fetches, and content already present under
another hash reference.

For new Missing content, use header length to reserve bounded fetch capacity.
Initially admit in ordinary order while capacity permits; under sustained
pressure compare its proposed `K` with the relevant retained victim boundary.
Skip low-ranked history rather than downloading it only to immediately evict
it. Release reservations on every completion, cancellation, or state change.
Do not give every pending header an unlimited disk reservation.

When policy declines a payload permanently for this budget, record local
quota pruning and remove ordinary retry work. If merely waiting for the worker
or a transient reservation, leave it Missing and pause scheduling instead.
Missing-to-Pruned changes missing accounting and reference counts; it does
not reduce current logical usage. Releasing that reference may nevertheless
make existing shared-store bytes eligible for GC; measure actual reclaim
separately.

Keep `Pruned` terminal to **ordinary** ingestion. Late network responses must
not undo the decision. Current terminal behavior is an important protection
against prune/refetch loops.

Explicitly opening old content is a different operation:

- First version may display an honest “not stored locally” result.
- A useful follow-up is a bounded, verified transient fetch for viewing, without
  reprocessing the event or repopulating durable projections.
- Durable restoration needs an explicit quota-pruned-to-Missing transition,
  fresh admission, exactly one restored reference, and correct reprojection.
  It must never clear author deletion, invalidity, or size-limit rejection.
- Avoid access-time score updates and indefinite refreshable pins initially.

Distinguish local unavailability from “no peer has this content.” Failed fetches
cannot prove global disappearance.

## 9. Distance-aware fetching

**Yes: use closeness to order plausible holders, not to invent holders.**

Current fetch candidates are the author, known followers, and self.
`ConnectionCache::get_event_content_from_peers` races up to four peers;
the shared RPC helper and missing-content task build their own candidate lists.
See [RPC helpers](../crates/rostra-client/src/util/rpc.rs),
[connection cache](../crates/rostra-client/src/connection_cache.rs), and
[missing-content fetcher](../crates/rostra-client/src/task/missing_event_content_fetcher.rs).

Proposed common candidate-ranking helper:

1. Deduplicate candidates and retain existing authorization, reachability, and
   backoff restrictions.
2. Prefer a known successful holder and a reachable author/archive when such
   information already exists.
3. Among other plausible holder accounts, prefer the `RostraId` coordinate
   closest to the full event ID.
4. Keep small bounded concurrency and eventually try farther peers.

Candidate APIs already use `RostraId`, so their coordinates can be calculated
locally before endpoint resolution. Resolve and contact selected accounts
through ordinary transport discovery; no new content RPC or per-device
retention-coordinate advertisement is needed.

There is no need to enumerate an account's devices for distance ranking:
all share one coordinate. Ordinary connection fallback may still try another
device, and a miss on one device is not proof that every device lacks the
payload. Transport-key rotation does not change candidate ordering.

Peers need not publish their quota, cutoff, or payload inventory. Distance is a
cheap hint: heterogeneous budgets and whether a node ever received an event can
matter more. Keep the fallback and measure hit rate.

## 10. Abuse and operational limits

- **Event-ID grinding:** authors can vary event data to seek favorable
  coordinates. Domain separation does not prevent this. The bounded bonus and
  per-author quota limit the benefit; this is not Sybil resistance.
- **Holder-ID grinding:** peers can generate Rostra identities seeking attractive
  coordinates; rotating only an iroh key gives no retention advantage. Closeness is neither
  trust nor evidence of possession. Verify returned content normally.
- **Author churn:** new identities can evade per-author fairness only if admitted.
  Preserve Web-of-Trust admission and enforce the database-wide cap.
- **Republishing:** fresh wrappers can rejuvenate identical bytes logically.
  Charge each event to its author; do not increase every existing reference's
  timestamp when a hash is seen again.
- **Large bodies:** keep existing exclusive maximum-length validation. Size
  weighting is not a replacement for allocation and network limits.
- **Local-authored/pinned floods:** protection makes targets unattainable rather
  than silently relaxing the budget.
- **Privacy:** fetch ordering should not expand the follow/trust graph or
  disclose requests to arbitrary globally close strangers.

## 11. Evaluation before enabling destructive mode

Build a deterministic simulator before tuning production values. Compare:

- Oldest-first.
- Age plus size, without distance.
- The proposed age/size/distance score at several bonus caps and exponents.

Use identical budgets and traces with heavy-tailed sizes, bursty authors,
historical backfill, duplicate payload hashes, different node budgets,
partially overlapping follow graphs, same-account multi-device replicas,
rare accounts, correlated outages, and adversarial timestamps/ID selection.

Measure:

- Retained bytes and item count by author, age, and size.
- Unique network-wide history retained at equal aggregate storage.
- Last-copy loss and availability specifically for rare accounts.
- Fetch success rate and attempts with and without distance ordering.
- Churn: bytes downloaded again per byte evicted.
- Logical versus unique-byte reclaim and GC backlog.
- Protected-budget exhaustion, worker transaction latency, and write overhead.

Required correctness tests:

- Monotonic score properties, cap/zero distance, and static-key equivalence.
- Restart-stable keys; deterministic ties and policy-version rebuilds.
- Iroh key rotation leaves retention keys and candidate ordering unchanged;
  same-RostraId devices use the same distance, and the simulator models their
  correlated retention choices.
- Author/global hysteresis, empty payloads, oversized victims, no eligible
  victims, and admission under continuous overload.
- Shared hashes across authors, protected references, Missing references,
  and racing reference creation versus GC.
- Duplicate prune, prune/delete races, in-flight delivery after pruning,
  interrupted batches, and stale candidate entries.
- Social projection/reaction/reply correctness before and after pruning.
- Ordinary restart and total replay preserve prune decisions and do not refetch.
- Deliberate restoration/transient reads cannot override Deleted state.

Expose a dry-run report listing proposed victims and their age, size, distance
bonus, protection status, and expected logical versus unique-byte savings.
Never make a production-shaped database the first destructive test.

## 12. Decisions to discuss

The storage-coordinate choice is settled for this draft: use the holder's
`RostraId`, accepting correlated retention across its devices in exchange for
transport-key independence and account-level fetching.

1. **Tail length:** is a maximum distance age credit around four months a useful
   initial experiment, or should specialization reach much farther into history?
2. **Fairness:** strict per-author caps plus global score ordering, or guaranteed
   equal shares at the cost of more allocation machinery?
3. **Protected data:** protect local-authored and all state-bearing content by
   default? Which social content may disappear locally?
4. **User retrieval:** is an initial unavailable-content indicator sufficient,
   or should bounded transient viewing ship with pruning?
5. **Budget scope:** one client database first, or is multi-account host-wide
   budgeting required in the first release?
6. **Grace/admission tradeoff:** how much recently fetched content should receive
   guaranteed local dwell time before it competes on age and distance?

My suggested first implementation is the static score, per-author and
per-database logical caps, bounded worker and GC, conservative eligibility,
durable prune decisions, shared admission checks, and dry-run tooling.
Distance-aware fetch ordering can use existing RostraId candidate lists as a
small follow-up. Replica guarantees and state-source pruning remain separate
projects.
