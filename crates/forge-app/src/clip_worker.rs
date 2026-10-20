//! Every write of the Windows clipboard by this app goes through one worker thread: our Copy
//! and Cut, updates of a cut's list, and clearing. Only the newest request counts: requests
//! that queued up are replaced by the latest, so an old list can never land after a newer
//! one. A clipboard held open by another app is tried again with backoff; "the clipboard is
//! no longer ours" is an answer, not an error, and is not retried.
//!
//! The worker remembers the token of its last publish, so only our own content is ever
//! replaced or cleared (see `forge_win::ClipboardToken`).

use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use forge_win::{ClipboardEffect, ClipboardToken};

/// Waits between tries when the clipboard cannot be opened or written.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(400),
    Duration::from_millis(1000),
    Duration::from_millis(2500),
];

/// What to do with the clipboard.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// The user's Copy or Cut: a new publish. Skipped if another app has put something on
    /// the clipboard since `baseline` (its sequence number at the Copy or Cut): that content
    /// is newer, and a late or retried publish must not overwrite it.
    Publish {
        paths: Vec<PathBuf>,
        effect: ClipboardEffect,
        baseline: u32,
    },
    /// The cut's list changed: replace our publish with it, if the clipboard still holds it.
    Update {
        paths: Vec<PathBuf>,
        effect: ClipboardEffect,
    },
    /// Take our publish off the clipboard, if it is still there.
    ClearOurs,
    /// Another app's files (Explorer's cut) were pasted here: empty the clipboard if it still
    /// holds exactly that list at that sequence number.
    ClearForeign { paths: Vec<PathBuf>, sequence: u32 },
}

/// A request for the clipboard on behalf of the in-app clipboard `generation` at `revision`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub generation: u64,
    pub revision: u64,
    pub op: Op,
}

/// How a request ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Our list is on the clipboard under this token.
    Published(ClipboardToken),
    /// The clipboard held another app's content: left alone.
    NotOurs,
    /// Done (cleared, or nothing of ours to clear).
    Cleared,
    /// Still failing after every retry.
    Failed(String),
}

/// A finished request, reported back to the app.
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub generation: u64,
    pub revision: u64,
    pub outcome: Outcome,
}

/// The clipboard calls, replaceable in tests.
pub trait Clipboard: Send + 'static {
    /// Publishes `paths`, unless another app has put content on the clipboard since sequence
    /// `baseline` (`Ok(None)`; our own content, from this process, does not count). Checked
    /// and written in one clipboard session.
    fn publish(
        &mut self,
        paths: &[PathBuf],
        effect: ClipboardEffect,
        baseline: u32,
    ) -> Result<Option<ClipboardToken>, String>;
    fn replace(
        &mut self,
        paths: &[PathBuf],
        effect: ClipboardEffect,
        token: ClipboardToken,
    ) -> Result<Option<ClipboardToken>, String>;
    fn clear(&mut self, token: ClipboardToken) -> Result<bool, String>;
    fn clear_foreign(&mut self, paths: &[PathBuf], sequence: u32);
}

/// The real Windows clipboard.
pub struct Windows;

impl Clipboard for Windows {
    fn publish(
        &mut self,
        paths: &[PathBuf],
        effect: ClipboardEffect,
        baseline: u32,
    ) -> Result<Option<ClipboardToken>, String> {
        forge_win::set_files_if_unchanged(paths, effect, baseline).map_err(|e| e.to_string())
    }
    fn replace(
        &mut self,
        paths: &[PathBuf],
        effect: ClipboardEffect,
        token: ClipboardToken,
    ) -> Result<Option<ClipboardToken>, String> {
        forge_win::replace_files_if_token(paths, effect, token).map_err(|e| e.to_string())
    }
    fn clear(&mut self, token: ClipboardToken) -> Result<bool, String> {
        forge_win::clear_if_token(token).map_err(|e| e.to_string())
    }
    fn clear_foreign(&mut self, paths: &[PathBuf], sequence: u32) {
        forge_win::clear_if_owned_files(paths, Some(sequence));
    }
}

