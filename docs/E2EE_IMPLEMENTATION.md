# End-to-End Encryption (E2EE) Implementation Plan for Syncline

This document outlines the proposed architecture for implementing End-to-End Encryption in Syncline. E2EE ensures that only the user's devices can read the content of their vault; the server only sees encrypted blobs.

## 1. Threat Model

- **Compromised Server:** If the server is hacked, the attacker should not be able to read any file content or file names.
- **Malicious Administrator:** The server operator should not be able to peek into user data.
- **Network Eavesdropping:** Even if TLS is compromised or not used, the data remains encrypted.

## 2. Cryptographic Choices

### Key Derivation (KDF)
- **Algorithm:** Argon2id
- **Input:** User-provided passphrase + 16-byte random salt.
- **Output:** A 256-bit "Vault Key".
- **Salt Storage:** The salt is stored in a special unencrypted file in the vault (e.g., `.syncline/salt`) or provided by the server upon authentication.

### Symmetric Encryption (AEAD)
- **Algorithm:** XChaCha20-Poly1305 or AES-256-GCM.
- **Library (Rust):** `ring` or `rust-crypto`.
- **Library (TypeScript):** Web Crypto API (AES-GCM) or `libsodium.js`.
- **Nonce:** A unique 96-bit or 192-bit nonce is generated for every encrypted message.

## 3. Implementation Strategy

### 3.1 Encrypted Protocol
The current protocol sends Yjs updates directly in the payload. With E2EE, the `payload` in `MSG_UPDATE` and `MSG_SYNC_STEP_2` will be structured as follows:

```
[ 16-byte Nonce | Encrypted Data (AEAD Ciphertext + Tag) ]
```

The `doc_id` remains in the clear so the server knows which document to associate the update with. Since `doc_id` is a UUID, it leaks no content information.

### 3.2 Metadata Encryption
The `__index__` document, which currently stores the mapping of UUIDs to filenames and directory structure, will also be encrypted using the Vault Key. This ensures the server cannot see the file tree.

### 3.3 Client-Side Compaction
**Critical Change:** Because the server can no longer read the updates, it cannot perform "Compaction" (merging multiple Yjs updates into a single snapshot).

- **New Process:**
    1. A client detects that a document has too many individual updates on the server.
    2. The client fetches all updates and the latest snapshot.
    3. The client decrypts everything, merges them locally using `yrs`/`yjs`.
    4. The client encrypts the resulting "Super-Snapshot".
    5. The client uploads the new snapshot to the server and instructs it to prune the old update history.

### 3.4 Binary File Encryption
Images, PDFs, and other binary blobs are encrypted before upload.
- **Hash calculation:** The SHA256 hash used for content-addressing should be calculated on the *plain-text* content to allow deduplication across clients using the same key, OR on the *encrypted* content if we don't care about deduplication.
- **Recommendation:** Encrypt with a unique random nonce, then hash the *ciphertext*. This avoids leaking whether two different users (or even the same user with different keys) have the same file.

## 4. Key Management & User Experience

1. **Setup:** On the first device, the user enters a passphrase. Syncline generates a salt and derives the Vault Key.
2. **Onboarding:** When adding a new device (Obsidian plugin or Linux daemon), the user must enter the exact same passphrase.
3. **Recovery:** If the passphrase is lost, the data is unrecoverable. There is no "Password Reset" that preserves data.

## 5. Required Code Changes

### `syncline/src/protocol.rs`
Add helper functions for wrapping/unwrapping encrypted payloads.

### `syncline/src/wasm_client.rs` & `client_folder/src/client.rs`
- Integrate encryption/decryption in the WebSocket `on_message` and `observe_update` handlers.
- Add a `set_encryption_key(key: Vec<u8>)` method.

### `server/src/server.rs`
- Remove the server-side Yjs merging logic (compaction).
- Add an API for "Client-Initiated Compaction".

## 6. Security Considerations
- **Memory Safety:** In the Rust daemon, use `secrecy` crate to zeroize keys in memory.
- **Search:** Server-side full-text search is impossible. Searching must happen entirely on the client.
- **Authentication:** The server still needs a way to authenticate users. This should be separate from the encryption key (e.g., a hashed version of the password or a standard JWT).
