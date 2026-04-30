//! Robustness scenarios for the v1 manifest CRDT layer.
//!
//! These are pure portable tests (no I/O, no real client) that drive
//! the manifest API directly through ops/projection/sync to exercise
//! awkward concurrent edits, rename + delete races, conflict resolution
//! determinism, hidden-file path handling, and the parent-chain
//! pathologies (cycles, deep nesting) that the projection has to tame.

#![cfg(not(target_arch = "wasm32"))]

use syncline::v1::ids::{Lamport, Stamp};
use syncline::v1::projection::project;
use syncline::v1::{
    create_binary, create_text, delete_path, handle_manifest_payload, manifest_step1_payload,
    projection_hash, record_modify_text, rename, ActorId, Manifest, NodeKind,
};

/// Drive a full bidirectional sync between two manifests until they
/// converge (in practice two passes are enough for everything in this
/// file).
fn sync(a: &mut Manifest, b: &mut Manifest) {
    for _ in 0..2 {
        let s_a = manifest_step1_payload(a);
        let s_b = manifest_step1_payload(b);
        let r_for_a = handle_manifest_payload(b, &s_a).unwrap().unwrap();
        let r_for_b = handle_manifest_payload(a, &s_b).unwrap().unwrap();
        handle_manifest_payload(a, &r_for_a).unwrap();
        handle_manifest_payload(b, &r_for_b).unwrap();
    }
}

/// All peers must agree on the projection hash.
fn assert_converged(label: &str, peers: &[&Manifest]) {
    let head = projection_hash(peers[0]);
    for (i, p) in peers.iter().enumerate().skip(1) {
        assert_eq!(
            projection_hash(p),
            head,
            "{label}: peer #{i} diverged from peer #0",
        );
    }
}

/// Helper that asserts the projection paths on `a` and `b` are
/// identical (regardless of order).
fn assert_same_paths(label: &str, a: &Manifest, b: &Manifest) {
    let p_a = project(a);
    let p_b = project(b);
    let mut paths_a: Vec<_> = p_a.by_path.keys().cloned().collect();
    let mut paths_b: Vec<_> = p_b.by_path.keys().cloned().collect();
    paths_a.sort();
    paths_b.sort();
    assert_eq!(
        paths_a, paths_b,
        "{label}: peers diverged\n  A: {paths_a:?}\n  B: {paths_b:?}",
    );
}

// ===========================================================================
// Scenario 1: cycle introduced by concurrent moves
// ===========================================================================

#[test]
fn concurrent_moves_into_each_other_do_not_crash_or_diverge() {
    // Setup: both peers start synced with two empty top-level
    // directories D1 and D2 plus a file in each so we can see whether
    // children survive the cycle.
    let mut a = Manifest::new(ActorId::new());
    let d1 = a.create_node("D1", None, NodeKind::Directory, &[], 0);
    let d2 = a.create_node("D2", None, NodeKind::Directory, &[], 0);
    let _f1 = a.create_node("a.md", Some(d1), NodeKind::Text, &[], 0);
    let _f2 = a.create_node("b.md", Some(d2), NodeKind::Text, &[], 0);

    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Concurrent moves: A puts D1 inside D2, B puts D2 inside D1.
    a.set_parent(d1, Some(d2));
    b.set_parent(d2, Some(d1));

    // Sync — expected to converge despite the cycle.
    sync(&mut a, &mut b);
    assert_converged("cycle", &[&a, &b]);

    // The projection must terminate (this would loop without MAX_HOPS).
    let p_a = project(&a);
    let p_b = project(&b);

    // Whatever the resolution, both peers must agree on the live set.
    let mut paths_a: Vec<_> = p_a.by_path.keys().cloned().collect();
    let mut paths_b: Vec<_> = p_b.by_path.keys().cloned().collect();
    paths_a.sort();
    paths_b.sort();
    assert_eq!(paths_a, paths_b, "diverged paths under cycle");

    // The cycle must not surface a file that crosses the loop forever.
    // Either the file rows are absent (build_path returns None on
    // exceeding MAX_HOPS), or they appear under whichever parent the
    // LWW resolved. Either way, no path string is unbounded.
    for p in &paths_a {
        assert!(
            p.split('/').count() <= 1024,
            "path {p:?} exceeds MAX_HOPS depth — cycle leaked into projection",
        );
    }
}

// ===========================================================================
// Scenario 2: concurrent rename of the same node
// ===========================================================================

#[test]
fn concurrent_rename_of_same_node_converges_deterministically() {
    let mut a = Manifest::new(ActorId::new());
    let id = create_text(&mut a, "f.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Concurrent rename to different names — both stamp mod_lamp.
    a.set_name(id, "from-a.md");
    b.set_name(id, "from-b.md");

    sync(&mut a, &mut b);
    assert_converged("rename-race", &[&a, &b]);
    assert_same_paths("rename-race", &a, &b);

    // Exactly one of the two names wins; the other is gone.
    let p = project(&a);
    let names: Vec<_> = p.by_path.keys().cloned().collect();
    assert_eq!(names.len(), 1, "rename collision must collapse to 1 entry");
    assert!(
        names[0] == "from-a.md" || names[0] == "from-b.md",
        "winner must be one of the candidates, got {:?}",
        names[0]
    );
}

// ===========================================================================
// Scenario 3: concurrent rename + delete on same node
// ===========================================================================

#[test]
fn concurrent_rename_and_delete_converges() {
    // A renames f.md → g.md while B deletes f.md (same NodeId). After
    // sync, the projection on both peers must agree; either the rename's
    // mod_lamp beats the delete's del_lamp (file lives under new name)
    // or the reverse (file is gone). In either case, both peers must
    // see the same outcome.
    let mut a = Manifest::new(ActorId::new());
    let _id = create_text(&mut a, "f.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    rename(&mut a, "f.md", "g.md").unwrap();
    delete_path(&mut b, "f.md").unwrap();

    sync(&mut a, &mut b);
    assert_converged("rename-vs-delete", &[&a, &b]);
    assert_same_paths("rename-vs-delete", &a, &b);
}

#[test]
fn rename_after_observed_delete_resurrects() {
    // The intent here matches DESIGN_DOC_V1.md §6.3: a peer that
    // already saw the delete and *then* explicitly renames the dead
    // node must resurrect it (modify-wins-over-delete) on every peer.
    let mut a = Manifest::new(ActorId::new());
    let id = create_text(&mut a, "f.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // A deletes; sync; B observes; B renames the (tombstoned) NodeId.
    delete_path(&mut a, "f.md").unwrap();
    sync(&mut a, &mut b);
    assert!(b.get_entry(id).unwrap().deleted);

    // B already holds the NodeId from the editor's perspective.
    assert!(b.set_name(id, "renamed.md"));

    sync(&mut a, &mut b);
    assert_converged("modify-after-observed-delete", &[&a, &b]);

    // The resurrected entry shows up at the new path on both peers.
    for (label, m) in [("a", &a), ("b", &b)] {
        let p = project(m);
        assert!(
            p.by_path.contains_key("renamed.md"),
            "{label} should have resurrected as renamed.md, paths = {:?}",
            p.by_path.keys().collect::<Vec<_>>()
        );
    }
}

// ===========================================================================
// Scenario 4: rename target collides with concurrent create
// ===========================================================================

#[test]
fn rename_target_collides_with_concurrent_create() {
    let mut a = Manifest::new(ActorId::new());
    let _src = create_text(&mut a, "X.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // A: rename X.md → Y.md
    rename(&mut a, "X.md", "Y.md").unwrap();
    // B (independently): create a fresh Y.md.
    let _b_y = create_text(&mut b, "Y.md", 0).unwrap();

    sync(&mut a, &mut b);
    assert_converged("rename-vs-create", &[&a, &b]);
    assert_same_paths("rename-vs-create", &a, &b);

    // Two distinct nodes both want path Y.md → projection must
    // surface both (one wins the canonical name, the other gets a
    // conflict suffix). On both peers, the same naming pair appears.
    let p = project(&a);
    assert_eq!(p.len(), 2, "both renamed and created entry must surface");
    let mut paths: Vec<_> = p.by_path.keys().cloned().collect();
    paths.sort();
    assert!(paths.contains(&"Y.md".to_string()));
    assert!(paths.iter().any(|p| p.contains(".conflict-")));
}

// ===========================================================================
// Scenario 5: file created inside a directory that was concurrently deleted
// ===========================================================================

#[test]
fn child_created_under_concurrently_deleted_directory_is_orphaned() {
    // A creates a directory plus a child. Sync. Then concurrently:
    // A deletes the directory; B creates another child file in that
    // directory. After sync, the live status of the deleted directory
    // and the orphan child must agree on both peers, and the child
    // must not project to a path under the dead directory (cascade
    // safety net in `build_path`).
    let mut a = Manifest::new(ActorId::new());
    let dir = a.create_node("docs", None, NodeKind::Directory, &[], 0);
    let _existing = a.create_node("a.md", Some(dir), NodeKind::Text, &[], 0);

    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // A deletes the dir; B creates a new child while still seeing it.
    a.delete(dir);
    let _new_child = b.create_node("b.md", Some(dir), NodeKind::Text, &[], 0);

    sync(&mut a, &mut b);
    assert_converged("dir-delete-vs-create-child", &[&a, &b]);
    assert_same_paths("dir-delete-vs-create-child", &a, &b);

    // No path under "docs/" can survive on either peer; both children
    // are orphaned by the cascade-safety net (§5.6 / projection).
    let p = project(&a);
    for path in p.by_path.keys() {
        assert!(
            !path.starts_with("docs/"),
            "child {path:?} survived under tombstoned directory",
        );
    }
}

// ===========================================================================
// Scenario 6: hidden files & dotfile dirs project + sync correctly
// ===========================================================================

#[test]
fn dotfiles_and_dotdirs_project_correctly() {
    let mut a = Manifest::new(ActorId::new());

    // Pure dotfile at root, no extension.
    create_text(&mut a, ".gitignore", 0).unwrap();
    // Dotfile *with* an extension.
    create_text(&mut a, ".env.local", 0).unwrap();
    // Hidden directory holding a regular file.
    create_text(&mut a, ".config/secret.md", 0).unwrap();
    // Hidden directory + nested hidden file.
    create_text(&mut a, ".obsidian/.workspace.json", 0).unwrap();

    let mut b = Manifest::new(ActorId::new());
    sync(&mut a, &mut b);
    assert_converged("dotfiles", &[&a, &b]);

    let p = project(&a);
    for path in [
        ".gitignore",
        ".env.local",
        ".config/secret.md",
        ".obsidian/.workspace.json",
    ] {
        assert!(
            p.by_path.contains_key(path),
            "missing dotfile path {path}, got {:?}",
            p.by_path.keys().collect::<Vec<_>>()
        );
    }
}

#[test]
fn hidden_file_collisions_use_proper_extension_split() {
    // Two peers create the same dotfile-with-extension; conflict
    // suffix must keep `.local` as the extension and prepend the
    // suffix to the stem `.env`, not split between `.` and `env`.
    let mut a = Manifest::new(ActorId::new());
    let _ = create_text(&mut a, ".env.local", 0).unwrap();
    let mut b = Manifest::new(ActorId::new());
    let _ = create_text(&mut b, ".env.local", 0).unwrap();

    sync(&mut a, &mut b);
    assert_converged("dotfile-conflict", &[&a, &b]);

    let p = project(&a);
    let mut paths: Vec<_> = p.by_path.keys().cloned().collect();
    paths.sort();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&".env.local".to_string()));
    let suffixed = paths
        .iter()
        .find(|p| p.contains(".conflict-"))
        .expect("expected one conflict-suffixed sibling");
    assert!(
        suffixed.ends_with(".local"),
        "conflict path must keep .local extension, got {suffixed}",
    );
    assert!(
        suffixed.starts_with(".env."),
        "conflict path must keep .env stem, got {suffixed}",
    );
}

#[test]
fn pure_dotfile_with_no_extension_collision() {
    // ".gitignore" has no extension — split_ext should return (path, None).
    // Conflict copy must therefore have no trailing `.<ext>` segment.
    let mut a = Manifest::new(ActorId::new());
    let _ = create_text(&mut a, ".gitignore", 0).unwrap();
    let mut b = Manifest::new(ActorId::new());
    let _ = create_text(&mut b, ".gitignore", 0).unwrap();

    sync(&mut a, &mut b);
    assert_converged("dotfile-noext-conflict", &[&a, &b]);

    let p = project(&a);
    let mut paths: Vec<_> = p.by_path.keys().cloned().collect();
    paths.sort();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&".gitignore".to_string()));
    let suffixed = paths
        .iter()
        .find(|p| p.contains(".conflict-"))
        .expect("expected one conflict-suffixed sibling");
    // No trailing extension segment past the conflict marker.
    assert!(
        !suffixed.ends_with(".gitignore"),
        "conflict path must not duplicate full name, got {suffixed}",
    );
    assert!(
        suffixed.starts_with(".gitignore.conflict-"),
        "conflict path must extend the dotfile name, got {suffixed}",
    );
}