/// Handle to the worker thread; it ends when this is dropped.
pub struct ClipWorker {
    tx: Sender<Request>,
}

impl ClipWorker {
    pub fn start(report: impl Fn(Done) + Send + 'static) -> Self {
        Self::start_with(Windows, &BACKOFF, report)
    }

    fn start_with(
        clipboard: impl Clipboard,
        backoff: &'static [Duration],
        report: impl Fn(Done) + Send + 'static,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("forge-clipboard".into())
            .spawn(move || {
                let mut worker = Worker {
                    clipboard,
                    ours: None,
                    backoff,
                };
                while let Ok(first) = rx.recv() {
                    if let Some(done) = worker.serve(first, &rx) {
                        report(done);
                    }
                }
            })
            .expect("spawn the clipboard worker");
        Self { tx }
    }

    pub fn send(&self, generation: u64, revision: u64, op: Op) {
        let _ = self.tx.send(Request {
            generation,
            revision,
            op,
        });
    }
}

/// `next` replaces `queued` (requests for our own content). An update right after a publish
/// of the same cut that never ran is a publish: the cut is not on the clipboard yet.
fn supersede(queued: Request, next: Request) -> Request {
    match (&queued.op, next.op) {
        (Op::Publish { baseline, .. }, Op::Update { paths, effect })
            if queued.generation == next.generation =>
        {
            let baseline = *baseline;
            Request {
                op: Op::Publish {
                    paths,
                    effect,
                    baseline,
                },
                ..next
            }
        }
        (_, op) => Request { op, ..next },
    }
}

struct Worker<C> {
    clipboard: C,
    /// The token of our latest publish, while it may still be on the clipboard.
    ours: Option<ClipboardToken>,
    backoff: &'static [Duration],
}

impl<C: Clipboard> Worker<C> {
    /// Takes a request off the queue. Clearing another app's files is done at once and never
    /// replaces our own requests: it only acts on exactly that content, so it cannot undo a
    /// newer publish of ours. Our own requests are merged into `ours`, newest wins.
    fn take(&mut self, next: Request, ours: &mut Option<Request>) {
        if let Op::ClearForeign { paths, sequence } = &next.op {
            self.clipboard.clear_foreign(paths, *sequence);
            return;
        }
        *ours = Some(match ours.take() {
            Some(queued) => supersede(queued, next),
            None => next,
        });
    }

    /// Carries out `first` and everything queued behind it: the newest of our own requests
    /// is the one done, and the one reported (`None` if there was none).
    fn serve(&mut self, first: Request, rx: &Receiver<Request>) -> Option<Done> {
        let mut ours = None;
        self.take(first, &mut ours);
        for next in rx.try_iter() {
            self.take(next, &mut ours);
        }
        let mut request = ours?;
        let mut tries = 0;
        loop {
            match self.attempt(&request.op) {
                Ok(outcome) => {
                    return Some(Done {
                        generation: request.generation,
                        revision: request.revision,
                        outcome,
                    });
                }
                Err(e) if tries >= self.backoff.len() => {
                    return Some(Done {
                        generation: request.generation,
                        revision: request.revision,
                        outcome: Outcome::Failed(e),
                    });
                }
                Err(_) => {
                    // Wait, unless a newer request comes: that one is served instead.
                    match rx.recv_timeout(self.backoff[tries]) {
                        Ok(next) => {
                            let mut ours = Some(request);
                            self.take(next, &mut ours);
                            for next in rx.try_iter() {
                                self.take(next, &mut ours);
                            }
                            request = ours.expect("the request is still there");
                            tries = 0;
                        }
                        Err(RecvTimeoutError::Timeout) => tries += 1,
                        Err(RecvTimeoutError::Disconnected) => tries += 1,
                    }
                }
            }
        }
    }

