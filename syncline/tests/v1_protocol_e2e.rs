//! End-to-end integration test for the v1 manifest sync stack.
//!
//! Exercises the protocol as it would run on the wire: two (or three)
//! simulated peers drive real path-level operations through
//! `v1::ops`, then exchange `MSG_MANIFEST_SYNC` and `MSG_MANIFEST_VERIFY`
//! frames through the outer `protocol::encode_message` / `decode_message`
//! framing. Convergence is asserted by the projection hash plus per-path
//! field agreement.
//!
//! Lives outside `src/v1/` so it exercises the crate's public surface
//! the same way the client/server code will once v1 is wired through.

#![cfg(not(target_arch = "wasm32"))]

use syncline::protocol::{
    decode_message, encode_message, MANIFEST_DOC_ID, MSG_MANIFEST_SYNC, MSG_MANIFEST_VERIFY,
    MSG_VERSION,
};
use syncline::v1::{
    create_binary, create_text, delete_path, encode_verify_payload, encode_version_handshake,
    handle_manifest_payload, handle_verify_payload, manifest_step1_payload, projection_hash,
    record_modify_text, rename, ActorId, Manifest,
};

/// Single-peer wrapper that carries a Manifest plus an outgoing frame
/// queue. Mimics the read side of what a real client loop would do.
struct Peer {
    label: &'static str,
    manifest: Manifest,
}

impl Peer {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            manifest: Manifest::new(ActorId::new()),
        }
    }

    /// Build a SyncStep1 frame (outer-framed bytes).
    fn frame_step1(&self) -> Vec<u8> {
        encode_message(
            MSG_MANIFEST_SYNC,
            MANIFEST_DOC_ID,
            &manifest_step1_payload(&self.manifest),
        )
    }

    /// Build a VERIFY frame.
    fn frame_verify(&self) -> Vec<u8> {
        let hash = projection_hash(&self.manifest);
        encode_message(
            MSG_MANIFEST_VERIFY,
            MANIFEST_DOC_ID,
            &encode_verify_payload(&hash),
        )
    }

    /// Receive a framed message and produce the response frame (if any).
    fn receive(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let (msg_type, doc_id, payload) =
            decode_message(frame).expect("malformed frame arrived at peer");
        assert_eq!(
            doc_id, MANIFEST_DOC_ID,
            "{}: expected manifest doc-id",
            self.label
        );
        match msg_type {
            MSG_VERSION => None,
            MSG_MANIFEST_SYNC => handle_manifest_payload(&mut self.manifest, payload)
                .expect("handle manifest payload")
                .map(|resp| encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &resp)),
            MSG_MANIFEST_VERIFY => handle_verify_payload(&self.manifest, payload)
                .expect("handle verify payload")
                .map(|resp| encode_message(MSG_MANIFEST_SYNC, MANIFEST_DOC_ID, &resp)),
            other => panic!("{}: unexpected msg_type {:#x}", self.label, other),
        }
    }
}

/// Drive a full bidirectional exchange between two peers: each sends
/// its SyncStep1; each applies the other's SyncStep2.
fn bidirectional_sync(a: &mut Peer, b: &mut Peer) {
    let a1 = a.frame_step1();
    let b1 = b.frame_step1();
    // a's step1 -> b produces step2 for a
    let step2_for_a = b.receive(&a1).expect("b must reply to a's step1");
    let step2_for_b = a.receive(&b1).expect("a must reply to b's step1");
    assert!(a.receive(&step2_for_a).is_none(), "step2 must not reply");
    assert!(b.receive(&step2_for_b).is_none(), "step2 must not reply");
}

fn assert_converged(peers: &[&Peer]) {
    let head = projection_hash(&peers[0].manifest);
    for p in &peers[1..] {
        assert_eq!(
            projection_hash(&p.manifest),
            head,
            "peer {} diverged from {}",
            p.label,
            peers[0].label
        );
    }
}

#[test]
fn version_handshake_frames_roundtrip() {
    let hs = encode_version_handshake();
    let frame = encode_message(MSG_VERSION, MANIFEST_DOC_ID, &hs);
    let (t, d, p) = decode_message(&frame).unwrap();
    assert_eq!(t, MSG_VERSION);
    assert_eq!(d, MANIFEST_DOC_ID);
    assert_eq!(p, hs.as_slice());
}

#[test]
fn two_peer_create_converges() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "hello.md", 5).unwrap();
    create_binary(
        &mut b.manifest,
        "pic.png",
        &["cafebabe".to_string()],
        1024,
    )
    .unwrap();

    bidirectional_sync(&mut a, &mut b);
    assert_converged(&[&a, &b]);
}

#[test]
fn two_peer_nested_path_converges() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "notes/day/one.md", 0).unwrap();
    create_text(&mut b.manifest, "notes/day/two.md", 0).unwrap();
    create_text(&mut b.manifest, "notes/week/plans.md", 0).unwrap();

    bidirectional_sync(&mut a, &mut b);
    assert_converged(&[&a, &b]);
}

#[test]
fn rename_propagates_across_peers() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "old.md", 0).unwrap();
    bidirectional_sync(&mut a, &mut b);
    assert_converged(&[&a, &b]);

    // A renames; B picks it up after sync.
    rename(&mut a.manifest, "old.md", "renamed/deep/new.md").unwrap();
    bidirectional_sync(&mut a, &mut b);
    assert_converged(&[&a, &b]);
}

#[test]
fn delete_propagates_and_converges() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "doomed.md", 0).unwrap();
    bidirectional_sync(&mut a, &mut b);
    assert_converged(&[&a, &b]);

    delete_path(&mut a.manifest, "doomed.md").unwrap();
    bidirectional_sync(&mut a, &mut b);
    assert_converged(&[&a, &b]);
}

