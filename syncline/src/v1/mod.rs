//! Syncline v1 — manifest-CRDT core.
//!
//! This module is the entry point for the v1 rewrite described in
//! `docs/DESIGN_DOC_V1.md`. It contains only pure data structures and
//! projection logic on every target; wire integration, migration, and
//! operation handlers live in native-only submodules.
//!
//! Layering:
//!
//! - [`ids`]        — `NodeId`, `ActorId`, `Lamport` newtypes. (portable)
//! - [`hash`]       — `hash_hex` SHA-256 helper. (portable)
//! - [`manifest`]   — Yrs-backed manifest Y.Doc with `NodeEntry` CRUD. (portable)
//! - [`projection`] — projects the manifest into the vault namespace. (portable)
//! - [`ops`]        — high-level create/delete/rename/modify helpers. (portable)
//! - [`sync`]       — wire encoders/decoders + projection hash. (portable)
//! - [`blob_store`] — on-disk CAS for binary blobs. (native-only)
//! - [`disk`]       — `.syncline/` layout + version tripwire. (native-only)
//! - [`migration`]  — one-shot v0 → v1 local migration. (native-only)
//!
//! The portable core compiles on `wasm32-unknown-unknown` so the Obsidian
//! plugin can drive a v1 client directly from its WASM build.

pub mod hash;
pub mod ids;
pub mod manifest;
pub mod ops;
pub mod projection;
pub mod sync;

#[cfg(not(target_arch = "wasm32"))]
pub mod blob_store;
#[cfg(not(target_arch = "wasm32"))]
pub mod disk;
#[cfg(not(target_arch = "wasm32"))]
pub mod migration;

pub use hash::hash_hex;
pub use ids::{ActorId, Lamport, NodeId};
pub use manifest::{Manifest, NodeEntry, NodeKind};
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

#[cfg(not(target_arch = "wasm32"))]
pub use blob_store::BlobStore;
#[cfg(not(target_arch = "wasm32"))]
pub use disk::{migrate_vault_on_disk, read_or_create_actor_id, read_vault_version, MigrationReport};
#[cfg(not(target_arch = "wasm32"))]
pub use migration::{migrate_v0_vault, Migration};
