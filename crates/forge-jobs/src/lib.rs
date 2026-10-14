//! Background job infrastructure.
//!
//! * [`CancelToken`] — cheap, cloneable cooperative cancellation (with pause support).
//! * [`Progress`] — lock-free progress counters a UI can poll without blocking workers.
//! * [`WorkerPool`] — a fixed-size pool with a *bounded*, *prioritised* queue so that
//!   browsing can never spawn unlimited decode work (backpressure by design).
//! * [`JobRegistry`] — the list of user-visible long-running jobs (copy queues, indexing…).
//! * [`TransferQueue`] — runs copies and moves one at a time (per disk) in a user-ordered queue.

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
    started: Arc<Mutex<Instant>>,
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
    /// Time since the job started running (a queued job's clock starts when it leaves
    /// the queue).
    pub fn elapsed(&self) -> Duration {
        self.started.lock().elapsed()
    }
}

/// Registry of user-visible jobs; each job runs on its own dedicated thread
/// (jobs are few and long-lived; the bounded pools are for many short tasks).
#[derive(Clone, Default)]
pub struct JobRegistry {
    jobs: Arc<Mutex<Vec<JobHandle>>>,
    next: Arc<AtomicU64>,
    transfers: TransferQueue,
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
        self.spawn_inner(kind, title.into(), None, f, || {})
    }

    /// The queue that orders copies and moves (see [`TransferQueue`]).
    pub fn transfers(&self) -> &TransferQueue {
        &self.transfers
    }

    /// Like [`JobRegistry::spawn`], but the job waits in [`JobRegistry::transfers`] as
    /// [`JobState::Queued`] until its turn, and only then runs `f`. `disks` are the
    /// physical disks the job reads and writes (`None` if unknown). If the job is
    /// cancelled before it starts, `f` never runs and `unstarted` runs instead.
    pub fn spawn_queued<F, U>(
        &self,
        kind: &str,
        title: impl Into<String>,
        disks: DiskSet,
        f: F,
        unstarted: U,
    ) -> JobHandle
    where
        F: FnOnce(&JobHandle) -> Result<(), String> + Send + 'static,
        U: FnOnce() + Send + 'static,
    {
        self.spawn_inner(kind, title.into(), Some(disks), f, unstarted)
    }

    fn spawn_inner<F, U>(
        &self,
        kind: &str,
        title: String,
        queued: Option<DiskSet>,
        f: F,
        unstarted: U,
    ) -> JobHandle
    where
        F: FnOnce(&JobHandle) -> Result<(), String> + Send + 'static,
        U: FnOnce() + Send + 'static,
    {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let h = JobHandle {
            id,
            title: title.into(),
            kind: kind.into(),
            cancel: CancelToken::new(),
            progress: Progress::new(),
            state: Arc::new(Mutex::new(if queued.is_some() {
                JobState::Queued
            } else {
                JobState::Running
            })),
            log: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Mutex::new(Instant::now())),
        };
        // Enqueue before the thread starts, so the queue order is the order of the calls.
        let queue = queued.map(|disks| {
            self.transfers.enqueue(id, disks);
            self.transfers.clone()
        });
        self.jobs.lock().push(h.clone());
        let hh = h.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("job-{id}"))
            .spawn(move || {
                // Held while `f` runs; frees the job's disks when dropped, panic or not.
                let _slot = match &queue {
                    Some(q) => match q.wait_turn(id, &hh.cancel) {
                        Ok(slot) => {
                            *hh.started.lock() = Instant::now();
                            hh.set_state(JobState::Running);
                            Some(slot)
                        }
                        Err(Cancelled) => {
                            hh.log("cancelled before it started");
                            hh.set_state(JobState::Cancelled);
                            unstarted();
                            return;
                        }
                    },
                    None => None,
                };
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
            self.transfers.remove(id);
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
        self.transfers.wake();
    }

    /// Cancels one job. A queued job leaves the queue at once without running.
    pub fn cancel(&self, job: &JobHandle) {
        job.cancel.cancel();
        if self.transfers.remove(job.id) {
            // Show the outcome now; the job's thread confirms it within a moment.
            job.set_state(JobState::Cancelled);
        }
    }

    /// Copies and moves that are waiting in the queue or running.
    pub fn pending_transfers(&self) -> usize {
        self.transfers.waiting().len() + self.transfers.running()
    }
}

// ---------------------------------------------------------------------------
// Transfer queue
// ---------------------------------------------------------------------------

/// Physical disk numbers a transfer reads or writes. `None` means unknown, which is
/// treated as sharing a disk with every other transfer.
pub type DiskSet = Option<Vec<u32>>;

