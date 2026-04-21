//! v1 manifest sync — wire encoders, decoders, and the incoming-frame
//! dispatcher that drives the Yrs SyncStep1/SyncStep2/Update handshake
//! for the manifest Y.Doc.
//!
//! See `docs/DESIGN_DOC_V1.md` §4.
//!
//! # Framing
//!
//! Outer frames are built via [`crate::protocol::encode_message`]:
//!
//! ```text
//! [msg_type u8][doc_id_len u16 BE][doc_id utf8][payload]
//! ```
//!
//! For the manifest, `msg_type` is [`MSG_MANIFEST_SYNC`] and `doc_id`
//! is the reserved [`MANIFEST_DOC_ID`] (`"__manifest__"`). The payload
//! begins with a sub-type byte:
//!
//! | sub-type | name                | inner payload             |
//! |----------|---------------------|---------------------------|
//! | `0x00`   | [`MANIFEST_STEP_1`] | yrs state vector (`v1`)   |
//! | `0x01`   | [`MANIFEST_STEP_2`] | yrs update bytes (`v1`)   |
//! | `0x02`   | [`MANIFEST_UPDATE`] | yrs update bytes (`v1`)   |
//!
//! [`MSG_VERSION`] carries a separate two-byte `[major][minor]`
//! handshake frame.

use super::manifest::Manifest;
use super::projection::project;
use crate::protocol::{
    MANIFEST_STEP_1, MANIFEST_STEP_2, MANIFEST_UPDATE, V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR,
};
use sha2::{Digest, Sha256};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{ReadTxn, StateVector, Transact};

/// Encode the payload for an `MSG_VERSION` frame.
pub fn encode_version_handshake() -> Vec<u8> {
    vec![V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR]
}

/// Decode a version handshake payload. Returns `(major, minor)`.
pub fn decode_version_handshake(payload: &[u8]) -> Option<(u8, u8)> {
    if payload.len() != 2 {
        return None;
    }
    Some((payload[0], payload[1]))
}

/// Build a SyncStep1 payload from a pre-encoded state vector.
pub fn encode_manifest_step1(state_vector: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + state_vector.len());
    out.push(MANIFEST_STEP_1);
    out.extend_from_slice(state_vector);
    out
}

/// Build a SyncStep2 payload from a pre-encoded yrs update.
pub fn encode_manifest_step2(update: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + update.len());
    out.push(MANIFEST_STEP_2);
    out.extend_from_slice(update);
    out
}

/// Build an incremental Update payload from a pre-encoded yrs update.
pub fn encode_manifest_update(update: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + update.len());
    out.push(MANIFEST_UPDATE);
    out.extend_from_slice(update);
    out
}

/// Split a manifest-sync payload into its sub-type byte and inner yrs
/// bytes. Returns `None` on an empty payload.
pub fn split_manifest_payload(payload: &[u8]) -> Option<(u8, &[u8])> {
    if payload.is_empty() {
        None
    } else {
        Some((payload[0], &payload[1..]))
    }
}

/// Build a SyncStep1 payload carrying this manifest's current state
/// vector.
pub fn manifest_step1_payload(manifest: &Manifest) -> Vec<u8> {
    let sv_bytes = {
        let txn = manifest.doc().transact();
        txn.state_vector().encode_v1()
    };
    encode_manifest_step1(&sv_bytes)
}

/// Build a SyncStep2 payload carrying the updates needed to advance a
/// remote peer from `remote_sv_bytes` to our current state.
pub fn manifest_step2_payload(
    manifest: &Manifest,
    remote_sv_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let remote_sv = StateVector::decode_v1(remote_sv_bytes)?;
    let update = {
        let txn = manifest.doc().transact();
        txn.encode_state_as_update_v1(&remote_sv)
    };
    Ok(encode_manifest_step2(&update))
}

