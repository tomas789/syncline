//! Syncline v1 — manifest-CRDT core.
//!
//! This module is the entry point for the v1 rewrite described in
//! `docs/DESIGN_DOC_V1.md`. It contains only pure data structures and
//! projection logic; wire integration, migration, and operation handlers
//! land in follow-up commits on `release/v1`.
//!
//! Layering:
//!
//! - [`ids`]        — `NodeId`, `ActorId`, `Lamport` newtypes.
//! - [`manifest`]   — Yrs-backed manifest Y.Doc with `NodeEntry` CRUD.
//! - [`projection`] — projects the manifest into the vault namespace
//!                    (path → NodeEntry map), applying conflict-copy
//!                    suffixes and the modify-wins-over-delete rule.
//!
//! None of this is wired into the client or server yet.

#![cfg(not(target_arch = "wasm32"))]

pub mod blob_store;
pub mod disk;
pub mod ids;
pub mod manifest;
pub mod migration;
pub mod ops;
pub mod projection;
pub mod sync;

pub use blob_store::{hash_hex, BlobStore};
pub use disk::{migrate_vault_on_disk, read_or_create_actor_id, read_vault_version, MigrationReport};
pub use ids::{ActorId, Lamport, NodeId};
pub use manifest::{Manifest, NodeEntry, NodeKind};
pub use migration::{migrate_v0_vault, Migration};
pub use ops::{
    create_binary, create_text, create_text_allowing_collision, delete as delete_path,
    record_modify_binary, record_modify_text, rename,
};
pub use projection::{ProjectedEntry, Projection};
pub use sync::{
    decode_verify_payload, decode_version_handshake, encode_manifest_step1,
    encode_manifest_step2, encode_manifest_update, encode_verify_payload,
    encode_version_handshake, handle_manifest_payload, handle_verify_payload,
    manifest_step1_payload, manifest_step2_payload, projection_hash, split_manifest_payload,
};
