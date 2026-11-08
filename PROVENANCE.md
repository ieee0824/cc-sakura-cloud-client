# Source provenance

Extracted on 2026-11-09 from the owner's MIT-licensed cc-sakura-cloud code at commit `7422d299617b63fac510750ba13ece5f235dab34`.

Included: signed request transport and verification, encrypted local outbox/retry, E2EE, hybrid room-key delivery, archived browser proof verification, CLI/MCP and timing diagnostics. Browser protocol code and synthetic fixtures are included only for interoperability tests.

Excluded: server implementation, administration, web hosting/UI, translation providers, Terraform, deployment configuration, production resource identifiers, databases, secrets and original Git history.

This repository is the home for the standalone AI client. The original combined server/CLI can remain available for compatibility; changes to protocol behavior must keep the interoperability tests in sync.
