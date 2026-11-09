# Security

- Payload: AES-256-GCM, with operation/member/room context binding.
- Room-key delivery: ML-KEM-768 + P-256, HKDF-SHA-256, AES-GCM.
- API signatures: Ed25519. Archived browser WebAuthn proofs accept ES256 only.
- TLS and canonical project URLs are required. Insecure loopback is an explicit development opt-in. Authentication never falls back to legacy session/Bearer mode.
- Public keys are pinned locally; full key transparency is not implemented. Session-authenticated browser receipts are server-authenticated records, not independent user signatures.
- Post-quantum room-key delivery does not make Ed25519, ES256 or TLS fully post-quantum. This project does not claim a fully post-quantum authenticated protocol.

Private keys, local profiles/outboxes, delivery-device files, recovery bundles and invitation tokens must not enter source control. Unix private-file checks and restrictive file creation permissions are enforced by the client. Protect the directory and backups too. Decrypted messages printed to stdout are sensitive; applications and agents must protect their own logs.

`deny.toml` documents a narrow pre-existing RSA advisory exception inherited through webauthn-rs. This client accepts ES256 WebAuthn public keys only and performs no RSA private-key operation. Re-evaluate the exception on dependency upgrades.

Report suspected vulnerabilities privately to the repository maintainer through GitHub's private vulnerability reporting when enabled. Do not post live credentials or exploit details with private data in public issues.

The optional SSE watcher invokes only the explicitly configured executable/arguments, never commands from chat text. State stores IDs, cursor and dispatch timestamps, not clear messages. It is scoped to server/project/member/alias, private on Unix, and protected by an OS lock. A pending marker is durable before handler dispatch; crash/timeout/failure requires explicit resolution because a send may already have succeeded. Automatic retries of side effects and exactly-once claims are avoided. A rolling dispatch cap bounds automatic AI calls. Other agents, self messages and unsigned legacy imports are excluded by default (self/legacy always excluded).

The bundled explanatory responder forwards decrypted chat context to the selected Codex model, keeps authentication/profile/key material out of the prompt, and uses temporary ephemeral sessions with read-only sandbox and shell/web search disabled. It validates a structured text answer and posts using the signed client, never evaluates model output as code. Custom handlers remain trusted local programs with the user's local permissions. Monitoring is not enabled simply by installing the skill.
