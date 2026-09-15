# CLAIM-direct-message-security: Qualified direct-message confidentiality

For honest sending and receiving installations, a text direct message accepted
into local history is bound to its signed sending account and authenticated
encrypted participants. Public ciphertext does not reveal its text to an
outsider without a selected decryption key. For an outsider without any selected
private key or file key, the envelope does not additionally identify selected
recipients or the number of real slots beyond public metadata and the text-size
bucket.

Independent epoch keys provide conditional protection of archived ciphertext:
once every secret capable of recovering a message is actually unavailable to a
later attacker, learning an account recovery/signing secret or a later epoch
secret does not recover that message from public ciphertext alone. Keys expired
at a receive transaction's sampled wall time are not used for its trials.
Private-message HTTP routes do not expose retained plaintext or authorize
message/device mutations using another session's unlocked authority.

These properties are not global anonymity, active-insider private broadcast,
per-message forward secrecy, delivery guarantees, or proof of a particular
person's authorship. Retained plaintext history is outside ciphertext-erasure
protection, and logical deletion is not forensic erasure.

## Status

Unverified.

## Assumptions

- The native age X25519 wrapping and authenticated file format provide their
  intended confidentiality, recipient-key concealment for independently
  generated honest keys, and authenticated-completion properties; X25519,
  HKDF, the age MAC/AEAD constructions, event hashes, and account signatures
  satisfy their intended computational security properties.
- Cryptographic randomness is unpredictable and independent; honest account,
  epoch, ephemeral, dummy, message-ID, and session-token generation does not
  suffer exploitable collisions or RNG failure.
- The protected participants' signing authority and every selected decryption
  key remain uncompromised during the message's protected use. Honest
  installations do not deliberately leak plaintext or publish adversarial
  destination keys; authenticated announcements are account statements, not
  proof of honest generation or exclusive key ownership.
- The host, browser, in-process callers, storage engine, operating system, and
  dependencies execute the relevant code correctly; there is no unrelated
  same-origin script compromise, credential theft, or memory-corruption attack.
  Private HTTP traffic uses a trusted local connection or authenticated
  confidential transport, and HTTP agents honor the sensitive response controls.
- The later-attacker epoch property applies only when no relevant selected
  recipient or sender-device key, age file key, or other recovery-capable copy
  survives for that attacker, including stolen keys, old pages, powered-off
  devices, backups, snapshots, swap, hibernation, or crash dumps. The attacker
  does not obtain retained plaintext history or an earlier plaintext copy.
- Public account authorship, key announcements and eligibility, the event graph,
  Web of Trust, timing, traffic, size buckets, and voluntary behavior are accepted
  leakage. An outsider's inference from those observations is not concealed;
  selected insiders and their membership/count predicates are outside the
  envelope recipient-concealment property.
