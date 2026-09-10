# Rostra 1:1 DMs — approved implementation plan

## Status and authority

Product/design planning is complete following the user's approval of the
full-age envelope direction and limited storage-erasure guarantee. This is a
preserved record of approved decisions, not a Linked Spec or a security
verification pass. The end-to-end implementation now includes the bounded
[native-age wire profile](../../crates/rostra-dm/README.md),
[durable lifecycle and history](../../crates/rostra-client-db/specs/ARCH-client-database.md),
[owned client worker](../../crates/rostra-client/specs/ARCH-client-runtime.md),
and session-protected HTML conversation/device workflows. The exact wire
encoding and engineering bounds are owned by those current implementation
artifacts rather than the historical sequencing below.
The [security claim](../../specs/CLAIM-direct-message-security.md) states the
qualified property and its immediate assumptions; its sibling proof describes
the implementation argument. This historical plan does not establish those
security properties.
Escalate any required change to approved behavior or security scope.

The coordinator relayed this additional direct user instruction verbatim:
“This DM-plan - I need it implemented e2e, including the UI fitting existing UI.”
Implementation therefore includes protocol, storage, client/server integration,
and a usable DM interface consistent with Rostra's existing UI. Follow the
server-rendered hypermedia convention: ordinary HTTP workflows must work without
JavaScript; Alpine is progressive enhancement. Exact navigation/composer/layout
choices should follow existing patterns and can be made by implementing
engineers without another planning round.

The user ended this planning conversation and directed all subsequent handoff,
questions, and results through `coordinator-CmQG`; do not expect the user to watch
this research conversation.

The sections marked **Approved** record literal user decisions from the planning
conversation. **Implementation obligations** translate those decisions and
research findings into work; they do not authorize expanding product scope.

Artifacts:

- Independent senior security research:
  `/tmp/public/rostra-dm-security-review-2bRAvjlN.md`
- Archived discussion and superseded proposals:
  `/tmp/public/rostra-dm-planning-history-lIQOJZ9P.md`

The senior report is starting evidence for the required claim/proof work, not
an audit or verification pass. Where exploratory notes conflict with this
consolidated plan, the approved decisions here supersede them.

## 1. Approved scope

- Text-only 1:1 direct messages in public, sender-signed Rostra events.
- Recipients discover candidate DMs through ordinary WoT event replication.
  Original ciphertext replication already exists; do not implement a separate
  DM event-replication protocol.
- Public sender identity, DM existence, timing, and size buckets are accepted.
- Conceal the selected recipient keys and real device-slot count from the
  encrypted envelope, subject to the qualified threat model below.
- Independent, expiring per-device encryption keys; no conversational ratchets.
- No MLS/groups, one-time-prekey pools, post-compromise recovery requirement,
  or post-quantum requirement.
- Recovery phrase does not restore historical DMs or regenerate epoch secrets.
- No additional history synchronization initially.
- No automatic history expiration initially; future retention work is required.
- No delivery acknowledgments or read receipts initially.

## 2. Approved security scope

### Content and authentication

Target content confidentiality for honest endpoints with uncompromised required
secrets, authenticated key announcements, sound randomness, and correct
encryption/event validation. Authenticate the sending account through its
signed event and verify the encrypted sender against that author.

All full installations have account signing authority. This is account
authentication, not proof that a particular person or installation authored a
message. Device retirement does not recover security after account-signing-key
compromise.

### Recipient and slot privacy

The approved full-age direction adopts the senior report's qualified target:
the envelope should not identify selected recipients or real-slot count to
outsiders beyond what accepted public metadata already reveals.

Do not claim global anonymity. Public key eligibility, WoT membership, timing,
and behavior can narrow or identify recipients independently of ciphertext.
Selected compromised devices already learn the plaintext recipient and their
own membership; 4+4 spillover can reveal further count predicates to them.
Do not claim active-insider private-broadcast security: a file-key holder can
rewrite an age header/body and sign a new event under its own account.

