# Independent security review: Rostra 1:1 DM direction

Research date: September 10, 2026. Input: `/tmp/public/rostra-dm-design-AFuQy4Il.md`.
Research only; no project source/history changes. Approved product decisions remain authoritative. Recommendations below are NOT approved changes, an audit, a proof, or a frozen protocol.

## Decision summary

**Continue, but narrow the claim and prefer a complete existing envelope over the custom shared-ephemeral candidate.**

1. Ordinary confidentiality and sender authentication are attainable with conventional encryption plus Rostra's authenticated event. Confidence: strong in the architecture, conditional on a precise format and correct implementation.
2. Independent random epoch secrets can prevent *a subsequently stolen recovery phrase* from decrypting archived ciphertext. They do **not** make a subsequently stolen installation forget its deliberately retained plaintext. Confidence: strong in this distinction.
3. Ciphertext-only recipient hiding from outsiders is plausible and has a substantially better practical lead: **native X25519 age encryption**, with eight real-or-fresh-throwaway recipients. Its Rust documentation explicitly describes ciphertext-only recipient anonymity. This is stronger evidence than merely observing that a format omits key IDs, but not a proof of this complete Rostra system. [S1,S2]
4. Global “observers cannot learn the recipient” is false: eligibility announcements, traffic, endpoint behavior, compromised selected keys, and the plaintext's explicit recipient all constrain it. The defensible target is **no additional recipient/device-count disclosure by the cryptographic envelope, conditional on public metadata and honest uncompromised endpoints**.
5. “Thirty-five days maximum” is a scheduled key lifecycle, not guaranteed physical erasure. One surviving recipient key, sender-device key, sending ephemeral secret, message key, plaintext copy, snapshot, or future history transfer defeats the corresponding stronger claim.

The report's attacks and deductions are my independent analysis of the supplied design, not claims that the cited literature has evaluated Rostra.

## 1. What the literature actually supports

**Private broadcast encryption:** Barth, Boneh and Waters explicitly distinguish passive key privacy from active recipient privacy. Their attack copies a recipient header and substitutes a new body, using a file key known to a legitimate recipient; observable decryption then reveals membership. Their stronger construction binds the complete ciphertext with a one-time signature whose verification key is protected inside recipient components. Dummy recipients can pad the set size. Importantly, their game uses honestly generated public keys and equal-size challenge recipient sets; it does not certify arbitrary adversarial account announcements. [S3]

**PURBs:** useful evidence that hidden-recipient, padded multirecipient encryption is achievable, not a necessary dependency. Its analysis separates outsider CCA security and insider CPA recipient privacy, warns about observable decryption and header modification by insiders, and suggests stronger signatures for the latter. Its multisuite machinery and uniformly-random encoding solve problems beyond Rostra's accepted public sender/version/type metadata. I would not import that complexity for eight slots. [S4]

**HPKE:** a good conventional implementation option, but RFC 9180 §9.9 does not promise metadata protection, and §9.2.3 disallows reusing encapsulation randomness elsewhere. Independently encapsulating to every slot avoids that misuse; it does not automatically establish recipient anonymity. [S5]

**age:** native X25519 has the explicit library-level anonymity claim and a fully specified file envelope. The specification fixes per-stanza fresh ephemeral randomness, binds ephemeral and destination public keys through HKDF, and uses fixed-size authenticated file-key wrapping. Other recipient types must not be silently substituted: tags or SSH fingerprints violate this use case. The same file key is wrapped for every recipient; a header MAC protects the ordered header, and authenticated chunk encryption protects the body. These are useful existing components, not an audit claim. [S1,S2]

## 2. Concrete full-age recommendation and Rust integration review

Evaluate a **complete native age v1 file inside the signed event**, not an extracted/reimplemented wrapping algorithm:

- Generate independent installation epoch identities with `age::x25519::Identity::generate()`. Publish the associated public key under the approved account-signed announcement.
- Select destinations by the approved 4+4 spillover policy.
- Generate fresh throwaway X25519 identities for missing capacity, retain only their public keys, and erase/drop the private material. Encrypt the SAME actual file key to these recipients through the normal API. Do not use a special dummy tag, fake point encoding, zero body, or reused publicly known dummy key.
- Uniformly shuffle the eight recipient objects before `Encryptor::with_recipients`.
- Write the canonical, internally length-delimited padded application body through `wrap_output`; call `finish`.
- Sign the final event only after the complete encrypted payload exists. Verify event signature, author, hash and length before attempting decryption.
- Use `Decryptor` with retained identities, finish full authenticated reading, then check encrypted sender against signed author and verify that the local account is one of the two named conversation participants. No history entry, notification, link preview, network acknowledgment, or distinguishable external error before those checks.

