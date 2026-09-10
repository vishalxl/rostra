# Experimental payload retention policy

**Current integration:** the pure policy below now feeds the explicitly configured,
joined Client retention runtime. See the [startup guide](payload-retention-startup.md).
Disabled remains the default, but immutable startup opt-in is available. Broad operational tuning and
holder-distance fetch ordering are separate work.

`rostra_core::retention` implements the pure ranking portion of the
[pruning proposal](payload-pruning.md), plus an in-memory logical-byte simulator.
It does not enable pruning, change database state, or change fetching. The
database's lifecycle, projection, admission and garbage-collection integration is
separate from this pure library. The database records immutable
retention origins and preserves quota-decision source metadata across replay;
see the [database checkpoint](payload-retention-database.md). The database now
also exposes explicit checked quota transitions, projection dematerialization,
and quota-only nominations/recovery. Explicit startup Enforce enables the worker;
accounting readiness does not establish policy-index readiness. Disposable database
indexes now store full policy/holder generations, bound rebuild and grace work,
and expose advisory author/global selection. Pressure, runtime clock trust and
transactional worker integration supply authority, not the pure score.
The database also has a default-Disabled shared admission boundary with
explicit logical ceilings and separate acquisition-buffer leases. Client reads,
shared-store reuse, local serialization and raw signed HTTP parsing now use those
leases when Enforce is configured.
The retained Client driver connects bounded demand preemption,
author-first/global hysteresis, quota-only collection and maintenance to buffer-free
acquisition waiting.
Its demand worker also supports conservative ranked durable Missing rejection:
a fresh currently eligible retained index head must outrank the incoming event,
and retained-only pressure must independently exceed the applicable cap. Skipped
or protected-only boundaries still defer; this does not simulate future fit.
The same driver has an observation-only DryRun branch. Its independent
forecast config leaves acquisition Disabled; bounded whole-source snapshots
project retained-only author-first/global pressure without writes or GC.
Incomplete snapshots publish no projection, and repeated reports are as-of
replacements, not newly evicted bytes. See the database guide for bounds and
the deliberately unsupported demand/future-workload model.

## Version 1 arithmetic and encoding

The public Rust API documentation defines the byte-level contract. In particular:

- Coordinates use unkeyed BLAKE3 over the literal ASCII domain
  `rostra/retention/event/v1` or `rostra/retention/holder/v1`, immediately followed
  by the identifier's full 32 raw bytes. XOR bytes compare big-endian. A holder
  is a **storing account `RostraId`**, never an author substituted for the holder
  or an iroh endpoint. Fetch ordering can use the complete 256-bit distance.
- Scoring alone rounds distance down to its first 64 bits. A zero fraction is
  treated as one Q0.64 quantum before the bounded log bonus is applied; both
  values are well inside the allowed bonus caps. This quantization does not
  affect the complete metric used for holder ordering.
- `RetentionPolicy` validates positive u32 size floor, age scale and maximum
  bonus. Q16 exponents accept the full u32 range. Its 28-byte encoding is seven
  big-endian u32 values: version 1,
  size floor, tau seconds, alpha Q16, beta Q16, maximum bonus, grace seconds.
  There is no floating-point computation in this policy.
  All parameters permit their full u32 representation (except invalid zeros),
  rather than imposing operational limits. Grace permits any u32 seconds;
  deployment limits belong to later configuration/admission work.
- Logs are deterministic approximations, not platform `ln` calls. Normalize
  each positive integer to Q63, recover 32 fractional log2 bits by repeated
  squaring/truncation, then multiply by `2977044471` (floor of ln(2) in Q32)
  and truncate. Size penalty subtracts the two integer logarithms; distance
  bonus subtracts the fraction's logarithm from 64 times that constant and
  clamps to ln(maximum bonus).
- Each weighted nonnegative log term truncates after multiplying by tau and
  the Q16 exponent. The final key is signed Q32 seconds with full event ID as
  tie-breaker. All intermediates fit i128 across accepted inputs: log terms are
  below 2^38, pre-division weights below 2^102, weighted terms below 2^86 and
  timestamp terms below 2^96. The 48-byte
  ascending key encoding is a sign-flipped, big-endian i128 followed by the
  raw event ID. Near-equal mathematical scores can tie or change order due to
  approximation; the integer contract, not the ideal real-valued formula, is
  authoritative for version 1.

Golden vectors pin distance and negative-key encoding in unit tests. A sampled
floating-point reference comparison at the proposal's experimental settings
observes at most approximately **6.35 milliseconds** of virtual-time error.
This is a regression sample, **not a proven global approximation-error bound**.
Large permitted weights can amplify error; the experimental parameters are not
empirically selected deployment defaults.

## Persistence and lifecycle boundary for subsequent phases

- Persist `min(author_timestamp, first_accepted_header_receipt)` once. Payload
  redownload and duplicate delivery must not refresh the age origin.
- Persist the first payload materialization origin separately, also only once.
  Grace is an eligibility check, never a score change. Missing origins or a
  clock before that origin fail shut. Forward clock jumps cannot be detected
  by the pure helper; the integration must supply a clock-reliability and
  migration policy before pruning. Elapsed-time comparison avoids deadline
  addition overflow: a deadline beyond `Timestamp::MAX` never expires.
- Store the full policy encoding **and holder RostraId** with index generation
  metadata. Changes to either require a bounded rebuild. Do not mix keys from
  different generations even though they share the same Rust type.
- Recover full event IDs from retained verified headers, not abbreviated
  database keys. Key calculation accepts metadata but does not verify it.
- Independently classify safe content kinds, local-authored protection, pins,
  content state, projection readiness and zero-length payloads. A key or
  elapsed grace is not permission to discard content. See
  [SPEC-event-content-lifecycle](../crates/rostra-client-db/specs/SPEC-event-content-lifecycle.md).
- Immutable startup opt-in and Client worker integration live outside this module.
  Author-specific ceilings and reservation primitives live in the default-Disabled
  database foundation, not this pure policy module. The executable pressure path
   includes the configured database driver alongside the pure simulator, both
   with explicit budgets. No production GiB quota is supplied.

## Reproducible simulation

Run:

```sh
cargo test -p rostra-core retention -- --nocapture
```

The deterministic fixture has 1024 distinct 1-KiB events across four authors,
with hourly age increments and expired materialization grace. Each author cap
is explicitly 300000 bytes, with a 270000-byte low-water mark; the database
cap is 512000 bytes with a 460800-byte target. These tiny fixture budgets
exercise arithmetic, **not deployment sizing**.

At experimental settings, two different holder accounts each evict 574 events
and retain 460800 bytes. They share 229 evicted events (and thus 105 retained
events). Replicas of the same account produce identical results, including with
reversed input order; transport keys cannot enter the API. This demonstrates
account-level specialization, not realistic availability.

Other tests cover exact high-water boundaries, strict author ceilings, whole
payload undershoot, protected-byte overload, zero-byte exclusion, missing/future
grace origins, unknown policy versions, duplicate input rejection, static-score
reference equivalence, monotonicity, extreme arithmetic, and the bonus cap's
final tail cutoff. With equal 1-KiB sizes, even the closest possible old event
loses to the farthest new event after 125 days at experimental settings. Without
pressure, arbitrarily old content remains retained.

The simulator charges logical bytes per event, not deduplicated physical space.
It assumes all supplied candidates were already received and retained; it does
not model a live follow graph, correlated outages, missing/in-flight data,
incremental arrivals, realistic size distributions, pinned state semantics, or
the probability of preserving the last copy. Those remain necessary experiments
before selecting production policy.
