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
