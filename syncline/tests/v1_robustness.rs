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
