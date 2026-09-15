# REQ-ui-functionality-security: Preserve requested UI functionality within security boundaries

## Authority and scope

The project owner requires ordinary UI conveniences, including private-message
composer behavior consistent with existing post composers. On September 15,
2026, the owner reaffirmed Ctrl+Enter submission and browser-persisted,
account-and-recipient-specific drafts, and approved recording the governing
functionality and security balance here.

This is a stakeholder requirement for web UI behavior and its security
boundaries. It does not prescribe a particular CSP, storage API, draft lifetime,
or other implementation mechanism.

## Requirements

Core workflows remain usable through ordinary server-rendered HTTP as described
by
[DESIGN-server-rendered-hypermedia](DESIGN-server-rendered-hypermedia.md).
No-JavaScript usability does not prohibit trusted first-party Alpine
enhancements. The required private-message composer conveniences must not be
withheld merely because an earlier implementation omitted scripts or browser
persistence.

Authorization, CSRF protection, validation, and access to private data remain
server-enforced. User-controlled content remains non-executable data. Private
responses retain protections against unintended disclosure, unsafe embedding,
and leakage through URLs, logs, or third-party requests; security mechanisms
must accommodate the required UI behavior without bypassing these protections.

Private-message draft state must not mix accounts or recipients in ordinary UI
use. Browser-local plaintext persistence is accepted: encryption-key expiry
does not erase retained history or browser drafts, and access to a shared
browser profile is not a protected secrecy boundary. The browser, first-party
UI code, backend, host, and local storage remain within the trusted endpoint
assumptions of
[CLAIM-direct-message-security](../../../specs/CLAIM-direct-message-security.md).

## Interpreting restrictions

An agent-selected CSP, storage mechanism, draft lifetime, or interaction pattern
does not become a stakeholder mandate merely because the implementation or
ordinary documentation records it. When such a choice conflicts with requested
behavior, identify the underlying security property and its authority, then
reconcile non-gate documentation with the explicitly requested result. Escalate
a genuine unresolved property conflict, not an already-resolved implementation
choice. This record neither authorizes bypassing security checks nor changing a
gate.