    fn attempt(&mut self, op: &Op) -> Result<Outcome, String> {
        match op {
            Op::Publish {
                paths,
                effect,
                baseline,
            } => {
                let Some(token) = self.clipboard.publish(paths, *effect, *baseline)? else {
                    return Ok(Outcome::NotOurs);
                };
                self.ours = Some(token);
                Ok(Outcome::Published(token))
            }
            Op::Update { paths, .. } if paths.is_empty() => self.attempt(&Op::ClearOurs),
            Op::Update { paths, effect } => {
                let Some(token) = self.ours else {
                    return Ok(Outcome::NotOurs);
                };
                match self.clipboard.replace(paths, *effect, token)? {
                    Some(token) => {
                        self.ours = Some(token);
                        Ok(Outcome::Published(token))
                    }
                    None => {
                        self.ours = None;
                        Ok(Outcome::NotOurs)
                    }
                }
            }
            Op::ClearOurs => {
                if let Some(token) = self.ours {
                    // Kept on an error, so the next try (or a later clear) still knows it.
                    self.clipboard.clear(token)?;
                    self.ours = None;
                }
                Ok(Outcome::Cleared)
            }
            Op::ClearForeign { paths, sequence } => {
                // Normally done in `take`.
                self.clipboard.clear_foreign(paths, *sequence);
                Ok(Outcome::Cleared)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Records calls; fails the next `fail` calls; answers "not ours" if `foreign`.
    #[derive(Clone, Default)]
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
        fail: Arc<Mutex<usize>>,
        foreign: Arc<Mutex<bool>>,
        next: Arc<Mutex<u64>>,
        /// Another app wrote the clipboard at this sequence number.
        taken_after: Arc<Mutex<u32>>,
    }

    impl Fake {
        fn call(&self, what: String) -> Result<(), String> {
            self.log.lock().unwrap().push(what);
            let mut fail = self.fail.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err("clipboard busy".into());
            }
            Ok(())
        }
        fn token(&self) -> ClipboardToken {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            ClipboardToken::from_raw(*next)
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Clipboard for Fake {
        fn publish(
            &mut self,
            paths: &[PathBuf],
            _: ClipboardEffect,
            baseline: u32,
        ) -> Result<Option<ClipboardToken>, String> {
            self.call(format!("publish {}", paths.len()))?;
            if *self.taken_after.lock().unwrap() > baseline {
                return Ok(None);
            }
            Ok(Some(self.token()))
        }
        fn replace(
            &mut self,
            paths: &[PathBuf],
            _: ClipboardEffect,
            token: ClipboardToken,
        ) -> Result<Option<ClipboardToken>, String> {
            self.call(format!("replace {} {token:?}", paths.len()))?;
            if *self.foreign.lock().unwrap() {
                return Ok(None);
            }
            Ok(Some(self.token()))
        }
        fn clear(&mut self, token: ClipboardToken) -> Result<bool, String> {
            self.call(format!("clear {token:?}"))?;
            Ok(!*self.foreign.lock().unwrap())
        }
        fn clear_foreign(&mut self, paths: &[PathBuf], sequence: u32) {
            let _ = self.call(format!("clear-foreign {} {sequence}", paths.len()));
        }
    }

    const NO_WAIT: [Duration; 3] = [Duration::ZERO; 3];
    const LONG_WAIT: [Duration; 3] = [Duration::from_secs(30); 3];

    fn worker(fake: &Fake) -> Worker<Fake> {
        Worker {
            clipboard: fake.clone(),
            ours: None,
            backoff: &NO_WAIT,
        }
    }

    fn req(generation: u64, revision: u64, op: Op) -> Request {
        Request {
            generation,
            revision,
            op,
        }
    }

    fn publish(n: usize) -> Op {
        Op::Publish {
            paths: vec![PathBuf::from("a"); n],
            effect: ClipboardEffect::Move,
            baseline: 10,
        }
    }

    fn update(n: usize) -> Op {
        Op::Update {
            paths: vec![PathBuf::from("a"); n],
            effect: ClipboardEffect::Move,
        }
    }

    #[test]
    fn only_the_newest_queued_request_runs() {
        let fake = Fake::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(req(1, 1, update(2))).unwrap();
        tx.send(req(2, 0, publish(3))).unwrap();
        let done = worker(&fake).serve(req(1, 0, publish(4)), &rx).unwrap();
        assert_eq!(fake.log(), vec!["publish 3"], "older lists never land");
        assert_eq!((done.generation, done.revision), (2, 0));
        assert!(matches!(done.outcome, Outcome::Published(_)));
    }

    #[test]
    fn clearing_another_apps_files_never_replaces_our_publish() {
        let fake = Fake::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let foreign = Op::ClearForeign {
            paths: vec![PathBuf::from("x")],
            sequence: 9,
        };
        tx.send(req(0, 0, foreign.clone())).unwrap();
        let done = worker(&fake).serve(req(3, 0, publish(2)), &rx).unwrap();
        assert_eq!(done.generation, 3);
        assert_eq!(fake.log(), vec!["clear-foreign 1 9", "publish 2"]);
        // Alone, it is done and nothing is reported.
        let fake = Fake::default();
        assert_eq!(worker(&fake).serve(req(0, 0, foreign), &rx), None);
        assert_eq!(fake.log(), vec!["clear-foreign 1 9"]);
    }

    #[test]
    fn a_late_publish_never_overwrites_newer_content_of_another_app() {
        let fake = Fake::default();
        let (_tx, rx) = crossbeam_channel::unbounded();
        // The first try finds the clipboard busy; meanwhile another app copies something.
        *fake.fail.lock().unwrap() = 1;
        let taken = fake.taken_after.clone();
        let mut w = Worker {
            clipboard: Hook {
                inner: fake.clone(),
                after_failure: Box::new(move || *taken.lock().unwrap() = 11),
            },
            ours: None,
            backoff: &NO_WAIT,
        };
        let done = w.serve(req(1, 0, publish(1)), &rx).unwrap();
        assert_eq!(
            done.outcome,
            Outcome::NotOurs,
            "the retry found newer content"
        );
        assert_eq!(fake.log(), vec!["publish 1", "publish 1"]);
        assert_eq!(w.ours, None, "nothing of ours was written");
    }

    /// Runs `after_failure` once the wrapped clipboard has failed a call.
    struct Hook {
        inner: Fake,
        after_failure: Box<dyn FnMut() + Send>,
    }

    impl Clipboard for Hook {
        fn publish(
            &mut self,
            paths: &[PathBuf],
            effect: ClipboardEffect,
            baseline: u32,
        ) -> Result<Option<ClipboardToken>, String> {
            let result = self.inner.publish(paths, effect, baseline);
            if result.is_err() {
                (self.after_failure)();
            }
            result
        }
        fn replace(
            &mut self,
            paths: &[PathBuf],
            effect: ClipboardEffect,
            token: ClipboardToken,
        ) -> Result<Option<ClipboardToken>, String> {
            self.inner.replace(paths, effect, token)
        }
        fn clear(&mut self, token: ClipboardToken) -> Result<bool, String> {
            self.inner.clear(token)
        }
        fn clear_foreign(&mut self, paths: &[PathBuf], sequence: u32) {
            self.inner.clear_foreign(paths, sequence);
        }
    }

    #[test]
    fn an_update_of_a_publish_that_never_ran_publishes() {
        assert_eq!(
            supersede(req(1, 0, publish(3)), req(1, 1, update(2))),
            req(1, 1, publish(2))
        );
        // Of another cut, the update stays an update.
        assert_eq!(
            supersede(req(1, 0, publish(3)), req(2, 1, update(2))),
            req(2, 1, update(2))
        );
    }

    #[test]
    fn a_busy_clipboard_is_tried_again_and_our_token_is_kept() {
        let fake = Fake::default();
        let (_tx, rx) = crossbeam_channel::unbounded();
        let mut w = worker(&fake);
        assert!(matches!(
            w.serve(req(1, 0, publish(1)), &rx).unwrap().outcome,
            Outcome::Published(_)
        ));
        let token = w.ours.unwrap();
        *fake.fail.lock().unwrap() = 2;
        assert_eq!(
            w.serve(req(1, 1, Op::ClearOurs), &rx).unwrap().outcome,
            Outcome::Cleared
        );
        assert_eq!(
            fake.log().iter().filter(|c| c.starts_with("clear")).count(),
            3
        );
        assert!(fake.log().contains(&format!("clear {token:?}")));
        assert_eq!(w.ours, None);

        // Failing every time: reported, and the token is still known for a later clear.
        let token = match w.serve(req(2, 0, publish(1)), &rx).unwrap().outcome {
            Outcome::Published(t) => t,
            other => panic!("{other:?}"),
        };
        *fake.fail.lock().unwrap() = 99;
        assert!(matches!(
            w.serve(req(2, 1, Op::ClearOurs), &rx).unwrap().outcome,
            Outcome::Failed(_)
        ));
        assert_eq!(w.ours, Some(token));
    }

    #[test]
    fn a_clipboard_no_longer_ours_is_left_alone() {
        let fake = Fake::default();
        let (_tx, rx) = crossbeam_channel::unbounded();
        let mut w = worker(&fake);
        // Nothing of ours yet: an update does not write anything.
        assert_eq!(
            w.serve(req(1, 1, update(1)), &rx).unwrap().outcome,
            Outcome::NotOurs
        );
        assert!(fake.log().is_empty());

        w.serve(req(1, 0, publish(2)), &rx).unwrap();
        *fake.foreign.lock().unwrap() = true;
        assert_eq!(
            w.serve(req(1, 1, update(1)), &rx).unwrap().outcome,
            Outcome::NotOurs
        );
        assert_eq!(w.ours, None, "never retried, never written again");
        assert_eq!(
            w.serve(req(1, 2, Op::ClearOurs), &rx).unwrap().outcome,
            Outcome::Cleared
        );
        assert_eq!(fake.log().len(), 2, "no clear without a token of ours");
    }

    #[test]
    fn a_newer_request_during_backoff_is_served_instead() {
        let fake = Fake::default();
        *fake.fail.lock().unwrap() = 1;
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut w = Worker {
            clipboard: fake.clone(),
            ours: None,
            backoff: &LONG_WAIT,
        };
        // The first try fails; a newer request arrives during the long wait.
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            tx.send(req(2, 0, publish(5))).unwrap();
        });
        let started = std::time::Instant::now();
        let done = w.serve(req(1, 0, publish(1)), &rx).unwrap();
        sender.join().unwrap();
        assert_eq!(done.generation, 2);
        assert!(matches!(done.outcome, Outcome::Published(_)));
        assert_eq!(fake.log(), vec!["publish 1", "publish 5"]);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "did not sit out the wait"
        );
    }

    #[test]
    fn the_worker_thread_reports_each_result() {
        let fake = Fake::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let worker = ClipWorker::start_with(fake.clone(), &NO_WAIT, move |d| {
            let _ = tx.send(d);
        });
        worker.send(1, 0, publish(1));
        let done = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((done.generation, done.revision), (1, 0));
        let foreign = Op::ClearForeign {
            paths: vec![PathBuf::from("x")],
            sequence: 9,
        };
        worker.send(0, 0, foreign);
        worker.send(1, 1, Op::ClearOurs);
        let done = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((done.revision, done.outcome), (1, Outcome::Cleared));
        assert!(fake.log().contains(&"clear-foreign 1 9".to_string()));
    }
}