// ===========================================================================
// Scenario 7: directory rename reflects on every descendant
// ===========================================================================

#[test]
fn directory_rename_propagates_to_all_descendants_on_both_peers() {
    let mut a = Manifest::new(ActorId::new());
    let dir = a.create_node("docs", None, NodeKind::Directory, &[], 0);
    let _f1 = a.create_node("a.md", Some(dir), NodeKind::Text, &[], 0);
    let _f2 = a.create_node("b.md", Some(dir), NodeKind::Text, &[], 0);
    let sub = a.create_node("sub", Some(dir), NodeKind::Directory, &[], 0);
    let _f3 = a.create_node("deep.md", Some(sub), NodeKind::Text, &[], 0);

    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Rename the top directory: just updating its `name` field.
    a.set_name(dir, "books");
    sync(&mut a, &mut b);

    assert_converged("dir-rename", &[&a, &b]);
    let p_a = project(&a);
    for path in ["books/a.md", "books/b.md", "books/sub/deep.md"] {
        assert!(
            p_a.by_path.contains_key(path),
            "missing renamed path {path}, got {:?}",
            p_a.by_path.keys().collect::<Vec<_>>()
        );
    }
    for path in ["docs/a.md", "docs/b.md", "docs/sub/deep.md"] {
        assert!(
            !p_a.by_path.contains_key(path),
            "stale path {path} survived directory rename",
        );
    }
}

// ===========================================================================
// Scenario 8: three-peer mesh convergence under combined ops
// ===========================================================================

fn full_mesh_sync(peers: &mut [Manifest], passes: usize) {
    for _ in 0..passes {
        let n = peers.len();
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                // i sends step1; j replies; i applies.
                let step1 = manifest_step1_payload(&peers[i]);
                let resp = handle_manifest_payload(&mut peers[j], &step1)
                    .unwrap()
                    .unwrap();
                handle_manifest_payload(&mut peers[i], &resp).unwrap();
            }
        }
    }
}

#[test]
fn three_peers_rename_modify_delete_converge() {
    let mut a = Manifest::new(ActorId::new());
    let id = create_text(&mut a, "shared.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();
    let mut c = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Concurrently: A renames, B modifies, C deletes.
    a.set_name(id, "renamed.md");
    b.record_modify(id);
    c.delete(id);

    let mut peers = vec![a, b, c];
    full_mesh_sync(&mut peers, 2);

    let head = projection_hash(&peers[0]);
    for (i, p) in peers.iter().enumerate().skip(1) {
        assert_eq!(
            projection_hash(p),
            head,
            "three-peer convergence: peer #{i} diverged from #0",
        );
    }
}

// ===========================================================================
// Scenario 9: binary file same-path collision
// ===========================================================================

#[test]
fn binary_file_same_path_collision_is_deterministic() {
    let mut a = Manifest::new(ActorId::new());
    let _ = create_binary(&mut a, "img.png", &["aaaaaaaa".to_string()], 100).unwrap();
    let mut b = Manifest::new(ActorId::new());
    let _ = create_binary(&mut b, "img.png", &["bbbbbbbb".to_string()], 100).unwrap();

    sync(&mut a, &mut b);
    assert_converged("binary-collision", &[&a, &b]);
    assert_same_paths("binary-collision", &a, &b);

    let p = project(&a);
    assert_eq!(p.len(), 2, "both binary entries must survive");
    let canonical = p.by_path.get("img.png").expect("canonical path remains");
    assert_eq!(canonical.kind, NodeKind::Binary);
    let conflicts: Vec<_> = p
        .by_path
        .iter()
        .filter(|(k, _)| k.contains(".conflict-"))
        .collect();
    assert_eq!(conflicts.len(), 1, "exactly one conflict-suffixed sibling");
    assert!(conflicts[0].0.ends_with(".png"));
}

// ===========================================================================
// Scenario 10: delete then create same name produces a new NodeId
// ===========================================================================

#[test]
fn delete_then_create_same_name_is_a_new_node() {
    let mut a = Manifest::new(ActorId::new());
    let id1 = create_text(&mut a, "f.md", 0).unwrap();
    delete_path(&mut a, "f.md").unwrap();
    let id2 = create_text(&mut a, "f.md", 0).unwrap();

    assert_ne!(id1, id2, "fresh create after delete must mint a new NodeId");

    let p = project(&a);
    let row = p.by_path.get("f.md").expect("file is live");
    assert_eq!(row.id, id2, "live entry must be the fresh node");
}

// ===========================================================================
// Scenario 11: ordering invariance — applying updates in different orders
// must yield byte-equal projections.
// ===========================================================================

#[test]
fn merge_order_does_not_change_projection_hash() {
    // Three independent peers each do a concurrent op; we then merge in
    // two different orders and assert both arrive at the same hash.
    let make_seed = || {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "shared.md", 0).unwrap();
        m
    };

    let seed = make_seed();

    // Build A, B, C from the same starting state.
    let mut a = Manifest::from_update(
        ActorId::new(),
        seed.lamport(),
        &seed.encode_state_as_update(),
    )
    .unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        seed.lamport(),
        &seed.encode_state_as_update(),
    )
    .unwrap();
    let mut c = Manifest::from_update(
        ActorId::new(),
        seed.lamport(),
        &seed.encode_state_as_update(),
    )
    .unwrap();

    // Each does a different op concurrently.
    create_text(&mut a, "from_a.md", 1).unwrap();
    create_text(&mut b, "from_b.md", 2).unwrap();
    create_text(&mut c, "from_c.md", 3).unwrap();

    // Path 1: merge order A → B → C
    let mut order1 = Manifest::new(ActorId::new());
    order1.apply_update(&a.encode_state_as_update()).unwrap();
    order1.apply_update(&b.encode_state_as_update()).unwrap();
    order1.apply_update(&c.encode_state_as_update()).unwrap();

    // Path 2: merge order C → A → B
    let mut order2 = Manifest::new(ActorId::new());
    order2.apply_update(&c.encode_state_as_update()).unwrap();
    order2.apply_update(&a.encode_state_as_update()).unwrap();
    order2.apply_update(&b.encode_state_as_update()).unwrap();

    // Path 3: merge order B → C → A
    let mut order3 = Manifest::new(ActorId::new());
    order3.apply_update(&b.encode_state_as_update()).unwrap();
    order3.apply_update(&c.encode_state_as_update()).unwrap();
    order3.apply_update(&a.encode_state_as_update()).unwrap();

    let h1 = projection_hash(&order1);
    let h2 = projection_hash(&order2);
    let h3 = projection_hash(&order3);
    assert_eq!(h1, h2, "merge-order ABC vs CAB must match");
    assert_eq!(h1, h3, "merge-order ABC vs BCA must match");
}

