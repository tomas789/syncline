use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

pub struct SynclineWatcher {
    watcher: RecommendedWatcher,
}

impl SynclineWatcher {
    /// Create a new watcher that sends raw notify events over the provided channel.
    /// This is step 1: understanding exactly what inotify/fsevents does.
    pub fn new(tx: mpsc::Sender<Event>) -> notify::Result<Self> {
        let watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                match res {
                    Ok(event) => {
                        // We do a heavy amount of logging here to understand the behavior
                        debug!(
                            "RAW EVENT: kind={:?}, paths={:?}, attrs={:?}",
                            event.kind, event.paths, event.attrs
                        );
                        // Apply backpressure: blocking_send is safe
                        // here because notify invokes this callback on
                        // its own (non-tokio) thread. We'd rather wait
                        // for the consumer to drain than silently
                        // drop file events under bursty load.
                        if let Err(e) = tx.blocking_send(event) {
                            error!("Channel closed, dropped raw file event: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Raw watcher error: {:?}", e);
                    }
                }
            },
            Config::default(),
        )?;

        Ok(Self { watcher })
    }

    /// Recursively watch a directory
    pub fn watch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        let path = path.as_ref();
        info!("Starting to watch directory: {:?}", path);
        self.watcher.watch(path, RecursiveMode::Recursive)
    }

    /// Stop watching a directory
    pub fn unwatch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        let path = path.as_ref();
        info!("Stopping watch for directory: {:?}", path);
        self.watcher.unwatch(path)
    }
}

pub struct DebouncedWatcher {
    _watcher: RecommendedWatcher,
}

impl DebouncedWatcher {
    /// Build a watcher whose callbacks **drop `EventKind::Access` events
    /// at the notify-callback level** before any debouncing happens.
    ///
    /// The notify 8.x inotify backend subscribes to read-side events
    /// (`IN_OPEN`, `IN_ACCESS`, `IN_CLOSE_NOWRITE`) by default. The
    /// client's `scan_once` opens every file in the vault on each pass,
    /// so without this filter the watcher delivers a flood of
    /// `Access(Open(Any))` events that map through `notify-debouncer-mini`
    /// to discrete `DebouncedEventKind::Any` entries — indistinguishable
    /// downstream from real writes. That feedback loop pins the client
    /// at 100% CPU forever (each scan triggers events that trigger
    /// another scan, etc.). `notify-debouncer-mini` v0.7.0 does *not*
    /// filter these, so we must do it ourselves.
    ///
    /// We also drop the original `notify-debouncer-mini::Debouncer`
    /// entirely and roll a small time-window debouncer here. That keeps
    /// the public channel API (`Vec<DebouncedEvent>`) for callers
    /// unchanged while letting the filter run before any debouncing.
    pub fn new(
        tx: mpsc::Sender<Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>>,
        timeout: std::time::Duration,
    ) -> notify::Result<Self> {
        use notify::EventKind;
        use notify_debouncer_mini::{DebouncedEvent, DebouncedEventKind};
        use std::sync::mpsc as std_mpsc;

        let (raw_tx, raw_rx) = std_mpsc::channel::<notify::Event>();

        let watcher = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| match res {
                Ok(event) => {
                    if matches!(
                        event.kind,
                        EventKind::Access(_) | EventKind::Other | EventKind::Any
                    ) {
                        return;
                    }
                    let _ = raw_tx.send(event);
                }
                Err(e) => {
                    error!("Raw watcher error: {:?}", e);
                }
            },
            Config::default(),
        )?;

        // Time-window debouncer thread: collect filtered events for
        // `timeout` after the *first* event of a quiet window, then ship
        // them as a single batch. This matches the
        // `notify-debouncer-mini` semantics that callers expect.
        std::thread::spawn(move || {
            let mut batch: Vec<DebouncedEvent> = Vec::new();
            let mut deadline: Option<std::time::Instant> = None;
            loop {
                let wait = match deadline {
                    Some(t) => t.saturating_duration_since(std::time::Instant::now()),
                    None => std::time::Duration::from_secs(60 * 60),
                };
                match raw_rx.recv_timeout(wait) {
                    Ok(event) => {
                        if deadline.is_none() {
                            deadline = Some(std::time::Instant::now() + timeout);
                        }
                        for path in event.paths {
                            batch.push(DebouncedEvent {
                                path,
                                kind: DebouncedEventKind::Any,
                            });
                        }
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        if !batch.is_empty() {
                            if let Err(e) = tx.blocking_send(Ok(std::mem::take(&mut batch))) {
                                error!("Channel closed, dropped debounced file batch: {:?}", e);
                                break;
                            }
                        }
                        deadline = None;
                    }
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }

    pub fn watch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        let path = path.as_ref();
        info!("Starting debounced watcher on directory: {:?}", path);
        self._watcher.watch(path, RecursiveMode::Recursive)
    }

    pub fn unwatch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        let path = path.as_ref();
        info!("Stopping debounced watch on directory: {:?}", path);
        self._watcher.unwatch(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_inotify_behavior() {
        // Initialize tracing for the test if not already done
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();

        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(100000);
        let mut watcher = SynclineWatcher::new(tx).unwrap();

        watcher.watch(temp_dir.path()).unwrap();

        // Let the watcher spin up
        tokio::time::sleep(Duration::from_millis(100)).await;

        let file_path = temp_dir.path().join("test.md");

        info!("--- STARTING TEST FILE CREATION ---");
        fs::write(&file_path, "initial content").unwrap();

        // Wait to collect events
        tokio::time::sleep(Duration::from_millis(100)).await;

        info!("--- STARTING TEST FILE MODIFICATION ---");
        fs::write(&file_path, "modified content").unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        info!("--- STARTING TEST FILE RENAME ---");
        let new_file_path = temp_dir.path().join("test2.md");
        fs::rename(&file_path, &new_file_path).unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        info!("Collect events");
        let mut events = vec![];
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        info!(
            "Collected {} events in total during this burst.",
            events.len()
        );
        for (i, ev) in events.iter().enumerate() {
            info!("Event {}: {:?}", i, ev);
        }

        // We expect multiple events for the simple actions above,
        // demonstrating the OS X / inotify behaviour.
        assert!(
            !events.is_empty(),
            "We should have captured file system events."
        );
    }
}
