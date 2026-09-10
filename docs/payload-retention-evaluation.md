# Disposable payload retention evaluation

## Conclusion and scope

This is an **offline synthetic experiment, not evidence for enabling Enforce or
choosing production quotas**. On the fixed snapshot below, distance specialization
increases network-wide retained event diversity at similar aggregate logical
usage. It does not preserve every last copy, including rare authors. Distance
ordering helps only when retention actually correlates with distance; it makes
the oldest-first and age/size baselines worse in this fixture. Two devices using
the same storing RostraId make identical retention choices and are not independent
replicas.

The disk workload also exposed and reproduced a runtime scheduling bug: completed
cycle progress could be reused as fresh progress, preventing the idle/recovery
boundary needed to continue pruning and GC. This phase fixes that bookkeeping,
without changing quotas, policies, byte allowances or writer authority. The
pre-fix failed measurements below remain part of the evidence.

No live database, copied user database, service, startup configuration, external
network peer, deployment or publication participates. The DB experiment creates
new temporary directories internally and removes them on normal completion.
Crashes may leave disposable temporary files; never substitute a real DB path.
No universal budget or production policy default changes.

## Reproduce

From the repository's development shell:

```sh
bash misc/evaluate-payload-retention
```

The script prints a newly created output directory with environment, policy CSV,
author/age/size distribution CSV and release DB timing output. It runs the policy
experiment twice and byte-compares both outputs. It takes minutes, not a tiny
correctness-test timescale. The opt-in DB workload is ignored by routine tests;
the source still builds and is checked by clippy and full SelfCI. The script caps
the release command at 45 minutes (including build); forced termination can
leave newly generated temporary data. The runtime loop also has a cooperative
15-minute measurement deadline and 100,000-turn allowance and fails, rather than
claiming completion, if either is exhausted.

Individual commands:

```sh
cargo run --offline --locked --quiet -p rostra-core --example retention-evaluation > policies.csv 2> distributions.csv
cargo test --offline --locked --release -p rostra-client-db disposable_payload_index_evaluation -- --ignored --nocapture
```

The simulator has no random OS seed: BLAKE3 of literal domain/counter strings
defines synthetic IDs. DB signing keys derive from fixed bytes, and timestamps
are fixed. Measurements of real wall-clock latency and filesystem allocation are
not deterministic. The DB runtime deliberately uses trusted real system time.

## Snapshot experiment

`crates/rostra-core/examples/retention-evaluation.rs` calls the existing pure
simulator. Every policy gets the same 4,096 events, 32 authors, follow sets,
per-account budgets and replay-request counts:

* Twelve devices, eleven storing accounts. Devices 0 and 1 share identity, budget
  and follow set. Other accounts have 4, 6 or 8 MiB global ceilings. Each author
  has a 2 MiB ceiling. Low waters are explicit (90% global; 1,800 KiB author).
* A finite heavy tail: 1 KiB usually, 16 KiB every eighth event, 256 KiB every
  sixty-fourth event. These are declared logical sizes, not allocated payloads.
* Sixty-four-event author blocks represent bursts; 256-event receipt-day groups
  and old signed times represent backfill. This is a **retained snapshot** of
  that corpus, not a simulation of ingestion order or burst-time admission.
* Shared synthetic hashes pair blocks of 64 with equal size classes. Duplicate
  delivery is represented by three later requests for every projected victim,
  not duplicate event rows (the existing simulator rejects duplicate IDs).
* Most authors reach three quarters of accounts; rare author 31 has 16 events
  reaching only accounts 0 and 1. Those two accounts, including both devices of
  account 0, suffer the correlated outage in the fetch experiment.
* Every 97th event chooses the closest of sixteen candidate IDs to account 0.
  This bounded grinding sample is not a cryptographic attack-cost estimate.
* Every 101st signed time is far in the future and is clamped to first receipt.
  Every 113th materialization origin is unknown; every 127th event is explicitly
  protected. A backward observation protects future receipt origins. There is
  no fabricated startup clock distrust.

