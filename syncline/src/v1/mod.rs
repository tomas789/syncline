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

pub mod ids;
pub mod manifest;
pub mod projection;

pub use ids::{ActorId, Lamport, NodeId};
pub use manifest::{Manifest, NodeEntry, NodeKind};
pub use projection::{ProjectedEntry, Projection};
