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
                        // Send over channel (non-blocking)
                        let _ = tx.blocking_send(event);
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
    debouncer: notify_debouncer_mini::Debouncer<RecommendedWatcher>,
}

impl DebouncedWatcher {
    pub fn new(
        tx: mpsc::Sender<Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>>,
        timeout: std::time::Duration,
    ) -> notify::Result<Self> {
        let debouncer = notify_debouncer_mini::new_debouncer(
            timeout,
            move |res: notify_debouncer_mini::DebounceEventResult| {
                // Determine whether res has Vec<notify::Error> or notify::Error
                let mapped_res = match res {
                    Ok(events) => Ok(events),
                    Err(e) => {
                        let errstr = format!("{:?}", e);
                        Err(notify::Error::generic(&errstr))
                    }
                };
                let _ = tx.blocking_send(mapped_res);
            },
        )?;

        Ok(Self { debouncer })
    }

    pub fn watch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        let path = path.as_ref();
        info!("Starting debounced watcher on directory: {:?}", path);
        self.debouncer
            .watcher()
            .watch(path, RecursiveMode::Recursive)
    }

    pub fn unwatch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        let path = path.as_ref();
        info!("Stopping debounced watch on directory: {:?}", path);
        self.debouncer.watcher().unwatch(path)
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
        let (tx, mut rx) = mpsc::channel(100);
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