### Epoch protection and erasure

After every secret capable of recovering a message from its ciphertext is
actually unavailable to a later attacker, independent epoch keys should prevent
that attacker recovering it from archived public ciphertext alone.

This is conditional epoch protection, not per-message forward secrecy and not
a promise that all messages become unrecoverable after 35 wall-clock days.
Any surviving selected sender-device key is as relevant as a recipient key.
Keys stolen before deletion remain useful to the attacker.

The user explicitly approved a limited storage-erasure promise:

- Expired keys must stop being used and be removed from live application state.
- No promise of forensic erasure from arbitrary storage, snapshots, or backups.
- Document that protection requires no recoverable relevant secret copies.
- Logical DB deletion alone is not evidence of physical erasure.

Retained plaintext history is deliberately outside ciphertext-erasure protection.
A readable database can expose stored DMs. Powered-off devices, old DB pages,
backups, snapshots, swap/hibernation, and crash dumps need explicit treatment in
the implementation proof's assumptions and limitations, not hidden assumptions
that they magically erase keys.

## 3. Approved device identity and authority

A messaging device is one Rostra client installation with durable secret
storage, not a browser tab or login session. Multiple browsers using one hosted
client use the same messaging device.

Every messaging installation is a full account client with the account signing
key. No restricted-device authorization or additional device signing keys.

- Stable device ID across ordinary restarts and epoch rotations.
- Device-state loss/re-enrollment creates a new ID.
- Independently random encryption private keys remain installation-local.
- Account-signed announcements authorize their public keys.
- Never derive epoch private keys reproducibly from the identity phrase.
- Do not share epoch private keys among installations.

A remotely hosted instance is a trusted messaging endpoint, not a blind relay.

## 4. Approved epoch schedule and announcements

Use visible named duration constants so defaults can be adjusted:

```text
DM_KEY_SEND_WINDOW = 7 days
DM_KEY_RECEIVE_GRACE = 28 days
maximum scheduled lifetime = their sum, 35 days
```

Names are illustrative; durations and derived-sum behavior are approved.

Publish independent account-signed announcements per device, not an account-wide
device-list snapshot that concurrent installations overwrite:

```text
device_id
state: active | retired

if active:
    protocol_version
    epoch_public_key
    send_from
    send_until
    decrypt_until
```

Deadlines are absolute:

- Senders use `[send_from, send_until)`, not their compiled-in defaults.
- `decrypt_until` communicates retention, not permission to continue sending.
- Changed constants apply to newly generated keys, not existing announcements.
- Store a private key's assigned deadlines alongside it.
- No advance publication of future keys; rotate while running.
- Retain superseded private keys until their individual deletion deadlines.
- On restart, delete expired keys before processing queued messages and publish
  a fresh key when needed.
- Do not use an expired key because discovery data is stale.
- Receiving grace allows late arrival of earlier sends; it does not keep an
  offline installation addressable for new messages after `send_until`.

### Latest state and permanent retirement

- Select the newest known active announcement per device.
- Competing active announcements use deterministic `(event timestamp, event ID)`
  ordering; use a precisely specified representation consistent with Rostra's
  ordering conventions.
- If the selected key is expired or not yet valid, skip the device. No fallback
  to older announcements.
- Any full account installation can announce another device's retirement.
- Retirement of a device ID is permanent and dominates active announcements,
  including delayed ones. Re-enrollment uses a fresh ID.
- Retirement affects senders only after they learn it; it cannot erase remote
  secrets or stop stale senders using an otherwise unexpired key.

## 5. Approved destination allocation

Eight total fixed cryptographic slots; make this a visible protocol constant.
Changing it requires a compatible format/version decision, not a local setting.

For each side, rank eligible devices by newest key publication:

1. Select up to four recipient devices.
2. Select up to four of the sender's **other** devices.
3. Give unused capacity from either side to the next newest devices on the
   other side, up to eight total.