All policies use a 1 KiB floor, 30-day time scale and one-day grace. The CSV names
mean: `oldest` α=0, β=0; `age-size` α=0.5, β=0; `distance-8` α=0.5, β=1, cap=8;
`distance-64` α=0.5, β=1, cap=64; `distance-strong` α=1, β=2, cap=64.
Exponents are supplied as exact unsigned Q16 parameters to the production score.

### Observed normal-clock results

Observation time is synthetic day 400. Network diversity counts unique **events**,
not shared content hashes. Aggregate retained logical bytes include every device.
Budgets are identical across policies, but whole-event eviction means actual
retained bytes are only approximately equal (62.94–63.28 million).

| Policy | Network events retained / 4096 | Last-copy events lost | Rare events lost / 16 | Online hits / 4096 | Attempts unordered / distance |
| --- | ---: | ---: | ---: | ---: | ---: |
| Oldest | 1,556 | 2,540 | 11 | 1,551 | 25,147 / 25,930 |
| Age + size | 2,192 | 1,904 | 8 | 2,184 | 20,368 / 21,246 |
| Distance cap 8 | 2,323 | 1,773 | 7 | 2,314 | 19,916 / 19,672 |
| Distance cap 64 | 2,448 | 1,648 | 7 | 2,431 | 19,397 / 18,655 |
| Strong distance + size | 3,450 | 646 | 4 | 3,361 | 13,315 / 11,481 |

Each event has one request, with the same nine online account candidates in both
orders. Failed requests count all nine attempts. Success is independent of order
because all candidates are tried; no timeout, bandwidth, reachability learning,
inventory, discovery or malicious response behavior is modeled. Runtime fetching
also applies existing reachability preferences first; this isolates only the
distance ordering term and is not an end-to-end network benchmark.

Rare-author availability is also reported separately for two correlated outages.
With only duplicated account 0 offline and account 1 still online, normal-clock
rare hits out of 16 are respectively **5, 8, 8, 8, 11** in the table's policy
order. With the entire two-account rare follow set offline, all policies have
zero rare hits by construction. This distinguishes actual partial-outage loss
from the structurally unavoidable full-follow-set outage rather than hiding rare
availability in the network-wide hit count.

Normal-clock logical eviction totals are 185.30–185.63 million bytes across
devices. Under this synthetic sharing pattern, removed unique values total
108.87–111.84 million bytes, while 25.92–30.38 million unique bytes associated with
victims remain pinned by retained references. These are model sums over devices,
not real GC measurements or disk reclamation. There are no Missing/protected
historical-reference pins in this simplified hash model.

The constructed three-request replay trace would redownload 555.89–556.90 million
bytes if every victim were blindly downloaded again; durable terminal decisions
would suppress all three requests, yielding zero redownload bytes. The apparent
3:1 ratio is **an input, not an empirical churn result**. Actual runtime terminal
checks, shared reuse, failed reservations and restart/replay suppression are
verified separately by the correctness audit below.

At synthetic day 340, all receipts are future: every policy retains 248,573,952
logical bytes, all twelve devices have unmet pressure, and no eviction occurs.
At day 800, results equal day 400: all known origins had already passed grace.
That equality does not establish forward-jump safety; trusted forward jumps can
expire grace earlier. Unknown origins remain protected. Every run reports equal
retained sets for the two same-account devices.

The separate distribution output lists retained network event count and logical
bytes for every author, plus small/medium/large and old/recent (180-day effective
timestamp boundary) bins. Age and size bins overlap and must not be summed
together. This preserves inspection of distribution rather than only a favorable
aggregate diversity number.

For example, normal-clock network-union distribution (count / logical bytes):

| Bin | Oldest | Distance cap 64 |
| --- | ---: | ---: |
| Small (1 KiB) | 1,365 / 1,397,760 | 2,206 / 2,258,944 |
| Medium (16 KiB) | 168 / 2,752,512 | 219 / 3,588,096 |
| Large (256 KiB) | 23 / 6,029,312 | 23 / 6,029,312 |
| Old (effective timestamp before day 180) | 34 / 96,256 | 470 / 757,760 |
| Recent | 1,522 / 10,083,328 | 1,978 / 11,118,592 |
| Rare author 31 | 5 / 266,240 | 9 / 285,696 |

Distance cap 64's added diversity in this fixture is predominantly small/older
events, not extra large payloads. Network-union bytes count each event once,
unlike the aggregate device storage numbers above; shared hashes still count per
event in this distribution.

