# Cryptographic implementation guidance

Follow the repository root guidance and CONVENTIONS.md. Before changing this
crate, read its SECURITY.md and README.md, and the approved plan and independent
design research in ../../docs/direct-messages/.

Age or profile changes require re-deriving framing/resource bounds and running
adversarial tests; do not treat old source research as a current audit. Keep
caller verification, lifetime, background scheduling, and session-access
preconditions explicit. Do not activate partial DM implementations or claim
that this envelope alone establishes end-to-end security.