4. Fill remaining slots with indistinguishable dummy recipients.
5. Shuffle real and dummy slots together, without public side labels.

Examples: recipient/sender-other availability of 7/7 selects 4/4; 6/2 selects
6/2; 2/7 selects 2/6; 8/0 selects 8/0; 3/1 leaves four dummies.

The sending installation retains local history and needs no slot.
More than eight eligible destinations means truncation under this policy,
not overflow failure, extra events, or a larger envelope.

If no recipient device has an eligible key, fail visibly before creating an
event; do not silently queue it or publish an unreadable message. Missing
eligible sender-other devices does not block sending.

Successful sending does not promise delivery to every registered device.
Omitted devices have no additional catch-up mechanism in this version.

## 6. Approved full-age envelope direction

Use a **complete native-X25519 age file**, not a custom shared-ephemeral
construction or extracted/reimplemented wrapping primitive.

- Supply eight shuffled native X25519 recipient public keys.
- Selected devices supply real keys.
- Fill unused capacity using freshly generated throwaway native recipients.
  Erase/drop dummy private material; do not use known reusable dummy keys.
- Use normal age encryption for all eight slots, wrapping the same file key.
- Independent encapsulation per slot is accepted despite its greater trial cost.
- No public recipient/device/key identifiers in the DM envelope.
- Use the library's complete header authentication and payload encryption.
- No passphrase/SSH/tagged/PQ/plugin recipient alternatives in this profile.

The library creates its own file key; do not carry forward the earlier custom
proposal's assumptions about its size or derive it from a permanent secret.

### Approved framing direction; exact encoding is implementation work

```text
signed Rostra event
    DM kind, author, timestamp, graph references
    aux-key = 16 zero bytes
    final content hash and length

payload
    fixed-width age_file_length
    complete age file encrypting:
        fixed application metadata
        actual text byte length
        text padded to its approved bucket
    outer padding to fixed overhead + text bucket
```

The assessed Rust age library adds variable-length GREASE metadata. Bound that
overhead and pad outside the complete file to keep externally visible payload
size dependent only on the approved text bucket. Exactly eight **cryptographic
recipient slots** does not mean exactly eight total age header stanzas.

Inner text must already be padded: a public inner file length must not expose
true text length. With inner padding it reveals the accepted bucket plus
already-visible, message-independent header variability.

Authenticate the outer event first. Range-check the file length, feed only the
exact bounded age slice to decryption, and require authenticated completion
through EOF. Do not strip/reorder a finished age header or feed trailing outer
padding into the age stream.

Calculate exact fixed metadata/overhead bounds from the selected pinned profile.
Do not retry encryption until random metadata happens to fit.

## 7. Approved discovery and encrypted body

No discovery shared secret or aux-key filter. DM aux-key is all zeros.

Verify candidate events from the WoT, fetch payloads within size policy, and try
native slots with locally retained epoch keys. Failure means unreadable on this
installation, not necessarily addressed to another account.

The approved logical encrypted-body fields are:

```text
message_format_version
sender_account
recipient_account
message_id
message_text
```

Use the signed event timestamp; no separate encrypted timestamp initially.
The encrypted recipient tells sender-side installations which conversation
contains a sent message.

### Text limits and padding

- Maximum text: **16 KiB UTF-8 bytes**.
- Text buckets: **1, 2, 4, 8, 16 KiB**.
- Pad only text, not text plus metadata.
- Encrypt the true byte length and the fixed metadata together with padded text.
- A maximum-sized text uses the 16 KiB bucket plus fixed overhead, never a
  32 KiB text bucket merely because metadata adds a few bytes.
- Enforce on the server, not only in browser controls.
- Limits/buckets must be visible constants with format-compatibility discipline.

Fixed-width metadata/length encoding is the planned approach; exact widths,
canonical encoding, padding bytes, and version numbers remain to be specified.