## Actual disk-backed index and maintenance workload

The ignored `disposable_payload_index_evaluation` test creates two new redb files
sequentially and feeds identical 8,192 signed SocialPosts from 32 authors through
the real ingestion boundary. Signing and serialization are outside timing.
Logical retained content is 55,826,516 bytes; shared content values occupy
52,586,388 bytes. Sizes have the same 1/16/256 KiB tail plus actual CBOR overhead.
Both arms have ready accounting; only the second maintains the retention index.
There are no live acquisitions or concurrent writers in this experiment.

The indexed arm changes the disposable policy generation and times public
64-row rebuild calls over the populated index. Each call is one real writer
transaction including commit, unlike a timer around only score arithmetic.
It then installs the test runtime against the already loaded disposable corpus,
with global ceiling half initial logical usage, no author pressure, zero grace,
64 operations, 1 MiB logical eviction, 1 MiB unique GC and a cooperative 10 ms
turn allowance. Nomination reconstruction is separately measured in bounded
64-row writer transactions before the destructive turn measurements. Due grace
promotion is likewise separately timed and must visit exactly 8,192 rows; the
final destructive measurements therefore begin with accounting, index,
nominations and the fixed-time due grace prefix ready. This internal
fixture setup is not a supported live config change. The actual runtime performs
readiness checks, indexed selection, checked pruning and quota-only GC. The test
honors `Wait` as a recovery boundary (100 ms spacing), not as evidence that
pressure was satisfied, and requires both low-water attainment and an empty GC
nomination queue within a finite 100,000-turn / cooperative 900-second allowance. Progress usage reads
every thousand turns and reads during recovery are outside turn timing.

Afterward, 32 actually pruned events each receive three calls through the real
acquisition-preparation API. All 96 must return `Unneeded`, with unchanged usage
and zero remaining guarded buffers. The declared bytes suppressed in that
specific sample are measured; they are not a natural-network churn ratio.

### Environment and measured output

Run on September 10, 2026: NixOS Linux 7.1.8 x86_64; AMD Ryzen 9 7950X3D
(16 cores / 32 threads), 61 GiB reported RAM; rustc 1.97.0
`2d8144b78` (2026-07-07). `/tmp` was disk-backed, on a 6.9 TiB filesystem
with about 1.2 TiB free, not tmpfs. This is a shared, non-isolated host;
background activity, page cache, storage, frequency scaling and fsync dominate
some tails. These timings are not hardware-independent bounds.

The complete post-fix script **passed**, including byte-identical repeated policy
and distribution output. The DB test took **857.21 seconds**; its steady
maintenance portion took **502.526 seconds**. Exact Rust and script source:
commit `489858942fbe88b4b7bf0673b005e85761efd7d3` (subsequent report-only edits do
not change the measured experiment).

| Measured operation | Samples | p50 µs | p95 µs | p99 µs | Maximum µs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ingest, no retention index | 8,192 | 10,233 | 60,318 | 158,517 | 1,085,716 |
| Ingest, with retention index | 8,192 | 10,921 | 51,063 | 170,164 | 3,905,674 |
| Policy rebuild, 64 rows/transaction | 257 | 10,406 | 85,314 | 114,085 | 121,410 |
| Nomination reconstruction (empty source) | 1 | 4,121 | 4,121 | 4,121 | 4,121 |
| Due grace promotion, 64 rows/transaction | 129 | 28,910 | 142,913 | 255,820 | 354,171 |
| Actual runtime turn, 64 ops / 1 MiB / 10 ms | 28,179 | 12,725 | 35,179 | 92,756 | 1,358,894 |

The rebuild visited **16,384 rows** (8,192 old reverse entries plus 8,192 event
headers); promotion visited exactly **8,192 rows**. The single empty nomination
transaction is not evidence of large-backlog reconstruction performance.
The indexed ingestion median is about 6.7% higher in this sequential pair, but
p95 is lower and tail variability is large; this does not establish a universal
write-overhead multiplier. Full SelfCI ran concurrently for part of this shared
host experiment, so its load is included, not filtered out.

