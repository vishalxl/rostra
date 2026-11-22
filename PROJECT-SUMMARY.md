# Rostra — Project Summary

A consolidated overview of the Rostra project, synthesized from the repository's
documentation. This file is a navigational summary, **not** a specification: the
linked specs, `SECURITY.md` files, and source remain authoritative. Where this
summary and a spec disagree, the spec wins.

---

## 1. What Rostra is

Rostra is a **peer-to-peer, mostly friend-to-friend (F2F) social network written
in Rust**.

- Users "follow" (subscribe to) other identities, much like conventional social
  networks, and followers track, download, and help re-share the data of the
  people they follow.
- There is **no global feed**. New identities and content are discovered only
  through existing connections, which is what makes the network mostly-F2F.
- It is **self-hostable** and can run on any device, from a laptop to a server.
  A public instance is available at <https://rostra.me/>.
- It is **extensible**: the social-graph data model can support applications
  other than a Twitter-like feed.
- Its inherent spam resistance makes it a good fit for **AI agents** interacting
  with each other or with humans; ready-made agent skills live in
  [`agents/skills/`](agents/skills).

The project is licensed **MPL-2.0 OR Apache-2.0 OR MIT** (your choice), and the
author discloses LLM usage in development — see [`README.md`](README.md).

---

## 2. Design rationale

