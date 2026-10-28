R2 separates fallback-key reads from key generation. Secret reads only load an existing key and fail per row when it is absent.
Upsert and plaintext migration generate keys under the exclusive account lock.
A unique mode-0600 temporary file is published with a non-overwriting hard link; a racing publisher re-reads the winning key.
The automatic backend reuses one encrypted-file handle, preventing divergent in-process key caches.
Linux enables keyring's maintained MIT/Apache-2.0 `vendored` feature so libdbus is linked statically.
This adds no new crate and avoids a runtime libdbus-1.so dependency that would prevent the headless encrypted-file fallback from starting.
Windows uses keyring-core's actual `api::CredentialStoreApi` path and the four-argument entry signature; the target is not installed locally, so those call sites were reviewed manually.
Tests cover missing-key reads, second publication, wrong-backend operations, and kind tampering.