// ===========================================================================
// Scenario 12: a deeply nested but non-cyclic path syncs cleanly
// ===========================================================================

#[test]
fn deeply_nested_path_with_many_levels_syncs() {
    let mut deep = String::from("a");
    for i in 1..50 {
        deep.push('/');
        deep.push_str(&format!("d{i}"));
    }
    deep.push_str("/leaf.md");

    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, &deep, 0).unwrap();
    let mut b = Manifest::new(ActorId::new());
    sync(&mut a, &mut b);
    assert_converged("deep-nesting", &[&a, &b]);

    let p = project(&b);
    assert!(
        p.by_path.contains_key(&deep),
        "deep path {deep} not projected on the receiving peer",
    );
}

// ===========================================================================
// Scenario 13: lamport advance after observing a far-future remote
// ===========================================================================

// ===========================================================================
// Scenario 14: stamp tiebreak — same lamport, actor cmp decides
// ===========================================================================

#[test]
fn stamp_tiebreak_picks_higher_actor_on_lamport_equality() {
    // Synthesise the lo / hi actor ids deterministically.
    let (lo_actor, hi_actor) = {
        let a1 = ActorId::new();
        let a2 = ActorId::new();
        if a1 < a2 { (a1, a2) } else { (a2, a1) }
    };
    let s_lo = Stamp::new(Lamport(7), lo_actor);
    let s_hi = Stamp::new(Lamport(7), hi_actor);
    assert!(s_hi.beats(&s_lo));
    assert!(!s_lo.beats(&s_hi));

    // Stamp ordering is symmetric & total under PartialOrd / Ord.
    assert!(s_hi > s_lo);
    let mut v = vec![s_hi, s_lo];
    v.sort();
    assert_eq!(v, vec![s_lo, s_hi]);
}

// ===========================================================================
// Scenario 15: find_entry_by_path returns the live entry even with a
// stale tombstone parked at the same path
// ===========================================================================

#[test]
fn find_entry_by_path_returns_live_when_tombstones_exist() {
    let mut m = Manifest::new(ActorId::new());
    let dead1 = m.create_node("twin.md", None, NodeKind::Text, &[], 0);
    m.delete(dead1);
    let dead2 = m.create_node("twin.md", None, NodeKind::Text, &[], 0);
    m.delete(dead2);
    // Now create a live entry with the same name. find_entry_by_path
    // must return *this one* regardless of HashMap iteration order.
    let live = m.create_node("twin.md", None, NodeKind::Text, &[], 0);

    let found = m.find_entry_by_path("twin.md").unwrap();
    assert_eq!(found.id, live, "must prefer live over tombstoned");
    assert!(!found.deleted);
}

// ===========================================================================
// Scenario 16: deeply nested rename composition
// ===========================================================================

#[test]
fn rename_chain_eventually_lands_at_target() {
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, "f.md", 0).unwrap();
    rename(&mut a, "f.md", "g.md").unwrap();
    rename(&mut a, "g.md", "h.md").unwrap();
    rename(&mut a, "h.md", "i.md").unwrap();
    rename(&mut a, "i.md", "f.md").unwrap();

    let p = project(&a);
    assert!(p.by_path.contains_key("f.md"));
    assert_eq!(p.len(), 1, "exactly one live entry");
}

// ===========================================================================
// Scenario 17: parent-of-self loop is dropped from projection
// ===========================================================================

#[test]
fn self_parent_loop_is_dropped_from_projection() {
    let mut m = Manifest::new(ActorId::new());
    let dir = m.create_node("loop", None, NodeKind::Directory, &[], 0);
    let file = m.create_node("under.md", Some(dir), NodeKind::Text, &[], 0);
    // Make the directory its own parent — pathological but the
    // projection must terminate and drop the orphans.
    m.set_parent(dir, Some(dir));

    let p = project(&m);
    assert!(p.get_by_id(file).is_none(), "file under self-loop must not project");
    assert!(p.is_empty(), "projection should be empty");
}

// ===========================================================================
// Scenario 18: rename to currently-tombstoned path succeeds
// ===========================================================================

#[test]
fn rename_to_path_held_only_by_tombstone_succeeds() {
    let mut a = Manifest::new(ActorId::new());
    let _id = create_text(&mut a, "f.md", 0).unwrap();
    delete_path(&mut a, "f.md").unwrap();
    let _g = create_text(&mut a, "g.md", 0).unwrap();
    // f.md is tombstoned (not in projection.by_path), so rename should succeed.
    rename(&mut a, "g.md", "f.md").unwrap();

    let p = project(&a);
    assert!(p.by_path.contains_key("f.md"));
    assert!(!p.by_path.contains_key("g.md"));
}

// ===========================================================================
// Scenario 19: rename to currently-occupied path fails
// ===========================================================================

#[test]
fn rename_to_occupied_path_fails_clean() {
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, "a.md", 0).unwrap();
    create_text(&mut a, "b.md", 0).unwrap();
    let result = rename(&mut a, "a.md", "b.md");
    assert!(result.is_err(), "should refuse to overwrite live entry");

    let p = project(&a);
    assert!(p.by_path.contains_key("a.md"));
    assert!(p.by_path.contains_key("b.md"));
}

// ===========================================================================
// Scenario 20: concurrent delete vs modify converges on both peers
// ===========================================================================

#[test]
fn concurrent_delete_and_modify_converges_on_both_peers() {
    let mut a = Manifest::new(ActorId::new());
    let _id = create_text(&mut a, "f.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Concurrent: A deletes, B modifies. Both stamp at the same
    // lamport (each just ticked from the same baseline).
    delete_path(&mut a, "f.md").unwrap();
    record_modify_text(&mut b, "f.md").unwrap();

    sync(&mut a, &mut b);
    assert_converged("delete-vs-modify", &[&a, &b]);
    assert_same_paths("delete-vs-modify", &a, &b);
}

// ===========================================================================
// Scenario 21: 4-peer same-path collision converges to 4 entries
// ===========================================================================

#[test]
fn four_peer_same_path_collision_yields_four_distinct_paths() {
    let mut peers = Vec::new();
    for _ in 0..4 {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "popular.md", 0).unwrap();
        peers.push(m);
    }

    full_mesh_sync(&mut peers, 3);

    let head = projection_hash(&peers[0]);
    for (i, m) in peers.iter().enumerate().skip(1) {
        assert_eq!(projection_hash(m), head, "peer #{i} diverged");
    }
    let p = project(&peers[0]);
    assert_eq!(p.len(), 4, "all four creators surface as separate entries");
    let canonical_count = p
        .by_path
        .values()
        .filter(|e| !e.is_conflict_copy)
        .count();
    let conflict_count = p
        .by_path
        .values()
        .filter(|e| e.is_conflict_copy)
        .count();
    assert_eq!(canonical_count, 1);
    assert_eq!(conflict_count, 3);
}

// ===========================================================================
// Scenario 22: rename ping-pong between two peers
// ===========================================================================

#[test]
fn rename_ping_pong_eventually_converges() {
    let mut a = Manifest::new(ActorId::new());
    let id = create_text(&mut a, "f.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    for round in 0..5 {
        let a_name = format!("a{round}.md");
        let b_name = format!("b{round}.md");
        a.set_name(id, &a_name);
        b.set_name(id, &b_name);
        sync(&mut a, &mut b);
        assert_converged(&format!("ping-pong-r{round}"), &[&a, &b]);
        // The shared file always projects to exactly one path on both peers.
        let p_a = project(&a);
        let p_b = project(&b);
        assert_eq!(p_a.len(), 1);
        assert_eq!(p_b.len(), 1);
        assert_eq!(
            p_a.by_id.get(&id).map(|e| &e.path),
            p_b.by_id.get(&id).map(|e| &e.path),
            "round {round}: peer-projected paths diverged",
        );
    }
}