The runtime reached **25,112,586 logical retained bytes**, below its
**25,121,932-byte** low water, and **21,872,458 unique stored bytes**. It removed
**30,713,930 logical bytes and 30,713,930 unique bytes**, with **zero remaining
GC nominations** and **329 recovery waits**. Equal logical/unique removal is a
result of which values this particular fixture evicted, not a general property
of shared hashes. The 32-event terminal sample returned `Unneeded` on all
96 preparation calls, suppressing **9,685,860 declared bytes** in those
constructed retries; usage stayed unchanged and no guarded buffers remained.

The no-index file length after ingestion was **151,924,736 bytes**; the indexed
file was **129,511,424 bytes**, unchanged after pruning and GC. An earlier run's
indexed file was **193,998,848 bytes** with the same logical/unique payload totals.
This variability and the unchanged post-GC file are reasons **not** to call file
length differences index overhead or reclaimed physical bytes. The paired
transaction timings and bounded rebuild/promotion writes are the write-cost
evidence; hardware write amplification remains unmeasured.

Completion is meaningful but slow: this host needed roughly eight minutes of
already-initialized maintenance for about 29 MiB of logical eviction. The result
is not an operational latency recommendation, and it does not mask that cost by
increasing worker budgets. Deployment-specific evaluation is still necessary.

### Preserved pre-fix failures and diagnosis

The first exploratory run incorrectly treated the first runtime `Wait` as target
completion. It failed after 56,182 turns, about 1,196 seconds total, with only one
262,177-byte victim. Turn p50 was 11,523 µs, p99 85,942 µs and maximum
6,574,294 µs, not a hard 10 ms ceiling. This run included the rebuilt, unpromoted
grace frontier. It did not instrument per-phase latency, so none is claimed.
Rebuilt entries start unpromoted even with zero grace; configured runtime
maintenance interleaves one-row promotions with seven phases. Nomination
reconstruction scans quota decisions, initially empty here; attributing this
cost to 8,192 nominations would be incorrect.

Correcting the wait handling and pre-promoting **all 8,192 rows** did not resolve
the plateau. The second completed pre-fix run exhausted the cooperative
900-second steady measurement limit after **57,136 turns**. Logical usage stayed
at **55,564,339 bytes**, unique store at **52,586,388 bytes**, versus the
**25,121,932-byte** low-water target. Every thousand-turn observation after the
first showed the same usage. Steady turn p50/p95/p99/max were
11,966 / 27,730 / 93,186 / 5,066,631 µs. Thus grace initialization was not the
cause of the stable-input plateau. The test failed before its terminal-retry
sample and final GC assertions; those checks must not be claimed for this run.

A bounded 128-event disk diagnosis reproduced the plateau within 15 seconds.
Its cursor had all readiness flags true, `pressure_waiting=true`, `gc_done=true`
and `cycle_progress=true`, with **zero recovery waits** after 1,084 turns.
The scheduler carried a turn-wide progress bit back into later completed cycles;
cooperative time splits could keep that bit alive without new work. Returning
`Wait` is necessary to reset pressure/GC waiting flags. Fixed-count in-memory
turns can eventually escape, so an earlier loose 100-call target-only test did
not reproduce the failure.

The fix resets phase-local progress before each phase and accumulates only fresh
work for the current seven-phase cycle. The ten-post regression now requires
low-water attainment, an actual `Wait`, and GC completion within ten 64-operation
turns: old source fails, corrected source passes. The same 128-event disk
diagnosis reached its target in 213 turns / 3,230 ms, with two recovery waits
and an empty GC queue after that correction. No time/byte/operation allowances
were increased. Its later initial 32-victim sample assertion was unsuitable
because only two events were evicted; the diagnostic now samples two while the
8,192-event workload still samples 32.

The final fixture separates bounded startup reconstruction/promotion from
subsequent pruning and does not claim the configured worker uses 64-row startup
transactions. The original end-to-end startup cost remains visible above, but
includes the pre-fix scheduler and is **not** a post-fix startup benchmark.
An intermediate run was cancelled during ingestion to add explicit phase
measurement; it supplies no completed result.

### What the workload cannot establish