## 8. Approved local history, access, duplicates, and sending

### History and access

- Persist received plaintext after successful authenticated processing.
- Persist sent plaintext locally; the sending installation has no slot.
- Keep history across restarts and epoch-key deletion.
- Do not retain individual message/file keys after processing.
- No separate history-encryption password or automatic expiration initially.
- Expose history only to authenticated, unlocked full-account sessions.
  Read-only/locked sessions must not access it.
- Session checks do not encrypt the DB against direct storage access.

Background decryption while locked was not separately decided; implementation
must respect the approved access boundary without inferring a UI permission
from the presence of persisted keys.

### Duplicates and conflicts

- Generate one random logical message ID at send creation.
- Deduplicate by `(sender_account, message_id)`.
- Reprocessing must not duplicate history entries or notifications.
- Conflicting authenticated contents with the same identity: preserve existing
  entry and flag the conflict, rather than silently overwrite.
- Preserve message identity/provenance for any future history transfer.

### Sending and retries

- “Sent” means the event and local history entry are durably stored, ready for
  normal replication, not acknowledged by a recipient.
- Persist both consistently so a crash cannot leave a published message missing
  from sender history.
- Retry the exact existing event, even if selected destination keys later expire.
- Re-encryption is a new send operation, not a transport retry.
- Do not show delivered/read indicators without evidence.

## 9. Required deferred-work notes

During implementation, leave durable follow-up comments/notes for:

1. **Same-account message/history synchronization.** Some future mechanism
   should let installations obtain messages omitted from their slots.
   Transport, automation, and protocol remain undecided. Do not implement it
   now; no epoch-private-key sharing.
2. **Age-based local DM-history retention.** Deleting messages older than a
   configurable duration is a desired future policy. No duration or deletion
   implementation chosen now. Initial indefinite retention is not intended as
   the permanent policy.

## 10. Implementation obligations and remaining bounded engineering work

### Event and database integration

- DM events must **not** set singleton: zero aux-key plus singleton would make
  only the latest same-kind event matter.
- Existing singleton order is `(timestamp, ShortEventId)`; choose/document the
  corresponding exact announcement and cross-device ranking tie-breaks.
- Sticky retirement must survive out-of-order processing, applicable pruning,
  and projection rebuilding. Latest-active-value storage alone is insufficient.
- Plaintext history and local secrets are durable, non-replayable state.
  Rebuilding ordinary event projections must not discard them: ciphertext replay
  cannot regenerate history after keys expire.
- Verify signed author, payload hash, and length before trusting body data.
- Before history insertion or notification, verify encrypted sender equals
  signed author and local account is a named conversation participant.

### Validation and resource bounds

- Prevalidate untrusted announced X25519 keys before encryption. Senior research
  found a source-level encryption panic on non-contributory input in assessed
  age v0.12.1. Treat this as a negative-test obligation, not proof that the
  current selected dependency remains affected. Do not rely on panic catching.
- Define canonical public-key encoding and duplicate/equivalent key handling.
  Account signatures do not prove private-key possession or exclusive ownership.
  Do not invent a global first-seen key-ownership registry.
- Enforce the bounded native-only age profile before expensive operations.
  Limit header/stanza counts, field lengths, ciphertext and plaintext size,
  and trial work; never invoke plugins or expensive passphrase KDFs.
- Validate deadline ordering and checked arithmetic. Specify clock skew,
  announcement interval bounds, clock rollback, and future timestamps before
  deriving eligible/latest state. Defaults and validation bounds are distinct.
- Event timestamps are author assertions. Timestamp-based pruning of candidate
  decryption keys requires an explicit protocol rule or a correctness-preserving
  fallback, not an undocumented optimization.
- Network delivery may be stale; never extend signed key expiry to compensate.
- No decryption-success replies, external content fetches, or observable
  protocol callbacks before full validation. Avoid creating new decryption
  oracles through notifications, errors, or request-path timing.
