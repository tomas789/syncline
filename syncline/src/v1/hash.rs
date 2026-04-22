//! Portable SHA-256 → lowercase-hex helper shared by the blob store,
//! the projection hash, and the WASM client. Has no fs dependencies
//! so it compiles on both native and `wasm32-unknown-unknown`.

use sha2::{Digest, Sha256};

/// Hash `bytes` and return the 64-char lowercase hex digest.
pub fn hash_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(
            hash_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