// ===========================================================================
// Scenario 23: deterministic random fuzz of two-peer ops
// ===========================================================================

/// Tiny PRNG so the test is reproducible without pulling in `rand`.
struct Xs(u64);
impl Xs {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_op(rng: &mut Xs, m: &mut Manifest, names: &[String]) {
    // Pick a random op weighted toward creates/modifies. Skip invalid
    // ones (e.g., rename on empty manifest); we don't care about every
    // step succeeding, only that the manifest stays internally
    // consistent.
    let live: Vec<_> = m.live_entries().into_iter().filter(|e| e.kind == NodeKind::Text).collect();
    let op = rng.range(5);
    match op {
        0 | 1 => {
            // Create at a random name from the pool.
            let name = &names[(rng.range(names.len() as u64)) as usize];
            let _ = create_text(m, name, 0);
        }
        2 => {
            // Modify a random live entry.
            if let Some(e) = live.first() {
                let _ = m.record_modify(e.id);
            }
        }
        3 => {
            // Rename a random live entry.
            if let Some(e) = live.first() {
                let new = &names[(rng.range(names.len() as u64)) as usize];
                let cur_path = {
                    let p = project(m);
                    p.by_id.get(&e.id).map(|r| r.path.clone())
                };
                if let Some(cur) = cur_path {
                    let _ = rename(m, &cur, new);
                }
            }
        }
        _ => {
            // Delete a random live entry.
            if let Some(e) = live.first() {
                let cur_path = {
                    let p = project(m);
                    p.by_id.get(&e.id).map(|r| r.path.clone())
                };
                if let Some(cur) = cur_path {
                    let _ = delete_path(m, &cur);
                }
            }
        }
    }
}

#[test]
fn random_two_peer_fuzz_converges_for_many_seeds() {
    for seed in [42u64, 7, 0xDEADBEEF, 9999, 314159] {
        let mut rng_a = Xs(seed.wrapping_mul(0xA1));
        let mut rng_b = Xs(seed.wrapping_mul(0xB2));
        let names: Vec<String> = (0..8).map(|i| format!("file_{i}.md")).collect();
        let names2 = names.clone();

        // Synced starting point.
        let mut a = Manifest::new(ActorId::new());
        let mut b = Manifest::from_update(
            ActorId::new(),
            a.lamport(),
            &a.encode_state_as_update(),
        )
        .unwrap();

        for _ in 0..50 {
            random_op(&mut rng_a, &mut a, &names);
            random_op(&mut rng_b, &mut b, &names2);
        }
        sync(&mut a, &mut b);
        assert_converged(&format!("fuzz-{seed}"), &[&a, &b]);
        // Run another round and re-verify so we exercise post-sync state.
        for _ in 0..30 {
            random_op(&mut rng_a, &mut a, &names);
            random_op(&mut rng_b, &mut b, &names2);
        }
        sync(&mut a, &mut b);
        assert_converged(&format!("fuzz-{seed} round2"), &[&a, &b]);
    }
}

#[test]
fn random_n_peer_fuzz_converges() {
    // Stress: 4 peers each apply 30 random ops, full-mesh sync, verify
    // convergence. Repeat for several seeds.
    for seed in [101u64, 202, 303, 404] {
        let names: Vec<String> = (0..6).map(|i| format!("note_{i}.md")).collect();
        let mut peers: Vec<Manifest> = (0..4).map(|_| Manifest::new(ActorId::new())).collect();
        let mut rngs: Vec<Xs> = (0..peers.len())
            .map(|i| Xs(seed.wrapping_add(i as u64).wrapping_mul(0xC3)))
            .collect();

        for _ in 0..30 {
            for i in 0..peers.len() {
                random_op(&mut rngs[i], &mut peers[i], &names);
            }
        }

        full_mesh_sync(&mut peers, 4);
        let head = projection_hash(&peers[0]);
        for (i, m) in peers.iter().enumerate().skip(1) {
            assert_eq!(
                projection_hash(m),
                head,
                "n-peer fuzz seed={seed}: peer #{i} diverged",
            );
        }
    }
}

// ===========================================================================
// Scenario: conflict suffixes are pairwise distinct under heavy collision
// ===========================================================================

#[test]
fn conflict_suffixes_are_pairwise_distinct() {
    // 8 peers all create the same path. After full mesh sync, the 8
    // resulting projection paths must all be distinct — no two
    // entries can collide on the disk.
    let mut peers: Vec<Manifest> = (0..8).map(|_| Manifest::new(ActorId::new())).collect();
    for m in peers.iter_mut() {
        create_text(m, "popular.md", 0).unwrap();
    }

    full_mesh_sync(&mut peers, 4);

    let p = project(&peers[0]);
    let paths: Vec<String> = p.by_path.keys().cloned().collect();
    assert_eq!(paths.len(), 8, "all 8 entries must surface");
    let unique: std::collections::HashSet<_> = paths.iter().collect();
    assert_eq!(unique.len(), 8, "conflict suffixes must be pairwise distinct");

    // And every other peer agrees with peer 0's projection.
    for (i, m) in peers.iter().enumerate().skip(1) {
        assert_eq!(
            projection_hash(m),
            projection_hash(&peers[0]),
            "peer #{i} projection diverged from peer #0",
        );
    }
}

// ===========================================================================
// Scenario: directory resurrection should keep its children visible
// ===========================================================================

#[test]
fn directory_resurrected_via_modify_wins_keeps_children_visible() {
    // A directory was deleted, but a subsequent record_modify on it
    // beats the delete (modify-wins-over-delete §6.3 also applies to
    // directories). Children of that directory must therefore project,
    // not be silently orphaned by build_path's cascade-safety check.
    let mut m = Manifest::new(ActorId::new());
    let dir = m.create_node("docs", None, NodeKind::Directory, &[], 0);
    let file = m.create_node("note.md", Some(dir), NodeKind::Text, &[], 0);
    // Tombstone the directory (deletes don't cascade automatically;
    // children stay alive but lose their parent-chain).
    m.delete(dir);
    // Resurrect via a modify that ticks past the delete's lamport.
    assert!(m.record_modify(dir));

    let p = project(&m);
    assert!(
        p.get_by_id(file).is_some(),
        "child of resurrected directory must project; current paths = {:?}",
        p.by_path.keys().collect::<Vec<_>>()
    );
    assert_eq!(p.get_by_id(file).unwrap().path, "docs/note.md");
}

// ===========================================================================
// Scenario: record_modify_binary refuses to mutate Text entries
// ===========================================================================

#[test]
fn record_modify_binary_refuses_text_entries() {
    use syncline::v1::record_modify_binary;
    let mut m = Manifest::new(ActorId::new());
    create_text(&mut m, "note.md", 5).unwrap();

    // record_modify_binary should refuse when the entry kind is Text,
    // not silently overwrite size and stamp chunk hashes onto it.
    let result = record_modify_binary(
        &mut m,
        "note.md",
        &["abcdef0123456789".to_string()],
        100,
    );
    assert!(
        result.is_err(),
        "record_modify_binary on a Text entry must error",
    );

    // Sanity: the original Text entry survived intact.
    let p = project(&m);
    let row = p.by_path.get("note.md").unwrap();
    assert_eq!(row.kind, NodeKind::Text);
    assert_eq!(row.size, 5, "size must not be overwritten by failed binary modify");
    assert!(row.chunk_hashes.is_empty(), "chunks must not be set on Text entry");
}

#[test]
fn record_modify_text_refuses_binary_entries() {
    use syncline::v1::create_binary as v1_create_binary;
    let mut m = Manifest::new(ActorId::new());
    v1_create_binary(&mut m, "img.png", &["abcdef".to_string()], 10).unwrap();

    let result = record_modify_text(&mut m, "img.png");
    assert!(
        result.is_err(),
        "record_modify_text on a Binary entry must error",
    );
}

// ===========================================================================
// Scenario: every-op-then-sync convergence ladder
// ===========================================================================

#[test]
fn every_op_followed_by_sync_keeps_peers_aligned() {
    // Apply ops one-at-a-time on alternating peers, syncing after
    // every step. Convergence must hold at every step, not just the
    // end. Catches subtle "intermittently divergent" bugs.
    let mut a = Manifest::new(ActorId::new());
    let mut b = Manifest::new(ActorId::new());

    let ops: Vec<Box<dyn Fn(&mut Manifest)>> = vec![
        Box::new(|m| {
            let _ = create_text(m, "f.md", 0);
        }),
        Box::new(|m| {
            let _ = record_modify_text(m, "f.md");
        }),
        Box::new(|m| {
            let _ = rename(m, "f.md", "g.md");
        }),
        Box::new(|m| {
            let _ = create_text(m, "h.md", 5);
        }),
        Box::new(|m| {
            let _ = delete_path(m, "h.md");
        }),
        Box::new(|m| {
            let _ = rename(m, "g.md", "h.md");
        }),
    ];

    for (i, op) in ops.iter().enumerate() {
        if i % 2 == 0 {
            op(&mut a);
        } else {
            op(&mut b);
        }
        sync(&mut a, &mut b);
        assert_converged(&format!("ladder step {i}"), &[&a, &b]);
    }
}

// ===========================================================================
// Scenario 24: stamp + projection determinism through encode/apply
// ===========================================================================

#[test]
fn projection_hash_matches_after_full_state_roundtrip() {
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, "one.md", 1).unwrap();
    create_text(&mut a, "two/three.md", 2).unwrap();
    delete_path(&mut a, "one.md").unwrap();
    create_text(&mut a, "phoenix.md", 3).unwrap();
    record_modify_text(&mut a, "phoenix.md").unwrap();
    rename(&mut a, "phoenix.md", "renamed.md").unwrap();

