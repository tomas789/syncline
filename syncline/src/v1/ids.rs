//! Identifier types for the v1 manifest CRDT.
//!
//! All three are thin newtypes so the type system prevents accidental
//! confusion (a `Lamport` is not a `u64`, a `NodeId` is not a `String`).

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Per-file stable identity. UUIDv7 is used so IDs sort by creation time,
/// which is useful for the LWW tie-break and for debugging.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        NodeId(Uuid::now_v7())
    }

    pub fn from_uuid(u: Uuid) -> Self {
        NodeId(u)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub fn to_string_hyphenated(&self) -> String {
        self.0.as_hyphenated().to_string()
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().map(NodeId)
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0.as_hyphenated())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.as_hyphenated())
    }
}

/// One per installation. Generated once, persisted in
/// `.syncline/actor_id`, never changes. UUIDv4 — random, not temporal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ActorId(pub Uuid);

impl ActorId {
    pub fn new() -> Self {
        ActorId(Uuid::new_v4())
    }

    pub fn from_uuid(u: Uuid) -> Self {
        ActorId(u)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub fn to_string_hyphenated(&self) -> String {
        self.0.as_hyphenated().to_string()
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().map(ActorId)
    }

    /// Short form for use in conflict filenames (first 8 hex chars).
    pub fn short(&self) -> String {
        self.0.as_simple().to_string()[..8].to_string()
    }
}

impl Default for ActorId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActorId({})", self.0.as_hyphenated())
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.as_hyphenated())
    }
}

/// Per-actor monotonic counter. Combined with `ActorId` in an LWW tuple
/// `(Lamport, ActorId)` with Lamport max-wins, ActorId lex tie-break.
///
/// Advance rule: on every observed remote update, set `self = max(self,
/// remote + 1)`. On every local transaction, increment by one. The
/// counter is persisted to `.syncline/lamport` after each transaction.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Lamport(pub u64);

impl Lamport {
    pub const ZERO: Lamport = Lamport(0);

    pub fn get(&self) -> u64 {
        self.0
    }

    /// Produce the next local timestamp and advance `self`. Used at the
    /// top of every local transaction.
    pub fn tick(&mut self) -> Lamport {
        self.0 += 1;
        *self
    }

    /// Observe a remote timestamp; self becomes `max(self, remote + 1)`.
    pub fn observe(&mut self, remote: Lamport) {
        if remote.0 + 1 > self.0 {
            self.0 = remote.0 + 1;
        }
    }
}

impl fmt::Debug for Lamport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}", self.0)
    }
}

impl fmt::Display for Lamport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// LWW ordering key. Max-wins on `lamport`, then lex-wins on `actor`.
/// Used throughout projection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Stamp {
    pub lamport: Lamport,
    pub actor: ActorId,
}

impl Stamp {
    pub fn new(lamport: Lamport, actor: ActorId) -> Self {
        Self { lamport, actor }
    }

    /// Returns `true` if `self` beats `other` in LWW order.
    pub fn beats(&self, other: &Stamp) -> bool {
        match self.lamport.cmp(&other.lamport) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => self.actor > other.actor,
        }
    }
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.lamport
            .cmp(&other.lamport)
            .then_with(|| self.actor.cmp(&other.actor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_roundtrip() {
        let id = NodeId::new();
        let s = id.to_string_hyphenated();
        assert_eq!(NodeId::parse_str(&s), Some(id));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn uuidv7_sorts_temporally() {
        let a = NodeId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = NodeId::new();
        assert!(a < b, "later UUIDv7 should sort after earlier one");
    }

    #[test]
    fn actor_short_is_eight_chars() {
        let a = ActorId::new();
        assert_eq!(a.short().len(), 8);
    }

    #[test]
    fn lamport_tick_is_monotonic() {
        let mut l = Lamport::ZERO;
        assert_eq!(l.tick(), Lamport(1));
        assert_eq!(l.tick(), Lamport(2));
        assert_eq!(l.tick(), Lamport(3));
    }

    #[test]
    fn lamport_observe_advances_past_remote() {
        let mut l = Lamport(5);
        l.observe(Lamport(10));
        assert_eq!(l, Lamport(11));
    }

    #[test]
    fn lamport_observe_ignores_older_remote() {
        let mut l = Lamport(100);
        l.observe(Lamport(5));
        assert_eq!(l, Lamport(100));
    }

    #[test]
    fn stamp_beats_by_lamport() {
        let a = ActorId::new();
        let b = ActorId::new();
        let s1 = Stamp::new(Lamport(1), a);
        let s2 = Stamp::new(Lamport(2), b);
        assert!(s2.beats(&s1));
        assert!(!s1.beats(&s2));
    }

    #[test]
    fn stamp_beats_by_actor_on_lamport_tie() {
        let (lo, hi) = {
            let a = ActorId::new();
            let b = ActorId::new();
            if a < b { (a, b) } else { (b, a) }
        };
        let s1 = Stamp::new(Lamport(5), lo);
        let s2 = Stamp::new(Lamport(5), hi);
        assert!(s2.beats(&s1));
        assert!(!s1.beats(&s2));
    }

    #[test]
    fn stamp_does_not_beat_self() {
        let a = ActorId::new();
        let s = Stamp::new(Lamport(5), a);
        assert!(!s.beats(&s));
    }
}
