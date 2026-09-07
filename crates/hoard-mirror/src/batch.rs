//! Work a serving node accumulates instead of paying for per response.
//!
//! A catalog write is one redb transaction whatever it carries, so a node that writes once
//! per served response pays a transaction per response. [`Debounced`] holds the work in
//! memory and hands the whole of it to one call, at most once per interval while pokes keep
//! arriving, or immediately once enough has piled up.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use hoard_index::catalog::Catalog;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::{MirrorError, Result};

/// What a batch does with the items it collected. It runs on a blocking thread, so it may
/// open a transaction.
type Writer<T> = Arc<dyn Fn(Vec<T>) -> Result<()> + Send + Sync>;

/// Items waiting to be written together, and the writer that takes them.
struct Batch<T> {
    pending: Mutex<Vec<T>>,
    ready: Notify,
    capacity: usize,
    write: Writer<T>,
}

impl<T: Send + 'static> Batch<T> {
    fn take(&self) -> Vec<T> {
        match self.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }

    async fn drain(self: &Arc<Self>) -> Result<()> {
        let taken = self.take();
        if taken.is_empty() {
            return Ok(());
        }
        let write = Arc::clone(&self.write);
        tokio::task::spawn_blocking(move || write(taken))
            .await
            .map_err(|_| MirrorError::Index(hoard_index::IndexError::TaskJoin))?
    }
}

/// Work that accumulates while it waits.
///
/// A recorded item is held until the interval expires, until `capacity` items are waiting,
/// or until someone asks for a [`Debounced::flush`], and the whole of what is waiting is
/// then written in one call. Dropping the handle stops the timer without writing; use
/// [`Debounced::shutdown`] to write what is left.
pub struct Debounced<T> {
    batch: Arc<Batch<T>>,
    stop: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl<T> std::fmt::Debug for Debounced<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Debounced")
            .field("pending", &self.pending_len())
            .field("capacity", &self.batch.capacity)
            .finish_non_exhaustive()
    }
}

impl<T> Debounced<T> {
    fn pending_len(&self) -> usize {
        match self.batch.pending.lock() {
            Ok(pending) => pending.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

impl<T: Send + 'static> Debounced<T> {
    /// Starts the timer that writes what has accumulated. Outside a Tokio runtime no timer
    /// is started and only an explicit `flush` writes, which is what a synchronous test of
    /// something holding one of these gets.
    pub fn spawn<F>(interval: Duration, capacity: usize, write: F) -> Self
    where
        F: Fn(Vec<T>) -> Result<()> + Send + Sync + 'static,
    {
        let batch = Arc::new(Batch {
            pending: Mutex::new(Vec::new()),
            ready: Notify::new(),
            capacity: capacity.max(1),
            write: Arc::new(write),
        });
        let stop = Arc::new(Notify::new());
        let task = tokio::runtime::Handle::try_current().ok().map(|runtime| {
            let batch = Arc::clone(&batch);
            let stop = Arc::clone(&stop);
            runtime.spawn(run(batch, stop, interval))
        });
        Self { batch, stop, task }
    }

    /// Adds one item to what is waiting, and wakes the writer when enough has piled up.
    pub fn record(&self, item: T) {
        let full = match self.batch.pending.lock() {
            Ok(mut pending) => {
                pending.push(item);
                pending.len() >= self.batch.capacity
            }
            Err(poisoned) => {
                let mut pending = poisoned.into_inner();
                pending.push(item);
                pending.len() >= self.batch.capacity
            }
        };
        if full {
            self.batch.ready.notify_one();
        }
    }

    /// How many items are waiting to be written.
    pub fn pending(&self) -> usize {
        self.pending_len()
    }

    /// Writes what is waiting now.
    pub async fn flush(&self) -> Result<()> {
        self.batch.drain().await
    }

    /// Stops the timer and writes what is left.
    pub async fn shutdown(mut self) -> Result<()> {
        self.stop.notify_waiters();
        if let Some(task) = self.task.take()
            && task.await.is_err()
        {
            tracing::warn!("a batch writer did not stop cleanly");
        }
        self.batch.drain().await
    }
}

async fn run<T: Send + 'static>(batch: Arc<Batch<T>>, stop: Arc<Notify>, interval: Duration) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = batch.ready.notified() => {}
            () = stop.notified() => return,
        }
        if let Err(error) = batch.drain().await {
            tracing::warn!(%error, "a batch of accumulated work was not written");
        }
    }
}