    let h_a = projection_hash(&a);

    // Roundtrip through encode_state_as_update.
    let bytes = a.encode_state_as_update();
    let b = Manifest::from_update(ActorId::new(), Lamport::ZERO, &bytes).unwrap();
    let h_b = projection_hash(&b);
    assert_eq!(h_a, h_b, "projection_hash must be stable across encode/apply roundtrip");

    // Same projection paths.
    let p_a = project(&a);
    let p_b = project(&b);
    let mut paths_a: Vec<_> = p_a.by_path.keys().cloned().collect();
    let mut paths_b: Vec<_> = p_b.by_path.keys().cloned().collect();
    paths_a.sort();
    paths_b.sort();
    assert_eq!(paths_a, paths_b);
}

// ===========================================================================
// Scenario 26: UTF-8 / non-ASCII filenames sync deterministically
// ===========================================================================

#[test]
fn unicode_filenames_sync_correctly() {
    let mut a = Manifest::new(ActorId::new());
    let names = [
        "Привет.md",
        "안녕.md",
        "你好.md",
        "🚀rocket.md",
        "déjà-vu.md",
        "a/にほんご/note.md",
    ];
    for n in names {
        create_text(&mut a, n, 0).unwrap();
    }
    let mut b = Manifest::new(ActorId::new());
    sync(&mut a, &mut b);
    assert_converged("unicode-paths", &[&a, &b]);
    let p = project(&b);
    for n in names {
        assert!(
            p.by_path.contains_key(n),
            "unicode path missing on B: {n}, paths = {:?}",
            p.by_path.keys().collect::<Vec<_>>()
        );
    }
}

#[test]
fn unicode_filename_collisions_use_correct_extension_split() {
    // ".env" with cyrillic "локал" extension.
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, ".cfg.локал", 0).unwrap();
    let mut b = Manifest::new(ActorId::new());
    create_text(&mut b, ".cfg.локал", 0).unwrap();
    sync(&mut a, &mut b);
    assert_converged("unicode-conflict", &[&a, &b]);

    let p = project(&a);
    let conflict = p
        .by_path
        .keys()
        .find(|k| k.contains(".conflict-"))
        .expect("must have a conflict-suffixed sibling");
    assert!(
        conflict.ends_with(".локал"),
        "conflict path must keep cyrillic extension intact, got {conflict}",
    );
    assert!(
        conflict.starts_with(".cfg."),
        "conflict path must keep .cfg stem, got {conflict}",
    );
}

// ===========================================================================
// Scenario 27: state-vector roundtrip after many incremental updates
// ===========================================================================

#[test]
fn manifest_rehydrates_after_many_incremental_updates() {
    use yrs::{ReadTxn, Transact};

    let mut original = Manifest::new(ActorId::new());
    let mut snapshots = Vec::new();
    let mut prev_sv = original.doc().transact().state_vector();

    for i in 0..30 {
        create_text(&mut original, &format!("note_{i}.md"), i as u64).unwrap();
        if i % 3 == 0 && i > 0 {
            // Sometimes also delete a previous one.
            let _ = delete_path(&mut original, &format!("note_{}.md", i - 1));
        }
        let new_sv = original.doc().transact().state_vector();
        let delta = original
            .doc()
            .transact()
            .encode_state_as_update_v1(&prev_sv);
        snapshots.push(delta);
        prev_sv = new_sv;
    }

    // Rebuild fresh from incremental deltas, applied in reverse and
    // mid-shuffle order — yrs is order-independent for valid updates.
    let mut rebuilt = Manifest::new(ActorId::new());
    for s in snapshots.iter().rev() {
        rebuilt.apply_update(s).unwrap();
    }
    assert_eq!(
        projection_hash(&original),
        projection_hash(&rebuilt),
        "rehydration via incremental deltas (reversed order) must match",
    );

    // Same again, applying every-other-then-the-rest.
    let mut rebuilt2 = Manifest::new(ActorId::new());
    for s in snapshots.iter().step_by(2) {
        rebuilt2.apply_update(s).unwrap();
    }
    for s in snapshots.iter().skip(1).step_by(2) {
        rebuilt2.apply_update(s).unwrap();
    }
    assert_eq!(
        projection_hash(&original),
        projection_hash(&rebuilt2),
        "rehydration via interleaved deltas must match",
    );
}

// ===========================================================================
// Scenario 28: ops::rename rejects malformed input cleanly
// ===========================================================================

#[test]
fn rename_rejects_malformed_targets() {
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, "src.md", 0).unwrap();
    // Empty target leaf.
    assert!(rename(&mut a, "src.md", "").is_err());
    assert!(rename(&mut a, "src.md", "dir/").is_err());
    // Empty intermediate segment in target.
    assert!(rename(&mut a, "src.md", "a//b.md").is_err());
}

// ===========================================================================
// Scenario 29: broken parent reference is dropped from projection
// ===========================================================================

#[test]
fn entry_with_dangling_parent_does_not_crash_projection() {
    use syncline::v1::ids::NodeId;

    let mut m = Manifest::new(ActorId::new());
    let real_dir = m.create_node("real", None, NodeKind::Directory, &[], 0);
    let f = m.create_node("file.md", Some(real_dir), NodeKind::Text, &[], 0);
    // Re-parent the file to a NodeId that doesn't exist.
    let ghost = NodeId::new();
    m.set_parent(f, Some(ghost));

    let p = project(&m);
    assert!(
        p.get_by_id(f).is_none(),
        "dangling parent must drop the entry from projection",
    );
    // No live entries surface, so empty.
    assert!(p.is_empty());
}

// ===========================================================================
// Scenario 25: pathological filenames in conflict path
// ===========================================================================

/// Two-leading-dots filename: `..hidden` should produce a conflict
/// sibling that *prefixes* the original name, not one that mangles
/// `..hidden` into `.<conflict>.hidden`. The split_ext heuristic must
/// treat leading dots as part of the stem.
#[test]
fn double_dot_filename_conflict_keeps_full_basename() {
    let mut a = Manifest::new(ActorId::new());
    let _ = create_text(&mut a, "..hidden", 0).unwrap();
    let mut b = Manifest::new(ActorId::new());
    let _ = create_text(&mut b, "..hidden", 0).unwrap();

    sync(&mut a, &mut b);
    assert_converged("double-dot-conflict", &[&a, &b]);

    let p = project(&a);
    assert_eq!(p.len(), 2, "both nodes survive — one canonical, one conflict");
    assert!(
        p.by_path.contains_key("..hidden"),
        "canonical name preserved",
    );
    let conflict = p
        .by_path
        .keys()
        .find(|k| k.contains(".conflict-"))
        .expect("expected one conflict-suffixed sibling");
    assert!(
        conflict.starts_with("..hidden"),
        "conflict path must keep entire `..hidden` basename intact, got {conflict}",
    );
}

#[test]
fn lamport_advances_past_observed_remote() {
    let mut a = Manifest::new(ActorId::new());
    // Tick A's lamport up to a high value.
    for i in 0..50 {
        create_text(&mut a, &format!("x{i}.md"), 0).unwrap();
    }
    let a_lamp = a.lamport().get();
    assert!(a_lamp >= 50);

    let mut b = Manifest::new(ActorId::new());
    assert_eq!(b.lamport().get(), 0);
    b.apply_update(&a.encode_state_as_update()).unwrap();
    assert!(
        b.lamport().get() >= a_lamp,
        "B should have advanced past A's lamport, got {} vs {}",
        b.lamport().get(),
        a_lamp,
    );
}