**Pinned-source findings, age/rage v0.12.1:**

This is an assessed version, not a dependency-selection approval. GitHub's latest-release API reported `v0.12.1`, published July 14, 2026, when checked for this review; docs.rs `latest` also showed age 0.12.1. The crates.io API was inaccessible (HTTP 403), so I did not independently verify its registry/yank state. Recheck registry status and advisories before selecting a dependency.

* `Encryptor::with_recipients` loops over the supplied iterator and appends each recipient's stanzas: it neither sorts nor deduplicates them. Eight native recipients therefore produce eight cryptographic stanzas, including repeated public keys; independently fresh ephemeral generation avoids equality of duplicate-key wraps. Shuffle at the Rostra boundary. [S6,S8]
* **Not a fixed-size drop-in:** `HeaderV1::new` additionally appends a GREASE stanza for non-scrypt headers. GREASE has variable-length random framing/body and is explicitly distinguishable from native X25519 slots. It does not disclose which of the eight native slots are real, but it means stock output is not exactly eight total stanzas or fixed metadata. [S7,S9]
* **Malicious-key panic:** native recipient parsing accepts any correctly encoded 32-byte public key. Encryption computes DH, then panics if the result is zero, treating this as RNG failure. A malicious low-order recipient public key can cause that result without broken RNG. This is a source-level counterexample, not an executed exploit. Validate announced destination keys before calling the API; return a controlled failure, never let remote signed data reach this panic. Receiving detects zero output as an invalid header. [S8]
* Default decryption short-circuits on the first successful identity/stanza; it is not constant-work discovery. With `k` retained local epoch keys, an unreadable well-formed eight-native-slot file costs **8k DH operations**, plus symmetric work; a readable file may terminate earlier. Computing the local public key may add implementation cost. GREASE is skipped without DH. No measured throughput claim is made. [S6,S8,S10]
* Application code must bound total ciphertext size and stanza structure before the general parser and restrict native recipient types. Do not expose plugins, passphrase KDFs, arbitrary stanza counts or callbacks to untrusted DM events.

**How to resolve GREASE without inventing crypto:** preferred investigation is a supported upstream no-GREASE/fixed-profile option or a very small pinned encoding adaptation that retains the complete standard age encryption, MAC and payload processing. A stock-library alternative is a strictly bounded fixed outer frame that contains the complete variable-length age file and fills remaining frame space to the approved size bucket; its length/framing fields and padding must be covered by the event signature. That preserves crypto but is an additional wire-format decision, not yet approved. Do not strip/reorder a completed file header: its MAC would fail. Do not quietly call variable-sized stock output “fixed metadata.”

At the application level the signature binds the entire ciphertext to the claimed author; the encrypted sender check prevents simple cross-author re-signing from becoming an accepted message. Together with age integrity this is a reasonable outsider-authenticated envelope. It is **not** the BBW proof: a file-key holder can rewrite the body, recompute the age MAC, and sign as another account. See §5.

### Stock-library outer-frame feasibility

One particularly simple candidate avoids changing age at all:

```text
fixed-width age_file_length
complete age file encrypting (fixed metadata + bucket-padded text)
outer padding up to a fixed-overhead + body-bucket total
```

The **inner application body must already be bucket padded**. Padding only outside age would reveal its exact plaintext length through the visible age file length/header. A public fixed-width file-length field is sufficient; here it reveals only the accepted body bucket plus visible, message-independent GREASE variability. Encrypting that field is unnecessary and would complicate finding the age boundary. Its exact width and overhead bound are unresolved protocol choices.

Verify the outer event signature/hash/length first; range-check the inner length against the bounded available bytes and fixed profile; pass only that exact byte slice to age; authenticate it through EOF; separately validate/ignore only the specified outer padding. Never pass padding as part of the age stream and hope trailing garbage is accepted. Frame changes require the sender signature; altered/truncated inner age data must still pass age authentication. This is plain framing, not a new KDF/AEAD composition.