fn disks_overlap(a: &DiskSet, b: &DiskSet) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.iter().any(|d| b.contains(d)),
        _ => true,
    }
}

/// Orders copies and moves. In serial mode (the default) a transfer starts only when no
/// running transfer, and no transfer ahead of it in the queue, uses one of its disks, so
/// transfers between unrelated disks still run side by side. In parallel mode every
/// transfer starts at once. Pausing the queue holds waiting transfers and pauses running
/// ones; "start now" lets one transfer jump the queue.
#[derive(Clone)]
pub struct TransferQueue {
    inner: Arc<QueueInner>,
}

struct QueueInner {
    state: Mutex<QueueState>,
    cv: Condvar,
}

struct QueueState {
    serial: bool,
    paused: bool,
    waiting: Vec<Waiting>,
    running: Vec<Running>,
}

struct Running {
    id: JobId,
    disks: DiskSet,
    cancel: CancelToken,
}

struct Waiting {
    id: JobId,
    disks: DiskSet,
    forced: bool,
}

impl Default for TransferQueue {
    fn default() -> Self {
        Self {
            inner: Arc::new(QueueInner {
                state: Mutex::new(QueueState {
                    serial: true,
                    paused: false,
                    waiting: Vec::new(),
                    running: Vec::new(),
                }),
                cv: Condvar::new(),
            }),
        }
    }
}

/// Held by a running transfer; frees its disks when dropped.
pub struct QueueSlot {
    queue: TransferQueue,
    id: JobId,
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        let id = self.id;
        self.queue.inner.state.lock().running.retain(|r| r.id != id);
        self.queue.inner.cv.notify_all();
    }
}

impl QueueState {
    /// Whether `waiting[at]` may start now.
    fn admissible(&self, at: usize) -> bool {
        let w = &self.waiting[at];
        if w.forced {
            return true;
        }
        if self.paused {
            return false;
        }
        if !self.serial {
            return true;
        }
        !self
            .running
            .iter()
            .any(|r| disks_overlap(&r.disks, &w.disks))
            && !self.waiting[..at]
                .iter()
                .any(|e| disks_overlap(&e.disks, &w.disks))
    }
}

impl TransferQueue {
    pub fn new() -> Self {
        Self::default()
    }

    fn enqueue(&self, id: JobId, disks: DiskSet) {
        self.inner.state.lock().waiting.push(Waiting {
            id,
            disks,
            forced: false,
        });
        self.inner.cv.notify_all();
    }

    /// Blocks until the job may run, then returns its slot. `Err` if `cancel` fires or
    /// the job is removed first; the job has then left the queue.
    pub fn wait_turn(&self, id: JobId, cancel: &CancelToken) -> Result<QueueSlot, Cancelled> {
        let mut st = self.inner.state.lock();
        loop {
            let at = st.waiting.iter().position(|w| w.id == id);
            let at = match at {
                Some(at) if !cancel.is_cancelled() => at,
                _ => {
                    st.waiting.retain(|w| w.id != id);
                    drop(st);
                    self.inner.cv.notify_all();
                    return Err(Cancelled);
                }
            };
            if st.admissible(at) {
                let w = st.waiting.remove(at);
                st.running.push(Running {
                    id,
                    disks: w.disks,
                    cancel: cancel.clone(),
                });
                drop(st);
                // Jobs behind this one may have waited only for it to leave the queue.
                self.inner.cv.notify_all();
                return Ok(QueueSlot {
                    queue: self.clone(),
                    id,
                });
            }
            // Timed, so a cancelled token is noticed even without a notification.
            self.inner.cv.wait_for(&mut st, Duration::from_millis(100));
        }
    }

    /// Removes a waiting job; true if it was waiting.
    pub fn remove(&self, id: JobId) -> bool {
        let mut st = self.inner.state.lock();
        let before = st.waiting.len();
        st.waiting.retain(|w| w.id != id);
        let removed = st.waiting.len() != before;
        drop(st);
        self.inner.cv.notify_all();
        removed
    }

    fn wake(&self) {
        self.inner.cv.notify_all();
    }

    /// Waiting job ids, next to run first.
    pub fn waiting(&self) -> Vec<JobId> {
        self.inner
            .state
            .lock()
            .waiting
            .iter()
            .map(|w| w.id)
            .collect()
    }

    /// 1-based place of a waiting job in the queue.
    pub fn position(&self, id: JobId) -> Option<usize> {
        self.inner
            .state
            .lock()
            .waiting
            .iter()
            .position(|w| w.id == id)
            .map(|i| i + 1)
    }

    /// Number of transfers holding a slot.
    pub fn running(&self) -> usize {
        self.inner.state.lock().running.len()
    }