The corpus is larger than a tiny correctness fixture and exceeds the maximum
4,096-event DryRun snapshot, but is **not a million-event or full-disk capacity
test**. It exercises populated ordered indexes with tens of MiB, not years of a
large production account's graph. A sequential two-arm comparison is vulnerable
to ordering/cache/load bias and is evidence of cost on this host, not a causal
universal index-overhead percentage. No sustained throughput, allocator/RSS,
flash write amplification, hardware bytes-written or offline compaction is
measured. File length is allocated logical file extent, not secure erasure or
physical-device reclaimed blocks.

Count, byte and time allowances do not make an indivisible value or transaction
interruptible. Oversized victims/GC values can remain blocked forever. Repeated
mutation can invalidate author sweeps; progress requires sufficiently stable
inputs. No experiment here upgrades those conditional contracts to an
unconditional progress or latency guarantee.

## Operator presentation and safety

Authenticated, non-read-only Settings → Event Explorer links to
`/settings/retention`. The page always selects the session's storing account;
it does not accept a database path, target account or configuration edit.
It is ordinary server-rendered HTML with no JavaScript requirement and uses the
existing sensitive no-store response wrapper. GET observes; it never runs a
DryRun turn, rebuild, prune, reservation or GC.

Current mode and separately sampled accounting/guarded usage appear even when
no forecast exists. The last worker report shows its own as-of timestamp,
completeness, configured snapshot limits, visited rows, forecast high waters,
protected pressure and unmet targets. Complete victims include author, logical
length, effective age, quota reason and distance bonus expressed as exact capped
logarithmic age credit rounded down to seconds. Every victim was unprotected at
that snapshot; aggregate protected bytes are not described as future victims.

Unique/physical savings and GC backlog are explicitly **not projected**. Actual
logical retained/reserved bytes, unique store bytes, guarded buffer capacity and
demand intent are different quantities. Disabled/DryRun guarded zeros do not
mean zero buffers or RAM. Data samples can become stale immediately and are not
combined into a fake atomic snapshot. Unready accounting or a whole-source limit
can leave DryRun incomplete forever; no prefix or accumulated projection appears.

## Correctness audit and remaining evidence limits

Read this with the [combined eight-obligation startup audit](payload-retention-startup.md#combined-eight-obligation-semantic-audit).
This phase does not replace the source audits with synthetic scores.

| Obligation from proposal §11 | Existing source / evidence |
| --- | --- |
| Monotonic score, cap/zero distance, static key, deterministic ties | `rostra-core/src/retention/tests.rs`; diagnostic credit is checked against exact key differences |
| Restart/policy generation, stale candidates, bounded rebuild/grace | `retention_index_tests.rs`, `retention_tests.rs` in client-db |
| Transport rotation independence; same-account correlation | Full holder RostraId-only distance type; core/client holder-ordering tests; identical simulated device sets |
| Author/global hysteresis, zero/oversized/protected and overload | `payload_runtime_tests.rs`, `payload_admission_tests.rs`, `payload_demand_tests.rs` |
| Shared hashes, Missing/protected pins, reference/GC races | `payload_accounting_tests.rs`, runtime GC/requeue/race cases |
| Duplicate prune/delete/late delivery, total replay | `quota_pruning_tests.rs`, `retention_tests.rs`, ranked rejection replay runtime cases |
| Social projection/reaction/reply/replacement reversals | quota pruning and social post projection/materialization test suites |
| Durable no-refetch and acquisition owner cleanup | runtime, admission, demand and startup tests; client payload acquisition tests |
| DryRun no-write/config races, limits, replacement rather than accumulation | `payload_dry_run_tests.rs` compares every durable table and guarded ledger; complete/incomplete HTML and authenticated route smoke tests added here |
| Deliberate restore/transient read cannot override deletion | Restoration/transient viewing is not implemented or activated; existing lifecycle prevents terminal resurrection, not a new restoration feature |

Still unmeasured: real social graph or payload-size distributions, network
availability/latency, independent adversarial seeds, sustained admission under
bursts, dynamic downloads-per-eviction, million-event index scaling, prolonged
concurrent mutation, crash-time performance and physical write amplification.
These are evidence limits, not claimed coverage. Production rollout, quota
selection and restoration remain separate decisions; this evaluation supplies
no recommendation to enable destructive mode.