Before fixing a metadata budget, calculate its worst-case bound from the pinned eight canonical native stanzas, one bounded GREASE stanza, MAC/version, nonce/tag, and fixed application metadata. Enforce that bound rather than retrying encryption until random GREASE fits. If the project requires literally constant internal metadata rather than constant externally visible payload length, this outer-frame candidate does not satisfy that stronger requirement without an explicit decision.

For the sender panic, reject low-order/non-contributory announced public keys *before* converting them into usable age destinations. A straightforward screening candidate is RFC 7748 X25519 with a fixed valid clamped test scalar followed by an all-zero-output test; these calculations concern public inputs and do not require secret material. Define canonical announcement encoding separately. This is low-order rejection, not proof of secret possession or honest key generation. Confirm test coverage for all accepted encodings and pin the actual low-level library behavior; catching panics is not the primary validation mechanism. [S8,S12]

I favor this full-age profile first. Only if its library constraints cannot be resolved simply should Rostra choose a small independent-encapsulation frame for review. A shared ephemeral is an optimization to justify later, not a security requirement.

## 3. Content confidentiality and authentication

**Conditional confidence: strong.**

Honest sender and recipient installations, authentic eligible announcements, sound RNG, secret keys, checked signatures, and correct authenticated encryption provide a coherent boundary. A passive outsider knowing all public epoch keys cannot simply re-encrypt a guessed message and compare: fresh encryption randomness prevents that test. Known text is not a file key. [S1,S2]

Necessary application checks:

- Authenticate the precise version, event kind and encoding rules. Reject unsupported versions rather than downgrade.
- No circular use of the final content hash to generate the ciphertext whose hash it is. Final signature over the completed hash is natural.
- For a custom frame, explicitly settle canonical KDF context, AAD, nonce uniqueness, key/public-key binding, body length, slot order and complete-envelope binding. “HKDF + AEAD” is not enough specification.
- Store sender plus message ID as the dedup key; conflicts must not overwrite existing history. Exact transport retries must retain identical ciphertext, as already approved.
- Retain the original signed event/provenance with local messages when feasible; an authenticated sender can equivocate, and a later stolen signing key can forge backdated new events. Timestamp alone does not prove historical authorship.

The signed event authenticates the account, not a particular device/person. Any full installation has signing authority. No cryptographic format can stop an intended recipient from retaining or redistributing plaintext.

## 4. Epoch erasure and recovery-phrase compromise

**Confidence: strong for conditional cryptographic separation; unestablished for actual storage erasure.**

For message M, define E(M) as every secret that can recover its body key or plaintext. The intended claim requires the relevant members of E(M) to be unavailable to the later attacker, not just deletion of the currently displayed recipient's old key.

Counterexamples:

1. Bob deletes his key; Alice's selected other installation retains its corresponding key. That installation still decrypts the archived event.
2. All epoch keys disappear; the stolen installation's DB still contains M. This is exactly the approved plaintext-history policy, not an implementation error.
3. A device is off for six months. Its disk image still contains the nominally expired key. Deletion on restart is too late for an attacker who images the disk first.
4. A VM/disk snapshot retains the epoch key or sending ephemeral secret. Restoring the snapshot also risks repeated RNG state, device identity and time rollback.
5. Future history transfer preserves M elsewhere or re-encrypts it under current keys. Original-key deletion does not revoke the new copy.

Consequently:

- Recovery phrase alone, stolen after the fact, does not reconstruct independent random old epoch keys. But if that phrase also unlocks a retained secret backup/history store, that separate exposure defeats the claim.
- Phrase/signing compromise **does compromise future traffic**: the attacker announces its own fresh keys, creates device IDs, wins newest-first slots, or permanently retires honest devices.
- Changing the constants means 35 days is not a universal cross-version upper bound unless validation caps enforce it. Authenticated parameters are claims by a device, not proof of when it erases.
- Sender-side ephemeral/wrapping/file-key buffers and temporary dummy private keys require erasure too. “Do not persist file keys” must cover queues, crash dumps, debug logs and failed transactions.

Logical DB deletion, overwriting a file and dropping a Rust object are not a forensic erasure argument. Include DB WAL/old pages, filesystem COW, SSD remapping, swap, hibernation, core dumps, backup tools and host snapshots in the threat model. The SSD erasure literature directly demonstrates the gap between normal software-visible deletion and recoverable device copies. [S11]