- Locally caching attempted event/key combinations is optional; correctness
  must survive restarts and newly available keys without treating failure as
  proof of another recipient.

These are engineering obligations, not authorization to change approved
durations, eligibility/no-fallback, slot policy, or privacy scope. Escalate any
conflict or required behavioral change.

### Dependency and performance checks

- Consult project Rust dependency preferences before choosing dependencies.
- Verify actual available age version/advisories and pin/review relevant behavior.
  Research assessed v0.12.1; it did not verify crates.io yank state.
- Prefer the complete supported library and bounded outer framing, not a crypto
  fork merely to suppress GREASE.
- Benchmark worst-case unreadable traffic with all retained epoch keys:
  eight independent slots can require `8 * key_count` DH operations.
- Performance findings may motivate future work, not an unapproved public key
  hint, reduced padding, or return to custom shared encapsulation.

## 11. Required CLAIM, proof, and independent verification

The user explicitly requested creation of a Linked Specs `CLAIM-*` record
covering the design's security properties **as part of implementation**, followed
by proper verification.

- Follow `linked-specs`, `linked-specs-updating`, `linked-specs-claims`, and
  `linked-specs-claims-verification`.
- State concise, falsifiable properties and material immediate assumptions.
  Do not claim an unqualified “DMs are secure.”
- Cover content confidentiality, account authentication, qualified outsider
  recipient/count concealment, and conditional epoch protection at appropriate
  abstraction levels. Decompose only where necessary.
- Explicitly account for plaintext history, public metadata, endpoint compromise,
  signing-key compromise, and the approved storage-erasure limitations.
- Use the senior research as starting material for the proof, not as a prior
  verification pass or a checklist replacing independent attack derivation.
- Put property/status/assumptions in the claim record and the argument from the
  implemented source state in its sibling `proof.md`.
- New claim is `Unverified` until specifically requested verification completes.
  The proof author must not be its sole verifier.
- Independently verify the local implication at the implemented source state;
  distinguish assumed primitive guarantees from properties established by code.
- Report results and preserve material counterexamples in properly scoped
  falsification artifacts. Do not check in a verification report as companion
  evidence. Synchronize status using the claims workflow.

Planning approval does not waive or pre-judge verification.

## 12. Suggested implementation sequence and acceptance checks

The following preserves the suggested implementation sequence, not an outstanding
work list or a mandate to reorganize unrelated work. Current implementation and
verification status is stated above; deferred history retention/synchronization
remains explicitly out of scope.

1. Specify the bounded age profile, wire encoding, validation rules, threat
   statement, and local secret/history schema.
2. Implement epoch/device lifecycle and announcement projections, including
   permanent retirement and startup expiration.
3. Implement selection, full-age framing, sender/participant validation, and
   bounded trial decryption.
4. Integrate durable send/history storage, replication-driven receive processing,
   dedup/conflicts, and session-protected text DM UI.
5. Add required deferred-work notes; write the security claim and proof.
6. Run project checks and focused tests; perform independent claim verification.
   Escalate counterexamples rather than weaken the intended claim silently.

Test at least:

- All slot allocations and spillover, dummies, ordering and maximum text buckets.
- Expired/not-yet-valid/latest-invalid keys, stale views, retirement out of order,
  offline restart, and clock edge cases.
- Invalid/low-order/duplicate/cloned keys and announcement parsing boundaries.
- Altered author/body participants, removed/reordered slots, malformed/mixed
  headers, oversized fields, wrong length, truncation and authentication failure.
- Key deletion with retained plaintext history, projection rebuild, and documented
  backup/restore limitations.
- Exact event retries, duplicate notification suppression, conflicting IDs,
  crashes around durable send/history insertion, and locked/read-only access.
- Worst-case decryption cost and resource bounds.

No repository source or history was changed during this planning task. The
coordinator should integrate this plan and research into the implementation
ticket before relying on temporary artifact paths for long-term tracking.