    /// Moves a waiting job one place earlier (`up`) or later. False if it cannot move.
    pub fn move_by(&self, id: JobId, up: bool) -> bool {
        let mut st = self.inner.state.lock();
        let Some(at) = st.waiting.iter().position(|w| w.id == id) else {
            return false;
        };
        let to = if up { at.checked_sub(1) } else { Some(at + 1) };
        match to {
            Some(to) if to < st.waiting.len() => {
                st.waiting.swap(at, to);
                drop(st);
                self.inner.cv.notify_all();
                true
            }
            _ => false,
        }
    }

    /// Lets a waiting job start at once, even if the queue is busy or paused.
    pub fn start_now(&self, id: JobId) -> bool {
        let mut st = self.inner.state.lock();
        let Some(w) = st.waiting.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        w.forced = true;
        drop(st);
        self.inner.cv.notify_all();
        true
    }

    /// Pauses or resumes the whole queue: waiting transfers stay queued and running ones
    /// pause (or resume) at their next safe point. A transfer started with "start now"
    /// while the queue is paused runs until paused itself.
    pub fn set_paused(&self, paused: bool) {
        let mut st = self.inner.state.lock();
        st.paused = paused;
        for r in &st.running {
            r.cancel.set_paused(paused);
        }
        drop(st);
        self.inner.cv.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        self.inner.state.lock().paused
    }

    /// One at a time per disk (`true`, the default) or all at once.
    pub fn set_serial(&self, serial: bool) {
        let mut st = self.inner.state.lock();
        if st.serial != serial {
            st.serial = serial;
            drop(st);
            self.inner.cv.notify_all();
        }
    }

    pub fn is_serial(&self) -> bool {
        self.inner.state.lock().serial
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

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !cond() {
            assert!(t0.elapsed() < Duration::from_secs(5), "timed out: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A queued job that records its start in `order` and then runs until `gate` gets a
    /// message (or is dropped).
    fn gated(
        reg: &JobRegistry,
        name: &'static str,
        disks: DiskSet,
        order: &Arc<Mutex<Vec<&'static str>>>,
    ) -> (JobHandle, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel::<()>();
        let order = order.clone();
        let h = reg.spawn_queued(
            "transfer",
            name,
            disks,
            move |_| {
                order.lock().push(name);
                let _ = rx.recv();
                Ok(())
            },
            || {},
        );
        (h, tx)
    }

    fn quiet(reg: &JobRegistry) {
        wait_until("all finished", || reg.running_count() == 0);
    }

    #[test]
    fn queue_runs_one_at_a_time_in_order() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, ga) = gated(&reg, "a", None, &order);
        let (b, gb) = gated(&reg, "b", None, &order);
        let (c, gc) = gated(&reg, "c", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(b.state(), JobState::Queued);
        assert_eq!(c.state(), JobState::Queued);
        assert_eq!(reg.transfers().waiting(), vec![b.id, c.id]);
        assert_eq!(reg.transfers().position(c.id), Some(2));
        assert_eq!(reg.pending_transfers(), 3);
        drop(ga);
        wait_until("b runs", || b.state() == JobState::Running);
        assert_eq!(c.state(), JobState::Queued);
        drop(gb);
        drop(gc);
        quiet(&reg);
        assert_eq!(*order.lock(), vec!["a", "b", "c"]);
        assert_eq!(reg.pending_transfers(), 0);
    }

    #[test]
    fn queue_reordering_changes_what_runs_next() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, ga) = gated(&reg, "a", None, &order);
        let (b, gb) = gated(&reg, "b", None, &order);
        let (c, gc) = gated(&reg, "c", None, &order);
        let (d, gd) = gated(&reg, "d", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        let q = reg.transfers();
        assert!(q.move_by(d.id, true));
        assert!(q.move_by(d.id, true));
        assert!(!q.move_by(d.id, true), "already first");
        assert!(q.move_by(b.id, false));
        assert!(!q.move_by(b.id, false), "already last");
        assert!(!q.move_by(a.id, true), "running jobs are not in the queue");
        assert_eq!(q.waiting(), vec![d.id, c.id, b.id]);
        drop((ga, gb, gc, gd));
        quiet(&reg);
        assert_eq!(*order.lock(), vec!["a", "d", "c", "b"]);
    }

    #[test]
    fn cancelling_a_queued_job_never_runs_it() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, ga) = gated(&reg, "a", None, &order);
        let ran = Arc::new(AtomicBool::new(false));
        let unstarted = Arc::new(AtomicBool::new(false));
        let (r, u) = (ran.clone(), unstarted.clone());
        let b = reg.spawn_queued(
            "transfer",
            "b",
            None,
            move |_| {
                r.store(true, Ordering::SeqCst);
                Ok(())
            },
            move || u.store(true, Ordering::SeqCst),
        );
        let (c, gc) = gated(&reg, "c", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        reg.cancel(&b);
        assert_eq!(b.state(), JobState::Cancelled);
        assert_eq!(reg.transfers().waiting(), vec![c.id]);
        wait_until("unstarted callback", || unstarted.load(Ordering::SeqCst));
        drop((ga, gc));
        quiet(&reg);
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(b.state(), JobState::Cancelled);
        assert_eq!(*order.lock(), vec!["a", "c"]);
    }

