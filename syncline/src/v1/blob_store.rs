//! Content-addressed blob storage for v1 binary files.
//!
//! Blobs are stored at `<root>/ab/cd/abcd...` where the filename is the full
//! lowercase hex SHA-256 of the content and the two leading directory levels
//! are the first four hex characters. This keeps any single directory from
//! growing unbounded and lets us answer "do we have this hash?" with a single
//! `stat`.
//!
//! Writes are atomic (tmp file + fsync + rename) and verified against the
//! expected hash on `insert_verified`, so a peer pushing us a corrupted blob
//! can't poison the store.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Length in hex characters of a SHA-256 digest.
const HASH_HEX_LEN: usize = 64;

/// On-disk CAS for binary blobs, keyed by hex SHA-256.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Construct a store rooted at `root`. The directory is created lazily
    /// on first insert — callers may pass a path that doesn't yet exist.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Root directory on disk.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compute the on-disk path for a given hex hash. Does not touch the
    /// filesystem and does not validate existence — only syntactic layout.
    pub fn path_for(&self, hash_hex: &str) -> PathBuf {
        // Caller is expected to have validated via `validate_hex_hash` already,
        // but be defensive: if the hash is malformed we still want a sensible
        // (unique) path rather than a panic.
        let (a, b, rest) = if hash_hex.len() >= 4 {
            (&hash_hex[0..2], &hash_hex[2..4], &hash_hex[..])
        } else {
            ("__", "__", hash_hex)
        };
        self.root.join(a).join(b).join(rest)
    }

    /// Return true if a blob with this hash is present on disk.
    pub fn has(&self, hash_hex: &str) -> bool {
        if validate_hex_hash(hash_hex).is_err() {
            return false;
        }
        self.path_for(hash_hex).is_file()
    }

    /// Hash `bytes`, write them to the store (atomic), and return the hex
    /// digest. Idempotent — re-inserting the same bytes is a no-op.
    pub fn insert_bytes(&self, bytes: &[u8]) -> Result<String> {
        let hash_hex = hash_hex(bytes);
        self.write_atomic(&hash_hex, bytes)?;
        Ok(hash_hex)
    }

    /// Insert `bytes` and verify they hash to `expected_hash_hex`. Returns
    /// `Err` on mismatch without writing. Use this for anything received
    /// from a peer or other untrusted source.
    pub fn insert_verified(&self, expected_hash_hex: &str, bytes: &[u8]) -> Result<()> {
        validate_hex_hash(expected_hash_hex)?;
        let actual = hash_hex(bytes);
        if actual != expected_hash_hex {
            bail!(
                "blob hash mismatch: expected {}, computed {}",
                expected_hash_hex,
                actual
            );
        }
        self.write_atomic(&actual, bytes)
    }

    /// Read the blob with this hash. Errors if missing or malformed hash.
    pub fn read(&self, hash_hex: &str) -> Result<Vec<u8>> {
        validate_hex_hash(hash_hex)?;
        let path = self.path_for(hash_hex);
        fs::read(&path).with_context(|| format!("reading blob {}", path.display()))
    }

    fn write_atomic(&self, hash_hex: &str, bytes: &[u8]) -> Result<()> {
        validate_hex_hash(hash_hex)?;
        let final_path = self.path_for(hash_hex);
        if final_path.is_file() {
            return Ok(());
        }
        let parent = final_path
            .parent()
            .ok_or_else(|| anyhow!("blob path has no parent: {}", final_path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating blob dir {}", parent.display()))?;

        let tmp_path = parent.join(format!(".{}.tmp", hash_hex));
        {
            let mut f = fs::File::create(&tmp_path)
                .with_context(|| format!("creating tmp blob {}", tmp_path.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("writing tmp blob {}", tmp_path.display()))?;
            f.sync_all()
                .with_context(|| format!("fsync tmp blob {}", tmp_path.display()))?;
        }
        fs::rename(&tmp_path, &final_path).with_context(|| {
            format!(
                "renaming {} -> {}",
                tmp_path.display(),
                final_path.display()
            )
        })?;
        Ok(())
    }
}

/// Hash `bytes` and return the 64-char lowercase hex digest.
pub fn hash_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Return `Ok(())` iff `s` is exactly 64 lowercase-hex characters.
fn validate_hex_hash(s: &str) -> Result<()> {
    if s.len() != HASH_HEX_LEN {
        bail!("blob hash must be {} hex chars, got {}", HASH_HEX_LEN, s.len());
    }
    if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("blob hash must be lowercase hex: {}", s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, BlobStore) {
        let tmp = TempDir::new().unwrap();
        let store = BlobStore::new(tmp.path().to_path_buf());
        (tmp, store)
    }

    #[test]
    fn hash_hex_matches_known_vector() {
        // SHA-256("") per FIPS 180-4
        assert_eq!(
            hash_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // SHA-256("abc") per FIPS 180-4
        assert_eq!(
            hash_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn insert_then_has_and_read_roundtrip() {
        let (_tmp, s) = store();
        let bytes = b"hello, blob world";
        let h = s.insert_bytes(bytes).unwrap();
        assert!(s.has(&h));
        let got = s.read(&h).unwrap();
        assert_eq!(got, bytes);
    }

    #[test]
    fn has_returns_false_when_missing() {
        let (_tmp, s) = store();
        // Well-formed hash that was never inserted.
        let ghost = "0".repeat(64);
        assert!(!s.has(&ghost));
    }

    #[test]
    fn has_returns_false_for_malformed_hash() {
        let (_tmp, s) = store();
        assert!(!s.has("nope"));
        assert!(!s.has("ABCD")); // uppercase rejected
    }

    #[test]
    fn insert_is_idempotent() {
        let (_tmp, s) = store();
        let bytes = b"same bytes";
        let h1 = s.insert_bytes(bytes).unwrap();
        let h2 = s.insert_bytes(bytes).unwrap();
        assert_eq!(h1, h2);
        assert!(s.has(&h1));
        // Second insert should not have left a stray tmp file.
        let parent = s.path_for(&h1).parent().unwrap().to_path_buf();
        let strays: Vec<_> = fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with('.') && n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "found stray tmp files: {:?}", strays);
    }

    #[test]
    fn insert_verified_accepts_matching_hash() {
        let (_tmp, s) = store();
        let bytes = b"verify me";
        let h = hash_hex(bytes);
        s.insert_verified(&h, bytes).unwrap();
        assert!(s.has(&h));
    }

    #[test]
    fn insert_verified_rejects_mismatch() {
        let (_tmp, s) = store();
        let bytes = b"payload";
        // Valid 64-hex but not the real hash of `bytes`.
        let wrong = "a".repeat(64);
        let err = s.insert_verified(&wrong, bytes).unwrap_err().to_string();
        assert!(err.contains("mismatch"), "{}", err);
        assert!(!s.has(&wrong));
        // Real hash should also not have been written as a side-effect.
        let real = hash_hex(bytes);
        assert!(!s.has(&real));
    }

    #[test]
    fn insert_verified_rejects_malformed_hash() {
        let (_tmp, s) = store();
        assert!(s.insert_verified("tooshort", b"x").is_err());
        assert!(s.insert_verified(&"Z".repeat(64), b"x").is_err());
    }

    #[test]
    fn read_errors_on_missing() {
        let (_tmp, s) = store();
        let ghost = "0".repeat(64);
        assert!(s.read(&ghost).is_err());
    }

    #[test]
    fn path_for_uses_sharded_layout() {
        let (tmp, s) = store();
        let h = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let p = s.path_for(h);
        let rel = p.strip_prefix(tmp.path()).unwrap();
        let components: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(components, vec!["ab".to_string(), "cd".to_string(), h.to_string()]);
    }

    #[test]
    fn insert_materializes_sharded_directories() {
        let (tmp, s) = store();
        let bytes = b"layout test";
        let h = s.insert_bytes(bytes).unwrap();
        assert!(tmp.path().join(&h[0..2]).is_dir());
        assert!(tmp.path().join(&h[0..2]).join(&h[2..4]).is_dir());
        assert!(s.path_for(&h).is_file());
    }

    #[test]
    fn read_rejects_malformed_hash() {
        let (_tmp, s) = store();
        assert!(s.read("not-hex").is_err());
    }
}
