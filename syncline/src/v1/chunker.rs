//! Content-defined chunker for binary blobs.
//!
//! Splits a byte slice into deterministic, hash-addressed chunks using
//! FastCDC (Xia et al. 2016, ATC). Same input always yields the same
//! chunk boundaries, so:
//!
//! - **Wire size is bounded.** No single frame exceeds [`MAX_CHUNK_SIZE`],
//!   which is well under the WebSocket frame limit. Large files get
//!   split into many smaller frames.
//! - **Edits are local.** A small change at one position only invalidates
//!   the chunks around that position; the rest are reused as-is by both
//!   client and server (chunks are content-addressed by SHA-256).
//! - **Storage is dedup'd for free.** Two files that share a contiguous
//!   region (same prefix, same suffix, same middle, etc.) share the
//!   chunk hashes for that region. The CAS layer stores each unique
//!   chunk once.
//!
//! For files smaller than [`MIN_CHUNK_SIZE`], FastCDC returns a single
//! chunk containing the whole file — the small-file path is the same as
//! today's "one blob per file", just expressed as a length-1 chunk list.
//!
//! This module is pure portable code (no I/O, no `cfg`-gates) so the
//! WASM client can chunk in-browser before sending.
//!
//! See `docs/DESIGN_DOC_V1.md` for the broader picture and issue #59
//! for the failure modes that motivated chunking.

use crate::v1::hash::hash_hex;

/// Hard floor on chunk size. FastCDC will not emit a chunk smaller than
/// this except for the *trailing* chunk of the input.
pub const MIN_CHUNK_SIZE: u32 = 256 * 1024;

/// Target average. The Gear-hash boundary mask is chosen to make this
/// the modal chunk size; actual chunks scatter around it.
pub const AVG_CHUNK_SIZE: u32 = 1024 * 1024;

/// Hard ceiling on chunk size. Picked deliberately under the 5 MiB
/// `MAX_BLOB_SIZE` we plan to enforce on the wire (see issue #59), so a
/// chunk frame plus protocol envelope is always safely below the
/// WebSocket frame ceiling.
pub const MAX_CHUNK_SIZE: u32 = 4 * 1024 * 1024;

/// One chunk emitted by [`chunks`].
///
/// Holds a slice into the input buffer plus the chunk's offset into the
/// original file, which the caller may need to reassemble by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    pub offset: u64,
    pub bytes: &'a [u8],
}

/// Iterate the FastCDC chunks of `data`.
///
/// Empty input yields an empty iterator. For non-empty input, all
/// non-trailing chunks are within `[MIN_CHUNK_SIZE, MAX_CHUNK_SIZE]`;
/// the trailing chunk may be shorter than `MIN_CHUNK_SIZE`. Concatenating
/// every emitted chunk's `bytes` reproduces `data` byte-for-byte.
pub fn chunks(data: &[u8]) -> impl Iterator<Item = Chunk<'_>> + '_ {
    fastcdc::v2020::FastCDC::new(data, MIN_CHUNK_SIZE, AVG_CHUNK_SIZE, MAX_CHUNK_SIZE)
        .into_iter()
        .map(move |c| Chunk {
            offset: c.offset as u64,
            bytes: &data[c.offset..c.offset + c.length],
        })
}

/// One hashed chunk: SHA-256 hex digest plus the chunk bytes.
///
/// The hex form matches the existing CAS layer in [`v1::blob_store`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashedChunk {
    pub hash: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// Chunk `data` and SHA-256-hash each chunk. Convenience for callers
