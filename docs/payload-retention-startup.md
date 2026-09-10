# Experimental payload retention startup

Retention is **Disabled by default**. An operator can explicitly select Disabled,
DryRun or Enforce for each listed storing account. There are no universal GiB
defaults, automatic machine-size tuning, hot reload, or disk-free-space guarantees.
The account's stable `RostraId`, never its rotating transport key, determines rank.
The runtime trusts the system clock, including startup. Forward jumps can expire
grace; unknown and future origins remain protected.

## Startup interface

`rostra web-ui --payload-retention-config PATH` reads one JSON array before
constructing the account manager or exposing HTTP. Omission is equivalent to `[]`.
Each entry contains `id` and `retention`. Duplicate accounts, unknown fields,
missing inputs and invalid allowances fail startup. Unlisted identities remain
Disabled; unverified paths neither add registry entries nor open/create databases.
This interface does not configure other CLI commands or deploy anything.

A non-enabling example is:

```json
[
  {
    "id": "<storing RostraId>",
    "retention": { "mode": "disabled" }
  }
]
```

Replace the placeholder with an actual account ID to obtain valid input. Enabled
entries require all of the following explicit fields:

| Field | Meaning |
| --- | --- |
| `retention.mode` | `"enforce"` or `"dry-run"` |
| `retention.policy` | Object with `size_floor`, `tau_seconds`, `alpha_q16`, `beta_q16`, `max_bonus`, `grace_seconds`; version-one integer experimental score parameters |
| `retention.admission` | Object with `database_bytes`, `author_bytes`, `overrides` (full author ID → nonzero byte ceiling), `in_flight_count`, `in_flight_bytes` |
| Enforce: `retention.worker` | Object with `operations`, `bytes`, `gc_bytes`, `time_ms`; all positive, operations at most 4096 |
| DryRun: `retention.snapshot` | Object with `events`, `authors`, `logical_bytes`, `time_ms`; all positive, events at most 4096 and authors at most events |

Policy size floor, time scale and maximum bonus must be positive u32 values.
Other policy values are u32; exponents are unsigned Q16. Admission byte limits
are nonzero u64; in-flight count is 1–4096. Worker `bytes` bounds logical eviction
per turn; `gc_bytes` bounds unique content-store value removal, **not physical
reclamation**. Count/time allowances are cooperative: one database transaction or
sort is indivisible. Whole-event victims or values larger than a turn allowance
can remain blocked indefinitely. No implicit rounding-up bypass exists.

Library users construct `PayloadAccount::configured(id, PayloadRetentionConfig)`
and pass the validated handles to `MultiClient::new_with_payload_accounts`.
Direct Client builders consume a database with that account already attached.
Direct database users own and join `run_payload_retention()` themselves; the DB
does not spawn tasks. An enabled ledger without a runner still refuses capacity,
but cannot promise maintenance progress.

Change mode, policy or budgets only after orderly shutdown and quiescence of old
workers, acquisitions and guards, then construct fresh accounts/clients. There is
no live setter. Index maintenance does not change the immutable admission policy.
The runner initializes its disposable index generation under the same writer and
expected configuration identity before its first turn.

## Ownership and ingress audit

The manager installs a finite operator-provided account registry before HTTP.
Raw signed publication looks up its account and, in Enforce, reserves **five 2-MiB provisional
slots before JSON parsing**, including when unloaded. An Enforce stalled body
has a 30-second deadline. Author, signature and content checks precede lazy load.
The loaded DB attaches that exact ledger; concurrent loads share serialization.
Partial reservation failure and malformed bodies release every provisional owner.
Four slots cannot parse even a tiny raw request: refusal is explicit, not bypassed.
Unlisted/Disabled/DryRun bodies still have the pinned per-request body limit, not
an aggregate configured capacity bound.

Client retains the configured worker before starting request/signing tasks.
The optional replication-task switch does not disable configured retention.
The manager retains task completion *before construction*; failed or panicking
construction aborts that same task group. Reconstruction and eventual close wait
for actual joins, not weak-client disappearance or abort signaling. This includes
DB-less network tasks and cancellation of a join waiter. Retired live ownership is
reused rather than reopening an already-open DB.

| Ingress | Accounted ownership and materialization |
| --- | --- |
| Missing retries, ancestor sync, direct follower fetch | Shared reuse/terminal check, bounded expiring demand, logical reservation before peer read, separate read and Vec→Arc conversion guards; winner retained through ingestion |
| Pushed `FEED_EVENT` | Same preparation and separate read/conversion capacity before success/read; existing AlreadyHave or temporary refusal protocol |
| Local publication/head merge | Bounded CBOR writer charges before output growth, separate conversion allocation, surviving provisional bytes bind to logical reservation; empty merges need no payload allocation |
| Outgoing omni publication | Same bounded serializer; capacity retained through outbound attempts; does not materialize outgoing bytes in this database |
| Raw signed HTTP | Enforce's five preparse owners above; verified body owner survives load and binds through owned ingestion. Disabled/DryRun do not charge or apply the Enforce body deadline |
| Shared hash reuse | Logical charge per event, allocation before stored-value copy plus separate conversion capacity; shared bytes do not bypass admission |
| Public direct DB APIs | All fallible/panic wrappers converge on the logical gate. Already-allocated external input is caller-owned, not retroactively a pre-read capacity guarantee; explicit guarded API is required for controlled acquisition |
| Low-level P2P API | Mandatory caller guard, not a standalone global memory policy; dummy guards are not supported Client acquisition paths |