/// Dispatch an incoming manifest-sync payload. Applies remote state to
/// `manifest` as appropriate and returns an optional response payload
/// (already prefixed with its sub-type byte — the caller wraps it in
/// [`crate::protocol::encode_message`] with [`crate::protocol::MSG_MANIFEST_SYNC`]
/// and [`crate::protocol::MANIFEST_DOC_ID`]).
///
/// - `MANIFEST_STEP_1` → returns a `MANIFEST_STEP_2` with the updates
///   the remote peer is missing.
/// - `MANIFEST_STEP_2` / `MANIFEST_UPDATE` → applied; no response.
pub fn handle_manifest_payload(
    manifest: &mut Manifest,
    payload: &[u8],
) -> anyhow::Result<Option<Vec<u8>>> {
    let (sub_type, inner) = split_manifest_payload(payload)
        .ok_or_else(|| anyhow::anyhow!("empty manifest sync payload"))?;
    match sub_type {
        MANIFEST_STEP_1 => Ok(Some(manifest_step2_payload(manifest, inner)?)),
        MANIFEST_STEP_2 | MANIFEST_UPDATE => {
            manifest.apply_update(inner)?;
            Ok(None)
        }
        other => Err(anyhow::anyhow!(
            "unknown manifest sync sub-type {:#x}",
            other
        )),
    }
}

// ---------------------------------------------------------------------------
// Convergence verification (MSG_MANIFEST_VERIFY — §4.4.1)
// ---------------------------------------------------------------------------

/// SHA-256 over the canonical projection of `manifest`. Two peers that
/// project to the same set of user-visible files produce an identical
/// hash; divergent tombstones that never surface in the projection do
/// **not** cause a mismatch.
///
/// Canonical form: projected entries sorted by path, each encoded as
/// `path || 0x00 || node-id(hyphenated) || 0x00 || kind || 0x00 ||
/// blob-hash-or-empty || 0x00 || size(u64 BE) || 0x00`. The
/// zero-byte separators prevent `"a" + "bc"` from colliding with
/// `"ab" + "c"`.
pub fn projection_hash(manifest: &Manifest) -> [u8; 32] {
    let proj = project(manifest);
    let mut entries: Vec<_> = proj.by_path.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

    let mut hasher = Sha256::new();
    for (path, entry) in entries {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.id.to_string_hyphenated().as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.kind.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.blob_hash.as_deref().unwrap_or("").as_bytes());
        hasher.update([0u8]);
        hasher.update(entry.size.to_be_bytes());
        hasher.update([0u8]);
    }
    hasher.finalize().into()
}

/// Encode the payload for a `MSG_MANIFEST_VERIFY` frame: just the
/// 32-byte projection hash.
pub fn encode_verify_payload(hash: &[u8; 32]) -> Vec<u8> {
    hash.to_vec()
}

/// Decode a `MSG_MANIFEST_VERIFY` payload into its 32-byte hash.
pub fn decode_verify_payload(payload: &[u8]) -> Option<[u8; 32]> {
    if payload.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(payload);
    Some(out)
}