    #[test]
    fn cancel_all_releases_queued_jobs() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, _ga) = gated(&reg, "a", None, &order);
        let (b, _gb) = gated(&reg, "b", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        reg.cancel_all();
        wait_until("b cancelled", || b.state() == JobState::Cancelled);
        assert_eq!(*order.lock(), vec!["a"]);
    }

    #[test]
    fn pausing_the_queue_holds_waiting_and_pauses_running_jobs() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, ga) = gated(&reg, "a", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        let q = reg.transfers();
        q.set_paused(true);
        assert!(q.is_paused());
        assert_eq!(a.state(), JobState::Paused);
        let (b, gb) = gated(&reg, "b", None, &order);
        drop(ga);
        wait_until("a done", || a.state() == JobState::Completed);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(b.state(), JobState::Queued, "a paused queue starts nothing");
        q.set_paused(false);
        wait_until("b runs", || b.state() == JobState::Running);
        assert!(!b.cancel.is_paused());
        drop(gb);
        quiet(&reg);
        assert_eq!(*order.lock(), vec!["a", "b"]);
    }

    #[test]
    fn start_now_jumps_the_queue_even_when_paused() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, ga) = gated(&reg, "a", None, &order);
        let (b, gb) = gated(&reg, "b", None, &order);
        let (c, gc) = gated(&reg, "c", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        reg.transfers().set_paused(true);
        assert!(reg.transfers().start_now(c.id));
        wait_until("c runs beside a", || c.state() == JobState::Running);
        assert_eq!(b.state(), JobState::Queued);
        reg.transfers().set_paused(false);
        drop(gc);
        drop(ga);
        wait_until("b runs", || b.state() == JobState::Running);
        drop(gb);
        quiet(&reg);
        assert_eq!(*order.lock(), vec!["a", "c", "b"]);
    }

    #[test]
    fn transfers_on_different_disks_run_side_by_side() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, ga) = gated(&reg, "a", Some(vec![0, 1]), &order);
        let (b, gb) = gated(&reg, "b", Some(vec![2]), &order);
        let (c, gc) = gated(&reg, "c", Some(vec![1, 3]), &order);
        // Unknown disks wait for everything ahead of them.
        let (d, gd) = gated(&reg, "d", None, &order);
        // Disk 4 is free, but d (unknown disks) is ahead of it.
        let (e, ge) = gated(&reg, "e", Some(vec![4]), &order);
        wait_until("a and b run", || {
            a.state() == JobState::Running && b.state() == JobState::Running
        });
        std::thread::sleep(Duration::from_millis(150));
        for j in [&c, &d, &e] {
            assert_eq!(j.state(), JobState::Queued, "{}", j.title);
        }
        drop(ga);
        wait_until("c runs", || c.state() == JobState::Running);
        assert_eq!(d.state(), JobState::Queued);
        drop(gb);
        drop(gc);
        wait_until("d runs", || d.state() == JobState::Running);
        assert_eq!(e.state(), JobState::Queued);
        drop(gd);
        drop(ge);
        quiet(&reg);
    }

    #[test]
    fn parallel_mode_starts_everything() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (a, _ga) = gated(&reg, "a", None, &order);
        wait_until("a runs", || a.state() == JobState::Running);
        let (b, _gb) = gated(&reg, "b", None, &order);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(b.state(), JobState::Queued);
        reg.transfers().set_serial(false);
        wait_until("b runs", || b.state() == JobState::Running);
        let (c, _gc) = gated(&reg, "c", None, &order);
        wait_until("c runs", || c.state() == JobState::Running);
    }

    #[test]
    fn a_panicking_transfer_frees_the_queue() {
        let reg = JobRegistry::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let a = reg.spawn_queued("transfer", "a", None, |_| panic!("boom"), || {});
        let (b, gb) = gated(&reg, "b", None, &order);
        wait_until("b runs", || b.state() == JobState::Running);
        assert_eq!(a.state(), JobState::Failed);
        drop(gb);
        quiet(&reg);
    }
}