// ===========================================================================
// auto-apr28-001: pathological filenames (trailing whitespace, reserved
// names, leading-space segments). The manifest should preserve every
// byte exactly and converge after a two-peer sync — these names are
// awkward on Windows / case-insensitive FS but legal on POSIX, and a
// Linux peer will absolutely see them.
// ===========================================================================
#[test]
fn auto_apr28_001_pathological_filenames_roundtrip_exactly() {
    let cases: &[&str] = &[
        "trailing space .md",
        "  leading spaces.md",
        "tab\there.md",
        "CON.md",       // Windows reserved
        "PRN.md",       // Windows reserved
        "aux.md",       // Windows reserved (lowercase variant)
        "NUL",          // Windows reserved, no extension
        "name.with.many.dots.md",
        "ends-in-dot.",
        "..",           // not legal as a file name on POSIX, but let's see what manifest does
    ];

    let mut a = Manifest::new(ActorId::new());
    let mut created = Vec::new();
    for (i, name) in cases.iter().enumerate() {
        match create_text(&mut a, name, i as u64) {
            Ok(id) => created.push((*name, Some(id))),
            Err(_) => created.push((*name, None)), // manifest may reject
        }
    }

    // Bootstrap B from A's state.
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    sync(&mut a, &mut b);
    assert_converged("auto-apr28-001-paths", &[&a, &b]);

    // Every accepted name must show up byte-identical in both peers'
    // projections.
    let pa = project(&a);
    let pb = project(&b);
    for (name, id_opt) in &created {
        if id_opt.is_none() {
            continue; // legitimately rejected by create_text
        }
        assert!(
            pa.by_path.contains_key(*name),
            "peer A missing accepted path {name:?}"
        );
        assert!(
            pb.by_path.contains_key(*name),
            "peer B missing accepted path {name:?}"
        );
    }

    // Sharper variant: two fresh peers concurrently create files with
    // a trailing-space name. After sync, both must be present with
    // distinct paths — one canonical, one conflict-suffixed — and both
    // peers must agree byte-for-byte on the conflict path.
    let mut p = Manifest::new(ActorId::new());
    let mut q = Manifest::new(ActorId::new());
    create_text(&mut p, "trailing space .md", 0).unwrap();
    create_text(&mut q, "trailing space .md", 0).unwrap();
    sync(&mut p, &mut q);
    assert_converged("auto-apr28-001-trailing-conflict", &[&p, &q]);
    let pp = project(&p);
    assert!(
        pp.by_path.contains_key("trailing space .md"),
        "canonical trailing-space name preserved",
    );
    assert_eq!(
        pp.len(),
        2,
        "both nodes survive the same-path collision"
    );
}

// ===========================================================================
// auto-apr28-002: out-of-order delta delivery — child update arrives
// before its parent directory. The child must remain orphaned (dropped
// from projection) while the parent is missing, then self-heal into the
// projection once the parent's update arrives. No crash, no loss.
// ===========================================================================
#[test]
fn auto_apr28_002_orphaned_child_self_heals_when_parent_arrives() {
    use yrs::{ReadTxn, Transact};

    // Peer A builds: root → "Folder/" → "child.md"
    // We capture two distinct deltas: one with only the directory
    // creation, one with only the child creation, and feed them to a
    // fresh peer B in REVERSE ORDER.
    let mut a = Manifest::new(ActorId::new());

    // Snapshot 0: empty.
    let sv0 = a.doc().transact().state_vector();
    // Op 1: create the directory.
    use syncline::v1::ids::NodeId;
    let dir: NodeId = a.create_node("Folder", None, NodeKind::Directory, &[], 0);
    let sv1 = a.doc().transact().state_vector();
    let dir_delta = a
        .doc()
        .transact()
        .encode_state_as_update_v1(&sv0);
    // Op 2: create the child under the directory.
    let _child = a.create_node("child.md", Some(dir), NodeKind::Text, &[], 7);
    let child_delta = a
        .doc()
        .transact()
        .encode_state_as_update_v1(&sv1);

    // Sanity: peer A's child projects under the directory.
    // (Directories don't have their own row in `by_path` — see
    // projection.rs §5.6 — but the file does.)
    let pa = project(&a);
    assert!(pa.by_path.contains_key("Folder/child.md"));

    // Peer B: apply ONLY the child delta first.
    let mut b = Manifest::new(ActorId::new());
    b.apply_update(&child_delta).unwrap();
    let pb_partial = project(&b);
    assert!(
        !pb_partial.by_path.contains_key("Folder/child.md"),
        "child must not appear before parent has been received"
    );
    // Either the child shows nowhere, or under just its leaf name (if
    // projection mistakenly treats missing parent as root). The
    // dangling-parent guarantee says "dropped from projection".
    assert!(
        !pb_partial.by_path.contains_key("child.md"),
        "child must not be silently re-rooted at vault root when its \
         parent is missing — got pb_partial: {:?}",
        pb_partial.by_path.keys().collect::<Vec<_>>()
    );

    // Now apply the parent's delta. The child must appear at its
    // intended nested path. (Directory itself isn't projected.)
    b.apply_update(&dir_delta).unwrap();
    let pb_full = project(&b);
    assert!(
        pb_full.by_path.contains_key("Folder/child.md"),
        "child must self-heal into projection after parent arrives — \
         got pb_full: {:?}",
        pb_full.by_path.keys().collect::<Vec<_>>()
    );
    assert_eq!(projection_hash(&a), projection_hash(&b));
}

// ===========================================================================
// auto-apr28-004: three-way op divergence on the same node — A renames,
// B deletes, C modifies content. Modify-wins-over-delete (§6.3) means
// the entry survives. The rename and modify both produce non-trivial
// stamps; the projection must agree across all three peers, no peer
// can see a different name or a different live/dead state.
// ===========================================================================
#[test]
fn auto_apr28_004_three_way_rename_delete_modify_converges() {
    let mut a = Manifest::new(ActorId::new());
    let id = create_text(&mut a, "note.md", 0).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();
    let mut c = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Three concurrent ops, all stamped before any peer observes the
    // others.
    rename(&mut a, "note.md", "renamed.md").unwrap();
    delete_path(&mut b, "note.md").unwrap();
    record_modify_text(&mut c, "note.md").unwrap();

    // Full mesh sync — convergence requires several passes for ops
    // that interact across LWW dimensions.
    let mut peers = [a, b, c];
    full_mesh_sync(&mut peers, 4);
    let [a, b, c] = peers;
    assert_converged("auto-apr28-004", &[&a, &b, &c]);

    // The KEY robustness property here is that all three peers reach
    // **identical projections** — whichever way LWW resolves, every
    // peer must agree. Whether the entry is alive or dead is a
    // consequence of who wins on lamport+actor ordering: modify-wins-
    // over-delete is strict (`mod.beats(del)`), so a tie on lamport
    // with `del.actor > mod.actor` keeps the file deleted.
    let pa = project(&a);
    let pb = project(&b);
    let pc = project(&c);

    // Path agreement (most important): if there is a survivor, every
    // peer must see it at the same path.
    let path_a: Vec<_> = pa.by_path.keys().cloned().collect();
    let path_b: Vec<_> = pb.by_path.keys().cloned().collect();
    let path_c: Vec<_> = pc.by_path.keys().cloned().collect();
    assert_eq!(path_a, path_b, "A and B disagree on path");
    assert_eq!(path_a, path_c, "A and C disagree on path");

    // The survivor (if any) must keep the original NodeId — rename and
    // modify both target the same node.
    if let Some(survivor) = pa.by_path.values().next() {
        assert_eq!(survivor.id, id, "rename/modify preserves NodeId");
        // And the path must be the renamed one (only A renamed, the
        // other peers didn't): if the entry survived, the rename's
        // name change is still part of the merged state.
        let p = path_a.first().unwrap();
        assert_eq!(p, "renamed.md", "rename took effect");
    } else {
        // Else: delete won. Then nobody sees a live entry.
        assert!(pb.by_path.is_empty());
        assert!(pc.by_path.is_empty());
    }
}

// ===========================================================================
// auto-apr28-005: concurrent same-named directory creation. Two fresh
// peers each create a directory `Inbox/` at the vault root, then create
// a file inside it (`Inbox/note-A.md` on peer A, `Inbox/note-B.md` on
// peer B). After sync, both files must surface — there must NOT be two
// `Inbox` projections at the root that orphan one peer's children.
// ===========================================================================
#[test]
fn auto_apr28_005_concurrent_same_named_directories_keep_all_children() {
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, "Inbox/note-A.md", 1).unwrap();

    let mut b = Manifest::new(ActorId::new());
    create_text(&mut b, "Inbox/note-B.md", 1).unwrap();

    sync(&mut a, &mut b);
    assert_converged("auto-apr28-005-dirs", &[&a, &b]);

    let pa = project(&a);
    let pb = project(&b);

    // Both children must surface, regardless of which directory wins
    // ownership. The robust property is: every file we created is in
    // the projection on every peer, with the correct stem.
    let names_a: Vec<_> = pa
        .by_path
        .keys()
        .map(|s| s.split('/').next_back().unwrap().to_string())
        .collect();
    let names_b: Vec<_> = pb
        .by_path
        .keys()
        .map(|s| s.split('/').next_back().unwrap().to_string())
        .collect();
    assert!(
        names_a.iter().any(|n| n == "note-A.md"),
        "peer A missing note-A.md (paths: {:?})",
        pa.by_path.keys().collect::<Vec<_>>()
    );
    assert!(
        names_a.iter().any(|n| n == "note-B.md"),
        "peer A missing note-B.md (paths: {:?})",
        pa.by_path.keys().collect::<Vec<_>>()
    );
    assert!(names_b.iter().any(|n| n == "note-A.md"));
    assert!(names_b.iter().any(|n| n == "note-B.md"));

    // Critically: the *parent paths* on both peers must be byte-equal,
    // i.e. both files must end up in the SAME canonical directory or
    // the SAME conflict-suffixed pair of directories. If A sees them
    // both under `Inbox/` and B sees them under `Inbox/` and
    // `Inbox.conflict-…/`, that's a divergence.
    let mut paths_a: Vec<_> = pa.by_path.keys().cloned().collect();
    let mut paths_b: Vec<_> = pb.by_path.keys().cloned().collect();
    paths_a.sort();
    paths_b.sort();
    assert_eq!(
        paths_a, paths_b,
        "peers diverged on directory layout for concurrent same-named dirs"
    );
}