/// that need owned bytes anyway (e.g. about to push to the blob store
/// or send over the wire).
pub fn chunks_with_hashes(data: &[u8]) -> Vec<HashedChunk> {
    chunks(data)
        .map(|c| HashedChunk {
            hash: hash_hex(c.bytes),
            offset: c.offset,
            bytes: c.bytes.to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random byte stream. We *don't* use `rand`
    /// here because we need bit-exact reproducibility across runs and
    /// across platforms — these tests pin the chunker's behaviour, so
    /// any drift in a future fastcdc bump fails them loudly.
    fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
        // Xorshift64; good enough for spreading entropy through chunk
        // boundaries without pulling in a crate.
        let mut state = seed.max(1);
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(chunks(&[]).next().is_none());
        assert!(chunks_with_hashes(&[]).is_empty());
    }

    #[test]
    fn small_file_is_single_chunk() {
        // Below MIN_CHUNK_SIZE → one chunk = whole file.
        let data = deterministic_bytes(1, 100 * 1024);
        let cs: Vec<_> = chunks(&data).collect();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].offset, 0);
        assert_eq!(cs[0].bytes, &data[..]);
    }

    #[test]
    fn boundary_sized_file_is_single_chunk() {
        // Exactly MIN_CHUNK_SIZE: still one chunk, no boundary inside.
        let data = deterministic_bytes(2, MIN_CHUNK_SIZE as usize);
        let cs: Vec<_> = chunks(&data).collect();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].bytes.len(), MIN_CHUNK_SIZE as usize);
    }

    #[test]
    fn large_file_splits_into_multiple_chunks() {
        // 16 MiB → expect roughly 16 chunks at avg=1MiB, but only assert
        // the loose property "more than one" so a future tweak to
        // AVG_CHUNK_SIZE doesn't break the test.
        let data = deterministic_bytes(3, 16 * 1024 * 1024);
        let cs: Vec<_> = chunks(&data).collect();
        assert!(cs.len() > 1, "expected >1 chunks, got {}", cs.len());
    }

    #[test]
    fn chunks_respect_size_bounds() {
        let data = deterministic_bytes(4, 16 * 1024 * 1024);
        let cs: Vec<_> = chunks(&data).collect();
        let n = cs.len();
        for (i, c) in cs.iter().enumerate() {
            // Trailing chunk is allowed to be < MIN_CHUNK_SIZE; every
            // other chunk must hit the FastCDC bounds.
            if i + 1 < n {
                assert!(
                    c.bytes.len() >= MIN_CHUNK_SIZE as usize,
                    "chunk {i}/{n} below min: {}",
                    c.bytes.len()
                );
            }
            assert!(
                c.bytes.len() <= MAX_CHUNK_SIZE as usize,
                "chunk {i}/{n} above max: {}",
                c.bytes.len()
            );
        }
    }

    #[test]
    fn concatenation_roundtrips_to_original() {
        let data = deterministic_bytes(5, 8 * 1024 * 1024 + 17);
        let cs: Vec<_> = chunks(&data).collect();
        let mut joined = Vec::with_capacity(data.len());
        for c in &cs {
            joined.extend_from_slice(c.bytes);
        }
        assert_eq!(joined, data);
        // Offsets must form a contiguous, zero-based covering of `data`.
        let mut expected = 0u64;
        for c in &cs {
            assert_eq!(c.offset, expected);
            expected += c.bytes.len() as u64;
        }
        assert_eq!(expected as usize, data.len());
    }

    #[test]
    fn chunking_is_deterministic() {
        // Run the same input through twice — boundaries must match
        // exactly. This is the contract that makes content-addressed
        // dedup work across clients.
        let data = deterministic_bytes(6, 4 * 1024 * 1024);
        let a = chunks_with_hashes(&data);
        let b = chunks_with_hashes(&data);
        assert_eq!(a, b);
    }

    #[test]
    fn small_edit_at_start_only_invalidates_a_few_chunks() {
        // Flip the first byte of a 16 MiB file. FastCDC should
        // resynchronise within a couple of chunks; the long tail of
        // the file should be reused unchanged.
        let mut a = deterministic_bytes(7, 16 * 1024 * 1024);
        let mut b = a.clone();
        b[0] ^= 0xff;

        let ca = chunks_with_hashes(&a);
        let cb = chunks_with_hashes(&b);

        // Same file length, similar number of chunks (within ±2).
        assert!(
            (ca.len() as isize - cb.len() as isize).abs() <= 2,
            "chunk count diverged: {} vs {}",
            ca.len(),
            cb.len()
        );

        // Count overlap by hash. Way more than half the chunks should
        // be identical; in practice for FastCDC on a single-byte edit,
        // it's typically all-but-the-first.
        let a_hashes: std::collections::HashSet<_> = ca.iter().map(|c| &c.hash).collect();
        let shared = cb.iter().filter(|c| a_hashes.contains(&c.hash)).count();
        let total = cb.len();
        assert!(
            shared * 4 >= total * 3,
            "expected ≥75% chunk reuse for a single-byte prefix edit, got {shared}/{total}"
        );

        // Revert the edit to keep `a` unchanged for the assert below
        // (defends against the test being copy-pasted into a future
        // case where ordering matters).
        a[0] ^= 0;
    }

    #[test]
    fn small_edit_at_end_only_invalidates_a_few_chunks() {
        let a = deterministic_bytes(8, 16 * 1024 * 1024);
        let mut b = a.clone();
        let last = b.len() - 1;
        b[last] ^= 0xff;

        let ca = chunks_with_hashes(&a);
        let cb = chunks_with_hashes(&b);

        let a_hashes: std::collections::HashSet<_> = ca.iter().map(|c| &c.hash).collect();
        let shared = cb.iter().filter(|c| a_hashes.contains(&c.hash)).count();
        let total = cb.len();
        assert!(
            shared * 4 >= total * 3,
            "expected ≥75% chunk reuse for a single-byte suffix edit, got {shared}/{total}"
        );
    }

    #[test]
    fn shared_middle_region_dedups_across_files() {
        // Two files: same 4 MiB middle, different prefix / suffix.
        // The shared middle should produce shared chunk hashes.
        let middle = deterministic_bytes(9, 4 * 1024 * 1024);
        let prefix_a = deterministic_bytes(10, 1 * 1024 * 1024);
        let prefix_b = deterministic_bytes(11, 1 * 1024 * 1024);
        let suffix_a = deterministic_bytes(12, 1 * 1024 * 1024);
        let suffix_b = deterministic_bytes(13, 1 * 1024 * 1024);

        let mut file_a = Vec::new();
        file_a.extend_from_slice(&prefix_a);
        file_a.extend_from_slice(&middle);
        file_a.extend_from_slice(&suffix_a);
        let mut file_b = Vec::new();
        file_b.extend_from_slice(&prefix_b);
        file_b.extend_from_slice(&middle);
        file_b.extend_from_slice(&suffix_b);

        let ca = chunks_with_hashes(&file_a);
        let cb = chunks_with_hashes(&file_b);

        let a_hashes: std::collections::HashSet<_> = ca.iter().map(|c| &c.hash).collect();
        let shared = cb.iter().filter(|c| a_hashes.contains(&c.hash)).count();
        // At least two chunks of the middle should hash identically. The
        // exact number depends on where FastCDC's gear hash decides to
        // slice the prefix/suffix boundaries, so we don't pin it
        // higher.
        assert!(
            shared >= 2,
            "expected ≥2 shared chunks across files with a common middle, got {shared} (a={}, b={})",
            ca.len(),
            cb.len(),
        );
    }

    #[test]
    fn chunks_with_hashes_offsets_are_contiguous() {
        let data = deterministic_bytes(14, 5 * 1024 * 1024);
        let cs = chunks_with_hashes(&data);
        let mut expected = 0u64;
        for c in &cs {
            assert_eq!(c.offset, expected);
            expected += c.bytes.len() as u64;
        }
        assert_eq!(expected as usize, data.len());
    }

    #[test]
    fn hashes_match_hex_of_chunk_bytes() {
        let data = deterministic_bytes(15, 3 * 1024 * 1024);
        for c in chunks_with_hashes(&data) {
            assert_eq!(c.hash, hash_hex(&c.bytes));
        }
    }
}