A separate encrypted secret store can simplify key lifecycle, but a permanent recoverable wrapping key plus retained old ciphertext merely relocates the problem. Do not promise secure deletion until the storage design says exactly which later-compromise model it covers.

## 5. Recipient anonymity and active attacks

### Passive outsider, honest public keys

**Confidence: reasonable for native age ciphertext-only privacy; no complete Rostra proof.**

The right comparison is two honest destination sets compatible with the same accepted public information, same sender and same body bucket. Eight native stanzas with fresh throwaway destinations avoid directly exposing recipient count. Publicly knowing candidate keys does not by itself provide DH shared secrets. The strongest available implementation claim I found is age's own documentation, not a published proof specifically of eight padded age slots in a signed Rostra DAG. [S1,S2]

### Public eligibility is already a recipient filter

My counterexample: if exactly one plausible account has any currently eligible epoch key, a successfully sent message identifies that account by the sending policy alone. Retirement/no-fallback removes candidates; timing near key publication adds correlations. Public sender, follows/WoT, graph references and availability may shrink the set further.

This is accepted metadata leakage if the claim is explicitly conditional. It violates a blanket claim against an observer with *all* public announcements, regardless of metadata.

### Key cloning, malicious accounts and slot starvation

Account signatures authenticate authorization, not possession or exclusivity of an encryption private key. Mallory can sign Bob's public epoch key into Mallory's own device announcement without knowing its secret. A message addressed to Mallory can then be cryptographically opened by Bob. This is not a break for two honest endpoints: malicious Mallory deliberately authorized a key Bob controls. It does make “this public key uniquely identifies one account/device” false.

Bob must reject such plaintext unless its named participants include Bob. Still, avoiding an observable difference between failed wrapper opening, successful wrapper opening followed by participant rejection, and normal message acceptance is an application concern.

Likewise, malicious same-account authority can clone a key across many fresh device IDs and monopolize newest-first slots. Distinct IDs are not distinct installations or independent keys. Deduplicating exact public keys before capacity assignment may be a useful validation policy, but is a new selection-detail decision and cannot stop a signing-key holder generating genuinely distinct malicious keys. Global first-seen ownership would invite preclaiming/DoS; do not add that.

RFC 7748 warns that multiple public keys can yield equivalent shared secrets. Bind public-key bytes in derivation and define acceptable announcement encoding. Merely comparing raw byte strings is not a universal distinct-secret test. [S12]

### Forged freshness

With signing compromise, an attacker can publish many current attacker-owned keys, future-dated newest announcements, or sticky retirements. Future-dated newest records can suppress valid older keys under the approved no-fallback rule. Without signing compromise, a network adversary can hide newer announcements or retirement, but cannot extend a correctly checked absolute expiry. Lack of fresh evidence should cause unavailable/partial delivery, not fallback to expired keys.

Validate deadlines, ordering, maximum advertised interval, timestamp skew and arithmetic bounds *before* eligibility/projection decisions. Rotation is not post-compromise recovery: retained signing authority lets the attacker reauthorize itself indefinitely.

### Active failure oracles and known-file-key insiders

An outsider can copy an old age file into a new signed event. Its encrypted sender check fails, but early unwrap success, notifications, remote logs, timing or fetch behavior must not expose who opened it. Keep decryption off externally observable request paths where possible; no automatic error replies or content-triggered remote fetches.

A legitimate/compromised selected key holder knows the file key and, in this format, the recipient account directly. Therefore recipient-account anonymity against it is already out of scope. Nevertheless it can remove slots, recompute the MAC, change the plaintext's sender/recipient, sign as itself and probe **which devices** can open retained slots. Outer signature plus mutable encrypted sender is not a proof against this insider operation. The literature's header-reuse attack is the analogous reason not to claim insider CCA recipient privacy. [S3,S4]

No read receipts helps; it does not stop human replies, browser requests, timing or selective replication becoming an oracle. Signed senders can also send an ordinary chosen message to a known candidate and observe a reply: cryptography cannot hide voluntary behavior.

## 6. Dummy and selected-copy-count concealment

**For outsiders:** real encryption to fresh throwaway public keys is the simplest defensible padding, conditional on key privacy. It matches normal point and ciphertext distributions; random bytes used as fake X25519 points need not. No published dummy key/secret, count field, recipient-list ordering or per-device event. [S1,S2; inference]

