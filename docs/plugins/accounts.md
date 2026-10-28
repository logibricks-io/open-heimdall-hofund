# Account storage

`hofund_plugin_host::AccountStore` keeps account kinds, ids, labels, and opaque secret references in `accounts.json`. Missing kinds in legacy rows mean `subscription`. The metadata, lock, ciphertext, and key files are mode `0600` on Unix. Secret values are never written to the metadata file.

New writes probe the native OS credential service: macOS Keychain, Windows Credential Manager, or Linux Secret Service. `AccountStore::storage_kind` exposes that selection. If probing fails, a warning records the secret-free reason and new writes use `accounts.json.secrets`. Selection does not control existing rows: reads and deletes route each row's stored `secret_ref` to its native or encrypted-file backend. An unavailable backend fails only the accounts that reference it.

New native entries use `os-keychain:v2` and are named by plugin kind, plugin id, and account id. New encrypted records use `encrypted-file:v2`, with the same identity bound into the associated data. Version-1 entries remain readable with their legacy unscoped names and subscription identity. This prevents an agent and a subscription plugin with the same bare id and account id from sharing a secret.

Lists and metadata writes do not read every secret. `secret()` validates the row's kind and kebab-case ids, loads only that row, and caches it per row until a write invalidates the snapshot. One missing, wrong-key, or tampered secret therefore fails only that account. The same validation happens before migration creates native names or associated data.

The fallback uses XChaCha20-Poly1305 with a fresh random nonce for every secret write. A random 256-bit key is stored in `accounts.json.key`, which must be a regular non-symlink file with mode `0600` on Unix. Because the OS credential service is unavailable in this mode, that separate key file only protects against accidental disclosure or backup of the data file without its key. It does not protect against a same-user attacker who can read both files.

Opening a legacy plaintext store takes the exclusive sibling-file lock. It writes each secret to the selected backend, hard-links the legacy inode to a recovery name, atomically replaces the metadata with secret references, and then overwrites and deletes the recovery link. Filesystems without hard-link support fall back to a copied recovery file. The old `accounts.tmp` plaintext temporary file is also overwritten and removed. A failure at any stage can be retried without losing the old or new readable state. Overwriting is best effort: copy-on-write filesystems, flash wear leveling, and existing external backups can retain old bytes.

An upsert writes the new secret before replacing metadata. A crash between those steps can leave an orphan secret that no row references; it does not leave a row with a missing secret. Orphan cleanup is not currently performed on open.

The Linux Secret Service backend is built without the optional encrypted D-Bus session feature. Secrets therefore cross the user's session bus unencrypted. This is acceptable for the same-user local bus, but it is not protection against a process that can access that bus.

The reference reads native agent credentials and treats read failure as different from an absent sign-in (reference: internal/agent/codex.go:2037-2053). It also writes credentials through native platform facilities (reference: internal/agent/zed_credential_other.go:19-149; reference: internal/agent/zed_credential_windows.go:16-40). Hofund adopts native storage but owns account entries directly and adds the authenticated fallback and crash-safe migration.

Ordinary tests use in-memory credential backends and do not access D-Bus or a real keychain. `real_backend_smoke_test` is ignored by default and is the only account-store test that touches the real selected backend.