/// Handle an incoming `MSG_MANIFEST_VERIFY` payload. If the remote
/// hash matches the local projection hash, returns `Ok(None)`
/// (convergence confirmed). If they disagree, returns
/// `Ok(Some(manifest_sync_payload))` — a `MANIFEST_STEP_1` payload
/// ready to be wrapped in a `MSG_MANIFEST_SYNC` frame to force a
/// full re-sync.
pub fn handle_verify_payload(
    manifest: &Manifest,
    payload: &[u8],
) -> anyhow::Result<Option<Vec<u8>>> {
    let remote = decode_verify_payload(payload)
        .ok_or_else(|| anyhow::anyhow!("malformed MSG_MANIFEST_VERIFY payload"))?;
    let local = projection_hash(manifest);
    if local == remote {
        Ok(None)
    } else {
        Ok(Some(manifest_step1_payload(manifest)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::ids::{ActorId, NodeId};
    use super::super::manifest::{Manifest, NodeKind};
    use super::*;
    use crate::protocol::{
        decode_message, encode_message, MANIFEST_DOC_ID, MSG_MANIFEST_SYNC, MSG_VERSION,
    };

    #[test]
    fn version_handshake_roundtrip() {
        let payload = encode_version_handshake();
        assert_eq!(payload, vec![V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR]);
        let (major, minor) = decode_version_handshake(&payload).unwrap();
        assert_eq!((major, minor), (V1_PROTOCOL_MAJOR, V1_PROTOCOL_MINOR));
    }

    #[test]
    fn version_handshake_rejects_wrong_length() {
        assert!(decode_version_handshake(&[]).is_none());
        assert!(decode_version_handshake(&[1]).is_none());
        assert!(decode_version_handshake(&[1, 0, 0]).is_none());
    }

    #[test]
    fn version_handshake_wraps_in_outer_frame() {
        let payload = encode_version_handshake();
        let frame = encode_message(MSG_VERSION, MANIFEST_DOC_ID, &payload);
        let (t, d, p) = decode_message(&frame).unwrap();
        assert_eq!(t, MSG_VERSION);
        assert_eq!(d, MANIFEST_DOC_ID);
        assert_eq!(p, payload.as_slice());
    }

    #[test]
    fn split_payload_extracts_sub_type() {
        let inner = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let framed = encode_manifest_step1(&inner);
        let (sub, body) = split_manifest_payload(&framed).unwrap();
        assert_eq!(sub, MANIFEST_STEP_1);
        assert_eq!(body, inner.as_slice());

        let framed = encode_manifest_step2(&inner);
        let (sub, body) = split_manifest_payload(&framed).unwrap();
        assert_eq!(sub, MANIFEST_STEP_2);
        assert_eq!(body, inner.as_slice());

        let framed = encode_manifest_update(&inner);
        let (sub, body) = split_manifest_payload(&framed).unwrap();
        assert_eq!(sub, MANIFEST_UPDATE);
        assert_eq!(body, inner.as_slice());
    }

    #[test]
    fn split_payload_rejects_empty() {
        assert!(split_manifest_payload(&[]).is_none());
    }

    #[test]
    fn step1_payload_reflects_current_state() {
        let mut m = Manifest::new(ActorId::new());
        m.create_node("a.md", None, NodeKind::Text, None, 0);
        let payload = manifest_step1_payload(&m);
        let (sub, sv_bytes) = split_manifest_payload(&payload).unwrap();
        assert_eq!(sub, MANIFEST_STEP_1);
        // Must decode as a valid state vector.
        let _ = StateVector::decode_v1(sv_bytes).expect("valid state vector");
    }

    #[test]
    fn step1_then_step2_transfers_missing_updates() {
        // m1 has an entry; m2 is empty. m2 sends its SV to m1; m1 replies
        // with a Step2 containing what m2 is missing.
        let mut m1 = Manifest::new(ActorId::new());
        let id = m1.create_node("hello.md", None, NodeKind::Text, None, 5);

        let mut m2 = Manifest::new(ActorId::new());
        let step1 = manifest_step1_payload(&m2);

        let response = handle_manifest_payload(&mut m1, &step1)
            .expect("step1 handled")
            .expect("step1 yields a step2 response");
        let (sub, _update_bytes) = split_manifest_payload(&response).unwrap();
        assert_eq!(sub, MANIFEST_STEP_2);

        // Applying that response to m2 should make the entry visible.
        handle_manifest_payload(&mut m2, &response).unwrap();
        assert!(m2.get_entry(id).is_some());
    }

    #[test]
    fn step2_and_update_produce_no_response() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("x.md", None, NodeKind::Text, None, 0);

        let mut m2 = Manifest::new(ActorId::new());
        // Craft a step2 payload from m1's full state.
        let full_state = m1.encode_state_as_update();
        let step2 = encode_manifest_step2(&full_state);
        let resp = handle_manifest_payload(&mut m2, &step2).unwrap();
        assert!(resp.is_none());

        // And an incremental UPDATE frame likewise produces no response.
        let update = encode_manifest_update(&full_state);
        let resp = handle_manifest_payload(&mut m2, &update).unwrap();
        assert!(resp.is_none());
    }

    #[test]
    fn unknown_sub_type_is_rejected() {
        let mut m = Manifest::new(ActorId::new());
        let bogus = vec![0xFF, 0x00, 0x00];
        assert!(handle_manifest_payload(&mut m, &bogus).is_err());
    }

    #[test]
    fn handler_rejects_empty_payload() {
        let mut m = Manifest::new(ActorId::new());
        assert!(handle_manifest_payload(&mut m, &[]).is_err());
    }

    /// Full bidirectional sync simulation: each peer sends its SyncStep1,
    /// receives the other's SyncStep2, applies it, and converges.
    #[test]
    fn bidirectional_sync_converges() {
        let mut m1 = Manifest::new(ActorId::new());
        let mut m2 = Manifest::new(ActorId::new());

        let id1 = m1.create_node("from_m1.md", None, NodeKind::Text, None, 1);
        let id2 = m2.create_node("from_m2.md", None, NodeKind::Text, None, 2);

        // Each peer asks the other for what it's missing.
        let m1_step1 = manifest_step1_payload(&m1);
        let m2_step1 = manifest_step1_payload(&m2);

        // Exchange SyncStep2 responses.
        let m2_response_for_m1 = handle_manifest_payload(&mut m2, &m1_step1)
            .unwrap()
            .unwrap();
        let m1_response_for_m2 = handle_manifest_payload(&mut m1, &m2_step1)
            .unwrap()
            .unwrap();

        // Apply the responses.
        assert!(handle_manifest_payload(&mut m1, &m2_response_for_m1)
            .unwrap()
            .is_none());
        assert!(handle_manifest_payload(&mut m2, &m1_response_for_m2)
            .unwrap()
            .is_none());

        // Both peers now see both entries.
        for m in [&m1, &m2] {
            assert!(m.get_entry(id1).is_some(), "entry1 missing from a peer");
            assert!(m.get_entry(id2).is_some(), "entry2 missing from a peer");
        }
    }

    /// Concurrent edits on both peers still converge after a second
    /// round of SyncStep1 ↔ SyncStep2.
    #[test]
    fn bidirectional_sync_converges_with_concurrent_edits() {
        let a1 = ActorId::new();
        let a2 = ActorId::new();
        let mut m1 = Manifest::new(a1);
        let mut m2 = Manifest::new(a2);

        // Both create a node while disconnected.
        let id1 = m1.create_node("a.md", None, NodeKind::Text, None, 10);
        let id2 = m2.create_node("b.md", None, NodeKind::Text, None, 20);

        // Round 1: mutual SyncStep1 / SyncStep2.
        let s1 = manifest_step1_payload(&m1);
        let s2 = manifest_step1_payload(&m2);
        let r_for_m1 = handle_manifest_payload(&mut m2, &s1).unwrap().unwrap();
        let r_for_m2 = handle_manifest_payload(&mut m1, &s2).unwrap().unwrap();
        handle_manifest_payload(&mut m1, &r_for_m1).unwrap();
        handle_manifest_payload(&mut m2, &r_for_m2).unwrap();

        // Now concurrently: m1 renames id2, m2 deletes id1.
        assert!(m1.set_name(id2, "b-renamed.md"));
        assert!(m2.delete(id1));

        // Round 2: mutual SyncStep1 / SyncStep2 again.
        let s1 = manifest_step1_payload(&m1);
        let s2 = manifest_step1_payload(&m2);
        let r_for_m1 = handle_manifest_payload(&mut m2, &s1).unwrap().unwrap();
        let r_for_m2 = handle_manifest_payload(&mut m1, &s2).unwrap().unwrap();
        handle_manifest_payload(&mut m1, &r_for_m1).unwrap();
        handle_manifest_payload(&mut m2, &r_for_m2).unwrap();

        // Convergence check: both peers agree on all observable fields.
        for target in [id1, id2] {
            let e1 = m1.get_entry(target).expect("m1 has entry");
            let e2 = m2.get_entry(target).expect("m2 has entry");
            assert_eq!(e1.name, e2.name, "name diverged for {:?}", target);
            assert_eq!(e1.deleted, e2.deleted, "deleted diverged for {:?}", target);
            assert_eq!(e1.parent, e2.parent, "parent diverged for {:?}", target);
        }
    }

    /// Full wire-framed round-trip: wrap each payload in an outer
    /// `encode_message` frame, transport as bytes, decode, dispatch.
    #[test]
    fn wire_framed_sync_roundtrip() {
        let mut m1 = Manifest::new(ActorId::new());
        let id = m1.create_node("wire.md", None, NodeKind::Text, None, 0);
        let mut m2 = Manifest::new(ActorId::new());

        // m2 sends a framed SyncStep1.
        let s1_payload = manifest_step1_payload(&m2);
        let frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &s1_payload);

        // m1 receives bytes off the wire.
        let (msg_type, doc_id, payload) = decode_message(&frame).unwrap();
        assert_eq!(msg_type, MSG_MANIFEST_SYNC);
        assert_eq!(doc_id, MANIFEST_DOC_ID);

        // m1 dispatches and produces a response payload.
        let resp_payload = handle_manifest_payload(&mut m1, payload)
            .unwrap()
            .expect("step1 must elicit step2");
        let resp_frame = encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &resp_payload);

        // m2 receives the response and applies it.
        let (_t, _d, p) = decode_message(&resp_frame).unwrap();
        handle_manifest_payload(&mut m2, p).unwrap();

        assert!(m2.get_entry(id).is_some());
    }

    // -----------------------------------------------------------------
    // Convergence verification (MSG_MANIFEST_VERIFY)
    // -----------------------------------------------------------------

    #[test]
    fn projection_hash_is_deterministic() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a.md", None, NodeKind::Text, None, 10);
        m1.create_node("b.md", None, NodeKind::Text, None, 20);
        let h1 = projection_hash(&m1);
        let h2 = projection_hash(&m1);
        assert_eq!(h1, h2);
    }

    #[test]
    fn projection_hash_differs_when_content_differs() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a.md", None, NodeKind::Text, None, 10);
        let h1 = projection_hash(&m1);

        let mut m2 = Manifest::new(ActorId::new());
        m2.create_node("a.md", None, NodeKind::Text, None, 11);
        let h2 = projection_hash(&m2);

        assert_ne!(h1, h2, "size difference must affect projection hash");
    }

    #[test]
    fn projection_hash_matches_after_full_sync() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a.md", None, NodeKind::Text, None, 10);
        m1.create_node("b.md", None, NodeKind::Text, None, 20);

        let mut m2 = Manifest::new(ActorId::new());
        m2.create_node("c.md", None, NodeKind::Text, None, 30);

        // Full bidirectional sync via the manifest protocol.
        let s1 = manifest_step1_payload(&m1);
        let s2 = manifest_step1_payload(&m2);
        let r1 = handle_manifest_payload(&mut m2, &s1).unwrap().unwrap();
        let r2 = handle_manifest_payload(&mut m1, &s2).unwrap().unwrap();
        handle_manifest_payload(&mut m1, &r1).unwrap();
        handle_manifest_payload(&mut m2, &r2).unwrap();

        assert_eq!(
            projection_hash(&m1),
            projection_hash(&m2),
            "post-sync peers must agree on projection hash"
        );
    }

    #[test]
    fn verify_payload_roundtrip() {
        let m = Manifest::new(ActorId::new());
        let h = projection_hash(&m);
        let payload = encode_verify_payload(&h);
        assert_eq!(payload.len(), 32);
        let decoded = decode_verify_payload(&payload).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn verify_payload_rejects_wrong_length() {
        assert!(decode_verify_payload(&[]).is_none());
        assert!(decode_verify_payload(&[0u8; 31]).is_none());
        assert!(decode_verify_payload(&[0u8; 33]).is_none());
    }

    #[test]
    fn handle_verify_on_match_returns_none() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a.md", None, NodeKind::Text, None, 0);
        let mut m2 = Manifest::new(ActorId::new());
        // Seed m2 with the same state.
        m2.apply_update(&m1.encode_state_as_update()).unwrap();

        let remote_hash = projection_hash(&m2);
        let payload = encode_verify_payload(&remote_hash);
        let resp = handle_verify_payload(&m1, &payload).unwrap();
        assert!(resp.is_none(), "matching hashes must not trigger re-sync");
    }

    #[test]
    fn handle_verify_on_mismatch_returns_step1() {
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a.md", None, NodeKind::Text, None, 0);
        let mut m2 = Manifest::new(ActorId::new());
        m2.create_node("different.md", None, NodeKind::Text, None, 0);

        let remote_hash = projection_hash(&m2);
        let payload = encode_verify_payload(&remote_hash);
        let resp = handle_verify_payload(&m1, &payload)
            .unwrap()
            .expect("mismatch must trigger a sync");
        // The returned payload is a MANIFEST_STEP_1 ready to wrap.
        let (sub, _inner) = split_manifest_payload(&resp).unwrap();
        assert_eq!(sub, MANIFEST_STEP_1);
    }

    #[test]
    fn handle_verify_rejects_malformed_payload() {
        let m = Manifest::new(ActorId::new());
        assert!(handle_verify_payload(&m, &[0u8; 10]).is_err());
    }

    #[test]
    fn verify_then_sync_converges_mismatched_peers() {
        // Simulates the full §4.4.1 flow: a heartbeat VERIFY is sent,
        // the receiver notices divergence, returns a SyncStep1, the
        // originator then responds with a SyncStep2, and both peers
        // should now share a matching projection hash.
        let mut m1 = Manifest::new(ActorId::new());
        m1.create_node("a.md", None, NodeKind::Text, None, 10);
        let mut m2 = Manifest::new(ActorId::new());
        m2.create_node("b.md", None, NodeKind::Text, None, 20);

        // m2 heartbeats its projection hash to m1.
        let verify_payload = encode_verify_payload(&projection_hash(&m2));
        let step1_from_m1 = handle_verify_payload(&m1, &verify_payload)
            .unwrap()
            .expect("hashes should diverge");

        // m2 receives m1's step1, replies with step2 of what m1 is
        // missing from m2's state. m1 applies — m1 is now a superset.
        let step2_for_m1 = handle_manifest_payload(&mut m2, &step1_from_m1)
            .unwrap()
            .unwrap();
        handle_manifest_payload(&mut m1, &step2_for_m1).unwrap();

        // Second leg: m2 still lacks "a.md". m2 sends its own step1
        // so m1 can respond with what m2 is missing.
        let step1_from_m2 = manifest_step1_payload(&m2);
        let step2_for_m2 = handle_manifest_payload(&mut m1, &step1_from_m2)
            .unwrap()
            .unwrap();
        handle_manifest_payload(&mut m2, &step2_for_m2).unwrap();

        assert_eq!(
            projection_hash(&m1),
            projection_hash(&m2),
            "peers must converge after VERIFY-driven re-sync"
        );
    }

    /// Smoke test: NodeId parsing sanity (catches accidental formatting
    /// drift between encode/decode).
    #[test]
    fn node_id_survives_manifest_sync() {
        let mut m1 = Manifest::new(ActorId::new());
        let id = m1.create_node("id.md", None, NodeKind::Text, None, 0);
        let mut m2 = Manifest::new(ActorId::new());
        let s1 = manifest_step1_payload(&m2);
        let r = handle_manifest_payload(&mut m1, &s1).unwrap().unwrap();
        handle_manifest_payload(&mut m2, &r).unwrap();
        let e = m2.get_entry(id).unwrap();
        assert_eq!(e.id, id);
        // And NodeId formatting remains valid.
        let s = id.to_string_hyphenated();
        assert_eq!(NodeId::parse_str(&s), Some(id));
    }
}