#[test]
fn verify_heartbeat_no_op_when_synced() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "x.md", 0).unwrap();
    bidirectional_sync(&mut a, &mut b);

    // Heartbeat — should produce no response frame.
    let v = a.frame_verify();
    assert!(
        b.receive(&v).is_none(),
        "matching hashes must not trigger re-sync"
    );
    assert_converged(&[&a, &b]);
}

#[test]
fn verify_heartbeat_drives_reconvergence() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "diverged.md", 0).unwrap();
    // No initial sync — peers intentionally diverge.

    // A sends VERIFY. B compares, notices mismatch, responds with STEP_1.
    let verify_frame = a.frame_verify();
    let step1_from_b = b
        .receive(&verify_frame)
        .expect("mismatch should trigger re-sync");

    // A receives B's STEP_1 and returns STEP_2.
    let step2_for_b = a
        .receive(&step1_from_b)
        .expect("A must answer B's step1 with step2");
    assert!(
        b.receive(&step2_for_b).is_none(),
        "step2 does not trigger another response"
    );

    // B now has A's data. But A may still lack B's — if B also created,
    // run a second leg. Here B had nothing, so one-way is enough.
    assert_converged(&[&a, &b]);
}

#[test]
fn three_peer_full_mesh_converges() {
    // Simulates a "hub" sync: each pair exchanges, so everyone learns
    // everyone's state. Mimics what a server-relayed fan-out would
    // deliver once the manifest is fully broadcast.
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");
    let mut c = Peer::new("C");

    create_text(&mut a.manifest, "from_a.md", 1).unwrap();
    create_text(&mut b.manifest, "from_b.md", 2).unwrap();
    create_text(&mut c.manifest, "from_c.md", 3).unwrap();
    create_text(&mut a.manifest, "shared/a.md", 0).unwrap();
    create_text(&mut b.manifest, "shared/b.md", 0).unwrap();

    // Full mesh: every pair runs a bidirectional sync. Two passes are
    // needed so the knowledge transits from A→B→C and back.
    for _ in 0..2 {
        bidirectional_sync(&mut a, &mut b);
        bidirectional_sync(&mut b, &mut c);
        bidirectional_sync(&mut a, &mut c);
    }

    assert_converged(&[&a, &b, &c]);
}

#[test]
fn modify_beats_delete_end_to_end() {
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    let id = create_text(&mut a.manifest, "race.md", 0).unwrap();
    bidirectional_sync(&mut a, &mut b);

    // A deletes; B first observes it, then modifies the node. B's
    // modify stamp ends up strictly > A's delete stamp so the node
    // resurrects on both peers.
    delete_path(&mut a.manifest, "race.md").unwrap();
    bidirectional_sync(&mut a, &mut b);
    assert!(b.manifest.get_entry(id).unwrap().deleted);

    assert!(b.manifest.record_modify(id));
    bidirectional_sync(&mut a, &mut b);

    // Both peers must agree the file is live now.
    assert_converged(&[&a, &b]);
    use syncline::v1::projection::project;
    assert!(project(&a.manifest).by_path.contains_key("race.md"));
    assert!(project(&b.manifest).by_path.contains_key("race.md"));
}

#[test]
fn concurrent_modify_then_sync_preserves_both_writes() {
    // Both peers modify different files concurrently — a classic
    // "happy path" CRDT case. After sync, every modify stamp is
    // preserved.
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    let id1 = create_text(&mut a.manifest, "a.md", 0).unwrap();
    let id2 = create_text(&mut a.manifest, "b.md", 0).unwrap();
    bidirectional_sync(&mut a, &mut b);

    // Concurrent modifies.
    record_modify_text(&mut a.manifest, "a.md").unwrap();
    record_modify_text(&mut b.manifest, "b.md").unwrap();
    bidirectional_sync(&mut a, &mut b);

    assert_converged(&[&a, &b]);
    // Both modify stamps visible on both peers.
    for m in [&a.manifest, &b.manifest] {
        assert!(m.get_entry(id1).unwrap().modify_stamp.is_some());
        assert!(m.get_entry(id2).unwrap().modify_stamp.is_some());
    }
}

#[test]
fn reconnect_scenario_resyncs_via_verify() {
    // Peer A does work, disconnects (no sync), comes back, and a
    // VERIFY heartbeat from B correctly triggers the catch-up flow.
    let mut a = Peer::new("A");
    let mut b = Peer::new("B");

    create_text(&mut a.manifest, "shared.md", 0).unwrap();
    bidirectional_sync(&mut a, &mut b);

    // "Disconnected" — only A mutates.
    create_text(&mut a.manifest, "offline1.md", 0).unwrap();
    create_text(&mut a.manifest, "offline2.md", 0).unwrap();

    // On reconnect, B sends VERIFY. A notices mismatch, replies with
    // A's own STEP_1. This is the "hey, I'm out of sync too, let's
    // swap state vectors" signal.
    let v_from_b = b.frame_verify();
    let step1_from_a = a.receive(&v_from_b).expect("A mismatches with B");
    // B applies A's step1: returns STEP_2 of what A lacks (nothing, A
    // is the superset). A applies the empty update.
    let step2_for_a = b.receive(&step1_from_a).expect("B returns step2 for A");
    assert!(a.receive(&step2_for_a).is_none());

    // Second leg: B initiates its own STEP_1 to *fetch* A's offline
    // updates. A returns STEP_2 with offline1.md + offline2.md.
    let b_step1 = b.frame_step1();
    let step2_for_b = a.receive(&b_step1).expect("A returns step2 for B");
    assert!(b.receive(&step2_for_b).is_none());

    assert_converged(&[&a, &b]);
}
