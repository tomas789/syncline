//! Pure logic for the WASM client's blob-request retry sweep.
//!
//! `wasm_client_v1::SynclineV1Client::request_blob` records every
//! `MSG_BLOB_REQUEST` it sends as `(hash → wall-clock-ms-of-send)` so a
//! periodic sweep can re-issue any request whose reply never arrived.
//! That sweep is gated to `wasm32` (it touches `js_sys::Date`,
//! `web_sys::WebSocket`), but the *predicate* it uses is platform-pure
//! — and gets exercised on every CI run by the unit tests below.
//!
//! Without this retry the WASM client's session-dedupe set silently
//! pinned the sidebar's "in flight" counter forever whenever a request
//! frame got dropped under WS backpressure. Reproduced in the wild on
//! a 1.2.0 vault stuck at "2 blobs · 7.2 MiB remaining" — see #65 phase
//! 2 progress UI.

use std::collections::HashMap;

/// Pick out hashes whose last-send timestamp is older than
/// `stale_after_ms` relative to `now`. The filter uses `>=`, so a
/// `stale_after_ms` of 0 reports every entry (operator-driven
/// force-resync).
pub(crate) fn collect_stale_blob_requests(
    requests: &HashMap<String, f64>,
    now: f64,
    stale_after_ms: f64,
) -> Vec<String> {
    requests
        .iter()
        .filter(|&(_, &sent_at)| now - sent_at >= stale_after_ms)
        .map(|(h, _)| h.clone())
        .collect()
}

/// Pick out hashes that are still in the pending set but no longer
/// referenced by any live manifest entry — "ghost requests" left over
/// from a manifest update that swapped one set of chunk hashes for
/// another. Without pruning, the retry sweep would re-ask the server
/// for these forever and the server would keep replying "blob not
/// found".
///
/// `live_chunk_hashes` is the union of `chunk_hashes` across every
/// live binary entry currently in the projection.
pub(crate) fn collect_ghost_blob_requests(
    requests: &HashMap<String, f64>,
    live_chunk_hashes: &std::collections::HashSet<&str>,
) -> Vec<String> {
    requests
        .keys()
        .filter(|h| !live_chunk_hashes.contains(h.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_only_old_requests() {
        let mut req: HashMap<String, f64> = HashMap::new();
        // Wall-clock-style ms timestamps: imagine `now` is 100_000.
        req.insert("hash_recent".into(), 99_000.0); // 1 s old
        req.insert("hash_stale".into(), 80_000.0); // 20 s old
        req.insert("hash_borderline".into(), 85_000.0); // exactly 15 s old

        let now = 100_000.0;
        let mut stale = collect_stale_blob_requests(&req, now, 15_000.0);
        stale.sort();
        assert_eq!(stale, vec!["hash_borderline", "hash_stale"]);
    }

    #[test]
    fn returns_empty_when_no_requests_pending() {
        let req: HashMap<String, f64> = HashMap::new();
        assert!(collect_stale_blob_requests(&req, 1_000.0, 500.0).is_empty());
    }

    #[test]
    fn returns_empty_when_threshold_not_yet_reached() {
        let mut req: HashMap<String, f64> = HashMap::new();
        req.insert("h1".into(), 10_000.0);
        req.insert("h2".into(), 10_500.0);
        // 1 s elapsed, threshold 5 s → nothing is stale.
        assert!(collect_stale_blob_requests(&req, 11_000.0, 5_000.0).is_empty());
    }

    #[test]
    fn zero_threshold_force_resyncs_everything() {
        let mut req: HashMap<String, f64> = HashMap::new();
        req.insert("h1".into(), 5_000.0);
        req.insert("h2".into(), 5_001.0);
        let mut all = collect_stale_blob_requests(&req, 5_001.0, 0.0);
        all.sort();
        assert_eq!(all, vec!["h1", "h2"]);
    }

    #[test]
    fn ghost_collector_drops_hashes_not_in_live_set() {
        use std::collections::HashSet;
        let mut req: HashMap<String, f64> = HashMap::new();
        req.insert("alive_a".into(), 1_000.0);
        req.insert("alive_b".into(), 1_000.0);
        req.insert("ghost".into(), 1_000.0); // no live entry references it

        let alive_set: HashSet<&str> = ["alive_a", "alive_b", "alive_c"]
            .into_iter()
            .collect();
        let ghosts = collect_ghost_blob_requests(&req, &alive_set);
        assert_eq!(ghosts, vec!["ghost".to_string()]);
    }

    #[test]
    fn ghost_collector_returns_empty_when_all_hashes_live() {
        use std::collections::HashSet;
        let mut req: HashMap<String, f64> = HashMap::new();
        req.insert("a".into(), 1.0);
        req.insert("b".into(), 2.0);
        let alive: HashSet<&str> = ["a", "b", "c"].into_iter().collect();
        assert!(collect_ghost_blob_requests(&req, &alive).is_empty());
    }

    #[test]
    fn ghost_collector_handles_empty_live_set() {
        let req: HashMap<String, f64> = HashMap::from_iter([
            ("h1".to_string(), 1.0),
            ("h2".to_string(), 2.0),
        ]);
        let alive = std::collections::HashSet::new();
        let mut g = collect_ghost_blob_requests(&req, &alive);
        g.sort();
        assert_eq!(g, vec!["h1", "h2"]);
    }

    #[test]
    fn newly_inserted_entry_is_not_stale_at_same_tick_with_positive_threshold() {
        // Defends against the bug where the retry sweep would
        // re-send a request we just enqueued in the same JS tick —
        // `request_blob` writes `Date::now()` then the sweep reads
        // `Date::now()`; without the strict `>=` boundary semantic
        // they could converge and a forever-loop ensues.
        let mut req: HashMap<String, f64> = HashMap::new();
        req.insert("just_sent".into(), 1_000.0);
        let stale = collect_stale_blob_requests(&req, 1_000.0, 1.0);
        assert!(stale.is_empty());
    }
}