// ===========================================================================
// auto-apr28-007: two peers each rename their existing file to the same
// nested target path that requires creating new directories. Each peer's
// rename auto-creates the same directory chain. After sync, both peers
// must converge — likely with conflict suffix on one of the files — and
// the directory structure must agree.
// ===========================================================================
#[test]
fn auto_apr28_007_concurrent_rename_to_same_nested_target_converges() {
    // Both peers start with two distinct files at the root.
    let mut a = Manifest::new(ActorId::new());
    create_text(&mut a, "left.md", 1).unwrap();
    create_text(&mut a, "right.md", 1).unwrap();
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Concurrent: A renames left.md to inbox/today/note.md.
    //             B renames right.md to inbox/today/note.md (same path!).
    rename(&mut a, "left.md", "inbox/today/note.md").unwrap();
    rename(&mut b, "right.md", "inbox/today/note.md").unwrap();

    // Each peer's rename was locally legal (target was free at the time
    // the local op ran). After merge, both files claim the same target
    // → conflict resolution must kick in.
    sync(&mut a, &mut b);
    sync(&mut a, &mut b); // another pass for the conflict path
    assert_converged("auto-apr28-007", &[&a, &b]);

    let pa = project(&a);
    let pb = project(&b);

    // Both files must surface — neither was deleted.
    assert_eq!(
        pa.by_path.len(),
        2,
        "both files still alive on A; got: {:?}",
        pa.by_path.keys().collect::<Vec<_>>()
    );
    assert_eq!(pb.by_path.len(), 2);

    // Exactly one file at canonical "inbox/today/note.md", the other
    // under a conflict-suffixed sibling in the same dir.
    assert!(
        pa.by_path.contains_key("inbox/today/note.md"),
        "canonical path present on A: {:?}",
        pa.by_path.keys().collect::<Vec<_>>()
    );
    let conflict_count = pa
        .by_path
        .keys()
        .filter(|p| p.contains("inbox/today/") && p.contains(".conflict-"))
        .count();
    assert_eq!(
        conflict_count, 1,
        "exactly one conflict sibling expected; paths: {:?}",
        pa.by_path.keys().collect::<Vec<_>>()
    );
}

// ===========================================================================
// auto-apr28-009: peer A modifies a text file while peer B concurrently
// renames the parent directory. After sync, the modify must surface at
// the NEW (renamed) path on both peers — and the rename must propagate
// to all descendants. This is the directory-refactor + active-edit
// race that's bread-and-butter Obsidian usage.
// ===========================================================================
#[test]
fn auto_apr28_009_concurrent_directory_rename_with_descendant_modify_converges() {
    use syncline::v1::ids::NodeId;

    // Both peers start synced: dir/note.md
    let mut a = Manifest::new(ActorId::new());
    let dir = a.create_node("Projects", None, NodeKind::Directory, &[], 0);
    let note: NodeId = a.create_node("note.md", Some(dir), NodeKind::Text, &[], 5);
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Peer A modifies the descendant. Peer B renames the directory
    // by NodeId (rename-by-path doesn't work on directories — they
    // don't appear in the projection's `by_path`).
    record_modify_text(&mut a, "Projects/note.md").unwrap();
    record_modify_text(&mut a, "Projects/note.md").unwrap();

    // B looks up the directory NodeId — same as A's because both came
    // from the shared baseline.
    b.set_name(dir, "Archive");

    sync(&mut a, &mut b);
    assert_converged("auto-apr28-009", &[&a, &b]);

    let pa = project(&a);
    let pb = project(&b);

    // The note must surface on both peers under the renamed dir.
    // (The rename moves the directory NodeId; the file's parent NodeId
    // is the directory NodeId, so projection follows.)
    assert!(
        pa.by_path.contains_key("Archive/note.md"),
        "A should see the renamed-dir descendant; got {:?}",
        pa.by_path.keys().collect::<Vec<_>>()
    );
    assert!(
        pb.by_path.contains_key("Archive/note.md"),
        "B should see the renamed-dir descendant; got {:?}",
        pb.by_path.keys().collect::<Vec<_>>()
    );

    // The note's NodeId is preserved.
    let r = pa.by_path.get("Archive/note.md").unwrap();
    assert_eq!(r.id, note, "rename of parent does not mint a new file");
}

// ===========================================================================
// auto-apr28-012: two files differing only in case at the manifest layer.
// On POSIX both are legal; on case-insensitive FS (macOS APFS, NTFS) the
// disk layer can only materialise one. The manifest itself must keep
// both as distinct projections (different paths, different NodeIds) —
// case collapsing at the disk layer is a separate concern (see
// `e2e/test/specs/issue56.e2e.ts`). This test pins the manifest-level
// behaviour: case-different paths are distinct rows in `by_path`.
// ===========================================================================
#[test]
fn auto_apr28_012_case_only_difference_keeps_distinct_manifest_rows() {
    let mut a = Manifest::new(ActorId::new());
    let upper = create_text(&mut a, "README.md", 1).unwrap();
    let lower = create_text(&mut a, "readme.md", 2).unwrap();
    let mixed = create_text(&mut a, "ReadMe.md", 3).unwrap();

    let mut b = Manifest::new(ActorId::new());
    sync(&mut a, &mut b);
    assert_converged("auto-apr28-012-case", &[&a, &b]);

    let pa = project(&a);
    let pb = project(&b);

    // All three must surface byte-distinct on both peers.
    for p in &["README.md", "readme.md", "ReadMe.md"] {
        assert!(
            pa.by_path.contains_key(*p),
            "peer A missing {p}; got {:?}",
            pa.by_path.keys().collect::<Vec<_>>()
        );
        assert!(
            pb.by_path.contains_key(*p),
            "peer B missing {p}; got {:?}",
            pb.by_path.keys().collect::<Vec<_>>()
        );
    }

    // Distinct NodeIds — manifest must not have collapsed them.
    assert_ne!(upper, lower);
    assert_ne!(lower, mixed);
    assert_ne!(upper, mixed);

    // Their projection sizes match the source file sizes (sanity
    // they didn't get cross-wired).
    assert_eq!(pa.by_path["README.md"].size, 1);
    assert_eq!(pa.by_path["readme.md"].size, 2);
    assert_eq!(pa.by_path["ReadMe.md"].size, 3);
}

// ===========================================================================
// auto-apr28-013: 5-peer rename storm. Each peer concurrently renames
// the same file to a unique new name. After full-mesh sync, exactly
// one rename wins (lamport+actor LWW), the file is at exactly one
// path, and every peer agrees.
// ===========================================================================
#[test]
fn auto_apr28_013_five_peer_concurrent_rename_storm_converges() {
    // Bootstrap: every peer starts from the same baseline with
    // shared.md.
    let baseline = {
        let mut m = Manifest::new(ActorId::new());
        create_text(&mut m, "shared.md", 0).unwrap();
        m.encode_state_as_update()
    };

    let mut peers: Vec<Manifest> = (0..5)
        .map(|_| Manifest::from_update(ActorId::new(), Lamport::ZERO, &baseline).unwrap())
        .collect();

    // Each peer renames to a unique new name.
    let targets = [
        "winner-1.md",
        "winner-2.md",
        "winner-3.md",
        "winner-4.md",
        "winner-5.md",
    ];
    for (i, m) in peers.iter_mut().enumerate() {
        rename(m, "shared.md", targets[i]).unwrap();
    }

    full_mesh_sync(&mut peers, 6);
    let head_hash = projection_hash(&peers[0]);
    for (i, p) in peers.iter().enumerate().skip(1) {
        assert_eq!(
            projection_hash(p),
            head_hash,
            "peer {i} diverged from peer 0 after rename storm"
        );
    }

    // Every peer sees exactly one live file.
    for (i, p) in peers.iter().enumerate() {
        let proj = project(p);
        assert_eq!(
            proj.by_path.len(),
            1,
            "peer {i}: expected exactly one live entry, got {:?}",
            proj.by_path.keys().collect::<Vec<_>>()
        );
    }

    // The winning name must be one of the candidates.
    let winning_path: Vec<_> = project(&peers[0]).by_path.keys().cloned().collect();
    assert_eq!(winning_path.len(), 1);
    assert!(
        targets.contains(&winning_path[0].as_str()),
        "winner {:?} should be one of {:?}",
        winning_path[0],
        targets
    );
}

