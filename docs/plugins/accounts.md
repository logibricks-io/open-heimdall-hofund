# Account storage

`hofund_plugin_host::AccountStore` keeps account ids, labels, and opaque secret references in `accounts.json`. The metadata, lock, ciphertext, and key files are mode `0600` on Unix. Secret values are never written to the metadata file.

The store probes the native OS credential service: macOS Keychain, Windows Credential Manager, or Linux Secret Service. Each secret has one entry named by its plugin and account ids. `AccountStore::storage_kind` exposes the selection. If probing fails, a warning records the secret-free reason and the store uses `accounts.json.secrets` instead. Existing references from another backend fail explicitly rather than being silently reinterpreted.

The fallback uses XChaCha20-Poly1305 with a fresh random nonce for every secret write. Associated data binds the format version, plugin id, and account id, so a ciphertext moved to another account does not decrypt. A random 256-bit key is stored in `accounts.json.key`. Because the OS credential service is unavailable in this mode, that separate key file only protects against accidental disclosure or backup of the data file without its key. It does not protect against a same-user attacker who can read both files.

Opening a legacy plaintext store takes the exclusive sibling-file lock. It writes each secret to the selected backend, hard-links the legacy inode to a recovery name, atomically replaces the metadata with secret references, and then overwrites and deletes the recovery link. A failure at any stage can be retried without losing the old or new readable state. Overwriting is best effort: copy-on-write filesystems, flash wear leveling, and existing external backups can retain old bytes.

The reference reads native agent credentials and treats read failure as different from an absent sign-in (reference: internal/agent/codex.go:2037-2053). It also writes credentials through native platform facilities (reference: internal/agent/zed_credential_other.go:19-149; reference: internal/agent/zed_credential_windows.go:16-40). Hofund adopts native storage but owns account entries directly and adds the authenticated fallback and crash-safe migration.

Ordinary tests use in-memory credential backends and do not access D-Bus or a real keychain. `real_backend_smoke_test` is ignored by default and is the only account-store test that touches the real selected backend.