/// How long changes wait before the search index is brought up to date.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// The search index's refresh, run at most once per interval however many writers ask for
/// it. A rescan, an ingest, a retirement and a batch of usage rows all leave changes in the
/// catalog's log; applying them once for all of them is one Tantivy commit rather than one
/// per writer.
#[derive(Debug)]
pub struct SearchRefresher {
    sink: Debounced<()>,
}

impl SearchRefresher {
    /// Starts the timer. Outside a Tokio runtime nothing is timed and only `flush` refreshes.
    pub fn spawn(catalog: Arc<Catalog>, interval: Duration) -> Self {
        Self {
            sink: Debounced::spawn(interval, usize::MAX, move |_: Vec<()>| {
                let refreshed = catalog.refresh_search_reporting(|_| {})?;
                if refreshed.applied > 0 {
                    tracing::debug!(
                        applied = refreshed.applied,
                        documents = refreshed.documents,
                        bytes = refreshed.bytes,
                        commits = refreshed.commits,
                        "search refreshed"
                    );
                }
                Ok(())
            }),
        }
    }

    /// Says the catalog has changes the index has not seen. The refresh itself happens on
    /// the timer, so a writer never waits for one.
    pub fn poke(&self) {
        self.sink.record(());
    }

    /// Whether anyone has asked for a refresh that has not run yet.
    pub fn poked(&self) -> bool {
        self.sink.pending() > 0
    }

    /// Refreshes now if anyone asked, which is what a reader of search results waits on.
    pub async fn flush(&self) -> Result<()> {
        self.sink.flush().await
    }

    /// Stops the timer and refreshes if anyone asked.
    pub async fn shutdown(self) -> Result<()> {
        self.sink.shutdown().await
    }
}

/// A refresher for a test: its interval is long enough that only an explicit `flush` ever
/// refreshes, so a test never races the timer.
#[cfg(test)]
pub(crate) fn test_refresher(catalog: &Arc<Catalog>) -> SearchRefresher {
    SearchRefresher::spawn(Arc::clone(catalog), Duration::from_secs(3600))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn a_full_batch_is_written_without_waiting_for_the_interval() {
        let written = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let sink = {
            let written = Arc::clone(&written);
            let calls = Arc::clone(&calls);
            Debounced::spawn(Duration::from_secs(3600), 4, move |items: Vec<u8>| {
                written.fetch_add(items.len(), Ordering::Relaxed);
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };

        for value in 0..4 {
            sink.record(value);
        }
        for _ in 0..200 {
            if written.load(Ordering::Relaxed) == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(written.load(Ordering::Relaxed), 4);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "one call took all four");
        sink.shutdown().await.expect("shutdown writes what is left");
    }

    #[tokio::test]
    async fn what_is_left_is_written_by_a_flush_and_by_a_shutdown() {
        let written = Arc::new(AtomicUsize::new(0));
        let sink = {
            let written = Arc::clone(&written);
            Debounced::spawn(Duration::from_secs(3600), 256, move |items: Vec<u8>| {
                written.fetch_add(items.len(), Ordering::Relaxed);
                Ok(())
            })
        };

        sink.record(1);
        sink.record(2);
        assert_eq!(sink.pending(), 2);
        sink.flush().await.expect("flush writes");
        assert_eq!(written.load(Ordering::Relaxed), 2);
        assert_eq!(sink.pending(), 0);

        sink.record(3);
        sink.shutdown().await.expect("shutdown writes");
        assert_eq!(written.load(Ordering::Relaxed), 3);
    }
}