The project's presentation ([`docs/rostra-presenterm/rostra.md`](docs/rostra-presenterm/rostra.md),
also [recorded on YouTube](https://www.youtube.com/watch?v=KjUVKUOg4aE)) frames the
problem with mainstream social networks as threefold:

| Problem | Consequence |
| --- | --- |
| A global public square | Not how people actually interact; irrelevance or shouting matches with the whole Internet; little respect for real connections. |
| The attention economy | Rewards influencers and self-promoters; bots, spam, low-quality content; wrong discoverability defaults; controversy spreads fastest. |
| Moderation at global scale | Centralized, adversarial, and effectively impossible to get right for everyone. |

Rostra's answer is to make the **social graph the transport and the filter**:
relevance comes from whom you follow, not from an algorithm optimizing attention,
and moderation becomes a matter of choosing your peers. See
[`docs/design.md`](docs/design.md) for the full set of design decisions and
[`docs/comparison.md`](docs/comparison.md) for how Rostra differs from other
social protocols.

---

## 3. Core concepts

### 3.1 Signed event DAG

All data propagates as `Event`s: small, fixed-size packets signed by their author.
The cryptographic hash of an event is its `EventId`, which uniquely identifies it.

- An event carries the IDs of **up to two previous events** (parents), plus a
  `ContentHash` pointing at the actual payload.
- The history of a user's events therefore forms a **DAG**, traversable and
  replicable from newer to older events.
- Because payloads are referenced by hash rather than embedded, bulk data can be
  fetched selectively, incrementally, or partially.

Authoritative contract:
[`crates/rostra-core/specs/SPEC-event-graph.md`](crates/rostra-core/specs/SPEC-event-graph.md).

### 3.2 Sovereign identities and personas

- An identity is a keypair; there is no central account authority.
- **Personas (sub-identities)** let one identity publish and follow as a subset of
  itself. You might follow someone's technical work without their political
  opinions, and authors can stay wholesome without diluting their most popular
  aspects.
- **Multi-device** works by DAG merging: disjoint parts of a history are stitched
  together by any newer event that points at both heads.

### 3.3 Deletion is best-effort

Deletion is a signed metadata event, not secure erasure. Copies already
replicated elsewhere may survive. The specs are explicit about this so that no
interface promises more than the protocol can deliver.

### 3.4 Singleton vs non-singleton events

Some event kinds are singletons (for example profile snapshots) where only one
current value is meaningful; others (posts, reactions, follows) accumulate.
Reducers resolve conflicts deterministically by timestamp and then event ID, so
every replica converges on the same projection.

### 3.5 Full vs light nodes

A node may be **full** (downloads and stores data) or **light** (no persistence),
with a variety of storage policies in between. One identity can run several nodes
for privacy, availability, and multi-device reasons.

---

## 4. Architecture

[`ARCHITECTURE.md`](ARCHITECTURE.md) gives the short version;
[`specs/ARCH-rostra.md`](specs/ARCH-rostra.md) is the authoritative system
architecture, including crate dependency direction.

### 4.1 Network layer

- **[Pkarr](https://github.com/pubky/pkarr)** — sovereign distributed
  naming/identity. Provides an identity's connectivity information and one
  representative graph head as a discovery entry point.
- **[iroh-net](https://github.com/n0-computer/iroh)** — p2p transport and
  built-in discovery. Any Rostra node can connect to any other node.

The Pkarr representative is only a discovery entry point: the signed event graph
may legitimately have **multiple current heads** (see
[`SPEC-event-graph`](crates/rostra-core/specs/SPEC-event-graph.md)).

### 4.2 Privacy default

Clients default to **relay-only mode**, which keeps your IP from being directly
exposed to peers. Passing `--public` enables direct p2p connections. For stronger
anonymity, Rostra can run over Tor via `oniux`:

```console
nix run github:/dpc/rostra#rostra-web-ui-tor
```

### 4.3 Data flow

```text
author signs Event (parents + ContentHash)
        │
        ▼
   Pkarr discovery  ──►  iroh p2p transport
        │
        ▼
 per-identity client DB  ──►  projections  ──►  web UI / API
```

Events and content synchronize in real time according to the social graph.

---

## 5. Crate map

The workspace is multi-crate (Rust 2024 edition). Dependency direction flows from
utilities and domain types upward to clients and the UI.

| Crate | Responsibility |
| --- | --- |
| [`rostra-core`](crates/rostra-core/README.md) | Core domain types shared across the project: events, identities, event graph, retention ranking policy. |
| [`rostra-client`](crates/rostra-client/README.md) | Client orchestration: modes, replication tasks, DM worker, shared RPC utilities. |
| [`rostra-client-db`](crates/rostra-client-db/README.md) | Per-identity database: event/content storage, projections, lifecycle, migrations. |
| [`rostra-p2p`](crates/rostra-p2p/README.md) | P2P networking layer. |
| [`rostra-p2p-api`](crates/rostra-p2p-api/README.md) | P2P API definitions shared between peers. |
| [`rostra-web-ui`](crates/rostra-web-ui/design-ui.md) | Default web UI: Axum + Maud + Alpine/alpine-ajax. |
| [`rostra-dm`](crates/rostra-dm/README.md) | Native age X25519 direct-message envelopes, device announcements, epochs. |
| [`rostra-djot`](crates/rostra-djot/README.md) | Djot rendering, mention/link safety, semantic document output. |
| [`rostra-jpeg-validate`](crates/rostra-jpeg-validate/README.md) | Hardened restricted JFIF-baseline JPEG validator (vendored from `jpeg-decoder`). |
| [`rostra-util`](crates/rostra-util/README.md) | Small shared utility helpers. |
| [`rostra-util-error`](crates/rostra-util-error/README.md) | Error type aliases and formatting helpers. |
| [`rostra-util-fmt`](crates/rostra-util-fmt/README.md) | Compact option/result, byte-count, and relative-duration renderers. |
| [`rostra-util-dedup-chan`](crates/rostra-util-dedup-chan/README.md) | Multi-receiver work channel that suppresses duplicate queued items. |
| `rostra-util-bind-addr` | Address-binding helpers. |
| `crates/rostra` | The `rostra` binary entry point (web UI command). |
| [`bots/`](crates/bots) | Bot implementations built on the client and API. |
| `axum-dpc-static-assets`, `tower-sessions-redb-store` | Vendored/support integrations for the web UI. |

Applications that integrate with the network normally depend on
[`rostra-client`](https://docs.rs/rostra-client) rather than on the utility
crates directly.

---

## 6. Client runtime

Authoritative doc:
[`crates/rostra-client/specs/ARCH-client-runtime.md`](crates/rostra-client/specs/ARCH-client-runtime.md).
Security boundary:
[`crates/rostra-client/SECURITY.md`](crates/rostra-client/SECURITY.md).

The client:

- Runs in several **modes** (for example full vs light behavior) and owns the
  **replication tasks** that fetch event graphs and payloads along the social
  graph.
- Runs a **DM worker** that handles device announcements, key epochs, envelope
  decryption trials, and message lifecycle.
- Exposes shared **RPC utilities** (`rostra_client::util::rpc`) used by the P2P
  layer, keeping verification, admission, and database mutation ordering
  synchronized with the security model.
- Manages **heads** explicitly. The managed HTTP path can consume a supplied head
  exactly once (`social_post_if_head`), committing head consumption before
  activation, because activation itself can create heads. Callers must not
  separate the membership check from ingestion.
- Provides **idempotent request receipts** for locally authored operations
  (posts, edits, deletions, reactions, news, shoutbox, attachments), so retries
  return the original signed event rather than creating duplicates.

---

## 7. Storage and content lifecycle

Authoritative docs:
[`crates/rostra-client-db/specs/ARCH-client-database.md`](crates/rostra-client-db/specs/ARCH-client-database.md)
and
[`crates/rostra-client-db/specs/SPEC-event-content-lifecycle.md`](crates/rostra-client-db/specs/SPEC-event-content-lifecycle.md).
Security boundary:
[`crates/rostra-client-db/SECURITY.md`](crates/rostra-client-db/SECURITY.md).

### 7.1 One database per identity

Each identity has its own database. It stores signed envelopes, payload content,
derived projections, receipts, and extension tables. Schema and migration history
are documented in the architecture spec; the schema is currently well into the
high twenties/low thirties, with each migration carrying a compatibility
guarantee.

### 7.2 Payload state machine

Event content moves through explicit states (for example **Missing**,
**Processed**, locally pruned, or terminally deleted/invalid). Key properties:

- Content is deduplicated by hash; the same payload is stored once.
- Envelope delivery and content delivery are independent, and content may arrive
  before its envelope.
- Quota decisions constrain envelope replay before shared content can
  materialize, while **signed deletion remains stronger** than pruning.
- Projections can be dematerialized without losing the ability to rebuild them
  from retained signed events.

### 7.3 Retention and pruning (experimental)

Docs: [`docs/payload-pruning.md`](docs/payload-pruning.md) (proposal),
[`docs/payload-retention-policy.md`](docs/payload-retention-policy.md),
[`docs/payload-retention-database.md`](docs/payload-retention-database.md),
[`docs/payload-retention-startup.md`](docs/payload-retention-startup.md),
[`docs/payload-retention-evaluation.md`](docs/payload-retention-evaluation.md).

- **Disabled by default.** Operators may select `Disabled`, `DryRun`, or
  `Enforce` per storing account.
- Configuration is immutable at startup via
  `rostra web-ui --payload-retention-config PATH`, a JSON array of
  `{ "id": ..., "retention": ... }` entries. Duplicate accounts, unknown fields,
  missing inputs, and invalid allowances fail startup; unlisted identities stay
  Disabled.
- Ranking is by the account's **stable `RostraId`**, never its rotating transport
  key, using full-ID distance ordering.
- The runtime combines author-first and global pressure, reservation-aware
  low-water hysteresis, and bounded quota-only collection.
- `DryRun` computes read-only whole-snapshot projections with acquisition
  disabled; `Enforce` enables automatic admission/eviction and the joined worker.
- Evaluation is explicitly an **offline synthetic experiment**, not evidence for
  production quotas. It did surface and fix a real scheduling/bookkeeping bug.

### 7.4 Convergence guarantees

[`crates/rostra-client-db/docs/property-testing.md`](crates/rostra-client-db/docs/property-testing.md)
describes property tests that materialize deterministic signed input, clone a
closed database template to two replicas, and vary delivery permutation,
duplicates, transaction batching, aborted transactions, split vs atomic
envelope/content delivery, intermediate reopen points, pruning, and simulated
zero-reference byte collection. A final envelope/payload/intervention retry plus
reopen forms the quiescence fence.

---

## 8. Direct messages

Authoritative docs:
[`crates/rostra-dm/README.md`](crates/rostra-dm/README.md),
[`docs/direct-messages/approved-plan.md`](docs/direct-messages/approved-plan.md),
[`docs/direct-messages/design-security-research.md`](docs/direct-messages/design-security-research.md).
Security boundary: [`crates/rostra-dm/SECURITY.md`](crates/rostra-dm/SECURITY.md).

### 8.1 Format

- A **complete native age X25519 file** per message.
- **8 recipient slots**, shuffled, mixing real and dummy recipients so the public
  ciphertext does not reveal how many real recipients exist.
- Encrypted body in **1–16 KiB buckets**.
- **7-day send window** and **28-day receive grace**.
- Per-epoch keys provide conditional protection of archived ciphertext: learning
  an account recovery/signing secret or a later epoch secret does not recover an
  older message once every secret capable of decrypting it is actually gone.
- **Device announcements** (`DM_DEVICE`) publish keys as signed events.

### 8.2 Security process

Security is treated as a formal claim rather than a slogan:
[`specs/CLAIM-direct-message-security.md`](specs/CLAIM-direct-message-security.md)
states the property, and
[`specs/CLAIM-direct-message-security/proof.md`](specs/CLAIM-direct-message-security/proof.md)
argues it against a named model. The proof is implementation-grounded but **not**
an independent verification result; its assumptions are axioms.

The integrating caller must verify event signature, author, full content hash and
length, DM kind, zero aux key, and non-singleton flag before decryption trials,
and must supply only retained live identities. The crate itself is not an
end-to-end guarantee.

### 8.3 Product scope

[`docs/direct-messages/approved-plan.md`](docs/direct-messages/approved-plan.md)
defines the approved 1:1 DM scope. Private-message HTTP routes do not expose
retained plaintext or authorize mutations using another session's authority.

---

## 9. Web UI

Design language: [`crates/rostra-web-ui/design-ui.md`](crates/rostra-web-ui/design-ui.md).
Governing specs:
[`DESIGN-server-rendered-hypermedia`](crates/rostra-web-ui/specs/DESIGN-server-rendered-hypermedia.md),
[`REQ-ui-functionality-security`](crates/rostra-web-ui/specs/REQ-ui-functionality-security.md),
[`DESIGN-action-controls`](crates/rostra-web-ui/specs/DESIGN-action-controls.md).

### 9.1 Stack

- **Axum** web server, **Maud** templates, **Tower** middleware for sessions,
  cookies, and compression.
- **Alpine.js + alpine-ajax** as *progressive enhancement only*. Every changed
  workflow must work through ordinary HTTP without JavaScript; the `/unlock`
  "Create Account" form-fill is the documented browser-local exception.
- Vendored browser libraries: Alpine (persist, intersect, morph plugins),
  alpine-ajax, emoji-picker-element, text-field-edit, MathJax, and Prism.js.

### 9.2 Routes and workflows

Handlers live in `crates/rostra-web-ui/src/routes/` and cover timeline, post,
profile, avatar, messages, settings, unlock, and recovery, among others. UI
actions are server-rendered forms and links; Alpine adds inline enhancement.

Keyboard shortcuts that call `requestSubmit()` must use `keyup`, not `keydown`,
to avoid auto-repeat duplicate submissions.

### 9.3 Identity and recovery

[`SPEC-identity-recovery`](crates/rostra-web-ui/specs/SPEC-identity-recovery.md)
and [`docs/recovery-phrase-testing.md`](docs/recovery-phrase-testing.md).

- Identities are backed by a **24-word BIP39 mnemonic**.
- Recovery export is server-rendered, session-scoped, unavailable to read-only
  sessions, and protected by sensitive-response headers.
- The phrase is shown in a masked, read-only, keyboard-focusable field with a
  conventional copy control — no reveal dialogs or confirmation steps.
- `/unlock` Create Account may expose a generated credential only in a secure
  response; it fills editable login fields without submitting or navigating.

### 9.4 Media safety

Public attachments are opaque: no filename, extension, MIME input, codec, or
normalization is accepted (`application/octet-stream`, max 1,000,000 bytes).
Display is a separate, explicit decision.

Retained display readers enforce per-format bounds:

| Format | Source geometry | Notes |
| --- | --- | --- |
| PNG | ≤ 8192 per axis, ≤ 32,000,000 pixels | Noninterlaced above the old limit; complete static raster validation. |
| WebP | ≤ 2048 per axis, ≤ 4,000,000 pixels | Existing static WebP policy. |
| JPEG | ≤ 2048 per axis, ≤ 4,000,000 pixels | Restricted JFIF baseline only, via `rostra-jpeg-validate`. |

All formats still require ≤ 1,000,000 encoded bytes and ≤ 2,000,128 signed body
bytes. JPEG support is opt-in via the `native-media-jpeg` feature; EXIF, ICC,
progressive, and broad camera coverage are explicitly not promised. Provenance
and patch hashes are recorded in
[`crates/rostra-jpeg-validate/provenance/README.md`](crates/rostra-jpeg-validate/provenance/README.md).

### 9.5 Djot rendering

[`rostra-djot`](crates/rostra-djot/README.md) renders signed Djot source with
mention and link safety, plus an optional semantic document API
([`docs/native-semantic.md`](crates/rostra-djot/docs/native-semantic.md)) that
returns owned UTF-8 nodes rather than byte spans. Mention indexing parses
remote-authored Djot only after a conservative size and recursive-block-opener
guard; over-budget posts remain valid content but may be omitted from the mention
index.

---

## 10. External Web API

Docs: [`docs/web-api.md`](docs/web-api.md) and the agent-oriented
[`agents/skills/rostra-web-api.md`](agents/skills/rostra-web-api.md).

The web UI exposes an `/api/` surface for clients that cannot run Rust code —
bots, scripts, and AI agents. It is hosted by a Rostra Web UI instance (private or
public) and intended for use with servers you trust.

- Requests must carry `X-Rostra-Api-Version: 0`.
- Endpoints cover identity creation, posting, following, and timelines.
- The `-managed` suffix means the **server holds the secret key** and signs events
  on the caller's behalf. Managed secrets are 24-word BIP39 mnemonics and must be
  kept confidential.
- Managed writes enforce one-use head consumption: a stale `parent_head_id`
  returns `409 Conflict` rather than silently forking.
- Errors are explicit; clients should treat the API as an append-only signed-event
  publisher, not as a mutable CRUD store.

---

## 11. Security model

Security documentation is deliberately distributed and detailed:

| File | Covers |
| --- | --- |
| [`SECURITY.md`](SECURITY.md) | Root boundaries: media, DMs, browser guidance, credentials, HTTP routes. |
| [`crates/rostra-client/SECURITY.md`](crates/rostra-client/SECURITY.md) | Profile serialization, clock/queue budgets, head consumption, activation ordering. |
| [`crates/rostra-client-db/SECURITY.md`](crates/rostra-client-db/SECURITY.md) | Ingestion, transactions, mention guards, replay, resource bounds, migration safety. |
| [`crates/rostra-dm/SECURITY.md`](crates/rostra-dm/SECURITY.md) | Hostile frames/keys/timestamps, contributory-key prevalidation, caller duties. |

Recurring principles:

- Treat all remote data as hostile; signatures authenticate authority, not honesty
  or correct key generation.
- Bound every parse, allocation, traversal, and render.
- Keep verification, admission, and mutation ordering atomic and in that order.
- Never widen a reader or feature implicitly; opt-in features are explicit.
- Adversarial and property tests accompany the boundary they protect.

Agent guidance files
([`AGENTS.md`](AGENTS.md),
[`crates/rostra-client/AGENTS.md`](crates/rostra-client/AGENTS.md),
[`crates/rostra-client-db/AGENTS.md`](crates/rostra-client-db/AGENTS.md),
[`crates/rostra-dm/AGENTS.md`](crates/rostra-dm/AGENTS.md))
require reading the relevant `SECURITY.md` and specs before changing governed
code.

---

## 12. Development

Guidance: [`HACKING.md`](HACKING.md), [`CONVENTIONS.md`](CONVENTIONS.md),
[`AGENTS.md`](AGENTS.md).

### 12.1 Commands

```console
# Build
cargo build            # or: just build
cargo check            # or: just check

# Test
cargo nextest run      # or: just test

# Quality
just format            # cargo fmt + nixfmt
just lint              # pre-commit lints
just clippy
just clippy-fix
just final-check       # lint + clippy + test, before a PR
```

### 12.2 Running

```console
# Production-style web UI
cargo run --bin rostra --release -- web-ui

# Development with hot reload on port 2345
just dev
just dev 3000          # custom port
```

`cargo install` cannot bundle the embedded web UI assets; clone the repository or
use Nix / prebuilt CI artifacts instead. Nix: `nix run github:dpc/rostra`.

### 12.3 Conventions

- Rust 2024 edition, workspace layout, `tracing` for structured logging.
- No inline `mod`s — use standalone modules.
- Follow the [Linked Specs](CONVENTIONS.md) convention: specs govern code, and
  changes to governed code update the spec.
- Web changes must satisfy the server-rendered hypermedia rule and the
  functionality/security requirement spec.

### 12.4 Releasing

[`docs/releasing-rostra-client.md`](docs/releasing-rostra-client.md) describes
`just check-client-release`, which packages crates in publication order and
verifies each archive's file list:

```text
rostra-util
rostra-util-error
rostra-util-fmt
rostra-util-dedup-chan
rostra-jpeg-validate
rostra-core
rostra-p2p-api
rostra-djot
rostra-p2p
rostra-dm
rostra-client-db
rostra-client
```

The check snapshots package inputs into an isolated temporary workspace so
temporary dependency resolution cannot rewrite the live `Cargo.lock`.

---

## 13. Documentation index

### Root

| File | Read it for |
| --- | --- |
| [`README.md`](README.md) | Purpose, install/run options, license, AI disclosure. |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Event DAG and network overview. |
| [`SECURITY.md`](SECURITY.md) | Root security boundaries. |
| [`AGENTS.md`](AGENTS.md) | Agent/contributor entry point and web UI rules. |
| [`CONVENTIONS.md`](CONVENTIONS.md) | Linked Specs and coding conventions. |
| [`HACKING.md`](HACKING.md) | Development workflow. |

### `docs/`

| File | Read it for |
| --- | --- |
| [`design.md`](docs/design.md) | Design decisions and rationale. |
| [`FAQ.md`](docs/FAQ.md) | Common questions and objections. |
| [`comparison.md`](docs/comparison.md) | Comparison with other social protocols. |
| [`web-api.md`](docs/web-api.md) | External `/api/` reference. |
| [`payload-pruning.md`](docs/payload-pruning.md) | Pruning proposal (not adopted). |
| [`payload-retention-policy.md`](docs/payload-retention-policy.md) | Pure ranking policy and simulator. |
| [`payload-retention-database.md`](docs/payload-retention-database.md) | Durable retention metadata and quota transitions. |
| [`payload-retention-startup.md`](docs/payload-retention-startup.md) | Operator configuration and activation. |
| [`payload-retention-evaluation.md`](docs/payload-retention-evaluation.md) | Offline synthetic experiment results. |
| [`direct-messages/approved-plan.md`](docs/direct-messages/approved-plan.md) | Approved DM product scope. |
| [`direct-messages/design-security-research.md`](docs/direct-messages/design-security-research.md) | DM security research. |
| [`recovery-phrase-testing.md`](docs/recovery-phrase-testing.md) | Browser checks for recovery export. |
| [`releasing-rostra-client.md`](docs/releasing-rostra-client.md) | Crate release closure check. |
| [`rostra-presenterm/`](docs/rostra-presenterm) | Project presentation. |

### `specs/`

| File | Read it for |
| --- | --- |
| [`ARCH-rostra.md`](specs/ARCH-rostra.md) | Authoritative system architecture and dependency direction. |
| [`CLAIM-direct-message-security.md`](specs/CLAIM-direct-message-security.md) | DM confidentiality claim. |
| [`CLAIM-direct-message-security/proof.md`](specs/CLAIM-direct-message-security/proof.md) | Implementation-grounded argument for the claim. |

### Crate specs and docs

| File | Read it for |
| --- | --- |
| [`rostra-core/specs/SPEC-event-graph.md`](crates/rostra-core/specs/SPEC-event-graph.md) | Event graph contract: identities, parents, deletion, singletons. |
| [`rostra-client/specs/ARCH-client-runtime.md`](crates/rostra-client/specs/ARCH-client-runtime.md) | Client modes, replication, DM worker. |
| [`rostra-client-db/specs/ARCH-client-database.md`](crates/rostra-client-db/specs/ARCH-client-database.md) | Per-identity storage, projections, schema history. |
| [`rostra-client-db/specs/SPEC-event-content-lifecycle.md`](crates/rostra-client-db/specs/SPEC-event-content-lifecycle.md) | Payload state machine, retention, dedup. |
| [`rostra-client-db/docs/content-lifecycle.md`](crates/rostra-client-db/docs/content-lifecycle.md) | Detailed lifecycle implementation notes. |
| [`rostra-client-db/docs/social-post-idempotency.md`](crates/rostra-client-db/docs/social-post-idempotency.md) | Idempotent text posting. |
| [`rostra-client-db/docs/social-post-mutations.md`](crates/rostra-client-db/docs/social-post-mutations.md) | Idempotent edit/delete. |
| [`rostra-client-db/docs/news-requests.md`](crates/rostra-client-db/docs/news-requests.md) | News posts and votes. |
| [`rostra-client-db/docs/news-post-mutations.md`](crates/rostra-client-db/docs/news-post-mutations.md) | News body corrections. |
| [`rostra-client-db/docs/reactions.md`](crates/rostra-client-db/docs/reactions.md) | Independent reactions and removal. |
| [`rostra-client-db/docs/shoutbox.md`](crates/rostra-client-db/docs/shoutbox.md) | Durable shoutbox intents. |
| [`rostra-client-db/docs/public-attachments.md`](crates/rostra-client-db/docs/public-attachments.md) | Opaque public attachment publishing. |
| [`rostra-client-db/docs/retained-display.md`](crates/rostra-client-db/docs/retained-display.md) | Display readers and media bounds. |
| [`rostra-client-db/docs/retained-jpeg.md`](crates/rostra-client-db/docs/retained-jpeg.md) | Opt-in JPEG reading. |
| [`rostra-client-db/docs/property-testing.md`](crates/rostra-client-db/docs/property-testing.md) | Convergence properties. |
| [`rostra-web-ui/design-ui.md`](crates/rostra-web-ui/design-ui.md) | Visual language and interaction patterns. |
| [`rostra-web-ui/specs/DESIGN-server-rendered-hypermedia.md`](crates/rostra-web-ui/specs/DESIGN-server-rendered-hypermedia.md) | HTML-first, no-JS baseline. |
| [`rostra-web-ui/specs/REQ-ui-functionality-security.md`](crates/rostra-web-ui/specs/REQ-ui-functionality-security.md) | Functionality/security tradeoffs. |
| [`rostra-web-ui/specs/DESIGN-action-controls.md`](crates/rostra-web-ui/specs/DESIGN-action-controls.md) | Action control patterns. |
| [`rostra-web-ui/specs/SPEC-identity-recovery.md`](crates/rostra-web-ui/specs/SPEC-identity-recovery.md) | Recovery export behavior. |
| [`rostra-djot/docs/native-semantic.md`](crates/rostra-djot/docs/native-semantic.md) | Semantic document wire format. |
| [`rostra-jpeg-validate/provenance/README.md`](crates/rostra-jpeg-validate/provenance/README.md) | Vendored source provenance and hashes. |

### Agent skills

| File | Read it for |
| --- | --- |
| [`agents/skills/rostra-web-api.md`](agents/skills/rostra-web-api.md) | Driving Rostra programmatically as a bot or agent. |

---

## 14. Glossary

| Term | Meaning |
| --- | --- |
| **Event** | A small signed packet; the only unit of propagation. |
| **EventId** | Cryptographic hash uniquely identifying an event. |
| **Head** | An event with no known children in the local graph; one of possibly several current tips. |
| **Persona** | A sub-identity for publishing/following a subset of an identity. |
| **Projection** | Derived, queryable state built from signed events. |
| **Payload / content** | The bulk data referenced by an event's `ContentHash`. |
| **Lifecycle** | The state machine a payload moves through (missing, processed, pruned, deleted, invalid). |
| **Retention origin** | Immutable metadata recording where/when a payload was retained. |
| **Epoch (DM)** | A key generation whose expiry bounds which messages can still be decrypted. |
| **Managed endpoint** | A Web API write where the server holds the secret and signs for the caller. |
| **F2F** | Friend-to-friend: content flows along the social graph, with no global feed. |
| **Full / light node** | Full stores data; light does not persist. |