// ===========================================================================
// auto-apr28-015: adversarial parent-cycle. Synthesize a manifest where
// node A's parent is B, and B's parent is A. The projection's
// build_path must detect this and refuse to project either node —
// neither should appear in `by_path`. The manifest must also not
// crash, hang, or take quadratic time.
// ===========================================================================
#[test]
fn auto_apr28_015_parent_cycle_drops_both_from_projection_no_hang() {
    use syncline::v1::ids::NodeId;

    let mut m = Manifest::new(ActorId::new());

    // Create two nodes at root (parent=None for now), then re-parent
    // them into a cycle.
    let a = m.create_node("a.md", None, NodeKind::Text, &[], 0);
    let b = m.create_node("b.md", None, NodeKind::Text, &[], 0);

    // Re-parent: a→b and b→a. Now both nodes have parents that form a
    // 2-cycle.
    m.set_parent(a, Some(b));
    m.set_parent(b, Some(a));

    // Bound the projection time — if MAX_HOPS isn't honoured we'd
    // loop forever. Run on a fresh thread with a 5s timeout.
    let m_clone_update = m.encode_state_as_update();
    let actor = ActorId::new();
    let handle = std::thread::spawn(move || {
        let m_rebuilt =
            Manifest::from_update(actor, Lamport::ZERO, &m_clone_update).unwrap();
        let proj = project(&m_rebuilt);
        proj.by_path.keys().cloned().collect::<Vec<String>>()
    });

    // Poll for completion with a timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        handle.is_finished(),
        "projection on a 2-cycle must terminate within 5s"
    );

    let paths = handle.join().expect("projection thread panicked");
    // Both cyclic nodes must be dropped — the projection should be
    // empty.
    assert!(
        paths.is_empty(),
        "cyclic-parent nodes must be dropped from projection, got {paths:?}"
    );
}

// ===========================================================================
// auto-apr28-016: a peer joins with no state, the existing pair has
// already done a delete-then-modify-resurrect cycle on a file. The
// late-joining peer must see the file as alive (modify-wins).
// Validates that the LWW resolution is properly encoded in the CRDT
// updates a fresh peer pulls down — not just a function of in-memory
// state on a long-running peer.
// ===========================================================================
#[test]
fn auto_apr28_016_modify_resurrect_visible_to_late_joiner() {
    // Peer A creates note.md
    let mut a = Manifest::new(ActorId::new());
    let note_id = create_text(&mut a, "note.md", 7).unwrap();

    // Peer B copies A's state.
    let mut b = Manifest::from_update(
        ActorId::new(),
        a.lamport(),
        &a.encode_state_as_update(),
    )
    .unwrap();

    // Peer A deletes note.md
    delete_path(&mut a, "note.md").unwrap();
    // Peer B modifies note.md (concurrent with A's delete).
    record_modify_text(&mut b, "note.md").unwrap();
    // Sync A and B. After sync, B may see note.md as either alive (if
    // B's modify won the LWW tiebreak) or deleted (if A's delete
    // won).
    sync(&mut a, &mut b);
    // Issue another modify on B's side via NodeId — path-resolution
    // would fail if B's projection currently sees note.md as deleted.
    // The NodeId-level `record_modify` on B advances mod_lamp past
    // any del_lamp B observed from A, guaranteeing modify wins.
    b.record_modify(note_id);
    sync(&mut a, &mut b);

    // After convergence both A and B should see note.md alive (the
    // second modify on B has the highest stamp).
    let pa_after = project(&a);
    let pb_after = project(&b);
    assert!(
        pa_after.by_path.contains_key("note.md"),
        "after modify-resurrects-delete, A must see note.md alive; got {:?}",
        pa_after.by_path.keys().collect::<Vec<_>>()
    );
    assert!(
        pb_after.by_path.contains_key("note.md"),
        "after modify-resurrects-delete, B must see note.md alive"
    );

    // Now a fresh peer C joins, applying A's full state. C should
    // ALSO see note.md alive — the resurrection must be encoded in
    // the wire-level update bytes.
    let c = Manifest::from_update(
        ActorId::new(),
        Lamport::ZERO,
        &a.encode_state_as_update(),
    )
    .unwrap();
    let pc = project(&c);
    assert!(
        pc.by_path.contains_key("note.md"),
        "late-joining peer C must see note.md alive (modify-wins encoded \
         in wire bytes), got {:?}",
        pc.by_path.keys().collect::<Vec<_>>()
    );
    assert_converged("auto-apr28-016", &[&a, &b, &c]);
}

// ===========================================================================
// auto-apr28-017: two peers accidentally sharing an ActorId (vault
// folder copied to a second device including `.syncline/actor_id`).
// Both peers create distinct files concurrently, then sync. Both files
// must survive (different NodeIds, identical actor stamps), and
// projection must agree.
// ===========================================================================
#[test]
fn auto_apr28_017_duplicate_actor_id_does_not_cause_data_loss() {
    let shared_actor = ActorId::new();
    let mut a = Manifest::new(shared_actor);
    let mut b = Manifest::new(shared_actor);

    // Each creates a unique file.
    create_text(&mut a, "from-a.md", 1).unwrap();
    create_text(&mut b, "from-b.md", 2).unwrap();

    // Sync them (two passes for the bidirectional CRDT exchange).
    sync(&mut a, &mut b);
    sync(&mut a, &mut b);

    let pa = project(&a);
    let pb = project(&b);

    // Both files must surface on both peers.
    assert!(
        pa.by_path.contains_key("from-a.md"),
        "A missing from-a.md (got {:?})",
        pa.by_path.keys().collect::<Vec<_>>()
    );
    assert!(pa.by_path.contains_key("from-b.md"), "A missing from-b.md");
    assert!(pb.by_path.contains_key("from-a.md"), "B missing from-a.md");
    assert!(pb.by_path.contains_key("from-b.md"), "B missing from-b.md");

    // Convergence still required — different NodeIds for each file
    // even though actor IDs match.
    assert_converged("auto-apr28-017", &[&a, &b]);

    // Now stress: each peer creates the SAME path with different
    // content (logically a same-path collision). Both peers share
    // actor_id, so the conflict-suffix tiebreak (which uses actor)
    // can't distinguish — it falls back to NodeId tiebreak.
    let mut a2 = Manifest::new(shared_actor);
    let mut b2 = Manifest::new(shared_actor);
    create_text(&mut a2, "shared.md", 1).unwrap();
    create_text(&mut b2, "shared.md", 2).unwrap();
    sync(&mut a2, &mut b2);
    assert_converged("auto-apr28-017-collision", &[&a2, &b2]);
    let pa2 = project(&a2);
    // Both files survive — one canonical at "shared.md", the other
    // under a conflict-suffixed sibling.
    assert_eq!(
        pa2.by_path.len(),
        2,
        "same-actor same-path collision must still produce two distinct \
         live entries; got: {:?}",
        pa2.by_path.keys().collect::<Vec<_>>()
    );
    assert!(pa2.by_path.contains_key("shared.md"));
}

// ===========================================================================
// auto-apr28-019: a peer reconnecting after months of being offline
// while other peers did 1000s of ops. Local lamport is small (~10),
// remote lamport is huge (~10_000). After applying remote state, the
// local lamport must observe correctly so the next local op is
// stamped above the remote — no LWW backwards-compat issues.
// ===========================================================================
#[test]
fn auto_apr28_019_huge_lamport_gap_observes_correctly() {
    // Build a "big" peer with many ops to push lamport high.
    let mut big = Manifest::new(ActorId::new());
    for i in 0..2000 {
        let _ = create_text(&mut big, &format!("note_{i}.md"), 0);
    }
    let big_lamp = big.lamport().get();
    assert!(
        big_lamp >= 2000,
        "big peer should have lamport ≥ 2000, got {big_lamp}"
    );

    // Build a "small" peer that's been offline. Few ops, small lamport.
    let mut small = Manifest::new(ActorId::new());
    create_text(&mut small, "tiny.md", 0).unwrap();
    let small_lamp_pre = small.lamport().get();
    assert!(small_lamp_pre <= 5, "small peer lamport should still be small");

    // Small peer pulls big's state.
    small.apply_update(&big.encode_state_as_update()).unwrap();

    // Small's lamport must have observed past big's lamport.
    assert!(
        small.lamport().get() >= big_lamp,
        "small lamport must observe up to big's: {} vs {}",
        small.lamport().get(),
        big_lamp,
    );

    // Now small does a local op. Its stamp must beat any of big's
    // existing stamps.
    create_text(&mut small, "after-catchup.md", 0).unwrap();
    let after_lamp = small.lamport().get();
    assert!(
        after_lamp > big_lamp,
        "post-catchup local op must produce a higher stamp than \
         any of big's existing stamps; got after={} big={}",
        after_lamp,
        big_lamp
    );

    // And big, when pulling small's state, must observe small's new
    // op and rank it as the most recent.
    big.apply_update(&small.encode_state_as_update()).unwrap();
    let pa = project(&small);
    let pb = project(&big);
    assert!(pa.by_path.contains_key("after-catchup.md"));
    assert!(pb.by_path.contains_key("after-catchup.md"));
    assert_converged("auto-apr28-019", &[&small, &big]);
}