Four racing peers may need **eight slots**, although there is one logical
reservation. Tight budgets defer/reject read plus conversion or provisional
requirements; they never borrow uncharged room. Logical retained/reserved bytes,
pending-demand intent and live buffer capacity are distinct counters.

This is declared acquisition capacity, **not whole-process RAM**. Typed HTTP form/
JSON domain objects before local serialization, application-held preallocated
inputs, allocator/codec/transport overhead, outgoing serving reads, headers,
notification/caller clones after acquisition, DB file pages and unrelated accounts
are outside it. These exclusions do not bypass persistent logical admission.
Protected local/state/unknown content counts against ceilings; overload can stop
local publication as well as replication rather than silently unprotect content.

## Combined eight-obligation semantic audit

These boundaries must be evaluated together, not as a sum of checkpoint reviews:

1. **Unloaded raw HTTP:** immutable registry lookup, no attacker-driven account
   growth or DB creation, Enforce five-slot ownership through concurrent load. Disposable
   web fixtures stall real HTTP bodies, race loads and refusal, and exercise partial
   reservation cleanup under tight capacity.
2. **Configuration:** validated explicit startup modes/policy/budgets, Disabled
   default, no hot replacement; DryRun forecast and actual ledger are separate.
   CLI and DB startup fixtures reject missing/invalid inputs and duplicate accounts.
3. **Admission and refetch:** verified-header preparation tries shared reuse first.
   Missing rejection requires a current, eligible actual index HEAD outranking the
   incoming event and retained-only pressure with no reservations. Protected-only,
   future, skipped-prefix or exhausted scans remain Deferred. Durable decisions
   survive duplicates, shared hashes and replay; terminal fetches return Unneeded.
4. **Writer authority:** current immutable config identity, full generation,
   fixed fresh writer time/readiness, current candidate and reservation/demand
   ownership compose with the checked reducer inside the same writer transaction.
   Cancellation linearizes through writer→arbitration→state ordering. Author-first
   pressure and persistent 90% low-water latches use separate logical/GC allowances.
5. **Scheduling/readiness:** independent accounting, nomination recovery, policy
   backfill and fixed-time due-prefix draining precede selection. Registration
   before checks plus bounded recovery notices startup, committed usage, admission
   and grace. Protected/oversized/unattainable targets sleep; skipped heads do not
   grant rank proof. General author sweeps can restart under changing inputs:
   eventual progress is conditional on sufficiently stable inputs, not guaranteed
   under continuous adversarial churn.
6. **DryRun:** whole-source read-only snapshot, no maintenance, prune, rejection or
   GC. Above event/author/logical-byte/time bounds or without ready accounting,
   publish no projection. A large DB can remain incomplete forever. Reports
   replace totals and distinguish observed logical/unique usage and guarded
   counters from projected logical victims. No future demand, reservations,
   concurrent Enforce behavior or physical reclaim is simulated.
7. **Clock/protection:** trust system time including startup; preserve immutable
   origins, unknown-legacy and future protection, grace, local author and
   nonSocial/unknown-kind protection. No acknowledgement/monitor subsystem.
8. **Enabled integration:** public startup fixtures exercise absent-index
   initialization, actual runner pruning and terminal replay, external preallocated
   logical refusal, and DryRun's real Disabled ingestion. Client fixtures exercise
   configured task ownership with optional replication disabled, cancellation,
   actual DB-less joins and constructor panic. These supplement the combined
   demand/pressure/GC race, backfill, stale-policy, overload and refetch suites,
   not replace their semantic review.

Runtime errors fail the worker and log an error rather than mutating under stale
authority. Admission remains enforcing; this can leave progress paused pending
operator diagnosis. Diagnostics are available through `get_payload_usage()`,
`payload_admission_usage()` and `payload_retention_forecast()` (last as-of snapshot,
possibly incomplete/stale). No cumulative or physical-reclaim counters are implied.

## Deliberately separate follow-up

Individual post/thread surfaces now distinguish fetchable absent/Missing content
from durable quota pruning, other local pruning, signed deletion, and invalid
content. Payload reads deduplicate existing candidates and rank plausible holders
by full event/full holder distance after preferences supported by existing
reachability facts. They add no discovery, inventory RPC, restore path, or
transport-key coordinate. Broad production-shaped operational experiments,
retention diversity, fetch churn, availability, operator presentation and policy
tuning remain phase5. The tiny disposable correctness fixtures are not operational
recommendations. No live data, service restart, configuration activation,
deployment or publication is part of this work.

The Fetch control remains an ordinary POST form: no-JavaScript requests return
to the complete post page, while Alpine enhancement receives a server-rendered
content fragment. GET and HEAD on that resource only canonicalize or redirect;
they never start payload acquisition.
