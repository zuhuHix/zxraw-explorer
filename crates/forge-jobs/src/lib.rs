//! Background job infrastructure.
//!
//! * [`CancelToken`] — cheap, cloneable cooperative cancellation (with pause support).
//! * [`Progress`] — lock-free progress counters a UI can poll without blocking workers.
//! * [`WorkerPool`] — a fixed-size pool with a *bounded*, *prioritised* queue so that
//!   browsing can never spawn unlimited decode work (backpressure by design).
//! * [`JobRegistry`] — the list of user-visible long-running jobs (copy queues, indexing…).

use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// Cooperative cancellation + pause flag shared between a job and its controller.
#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default, Debug)]
struct CancelInner {
    cancelled: AtomicBool,
    paused: AtomicBool,
    parent: Option<CancelToken>,
}

/// Returned by [`CancelToken::check`] when work should stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation cancelled")
    }
}
impl std::error::Error for Cancelled {}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// A child token is cancelled when either it or its parent is cancelled.
    pub fn child(&self) -> Self {
        Self {
            inner: Arc::new(CancelInner {
                parent: Some(self.clone()),
                ..Default::default()
            }),
        }
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
            || self.inner.parent.as_ref().is_some_and(|p| p.is_cancelled())
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.paused.store(paused, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Acquire)
            || self.inner.parent.as_ref().is_some_and(|p| p.is_paused())
    }

    /// Blocks while paused (polling), returns `Err(Cancelled)` if cancelled.
    /// Call this at safe points inside long loops.
    pub fn check(&self) -> Result<(), Cancelled> {
        loop {
            if self.is_cancelled() {
                return Err(Cancelled);
            }
            if !self.is_paused() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Progress counters. All fields are atomics so workers never block the UI.
#[derive(Default, Debug)]
pub struct Progress {
    pub items_done: AtomicU64,
    pub items_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub errors: AtomicU64,
    current: Mutex<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ProgressSnapshot {
    pub items_done: u64,
    pub items_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub errors: u64,
    pub current: String,
}

impl ProgressSnapshot {
    /// Fraction in `0..=1`, preferring bytes when known. `None` if indeterminate.
    pub fn fraction(&self) -> Option<f32> {
        if self.bytes_total > 0 {
            Some((self.bytes_done as f64 / self.bytes_total as f64).min(1.0) as f32)
        } else if self.items_total > 0 {
            Some((self.items_done as f64 / self.items_total as f64).min(1.0) as f32)
        } else {
            None
        }
    }
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn set_current(&self, s: impl Into<String>) {
        *self.current.lock() = s.into();
    }
    pub fn add_items(&self, n: u64) {
        self.items_done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_bytes(&self, n: u64) {
        self.bytes_done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_total_items(&self, n: u64) {
        self.items_total.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_total_bytes(&self, n: u64) {
        self.bytes_total.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            items_done: self.items_done.load(Ordering::Relaxed),
            items_total: self.items_total.load(Ordering::Relaxed),
            bytes_done: self.bytes_done.load(Ordering::Relaxed),
            bytes_total: self.bytes_total.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            current: self.current.lock().clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Worker pool with bounded priority queue
// ---------------------------------------------------------------------------

/// Higher value = runs sooner. Visible thumbnails use `High`, prefetch uses `Low`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Queued {
    priority: Priority,
    seq: u64,
    task: Task,
}
impl PartialEq for Queued {
    fn eq(&self, o: &Self) -> bool {
        self.priority == o.priority && self.seq == o.seq
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Queued {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // Highest priority first; within a priority, LIFO (newest request first) —
        // the most recently requested thumbnail is the one the user is looking at.
        self.priority.cmp(&o.priority).then(self.seq.cmp(&o.seq))
    }
}

struct PoolShared {
    queue: Mutex<BinaryHeap<Queued>>,
    cv: Condvar,
    shutdown: AtomicBool,
    capacity: usize,
    seq: AtomicU64,
    active: AtomicUsize,
}

/// Error returned when the queue is full; the caller decides whether to drop or retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull;

/// Fixed-size thread pool with a bounded priority queue.
///
/// When the queue is full, [`WorkerPool::submit`] evicts the *lowest priority, oldest*
/// queued task if the new task outranks it, otherwise rejects with [`QueueFull`].
pub struct WorkerPool {
    name: String,
    shared: Arc<PoolShared>,
    threads: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    pub fn new(name: &str, threads: usize, capacity: usize) -> Self {
        let threads = threads.max(1);
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(BinaryHeap::new()),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
            capacity: capacity.max(1),
            seq: AtomicU64::new(0),
            active: AtomicUsize::new(0),
        });
        let handles = (0..threads)
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("{name}-{i}"))
                    .spawn(move || worker_loop(shared))
                    .expect("spawn worker thread")
            })
            .collect();
        Self {
            name: name.to_string(),
            shared,
            threads: handles,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn submit(
        &self,
        priority: Priority,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<(), QueueFull> {
        let mut q = self.shared.queue.lock();
        if q.len() >= self.shared.capacity {
            // Find the weakest queued item (lowest priority, then oldest).
            let weakest = q
                .iter()
                .min_by(|a, b| a.priority.cmp(&b.priority).then(a.seq.cmp(&b.seq)))
                .map(|w| (w.priority, w.seq));
            match weakest {
                Some((wp, ws)) if wp <= priority => {
                    let items: Vec<_> = std::mem::take(&mut *q)
                        .into_vec()
                        .into_iter()
                        .filter(|it| !(it.priority == wp && it.seq == ws))
                        .collect();
                    *q = BinaryHeap::from(items);
                }
                _ => return Err(QueueFull),
            }
        }
        let seq = self.shared.seq.fetch_add(1, Ordering::Relaxed);
        q.push(Queued {
            priority,
            seq,
            task: Box::new(task),
        });
        drop(q);
        self.shared.cv.notify_one();
        Ok(())
    }

    /// Drops all queued (not yet started) tasks. Running tasks should observe their own
    /// [`CancelToken`]s.
    pub fn clear_queue(&self) -> usize {
        let mut q = self.shared.queue.lock();
        let n = q.len();
        q.clear();
        n
    }

    pub fn queued(&self) -> usize {
        self.shared.queue.lock().len()
    }

    pub fn active(&self) -> usize {
        self.shared.active.load(Ordering::Relaxed)
    }

    pub fn is_idle(&self) -> bool {
        self.queued() == 0 && self.active() == 0
    }
}

fn worker_loop(shared: Arc<PoolShared>) {
    loop {
        let task = {
            let mut q = shared.queue.lock();
            loop {
                if shared.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Some(t) = q.pop() {
                    shared.active.fetch_add(1, Ordering::Relaxed);
                    break t.task;
                }
                shared.cv.wait(&mut q);
            }
        };
        // A panicking task (e.g. a buggy decoder on a corrupt file) must not take the
        // worker down with it.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)).is_err() {
            tracing::error!("background task panicked; worker continues");
        }
        shared.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.queue.lock().clear();
        self.shared.cv.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// User-visible job registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Completed,
    CompletedWithErrors,
    Cancelled,
    Failed,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::CompletedWithErrors | Self::Cancelled | Self::Failed
        )
    }
}

pub type JobId = u64;

/// Handle for a long-running user-visible job.
#[derive(Clone)]
pub struct JobHandle {
    pub id: JobId,
    pub title: Arc<str>,
    pub kind: Arc<str>,
    pub cancel: CancelToken,
    pub progress: Arc<Progress>,
    state: Arc<Mutex<JobState>>,
    log: Arc<Mutex<Vec<String>>>,
    started: Instant,
}

impl JobHandle {
    pub fn state(&self) -> JobState {
        let s = *self.state.lock();
        if s == JobState::Running && self.cancel.is_paused() {
            JobState::Paused
        } else {
            s
        }
    }
    pub fn set_state(&self, s: JobState) {
        *self.state.lock() = s;
    }
    /// Append a human-readable line to the job's report (errors, skipped files…).
    pub fn log(&self, line: impl Into<String>) {
        let mut l = self.log.lock();
        if l.len() < 10_000 {
            l.push(line.into());
        }
    }
    pub fn log_lines(&self) -> Vec<String> {
        self.log.lock().clone()
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Registry of user-visible jobs; each job runs on its own dedicated thread
/// (jobs are few and long-lived; the bounded pools are for many short tasks).
#[derive(Clone, Default)]
pub struct JobRegistry {
    jobs: Arc<Mutex<Vec<JobHandle>>>,
    next: Arc<AtomicU64>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns `f` on a new thread. `f` returns `Ok(())` / `Err(msg)`; errors counted in
    /// progress produce `CompletedWithErrors`.
    pub fn spawn<F>(&self, kind: &str, title: impl Into<String>, f: F) -> JobHandle
    where
        F: FnOnce(&JobHandle) -> Result<(), String> + Send + 'static,
    {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let h = JobHandle {
            id,
            title: title.into().into(),
            kind: kind.into(),
            cancel: CancelToken::new(),
            progress: Progress::new(),
            state: Arc::new(Mutex::new(JobState::Running)),
            log: Arc::new(Mutex::new(Vec::new())),
            started: Instant::now(),
        };
        self.jobs.lock().push(h.clone());
        let hh = h.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("job-{id}"))
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&hh)));
                let state = match r {
                    Ok(Ok(())) if hh.cancel.is_cancelled() => JobState::Cancelled,
                    Ok(Ok(())) if hh.progress.errors.load(Ordering::Relaxed) > 0 => {
                        JobState::CompletedWithErrors
                    }
                    Ok(Ok(())) => JobState::Completed,
                    Ok(Err(_)) if hh.cancel.is_cancelled() => JobState::Cancelled,
                    Ok(Err(e)) => {
                        hh.log(format!("failed: {e}"));
                        JobState::Failed
                    }
                    Err(_) => {
                        hh.log("internal error: job panicked");
                        JobState::Failed
                    }
                };
                hh.set_state(state);
            });
        if spawn.is_err() {
            h.log("could not start worker thread");
            h.set_state(JobState::Failed);
        }
        h
    }

    pub fn list(&self) -> Vec<JobHandle> {
        self.jobs.lock().clone()
    }

    pub fn running_count(&self) -> usize {
        self.jobs
            .lock()
            .iter()
            .filter(|j| !j.state().is_finished())
            .count()
    }

    pub fn clear_finished(&self) {
        self.jobs.lock().retain(|j| !j.state().is_finished());
    }

    pub fn cancel_all(&self) {
        for j in self.jobs.lock().iter() {
            j.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn child_token_inherits_cancel() {
        let p = CancelToken::new();
        let c = p.child();
        assert!(!c.is_cancelled());
        p.cancel();
        assert!(c.is_cancelled());
        assert_eq!(c.check(), Err(Cancelled));
    }

    #[test]
    fn pool_runs_tasks_and_survives_panics() {
        let pool = WorkerPool::new("t", 2, 16);
        let (tx, rx) = mpsc::channel();
        pool.submit(Priority::Normal, || panic!("boom")).unwrap();
        for i in 0..5 {
            let tx = tx.clone();
            pool.submit(Priority::Normal, move || tx.send(i).unwrap())
                .unwrap();
        }
        let mut got: Vec<i32> = (0..5)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        got.sort();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn bounded_queue_evicts_low_priority() {
        // One worker blocked so the queue fills.
        let pool = WorkerPool::new("t", 1, 2);
        let (block_tx, block_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        pool.submit(Priority::High, move || {
            started_tx.send(()).unwrap();
            let _ = block_rx.recv();
        })
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        pool.submit(Priority::Low, || {}).unwrap();
        pool.submit(Priority::Low, || {}).unwrap();
        assert_eq!(pool.queued(), 2);
        // High evicts a Low.
        assert!(pool.submit(Priority::High, || {}).is_ok());
        assert_eq!(pool.queued(), 2);
        // Low cannot evict High but may evict the remaining Low (same priority => replace oldest).
        assert!(pool.submit(Priority::Low, || {}).is_ok());
        assert_eq!(pool.queued(), 2);
        drop(block_tx);
    }

    #[test]
    fn registry_reports_states() {
        let reg = JobRegistry::new();
        let ok = reg.spawn("test", "ok", |_| Ok(()));
        let bad = reg.spawn("test", "bad", |_| Err("nope".into()));
        let can = reg.spawn("test", "cancel", |h| {
            h.cancel.cancel();
            h.cancel.check().map_err(|e| e.to_string())
        });
        let t0 = Instant::now();
        while reg.running_count() > 0 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ok.state(), JobState::Completed);
        assert_eq!(bad.state(), JobState::Failed);
        assert_eq!(can.state(), JobState::Cancelled);
    }
}