**Known text:** not a generic distinguisher. **Known body/file key:** removes confidentiality and directly reveals the body recipient, but does not alone calculate unknown DH wrapping keys. Header MAC verification is not a real-slot counter. Using the same real file key even for dummy destinations avoids designing a second dummy-plaintext distribution.

**Compromised subset:** it necessarily reveals membership of the compromised keys. Moreover 4+4 spillover leaks information about the other side's count. Example: Alice has at least seven eligible other-device keys. If seven of them open, Bob had one selected recipient device; six implies two; five implies three; four means at least four. A subset of Alice's keys gives weaker predicates. This is NOT an ordinary outsider break; compromised selected keys already expose Bob's account from plaintext. It is a reason not to advertise count concealment against insiders or arbitrary later compromises.

Even with no decrypting key, public device inventories and deterministic eligibility impose count bounds. Eight slots hide ciphertext-dependent real/dummy classification, not public facts.

## 7. Minimum work before implementation claims

1. Freeze a precise adversary statement: outsider ciphertext-only privacy conditional on public metadata; accepted endpoint/traffic leakage; independent-key epoch separation; no account-signing compromise recovery.
2. Choose full-age profile versus custom frame. Resolve GREASE/fixed metadata, strict native-only parsing, sender binding, malicious-key rejection and work limits.
3. Specify announcement validation and stale-view behavior, sticky retirement persistence, duplicate-key policy and clock rollback.
4. Specify secret-store deletion model separately from indefinite plaintext retention and UI/session access control.
5. Test negative cases: zero/low-order keys; repeated/equivalent public keys; cloned announcements; future timestamps; prolonged offline restart; key/DB backup restoration; eight-slot overflow; shuffled/removed slots; altered sender; body truncation; mixed stanzas; duplicate IDs with conflicting contents; replay and exact retries.
6. Benchmark unreadable-event discovery with all retained epoch keys. Eight independent encapsulations trade performance for simpler analysis. Do not reduce anonymity by silently adding a public key hint.

The requested implementation CLAIM and its proper verification remain a required future deliverable. This report provides a proposed threat statement, counterexamples, sources and test obligations; it is **not** a passed verification of any CLAIM or implementation.

**Bottom line:** the requested basic properties can coexist under a limited, useful threat model. Strong automatic erasure, global recipient anonymity, count secrecy from selected insiders, and safety after account-signing compromise do not follow from this design. The best next step is a precise full-age application profile and lifecycle specification, not a new shared-DH construction merely because its primitives are familiar.

## Primary sources

Sources were read directly. Source-based summaries above are intentionally limited; attacks identified as mine are deductions from the working notes. URLs are included in code for reliable handoff.

```text
[S1] Native age X25519 recipient documentation:
https://docs.rs/age/0.12.1/age/x25519/struct.Recipient.html
[S2] Complete age specification (X25519, file key, header MAC, payload):
https://c2sp.org/age@v1.1.0
[S3] Barth, Boneh, Waters, Privacy in Encrypted Content Distribution (FC 2006):
https://www.adambarth.com/papers/2006/barth-boneh-waters.pdf
[S4] Nikitin et al., Reducing Metadata Leakage ... PURBs (2019 version), §§3.7–3.8:
https://arxiv.org/pdf/1806.03160
[S5] RFC 9180 §§9.2.3, 9.7.4, 9.9:
https://www.rfc-editor.org/rfc/rfc9180.html
[S6] Pinned age 0.12.1 Encryptor/Decryptor source:
https://docs.rs/age/0.12.1/src/age/protocol.rs.html
[S7] Pinned age header source (automatic GREASE):
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age/src/format.rs
[S8] Pinned native X25519 source (generation, parsing, wrap, unwrap, panic):
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age/src/native/x25519.rs
[S9] Pinned GREASE generator:
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age-core/src/format.rs
[S10] Pinned default Identity::unwrap_stanzas:
https://raw.githubusercontent.com/str4d/rage/v0.12.1/age/src/lib.rs
[S11] Wei et al., Reliably Erasing Data From Flash-Based SSDs (FAST 2011):
https://www.usenix.org/conference/fast11/reliably-erasing-data-flash-based-solid-state-drives
[S12] RFC 7748 §7 (non-contributory behavior, equivalent public keys):
https://www.rfc-editor.org/rfc/rfc7748.html
[S13] Latest release API checked during review:
https://api.github.com/repos/str4d/rage/releases/latest
https://github.com/str4d/rage/releases/tag/v0.12.1
```
