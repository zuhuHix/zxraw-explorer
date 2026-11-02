//! One window per user session: a second launch hands its paths to the running window.
//!
//! The running window owns a named pipe, `\\.\pipe\ZXrawExplorer-<SID>-<session>-<IL>-<tag>`:
//! the user's SID, the Terminal Services session, the integrity level (RID) and a tag that
//! separates installed, development and test builds. A launch first tries to create that pipe
//! with `FILE_FLAG_FIRST_PIPE_INSTANCE`; Windows lets exactly one process do that, so this is
//! the first-instance test, with no window search and no separate mutex. The loser connects
//! as a client, sends an [encoded request](encode_request) and waits for the answer.
//!
//! Why a pipe rather than `WM_COPYDATA` to a found window: the pipe exists from the first
//! moments of startup (before the window), so requests that arrive while the first instance
//! is still starting are queued instead of finding no window; the receiver can ask who sent
//! a message (`GetNamedPipeClientProcessId`) and check that process's user, session and
//! integrity level; the sender can check the same of the receiver, so a pipe squatted by
//! another user is never trusted; and every step has a timeout. A window message needs a
//! uniquely findable window and a hook in the window procedure, and says nothing about the
//! sender.
//!
//! Security: the pipe's DACL grants access to the current user only, its mandatory label is
//! the owner's integrity level (no write-up from lower levels), remote clients are refused,
//! and both ends compare user SID, session and integrity level with their own and drop the
//! connection on any difference. Elevated and normal windows therefore never exchange
//! requests (the integrity level is in the name as well): each forms its own group. The
//! payload is untrusted: it is size-limited, versioned, must be valid UTF-16 without NULs,
//! and can only carry absolute local or UNC paths, which the app only navigates to.
//!
//! Races: the pipe always has a listening instance while the owner runs (the next one is
//! created before a connected one is handled), so a second launch never sees "no owner"
//! while one exists. A request is answered "accepted" at the moment the window takes it
//! ([`Inbox::take`] hands the answer to the waiting pipe thread directly). The receiver holds
//! the answer while the window is still starting, for as long as the sender said it would
//! wait minus [`REPLY_MARGIN`], and for at most [`UI_WAIT`] once the window is up. A request
//! not taken by then is withdrawn and answered "busy" (or "closing" once the window began
//! closing, see [`Inbox::close`]), and the sender opens a window of its own.
//!
//! The rule when in doubt is that the sender opens its own window: a launch is never lost.
//! Under rare scheduling delays (the "accepted" answer arrives after the sender stopped
//! waiting) a launch of paths may also open its own window, besides the tab in the running
//! one. That extra window is accepted for opening folders.
//!
//! Actions (a request with an [action](Handoff::action), e.g. "extract here" from the
//! Explorer menu) must run exactly once, so they take one more step: after reading
//! "accepted" the sender confirms with a commit message, and the window runs the action
//! only once that confirmation arrived. A sender that stopped waiting never confirms (it
//! closes the pipe and runs the action in its own window), and the window then drops the
//! action. The receiver waits up to [`COMMIT_WAIT`] for the confirmation; only a sender
//! stalled that long between reading the answer and confirming it could lose an action.
//!
//! Versions: the protocol version is part of the pipe name, so windows of different
//! versions never talk to each other. While an update overlaps (the old window still open,
//! the new executable launched), a launch of the new version opens its own window.
//!
//! Identity: the sender also checks that the pipe's owner runs the same executable file as
//! itself (volume serial and file index), so another program of the same user that creates
//! the pipe first cannot silently swallow launches. Same-user processes are otherwise outside
//! the threat model: they can already do far more to this user than receive a folder path.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Mutex;
use std::time::Duration;

#[cfg(not(windows))]
use crate::Result;
use crate::WinError;

/// Payload format version, also part of the pipe name. A receiver refuses any other version.
/// 2 added the sender's wait; 3 the action and its commit step, and larger requests (a
/// whole Explorer selection).
pub const PROTOCOL_VERSION: u32 = 3;
/// Largest accepted request, in bytes.
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
/// Most paths in one request.
pub const MAX_PATHS: usize = 8192;
/// Longest action name, in UTF-16 units.
pub const MAX_ACTION_UNITS: usize = 32;
/// How long the receiver waits for an accepted action's commit message (see the module
/// notes) before dropping it.
pub const COMMIT_WAIT: Duration = Duration::from_secs(60);
/// Longest accepted path or working directory, in UTF-16 units (the Win32 maximum).
pub const MAX_PATH_UNITS: usize = 32_767;
/// How long the receiver waits for a ready window to take a request before answering busy.
pub const UI_WAIT: Duration = Duration::from_millis(1500);
/// How long a sender waits in total before starting a window of its own. Covers a window
/// that is still starting (its first frame), which the receiver waits for.
pub const FORWARD_TIMEOUT: Duration = Duration::from_millis(4000);
/// The receiver answers this long before the sender's stated wait runs out, so the answer
/// arrives while the sender still listens (see the module notes on rare late answers).
pub const REPLY_MARGIN: Duration = Duration::from_secs(1);
/// Upper bound on the wait a sender may ask for.
const MAX_REPLY_WAIT: Duration = Duration::from_secs(10);
/// Byte offset of the sender's wait (`u32` milliseconds) in a request.
const WAIT_OFFSET: usize = 8;

const REQUEST_MAGIC: [u8; 4] = *b"ZXOR";
const REPLY_MAGIC: [u8; 4] = *b"ZXRP";
/// The sender's confirmation after an "accepted" answer.
const COMMIT: [u8; 8] = *b"ZXCM\x01\0\0\0";

/// Why a request payload was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadError {
    TooLarge(usize),
    BadMagic,
    WrongVersion(u32),
    Truncated,
    TrailingBytes,
    TooManyPaths(usize),
    StringTooLong(usize),
    InvalidUtf16,
    ContainsNul,
    /// A relative path came without an absolute working directory to resolve it against.
    NoWorkingDirectory,
    /// Not an absolute drive or UNC path after resolving (e.g. `C:foo`, `\\.\pipe\x`).
    NotAbsolute(String),
    /// The action name is empty, too long or not lower-case letters, digits and dashes.
    BadAction(String),
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "request of {n} bytes is over the {MAX_PAYLOAD}-byte limit"),
            Self::BadMagic => f.write_str("not an open request"),
            Self::WrongVersion(v) => write!(f, "request version {v}, expected {PROTOCOL_VERSION}"),
            Self::Truncated => f.write_str("request ends early"),
            Self::TrailingBytes => f.write_str("request has trailing bytes"),
            Self::TooManyPaths(n) => write!(f, "{n} paths, at most {MAX_PATHS}"),
            Self::StringTooLong(n) => write!(f, "a path of {n} UTF-16 units"),
            Self::InvalidUtf16 => f.write_str("a path is not valid UTF-16"),
            Self::ContainsNul => f.write_str("a path contains NUL"),
            Self::NoWorkingDirectory => f.write_str("a relative path without a working directory"),
            Self::NotAbsolute(p) => write!(f, "not an absolute drive or network path: {p}"),
            Self::BadAction(a) => write!(f, "not an action name: {a:?}"),
        }
    }
}

impl std::error::Error for PayloadError {}

/// Encodes an open request: the sender's working directory (for relative paths) and the
/// paths as given on its command line. No paths means "just bring the window forward".
pub fn encode_request(cwd: &Path, paths: &[PathBuf]) -> std::result::Result<Vec<u8>, PayloadError> {
    encode_action_request(cwd, paths, None)
}

/// Encodes a request to run `action` (e.g. `extract-here`) on `paths`, or to open them when
/// `action` is `None`.
///
/// Layout, little-endian: magic `ZXOR`, `u32` version, `u32` milliseconds the sender waits
/// for the answer (filled in when sending, see [`set_reply_wait`]), the working directory,
/// the action (empty = open), a `u32` path count, then each path. A string is a `u32` count
/// of UTF-16 units followed by the units.
pub fn encode_action_request(
    cwd: &Path,
    paths: &[PathBuf],
    action: Option<&str>,
) -> std::result::Result<Vec<u8>, PayloadError> {
    if paths.len() > MAX_PATHS {
        return Err(PayloadError::TooManyPaths(paths.len()));
    }
    if let Some(action) = action
        && !valid_action(action)
    {
        return Err(PayloadError::BadAction(action.to_string()));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&REQUEST_MAGIC);
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    put_string(&mut out, cwd.as_os_str())?;
    put_string(&mut out, std::ffi::OsStr::new(action.unwrap_or("")))?;
    out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    for path in paths {
        put_string(&mut out, path.as_os_str())?;
    }
    if out.len() > MAX_PAYLOAD {
        return Err(PayloadError::TooLarge(out.len()));
    }
    Ok(out)
}

/// Lower-case ASCII letters, digits and dashes, at most [`MAX_ACTION_UNITS`] long.
fn valid_action(action: &str) -> bool {
    (1..=MAX_ACTION_UNITS).contains(&action.len())
        && action
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `request` (from [`encode_action_request`]) carries an action, which the sender
/// must confirm after the answer.
fn request_has_action(request: &[u8]) -> bool {
    decode_request(request).is_ok_and(|r| r.action.is_some())
}

/// Sets how long the sender of `request` (from [`encode_request`]) waits for the answer.
pub fn set_reply_wait(request: &mut [u8], wait: Duration) {
    if let Some(field) = request.get_mut(WAIT_OFFSET..WAIT_OFFSET + 4) {
        let ms = wait.as_millis().min(u128::from(u32::MAX)) as u32;
        field.copy_from_slice(&ms.to_le_bytes());
    }
}

fn put_string(out: &mut Vec<u8>, s: &std::ffi::OsStr) -> std::result::Result<(), PayloadError> {
    let units = wide_units(s);
    if units.len() > MAX_PATH_UNITS {
        return Err(PayloadError::StringTooLong(units.len()));
    }
    out.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

#[cfg(windows)]
fn wide_units(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().collect()
}

#[cfg(not(windows))]
fn wide_units(s: &std::ffi::OsStr) -> Vec<u16> {
    s.to_string_lossy().encode_utf16().collect()
}

/// A validated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Absolute paths to open (or to run the action on); empty = only bring the window
    /// forward.
    pub paths: Vec<PathBuf>,
    /// The command to run on the paths; `None` opens them.
    pub action: Option<String>,
    /// How long the sender waits for the answer (at most 10 s).
    pub reply_within: Duration,
}

/// Decodes and validates a request. Relative paths are resolved against the sender's
/// working directory.
pub fn decode_request(bytes: &[u8]) -> std::result::Result<Request, PayloadError> {
    if bytes.len() > MAX_PAYLOAD {
        return Err(PayloadError::TooLarge(bytes.len()));
    }
    let mut r = Reader(bytes);
    if r.take(4)? != REQUEST_MAGIC {
        return Err(PayloadError::BadMagic);
    }
    let version = r.u32()?;
    if version != PROTOCOL_VERSION {
        return Err(PayloadError::WrongVersion(version));
    }
    let reply_within = Duration::from_millis(u64::from(r.u32()?)).min(MAX_REPLY_WAIT);
    let cwd = r.string()?;
    let action = r.string()?;
    let action = if action.is_empty() {
        None
    } else if valid_action(&action) {
        Some(action)
    } else {
        return Err(PayloadError::BadAction(action));
    };
    let count = r.u32()? as usize;
    if count > MAX_PATHS {
        return Err(PayloadError::TooManyPaths(count));
    }
    let mut raw = Vec::with_capacity(count);
    for _ in 0..count {
        raw.push(r.string()?);
    }
    if !r.0.is_empty() {
        return Err(PayloadError::TrailingBytes);
    }
    let cwd = (!cwd.is_empty()).then(|| PathBuf::from(cwd));
    if let Some(cwd) = &cwd
        && !is_plain_absolute(cwd)
    {
        return Err(PayloadError::NotAbsolute(cwd.display().to_string()));
    }
    let paths = raw
        .into_iter()
        .map(|p| resolve(cwd.as_deref(), &p))
        .collect::<std::result::Result<_, _>>()?;
    Ok(Request {
        paths,
        action,
        reply_within,
    })
}

/// `path` as an absolute path: kept if it already is one, else joined to `cwd`.
fn resolve(cwd: Option<&Path>, path: &str) -> std::result::Result<PathBuf, PayloadError> {
    let path = Path::new(path);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // `\foo` keeps cwd's drive; `C:foo` (drive-relative) replaces cwd and stays relative,
        // so it is refused below.
        cwd.ok_or(PayloadError::NoWorkingDirectory)?.join(path)
    };
    if is_plain_absolute(&resolved) {
        Ok(resolved)
    } else {
        Err(PayloadError::NotAbsolute(path.display().to_string()))
    }
}

/// An absolute drive (`C:\`) or UNC (`\\server\share`) path, also in their `\\?\` forms.
/// Device paths (`\\.\…`, `\\?\GLOBALROOT\…`) are refused: they name no folder to open.
fn is_plain_absolute(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => matches!(
            prefix.kind(),
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) | Prefix::UNC(..) | Prefix::VerbatimUNC(..)
        ),
        // Only reachable off Windows, where `/` paths are absolute (unit tests).
        _ => !cfg!(windows),
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> std::result::Result<&'a [u8], PayloadError> {
        if self.0.len() < n {
            return Err(PayloadError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u32(&mut self) -> std::result::Result<u32, PayloadError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> std::result::Result<String, PayloadError> {
        let units = self.u32()? as usize;
        if units > MAX_PATH_UNITS {
            return Err(PayloadError::StringTooLong(units));
        }
        let bytes = self.take(units * 2)?;
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if wide.contains(&0) {
            return Err(PayloadError::ContainsNul);
        }
        String::from_utf16(&wide).map_err(|_| PayloadError::InvalidUtf16)
    }
}

/// The receiver's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Queued for the window (taken, or held until the starting window is ready).
    Accepted,
    /// The window did not take it in time (hung or blocked); it was withdrawn.
    Busy,
    /// The window is closing.
    Closing,
    /// The payload was invalid.
    Rejected,
}

impl Reply {
    fn encode(self) -> [u8; 8] {
        let code: u32 = match self {
            Self::Accepted => 0,
            Self::Busy => 1,
            Self::Closing => 2,
            Self::Rejected => 3,
        };
        let mut out = [0; 8];
        out[..4].copy_from_slice(&REPLY_MAGIC);
        out[4..].copy_from_slice(&code.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 8 || bytes[..4] != REPLY_MAGIC {
            return None;
        }
        match u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) {
            0 => Some(Self::Accepted),
            1 => Some(Self::Busy),
            2 => Some(Self::Closing),
            3 => Some(Self::Rejected),
            _ => None,
        }
    }
}

type Waker = Box<dyn Fn() + Send + Sync>;

/// A launch another process handed over: paths to open, or an action to run on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The command to run (e.g. `extract-here`); `None` opens the paths.
    pub action: Option<String>,
    pub paths: Vec<PathBuf>,
}

/// A request waiting for the window, with the channel its pipe thread waits on for the answer.
struct Pending {
    id: u64,
    handoff: Handoff,
    answer: std::sync::mpsc::SyncSender<Reply>,
}

#[derive(Default)]
struct InboxState {
    queue: VecDeque<Pending>,
    /// Actions the window took, waiting for their sender's commit message.
    awaiting: Vec<(u64, Handoff)>,
    /// Committed actions, handed out on the next take.
    committed: VecDeque<Handoff>,
    next_id: u64,
    /// The window has taken requests at least once: from then on, an untaken request means
    /// the window is not responding.
    ready: bool,
    closing: bool,
}

/// Requests received for the window, in arrival order. The pipe threads add to it; the
/// window takes from it every frame ([`Inbox::take`]).
#[derive(Default)]
pub struct Inbox {
    state: Mutex<InboxState>,
    waker: Mutex<Option<Waker>>,
    /// The window to restore and bring to the front, as an `HWND` value; 0 = none yet.
    window: std::sync::atomic::AtomicIsize,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called when a request arrives, from a pipe thread (e.g. to request a repaint).
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(waker));
    }

    /// The window that each request restores and brings to the front.
    pub fn set_window(&self, hwnd: isize) {
        self.window.store(hwnd, std::sync::atomic::Ordering::Relaxed);
    }

    /// The paths of every launch [`take_handoffs`](Self::take_handoffs) hands out.
    pub fn take(&self) -> Vec<Vec<PathBuf>> {
        self.take_handoffs().into_iter().map(|h| h.paths).collect()
    }

    /// Takes every waiting request and marks the window ready. Each request is answered
    /// "accepted" here, at the moment it is taken. Opens are handed out at once; an action
    /// is handed out by a later take, once its sender confirmed it (see the module notes),
    /// and never if it did not.
    pub fn take_handoffs(&self) -> Vec<Handoff> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.ready = true;
        let mut out: Vec<Handoff> = state.committed.drain(..).collect();
        let queue = std::mem::take(&mut state.queue);
        for pending in queue {
            // Never blocks: the channel holds one answer and only this one is sent.
            let _ = pending.answer.try_send(Reply::Accepted);
            if pending.handoff.action.is_some() {
                state.awaiting.push((pending.id, pending.handoff));
            } else {
                out.push(pending.handoff);
            }
        }
        out
    }

    /// The sender of action `id` confirmed it: the next take hands it out.
    fn commit(&self, id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = state.awaiting.iter().position(|(at, _)| *at == id) {
            let (_, handoff) = state.awaiting.remove(i);
            state.committed.push_back(handoff);
        }
        drop(state);
        if let Some(wake) = &*self.waker.lock().unwrap_or_else(|p| p.into_inner()) {
            wake();
        }
    }

    /// The sender of action `id` never confirmed it (it runs the action itself): drop it.
    fn abandon(&self, id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.awaiting.retain(|(at, _)| *at != id);
    }

    /// From now on, requests are answered [`Reply::Closing`] so their senders start their
    /// own window; requests queued but not taken yet are withdrawn and answered the same.
    /// What the window already took is its own.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closing = true;
        for pending in state.queue.drain(..) {
            let _ = pending.answer.try_send(Reply::Closing);
        }
    }

    pub fn is_closing(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closing
    }

    /// Queues `paths` and answers for the window once it took them, or withdraws them: see
    /// [`Reply`]. Waits at most `wait` while the window is starting and at most `ui_wait`
    /// (and `wait`) once it is up.
    pub fn deliver(&self, paths: Vec<PathBuf>, wait: Duration, ui_wait: Duration) -> Reply {
        self.deliver_handoff(Handoff { action: None, paths }, wait, ui_wait).0
    }

    /// [`deliver`](Self::deliver) for any handoff. For an accepted action, also returns the
    /// ticket its commit message ([`Inbox::commit`]) or its absence ([`Inbox::abandon`])
    /// is reported with.
    pub fn deliver_handoff(
        &self,
        handoff: Handoff,
        wait: Duration,
        ui_wait: Duration,
    ) -> (Reply, Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing {
            return (Reply::Closing, None);
        }
        let id = state.next_id;
        state.next_id += 1;
        let is_action = handoff.action.is_some();
        let (answer, reply) = std::sync::mpsc::sync_channel(1);
        state.queue.push_back(Pending { id, handoff, answer });
        let ready = state.ready;
        drop(state);
        if let Some(wake) = &*self.waker.lock().unwrap_or_else(|p| p.into_inner()) {
            wake();
        }
        self.bring_to_front();
        // A starting window gets the sender's whole wait; a running one that does not take a
        // request within `ui_wait` is not responding.
        let limit = if ready { wait.min(ui_wait) } else { wait };
        let ticket = |answer: Reply| (answer, (is_action && answer == Reply::Accepted).then_some(id));
        if let Ok(answer) = reply.recv_timeout(limit) {
            return ticket(answer);
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match state.queue.iter().position(|pending| pending.id == id) {
            Some(i) => {
                state.queue.remove(i);
                (Reply::Busy, None)
            }
            // Taken or withdrawn just now: its answer was sent under the lock held above.
            None => ticket(reply.try_recv().unwrap_or(Reply::Busy)),
        }
    }

    /// Restores the window if minimized and brings it to the front. Asynchronous, so a hung
    /// window cannot block the pipe thread. The sender called `AllowSetForegroundWindow`
    /// for this process first, so Windows lets it take the foreground.
    fn bring_to_front(&self) {
        let hwnd = self.window.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(windows)]
        if hwnd != 0 {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindowAsync,
            };
            let hwnd = HWND(hwnd as *mut core::ffi::c_void);
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindowAsync(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
        #[cfg(not(windows))]
        let _ = hwnd;
    }
}

/// The result of [`claim_or_forward`].
pub enum Startup {
    /// This process owns the pipe and should open its window.
    Primary(Server),
    /// The running window took the request; this process should exit.
    Forwarded,
    /// Open a window of its own; the text says why forwarding did not happen.
    Standalone(String),
}

/// Why forwarding failed.
#[derive(Debug)]
pub enum ForwardError {
    /// No process owns the pipe (it may have just exited).
    NoServer,
    /// The owner is another user, session or integrity level.
    Untrusted(String),
    TimedOut(&'static str),
    Refused(Reply),
    Failed(WinError),
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoServer => f.write_str("no running window"),
            Self::Untrusted(why) => write!(f, "the pipe's owner is not trusted: {why}"),
            Self::TimedOut(step) => write!(f, "the running window did not answer in time ({step})"),
            Self::Refused(Reply::Busy) => f.write_str("the running window is not responding"),
            Self::Refused(Reply::Closing) => f.write_str("the running window is closing"),
            Self::Refused(reply) => write!(f, "the running window refused the request ({reply:?})"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Becomes the pipe's owner, or hands `request` (from [`encode_request`]) to the owner.
/// Retries while the owner seems to have just exited, up to `timeout` in total.
pub fn claim_or_forward(
    channel: &Channel,
    inbox: std::sync::Arc<Inbox>,
    request: &[u8],
    timeout: Duration,
) -> Startup {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match Server::claim(channel, inbox.clone()) {
            Ok(Some(server)) => return Startup::Primary(server),
            Ok(None) => {}
            Err(e) => return Startup::Standalone(format!("could not create the instance pipe: {e}")),
        }
        match forward(channel, request, deadline) {
            Ok(()) => return Startup::Forwarded,
            Err(ForwardError::NoServer) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Startup::Standalone(e.to_string()),
        }
    }
}

/// Keeps trying to become the pipe's owner in the background (e.g. after the previous owner
/// was closing), for up to `for_how_long`, then serves until `inbox` is closed.
pub fn claim_later(channel: Channel, inbox: std::sync::Arc<Inbox>, for_how_long: Duration) {
    let spawned = std::thread::Builder::new()
        .name("instance-claim".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + for_how_long;
            while std::time::Instant::now() < deadline && !inbox.is_closing() {
                match Server::claim(&channel, inbox.clone()) {
                    Ok(Some(server)) => {
                        tracing::info!("now receiving launches from other instances");
                        while !inbox.is_closing() {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        drop(server);
                        return;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_secs(1)),
                    Err(e) => {
                        tracing::warn!("could not create the instance pipe: {e}");
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("could not start the instance claim thread: {e}");
    }
}

#[cfg(windows)]
pub use imp::{Channel, Server, forward};

#[cfg(windows)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
        ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, GetLastError, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel,
        TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT, WriteFile,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
        GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, SetNamedPipeHandleState,
        WaitNamedPipeW,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcessId, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION, SetEvent, WaitForMultipleObjects,
        WaitForSingleObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    use windows::core::{HRESULT, PCWSTR, PWSTR};

    use super::{
        COMMIT, COMMIT_WAIT, ForwardError, Handoff, Inbox, MAX_PAYLOAD, REPLY_MARGIN, Reply,
        UI_WAIT, decode_request, request_has_action, set_reply_wait,
    };
    use crate::{Result, WinError};

    /// How long the receiver waits for a connected client's request, and for its own reply
    /// to be read.
    const READ_TIMEOUT: Duration = Duration::from_secs(2);
    /// Connections handled at once; more are dropped (their senders fall back).
    const MAX_HANDLERS: usize = 16;

    fn is(e: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
        e.code() == HRESULT::from_win32(code.0)
    }

    /// A kernel handle closed on drop.
    struct Owned(HANDLE);
    // A kernel handle may be used and closed from any thread.
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// Who a process runs as: compared on both ends of the pipe.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Identity {
        sid: String,
        session: u32,
        /// Mandatory integrity level RID (0x2000 medium, 0x3000 high).
        integrity: u32,
    }

    fn identity_of(pid: u32) -> Result<Identity> {
        unsafe {
            let process = Owned(
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                    .map_err(|e| crate::api_err("OpenProcess", e))?,
            );
            let mut token = HANDLE::default();
            OpenProcessToken(process.0, TOKEN_QUERY, &mut token)
                .map_err(|e| crate::api_err("OpenProcessToken", e))?;
            let token = Owned(token);
            let user = token_info(token.0, TokenUser)?;
            let user = &*(user.as_ptr() as *const TOKEN_USER);
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text)
                .map_err(|e| crate::api_err("ConvertSidToStringSidW", e))?;
            let sid = text.to_string().unwrap_or_default();
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            let label = token_info(token.0, TokenIntegrityLevel)?;
            let label = &*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            let integrity = *GetSidSubAuthority(label.Label.Sid, u32::from(count.saturating_sub(1)));
            let mut session = 0;
            ProcessIdToSessionId(pid, &mut session)
                .map_err(|e| crate::api_err("ProcessIdToSessionId", e))?;
            Ok(Identity { sid, session, integrity })
        }
    }

    /// `a` and `b` are the same file: same volume serial and file index; when those cannot be
    /// read, the same final path (8.3 names, `subst` drives and `\\?\` prefixes resolved by
    /// Windows), ignoring case; when not even that, the same path as given, ignoring case.
    pub(crate) fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
        if let (Ok(a), Ok(b)) = (crate::files::file_id(a), crate::files::file_id(b)) {
            return a == b;
        }
        path_key(a) == path_key(b)
    }

    /// `path` as compared when file identities cannot be read: its final path (or the path
    /// as given), without a `\\?\` prefix, lowercased.
    pub(crate) fn path_key(path: &std::path::Path) -> String {
        let text = final_path(path).unwrap_or_else(|_| path.as_os_str().to_string_lossy().into_owned());
        strip_verbatim(&text).to_lowercase()
    }

    /// `\\?\C:\x` -> `C:\x`, `\\?\UNC\s\x` -> `\\s\x`.
    fn strip_verbatim(path: &str) -> String {
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else {
            path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
        }
    }

    /// The normalized path Windows reports for the open file (`GetFinalPathNameByHandleW`).
    pub(crate) fn final_path(path: &std::path::Path) -> Result<String> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Storage::FileSystem::{
            FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
        };
        const FILE_SHARE_ALL: u32 = 7;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|source| WinError::Io {
                op: "open for its final path",
                path: path.to_path_buf(),
                source,
            })?;
        let handle = HANDLE(file.as_raw_handle());
        let mut buf = vec![0u16; 1024];
        loop {
            let len = unsafe {
                GetFinalPathNameByHandleW(handle, &mut buf, windows::Win32::Storage::FileSystem::GETFINALPATHNAMEBYHANDLE_FLAGS(
                    FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0,
                ))
            } as usize;
            if len == 0 {
                return Err(crate::last_io_err("GetFinalPathNameByHandleW", path));
            }
            if len < buf.len() {
                return Ok(String::from_utf16_lossy(&buf[..len]));
            }
            buf.resize(len + 1, 0);
        }
    }

    /// The full Win32 path of the executable process `pid` runs.
    fn image_path(pid: u32) -> Result<std::path::PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows::Win32::System::Threading::{PROCESS_NAME_WIN32, QueryFullProcessImageNameW};
        let process = Owned(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                .map_err(|e| crate::api_err("OpenProcess", e))?,
        );
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
        }
        .map_err(|e| crate::api_err("QueryFullProcessImageNameW", e))?;
        Ok(std::ffi::OsString::from_wide(&buf[..len as usize]).into())
    }

    /// A token information block, in a buffer aligned for the structure it holds.
    unsafe fn token_info(
        token: HANDLE,
        class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
    ) -> Result<Vec<u64>> {
        let mut len = 0;
        let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        unsafe {
            GetTokenInformation(token, class, Some(buf.as_mut_ptr().cast()), len, &mut len)
        }
        .map_err(|e| crate::api_err("GetTokenInformation", e))?;
        Ok(buf)
    }

    /// The pipe for one user, session, integrity level and tag.
    #[derive(Debug, Clone)]
    pub struct Channel {
        name: Vec<u16>,
        display: String,
        me: Identity,
        /// The executable the pipe's owner must run: this process's own.
        exe: std::path::PathBuf,
    }

    impl Channel {
        /// The channel of the current process. `tag` separates builds (installed, dev,
        /// tests): 1 to 48 characters from `A-Z a-z 0-9 . _ -`.
        pub fn current(tag: &str) -> Result<Self> {
            Self::for_version(tag, super::PROTOCOL_VERSION)
        }

        /// The pipe ends in `-v<version>`: launches of another protocol version never find
        /// this one (each opens its own window instead).
        pub(crate) fn for_version(tag: &str, version: u32) -> Result<Self> {
            if tag.is_empty()
                || tag.len() > 48
                || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err(WinError::Invalid(format!("instance tag {tag:?}")));
            }
            let me = identity_of(unsafe { GetCurrentProcessId() })?;
            let display = format!(
                r"\\.\pipe\ZXrawExplorer-{}-{}-{:x}-{tag}-v{version}",
                me.sid, me.session, me.integrity
            );
            let exe = std::env::current_exe().map_err(|source| WinError::Io {
                op: "current_exe",
                path: std::path::PathBuf::new(),
                source,
            })?;
            Ok(Self { name: crate::to_wide(&display), display, me, exe })
        }

        /// Tests: expect the pipe's owner to run `exe` instead of this process's executable.
        #[cfg(test)]
        pub(crate) fn expecting_exe(mut self, exe: std::path::PathBuf) -> Self {
            self.exe = exe;
            self
        }

        /// The owner `pid` runs the same executable file as this process (see [`same_file`]).
        /// Keeps another program of this user that took the pipe name from swallowing launches.
        pub(crate) fn check_exe(&self, pid: u32) -> std::result::Result<(), String> {
            let theirs = image_path(pid).map_err(|e| format!("process {pid}: {e}"))?;
            if same_file(&theirs, &self.exe) {
                Ok(())
            } else {
                Err(format!(
                    "process {pid} runs {}, not {}",
                    theirs.display(),
                    self.exe.display()
                ))
            }
        }

        pub fn pipe_name(&self) -> &str {
            &self.display
        }

        /// Whether some process owns the pipe and has an instance free, without connecting.
        pub fn is_owned(&self) -> bool {
            unsafe { WaitNamedPipeW(PCWSTR(self.name.as_ptr()), 1) }.as_bool()
        }

        /// The other end is this user, in this session, at this integrity level.
        fn check(&self, pid: u32) -> std::result::Result<(), String> {
            let them = identity_of(pid).map_err(|e| format!("process {pid}: {e}"))?;
            if them.sid != self.me.sid {
                Err(format!("process {pid} runs as another user"))
            } else if them.session != self.me.session {
                Err(format!("process {pid} is in session {}", them.session))
            } else if them.integrity != self.me.integrity {
                Err(format!(
                    "process {pid} runs at integrity level {:#x}, this one at {:#x}",
                    them.integrity, self.me.integrity
                ))
            } else {
                Ok(())
            }
        }

        /// Owner-only DACL; the mandatory label is this process's level, so lower levels
        /// cannot write to the pipe.
        fn security_descriptor(&self, labelled: bool) -> Result<SecurityDescriptor> {
            let mut sddl = format!("D:P(A;;GA;;;{})", self.me.sid);
            if labelled {
                sddl.push_str(&format!("S:(ML;;NW;;;S-1-16-{})", self.me.integrity));
            }
            let wide = crate::to_wide(&sddl);
            let mut sd = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(wide.as_ptr()),
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )
            }
            .map_err(|e| crate::api_err("ConvertStringSecurityDescriptorToSecurityDescriptorW", e))?;
            Ok(SecurityDescriptor(sd))
        }
    }

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);
    // Read-only after creation; freed once.
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}
    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }

    /// Creates one instance of the pipe. `Ok(None)`: `first` was asked and another process
    /// owns the name.
    fn create_instance(
        channel: &Channel,
        sd: &SecurityDescriptor,
        first: bool,
    ) -> Result<Option<Owned>> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: false.into(),
        };
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(channel.name.as_ptr()),
                open_mode,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                MAX_PAYLOAD as u32,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            let error = unsafe { GetLastError() };
            // Another process created the first instance (or squats the name).
            if first && (error == ERROR_ACCESS_DENIED || error == ERROR_PIPE_BUSY) {
                return Ok(None);
            }
            return Err(WinError::Api {
                op: "CreateNamedPipeW",
                code: error.0,
                message: windows::core::Error::from(error).message(),
            });
        }
        Ok(Some(Owned(handle)))
    }

    /// The pipe's owner: accepts connections on a background thread until dropped.
    pub struct Server {
        stop: Arc<Owned>,
        thread: Option<std::thread::JoinHandle<()>>,
        name: String,
    }

    impl Server {
        /// Creates the pipe's first instance and starts accepting, or returns `None` when
        /// another process already owns the pipe.
        pub fn claim(channel: &Channel, inbox: Arc<Inbox>) -> Result<Option<Server>> {
            Self::claim_with(channel, inbox, Duration::ZERO)
        }

        /// Tests: answers every request `reply_delay` late, as a descheduled receiver would.
        #[cfg(test)]
        pub(crate) fn claim_with_reply_delay(
            channel: &Channel,
            inbox: Arc<Inbox>,
            reply_delay: Duration,
        ) -> Result<Option<Server>> {
            Self::claim_with(channel, inbox, reply_delay)
        }

        fn claim_with(
            channel: &Channel,
            inbox: Arc<Inbox>,
            reply_delay: Duration,
        ) -> Result<Option<Server>> {
            // A label above what the process may set fails; the DACL and the identity
            // checks still hold without it.
            let (sd, first) = match channel.security_descriptor(true).and_then(|sd| {
                create_instance(channel, &sd, true).map(|first| (sd, first))
            }) {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::debug!("instance pipe with a label failed ({e}); trying without");
                    let sd = channel.security_descriptor(false)?;
                    let first = create_instance(channel, &sd, true)?;
                    (sd, first)
                }
            };
            let Some(first) = first else {
                return Ok(None);
            };
            let stop = Arc::new(Owned(
                unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
                    .map_err(|e| crate::api_err("CreateEventW", e))?,
            ));
            let thread = {
                let stop = stop.clone();
                let channel = channel.clone();
                std::thread::Builder::new()
                    .name("instance-pipe".into())
                    .spawn(move || accept_loop(channel, sd, first, stop, inbox, reply_delay))
                    .map_err(|e| WinError::Invalid(format!("could not start the pipe thread: {e}")))?
            };
            Ok(Some(Server {
                stop,
                thread: Some(thread),
                name: channel.display.clone(),
            }))
        }

        pub fn pipe_name(&self) -> &str {
            &self.name
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            unsafe {
                let _ = SetEvent(self.stop.0);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn accept_loop(
        channel: Channel,
        sd: SecurityDescriptor,
        mut current: Owned,
        stop: Arc<Owned>,
        inbox: Arc<Inbox>,
        reply_delay: Duration,
    ) {
        let handlers = Arc::new(AtomicUsize::new(0));
        loop {
            match connect(&current, &stop) {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => {
                    tracing::warn!("instance pipe: waiting for a connection failed: {e}");
                    return;
                }
            }
            // The next instance exists before this one is handled, so a launch never finds
            // the pipe missing while this process owns it.
            let next = loop {
                match create_instance(&channel, &sd, false) {
                    Ok(Some(next)) => break next,
                    Ok(None) => unreachable!("only the first instance can be refused"),
                    Err(e) => {
                        tracing::warn!("instance pipe: creating the next instance failed: {e}");
                        if unsafe { WaitForSingleObject(stop.0, 500) } == WAIT_OBJECT_0 {
                            return;
                        }
                    }
                }
            };
            let connected = std::mem::replace(&mut current, next);
            if handlers.fetch_add(1, Ordering::SeqCst) >= MAX_HANDLERS {
                handlers.fetch_sub(1, Ordering::SeqCst);
                tracing::warn!("instance pipe: too many connections at once; dropping one");
                continue;
            }
            let (channel, inbox, handlers) = (channel.clone(), inbox.clone(), handlers.clone());
            let spawned = std::thread::Builder::new()
                .name("instance-request".into())
                .spawn(move || {
                    handle(&channel, connected, &inbox, reply_delay);
                    handlers.fetch_sub(1, Ordering::SeqCst);
                });
            if let Err(e) = spawned {
                tracing::warn!("instance pipe: could not start a handler: {e}");
            }
        }
    }

    /// Waits for a client on `pipe`. `Ok(false)`: `stop` was signalled.
    fn connect(pipe: &Owned, stop: &Owned) -> Result<bool> {
        let event = Owned(
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
                .map_err(|e| crate::api_err("CreateEventW", e))?,
        );
        let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
        match unsafe { ConnectNamedPipe(pipe.0, Some(&mut ov)) } {
            Ok(()) => return Ok(true),
            Err(e) if is(&e, ERROR_PIPE_CONNECTED) => return Ok(true),
            Err(e) if is(&e, ERROR_IO_PENDING) => {}
            Err(e) => return Err(crate::api_err("ConnectNamedPipe", e)),
        }
        let wait = unsafe { WaitForMultipleObjects(&[event.0, stop.0], false, u32::MAX) };
        if wait == WAIT_OBJECT_0 {
            let mut n = 0;
            unsafe { GetOverlappedResult(pipe.0, &ov, &mut n, false) }
                .map_err(|e| crate::api_err("ConnectNamedPipe", e))?;
            Ok(true)
        } else {
            unsafe {
                let _ = CancelIoEx(pipe.0, Some(&ov));
                let mut n = 0;
                let _ = GetOverlappedResult(pipe.0, &ov, &mut n, true);
            }
            Ok(false)
        }
    }

    enum IoError {
        TimedOut,
        MoreData,
        Os(windows::core::Error),
    }

    /// Runs one overlapped read or write on `handle`, cancelling it after `timeout`.
    fn overlapped(
        handle: HANDLE,
        timeout: Duration,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> std::result::Result<u32, IoError> {
        let event = Owned(unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(IoError::Os)?);
        let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
        match start(&mut ov) {
            Ok(()) => {}
            Err(e) if is(&e, ERROR_IO_PENDING) => {}
            Err(e) if is(&e, ERROR_MORE_DATA) => return Err(IoError::MoreData),
            Err(e) => return Err(IoError::Os(e)),
        }
        let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        if unsafe { WaitForSingleObject(event.0, ms) } != WAIT_OBJECT_0 {
            unsafe {
                let _ = CancelIoEx(handle, Some(&ov));
                let mut n = 0;
                let _ = GetOverlappedResult(handle, &ov, &mut n, true);
            }
            return Err(IoError::TimedOut);
        }
        let mut n = 0;
        match unsafe { GetOverlappedResult(handle, &ov, &mut n, false) } {
            Ok(()) => Ok(n),
            Err(e) if is(&e, ERROR_MORE_DATA) => Err(IoError::MoreData),
            Err(e) => Err(IoError::Os(e)),
        }
    }

    fn read_message(handle: HANDLE, max: usize, timeout: Duration) -> std::result::Result<Vec<u8>, IoError> {
        let mut buf = vec![0u8; max];
        let n = overlapped(handle, timeout, |ov| unsafe {
            ReadFile(handle, Some(&mut buf), None, Some(ov))
        })?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn write_message(handle: HANDLE, bytes: &[u8], timeout: Duration) -> std::result::Result<(), IoError> {
        let n = overlapped(handle, timeout, |ov| unsafe {
            WriteFile(handle, Some(bytes), None, Some(ov))
        })?;
        if n as usize == bytes.len() {
            Ok(())
        } else {
            Err(IoError::Os(windows::core::Error::from(ERROR_BROKEN_PIPE)))
        }
    }

    /// One connection: check the sender, read and validate its request, hand it to the
    /// window and answer.
    fn handle(channel: &Channel, pipe: Owned, inbox: &Inbox, reply_delay: Duration) {
        let mut pid = 0;
        if let Err(e) = unsafe { GetNamedPipeClientProcessId(pipe.0, &mut pid) } {
            tracing::warn!("instance pipe: unknown sender: {e}");
            return;
        }
        if let Err(why) = channel.check(pid) {
            tracing::warn!("instance pipe: refused a request: {why}");
            return;
        }
        let (reply, ticket) = match read_message(pipe.0, MAX_PAYLOAD, READ_TIMEOUT) {
            Ok(bytes) => match decode_request(&bytes) {
                Ok(request) => {
                    tracing::info!(
                        "received a launch with {} path(s) from process {pid} ({})",
                        request.paths.len(),
                        request.action.as_deref().unwrap_or("open")
                    );
                    let wait = request.reply_within.saturating_sub(REPLY_MARGIN);
                    let handoff = Handoff {
                        action: request.action,
                        paths: request.paths,
                    };
                    let (reply, ticket) = inbox.deliver_handoff(handoff, wait, UI_WAIT);
                    if reply != Reply::Accepted {
                        tracing::warn!("a launch from process {pid} was withdrawn: {reply:?}");
                    }
                    (reply, ticket)
                }
                Err(e) => {
                    tracing::warn!("instance pipe: invalid request from process {pid}: {e}");
                    (Reply::Rejected, None)
                }
            },
            Err(IoError::MoreData) => {
                tracing::warn!("instance pipe: request from process {pid} is too large");
                (Reply::Rejected, None)
            }
            Err(IoError::TimedOut) => {
                tracing::warn!("instance pipe: process {pid} sent nothing");
                return;
            }
            Err(IoError::Os(e)) => {
                tracing::warn!("instance pipe: reading from process {pid} failed: {e}");
                return;
            }
        };
        if !reply_delay.is_zero() {
            std::thread::sleep(reply_delay);
        }
        if write_message(pipe.0, &reply.encode(), READ_TIMEOUT).is_err() {
            if let Some(ticket) = ticket {
                // The sender runs it itself.
                inbox.abandon(ticket);
                tracing::warn!("instance pipe: could not answer process {pid}; dropped its action");
            } else if reply == Reply::Accepted {
                tracing::warn!(
                    "instance pipe: opened a launch from process {pid}, but could not tell it; \
                     it may open its own window as well"
                );
            } else {
                tracing::warn!("instance pipe: could not answer process {pid}");
            }
            return;
        }
        if let Some(ticket) = ticket {
            // An action runs only once its sender confirmed it read "accepted".
            match read_message(pipe.0, 16, COMMIT_WAIT) {
                Ok(bytes) if bytes == COMMIT => inbox.commit(ticket),
                _ => {
                    inbox.abandon(ticket);
                    tracing::warn!(
                        "instance pipe: process {pid} did not confirm its action; it runs it itself"
                    );
                }
            }
            return;
        }
        // Closing before the sender has read the answer could discard it: wait until the
        // sender closes its end (the read then fails with a broken pipe).
        let _ = read_message(pipe.0, 16, READ_TIMEOUT);
    }

    /// Sends `request` to the pipe's owner and waits for its answer, until `deadline`.
    pub fn forward(
        channel: &Channel,
        request: &[u8],
        deadline: Instant,
    ) -> std::result::Result<(), ForwardError> {
        let remaining = || deadline.saturating_duration_since(Instant::now());
        let pipe = loop {
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(channel.name.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    // The owner may only identify this process, never act as it.
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            match opened {
                Ok(handle) => break Owned(handle),
                Err(e) if is(&e, ERROR_FILE_NOT_FOUND) => return Err(ForwardError::NoServer),
                Err(e) if is(&e, ERROR_PIPE_BUSY) => {
                    let ms = remaining().as_millis().min(u128::from(u32::MAX - 1)) as u32;
                    if ms == 0 {
                        return Err(ForwardError::TimedOut("connecting"));
                    }
                    // Returns early when an instance is free, or false on timeout or when
                    // the pipe vanished; the next CreateFileW tells which.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(channel.name.as_ptr()), ms.min(250)) };
                }
                Err(e) => return Err(ForwardError::Failed(crate::api_err("CreateFileW", e))),
            }
        };
        let mode = PIPE_READMODE_MESSAGE;
        unsafe { SetNamedPipeHandleState(pipe.0, Some(&mode), None, None) }
            .map_err(|e| ForwardError::Failed(crate::api_err("SetNamedPipeHandleState", e)))?;
        let mut server_pid = 0;
        unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server_pid) }
            .map_err(|e| ForwardError::Failed(crate::api_err("GetNamedPipeServerProcessId", e)))?;
        channel.check(server_pid).map_err(ForwardError::Untrusted)?;
        // Checked before anything is sent or any foreground right is handed on.
        channel.check_exe(server_pid).map_err(ForwardError::Untrusted)?;
        // This process was just started by the user, so it may hand the foreground on.
        if let Err(e) = unsafe { AllowSetForegroundWindow(server_pid) } {
            tracing::debug!("AllowSetForegroundWindow({server_pid}) failed: {e}");
        }
        let io = |e: IoError, step: &'static str| match e {
            IoError::TimedOut => ForwardError::TimedOut(step),
            IoError::MoreData => ForwardError::Failed(WinError::Invalid("oversized reply".into())),
            IoError::Os(e) => ForwardError::Failed(crate::api_err("pipe I/O", e)),
        };
        // The owner answers within this (minus its margin), so the answer normally arrives
        // while this end listens. Under rare scheduling delays an "accepted" answer arrives
        // late: this end then opens its own window and the launch opens in both. Nothing is
        // ever lost (see the module notes).
        let mut request = request.to_vec();
        set_reply_wait(&mut request, remaining());
        write_message(pipe.0, &request, remaining()).map_err(|e| io(e, "sending"))?;
        let answer = read_message(pipe.0, 64, remaining()).map_err(|e| io(e, "waiting for the answer"))?;
        match Reply::decode(&answer) {
            Some(Reply::Accepted) => {
                // Confirms that this end read the answer and will not run the launch itself.
                // An action waits for this; for an open it only ends the exchange.
                let confirmed = write_message(pipe.0, &COMMIT, Duration::from_secs(2));
                if confirmed.is_err() && request_has_action(&request) {
                    return Err(ForwardError::Failed(WinError::Invalid(
                        "could not confirm the action to the running window".into(),
                    )));
                }
                Ok(())
            }

            Some(reply) => Err(ForwardError::Refused(reply)),
            None => Err(ForwardError::Failed(WinError::Invalid("unreadable answer".into()))),
        }
    }

    #[cfg(test)]
    pub(super) fn current_integrity() -> u32 {
        identity_of(unsafe { GetCurrentProcessId() }).unwrap().integrity
    }

    #[cfg(test)]
    pub(super) fn raw_send(channel: &Channel, bytes: &[u8]) -> Option<Reply> {
        let pipe = unsafe {
            CreateFileW(
                PCWSTR(channel.name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .ok()
        .map(Owned)?;
        let mode = PIPE_READMODE_MESSAGE;
        unsafe { SetNamedPipeHandleState(pipe.0, Some(&mode), None, None) }.ok()?;
        write_message(pipe.0, bytes, Duration::from_secs(2)).ok()?;
        Reply::decode(&read_message(pipe.0, 64, Duration::from_secs(3)).ok()?)
    }
}

/// Off Windows there is no instance pipe: every launch opens its own window.
#[cfg(not(windows))]
mod imp_stub {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct Channel;

    impl Channel {
        pub fn current(_tag: &str) -> Result<Self> {
            Err(WinError::Unsupported)
        }
        pub fn pipe_name(&self) -> &str {
            ""
        }
        pub fn is_owned(&self) -> bool {
            false
        }
    }

    pub struct Server;

    impl Server {
        pub fn claim(_channel: &Channel, _inbox: std::sync::Arc<Inbox>) -> Result<Option<Server>> {
            Err(WinError::Unsupported)
        }
        pub fn pipe_name(&self) -> &str {
            ""
        }
    }

    pub fn forward(
        _channel: &Channel,
        _request: &[u8],
        _deadline: std::time::Instant,
    ) -> std::result::Result<(), ForwardError> {
        Err(ForwardError::Failed(WinError::Unsupported))
    }
}

#[cfg(not(windows))]
pub use imp_stub::{Channel, Server, forward};

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u8> {
        let mut out = (s.encode_utf16().count() as u32).to_le_bytes().to_vec();
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    fn raw(version: u32, cwd: &[u8], paths: &[Vec<u8>]) -> Vec<u8> {
        let mut out = REQUEST_MAGIC.to_vec();
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(cwd);
        // No action.
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        for p in paths {
            out.extend_from_slice(p);
        }
        out
    }

    #[cfg(windows)]
    #[test]
    fn round_trip_keeps_absolute_paths_and_resolves_relative_ones() {
        let bytes = encode_request(
            Path::new(r"C:\Users\me\Desktop"),
            &[
                PathBuf::from(r"D:\Photos"),
                PathBuf::from("Trip"),
                PathBuf::from(r"..\Other\a.jpg"),
                PathBuf::from(r"\Root"),
                PathBuf::from(r"\\server\share\x"),
                PathBuf::from("Ünïcödé 📷"),
            ],
        )
        .unwrap();
        let paths = decode_request(&bytes).unwrap().paths;
        assert_eq!(
            paths,
            vec![
                PathBuf::from(r"D:\Photos"),
                PathBuf::from(r"C:\Users\me\Desktop\Trip"),
                PathBuf::from(r"C:\Users\me\Desktop\..\Other\a.jpg"),
                PathBuf::from(r"C:\Root"),
                PathBuf::from(r"\\server\share\x"),
                PathBuf::from(r"C:\Users\me\Desktop\Ünïcödé 📷"),
            ]
        );
    }

    #[test]
    fn no_paths_means_focus_only() {
        let bytes = encode_request(Path::new(""), &[]).unwrap();
        assert_eq!(decode_request(&bytes).unwrap().paths, Vec::<PathBuf>::new());
    }

    #[cfg(windows)]
    #[test]
    fn relative_paths_need_an_absolute_working_directory() {
        let bytes = raw(PROTOCOL_VERSION, &units(""), &[units("Trip")]);
        assert_eq!(decode_request(&bytes), Err(PayloadError::NoWorkingDirectory));
        let bytes = raw(PROTOCOL_VERSION, &units("relative"), &[units(r"C:\x")]);
        assert!(matches!(decode_request(&bytes), Err(PayloadError::NotAbsolute(_))));
    }

    #[cfg(windows)]
    #[test]
    fn drive_relative_and_device_paths_are_refused() {
        for bad in [r"C:foo", r"\\.\pipe\other", r"\\?\GLOBALROOT\Device\HarddiskVolume1"] {
            let bytes = raw(PROTOCOL_VERSION, &units(r"D:\work"), &[units(bad)]);
            assert!(
                matches!(decode_request(&bytes), Err(PayloadError::NotAbsolute(_))),
                "{bad} was accepted"
            );
        }
        let bytes = raw(PROTOCOL_VERSION, &units(r"D:\work"), &[units(r"\\?\C:\long\path")]);
        assert_eq!(decode_request(&bytes).unwrap().paths, vec![PathBuf::from(r"\\?\C:\long\path")]);
    }

    #[test]
    fn wrong_version_and_magic_are_refused() {
        // Strict: the previous version and unknown later ones are refused alike.
        for version in [1, PROTOCOL_VERSION + 1] {
            assert_eq!(
                decode_request(&raw(version, &units(""), &[])),
                Err(PayloadError::WrongVersion(version))
            );
        }
        let mut bytes = raw(PROTOCOL_VERSION, &units(""), &[]);
        bytes[0] = b'X';
        assert_eq!(decode_request(&bytes), Err(PayloadError::BadMagic));
        assert_eq!(decode_request(b"ZX"), Err(PayloadError::Truncated));
    }

    #[test]
    fn oversized_requests_are_refused_on_both_ends() {
        let long = PathBuf::from(format!(r"C:\{}", "a".repeat(30_000)));
        assert!(matches!(
            encode_request(Path::new(r"C:\"), &vec![long; MAX_PAYLOAD / 60_000 + 1]),
            Err(PayloadError::TooLarge(_))
        ));
        let many = vec![PathBuf::from(r"C:\x"); MAX_PATHS + 1];
        assert_eq!(
            encode_request(Path::new(r"C:\"), &many),
            Err(PayloadError::TooManyPaths(MAX_PATHS + 1))
        );
        assert!(matches!(
            decode_request(&vec![0u8; MAX_PAYLOAD + 1]),
            Err(PayloadError::TooLarge(_))
        ));
        let mut huge_count = raw(PROTOCOL_VERSION, &units(""), &[]);
        let at = huge_count.len() - 4;
        huge_count[at..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_request(&huge_count),
            Err(PayloadError::TooManyPaths(u32::MAX as usize))
        );
        // A string length past the data is caught, not trusted.
        let bytes = raw(PROTOCOL_VERSION, &(u32::MAX).to_le_bytes(), &[]);
        assert!(matches!(decode_request(&bytes), Err(PayloadError::StringTooLong(_))));
        let bytes = raw(PROTOCOL_VERSION, &1000u32.to_le_bytes(), &[]);
        assert_eq!(decode_request(&bytes), Err(PayloadError::Truncated));
    }

    #[test]
    fn junk_utf16_nul_and_trailing_bytes_are_refused() {
        // A lone high surrogate.
        let mut lone = 1u32.to_le_bytes().to_vec();
        lone.extend_from_slice(&0xD800u16.to_le_bytes());
        assert_eq!(
            decode_request(&raw(PROTOCOL_VERSION, &units(r"C:\"), &[lone])),
            Err(PayloadError::InvalidUtf16)
        );
        assert_eq!(
            decode_request(&raw(PROTOCOL_VERSION, &units(r"C:\"), &[units("C:\\a\0b")])),
            Err(PayloadError::ContainsNul)
        );
        let mut bytes = raw(PROTOCOL_VERSION, &units(""), &[]);
        bytes.push(0);
        assert_eq!(decode_request(&bytes), Err(PayloadError::TrailingBytes));
        // An odd byte count cannot hold the units it announces.
        let mut odd = 1u32.to_le_bytes().to_vec();
        odd.push(b'C');
        assert_eq!(decode_request(&raw(PROTOCOL_VERSION, &odd, &[])), Err(PayloadError::Truncated));
    }

    #[cfg(windows)]
    #[test]
    fn actions_round_trip_and_bad_names_are_refused_on_both_ends() {
        let bytes = encode_action_request(
            Path::new(r"C:\work"),
            &[PathBuf::from("a.zip"), PathBuf::from(r"D:\b.7z")],
            Some("extract-here"),
        )
        .unwrap();
        let request = decode_request(&bytes).unwrap();
        assert_eq!(request.action.as_deref(), Some("extract-here"));
        assert_eq!(request.paths, [PathBuf::from(r"C:\work\a.zip"), PathBuf::from(r"D:\b.7z")]);
        assert!(request_has_action(&bytes));
        assert!(!request_has_action(&encode_request(Path::new(r"C:\"), &[]).unwrap()));
        assert_eq!(decode_request(&encode_request(Path::new(r"C:\"), &[]).unwrap()).unwrap().action, None);
        for bad in ["", "Extract", "a b", "x/../y", &"a".repeat(MAX_ACTION_UNITS + 1)] {
            assert_eq!(
                encode_action_request(Path::new(r"C:\"), &[], Some(bad)),
                Err(PayloadError::BadAction(bad.to_string())),
                "{bad:?}"
            );
        }
        // A receiver checks the action as well.
        let mut forged = REQUEST_MAGIC.to_vec();
        forged.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        forged.extend_from_slice(&0u32.to_le_bytes());
        forged.extend_from_slice(&units(r"C:\"));
        forged.extend_from_slice(&units("RUN ME"));
        forged.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_request(&forged), Err(PayloadError::BadAction("RUN ME".into())));
    }

    #[test]
    fn an_action_is_handed_out_only_after_its_commit_and_never_after_an_abandon() {
        let inbox = std::sync::Arc::new(Inbox::new());
        inbox.take_handoffs();
        let action = |name: &str| Handoff {
            action: Some("add-zip".into()),
            paths: vec![PathBuf::from(name)],
        };
        let deliver = |handoff: Handoff| {
            let inbox = inbox.clone();
            std::thread::spawn(move || inbox.deliver_handoff(handoff, Duration::from_secs(5), UI_WAIT))
        };
        let committed = deliver(action("a"));
        let abandoned = deliver(action("b"));
        let mut taken = Vec::new();
        while !(committed.is_finished() && abandoned.is_finished()) {
            taken.extend(inbox.take_handoffs());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(taken.is_empty(), "taken before any commit");
        let (reply, a) = committed.join().unwrap();
        assert_eq!(reply, Reply::Accepted);
        let (reply, b) = abandoned.join().unwrap();
        assert_eq!(reply, Reply::Accepted);
        inbox.abandon(b.unwrap());
        inbox.commit(a.unwrap());
        assert_eq!(inbox.take_handoffs(), vec![action("a")]);
        assert!(inbox.take_handoffs().is_empty(), "handed out once");
        // An open needs no commit and has no ticket.
        let open = std::thread::spawn({
            let inbox = inbox.clone();
            move || inbox.deliver_handoff(Handoff { action: None, paths: vec![] }, Duration::from_secs(5), UI_WAIT)
        });
        while !open.is_finished() {
            taken.extend(inbox.take_handoffs());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(open.join().unwrap(), (Reply::Accepted, None));
        assert_eq!(taken.len(), 1);
    }

    #[test]
    fn replies_round_trip_and_junk_is_unreadable() {
        for r in [Reply::Accepted, Reply::Busy, Reply::Closing, Reply::Rejected] {
            assert_eq!(Reply::decode(&r.encode()), Some(r));
        }
        assert_eq!(Reply::decode(b"ZXRP\x09\0\0\0"), None);
        assert_eq!(Reply::decode(b"nope"), None);
    }

    #[test]
    fn the_sender_wait_travels_in_the_request_and_is_capped() {
        let mut bytes = encode_request(Path::new(""), &[]).unwrap();
        assert_eq!(decode_request(&bytes).unwrap().reply_within, Duration::ZERO);
        set_reply_wait(&mut bytes, Duration::from_millis(3210));
        assert_eq!(decode_request(&bytes).unwrap().reply_within, Duration::from_millis(3210));
        set_reply_wait(&mut bytes, Duration::from_secs(3600));
        assert_eq!(decode_request(&bytes).unwrap().reply_within, MAX_REPLY_WAIT);
    }

    /// Delivers `paths` on another thread, as a pipe thread does.
    fn deliver_async(
        inbox: &std::sync::Arc<Inbox>,
        name: &str,
        wait: Duration,
    ) -> std::thread::JoinHandle<Reply> {
        let (inbox, name) = (inbox.clone(), name.to_string());
        std::thread::spawn(move || inbox.deliver(vec![PathBuf::from(name)], wait, UI_WAIT))
    }

    #[test]
    fn a_request_during_startup_is_accepted_only_once_the_window_takes_it() {
        let inbox = std::sync::Arc::new(Inbox::new());
        let pending = deliver_async(&inbox, "a", Duration::from_secs(10));
        // Longer than UI_WAIT: a starting window gets the sender's whole wait.
        std::thread::sleep(UI_WAIT + Duration::from_millis(200));
        assert!(!pending.is_finished(), "no answer before the window took the request");
        let mut taken = Vec::new();
        while !pending.is_finished() {
            taken.extend(inbox.take());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pending.join().unwrap(), Reply::Accepted);
        assert_eq!(taken, vec![vec![PathBuf::from("a")]]);
    }

    #[test]
    fn a_startup_past_the_sender_wait_gets_busy_and_never_opens_it() {
        let inbox = std::sync::Arc::new(Inbox::new());
        let reply = deliver_async(&inbox, "a", Duration::from_millis(100)).join().unwrap();
        assert_eq!(reply, Reply::Busy);
        assert!(inbox.take().is_empty(), "the sender opens it, so this window must not");
    }

    #[test]
    fn a_window_closing_before_its_first_frame_answers_closing_and_never_opens_it() {
        let inbox = std::sync::Arc::new(Inbox::new());
        let pending = deliver_async(&inbox, "a", Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(100));
        inbox.close();
        assert_eq!(pending.join().unwrap(), Reply::Closing);
        assert!(inbox.take().is_empty());
        assert_eq!(inbox.deliver(vec![], Duration::from_secs(1), UI_WAIT), Reply::Closing);
    }

    #[test]
    fn a_ready_window_that_never_takes_a_request_gets_busy_and_the_request_is_withdrawn() {
        let inbox = Inbox::new();
        inbox.take();
        let started = std::time::Instant::now();
        assert_eq!(
            inbox.deliver(vec![PathBuf::from("a")], Duration::from_secs(10), Duration::from_millis(50)),
            Reply::Busy
        );
        assert!(started.elapsed() < Duration::from_secs(1), "a running window gets only ui_wait");
        assert!(inbox.take().is_empty(), "a withdrawn request must not open later as well");
    }

    #[cfg(windows)]
    mod pipe {
        use std::sync::Arc;
        use std::time::Instant;

        use super::super::*;

        fn unique_tag(name: &str) -> String {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos();
            format!("test-{name}-{}-{nanos}", std::process::id())
        }

        /// Takes requests like a window would, until stopped.
        fn window(inbox: Arc<Inbox>) -> (Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<Vec<Vec<PathBuf>>>) {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::spawn(move || {
                let mut all = Vec::new();
                while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                    all.extend(inbox.take());
                    std::thread::sleep(Duration::from_millis(10));
                }
                all.extend(inbox.take());
                all
            });
            (stop, thread)
        }

        #[test]
        fn a_second_launch_forwards_its_paths_to_the_owner() {
            let channel = Channel::current(&unique_tag("fwd")).unwrap();
            assert!(channel.pipe_name().contains(&format!("{:x}", imp::current_integrity())));
            let owner_inbox = Arc::new(Inbox::new());
            let Startup::Primary(server) =
                claim_or_forward(&channel, owner_inbox.clone(), &[], FORWARD_TIMEOUT)
            else {
                panic!("the first claim must own the pipe");
            };
            let (stop, window) = window(owner_inbox.clone());
            let request = encode_request(Path::new(r"C:\Users"), &[PathBuf::from("Public")]).unwrap();
            let started = Instant::now();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT) {
                Startup::Forwarded => {}
                Startup::Primary(_) => panic!("a second owner was allowed"),
                Startup::Standalone(why) => panic!("not forwarded: {why}"),
            }
            assert!(started.elapsed() < FORWARD_TIMEOUT);
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(window.join().unwrap(), vec![vec![PathBuf::from(r"C:\Users\Public")]]);
            drop(server);
            // Once the owner is gone, the next launch owns the pipe.
            assert!(matches!(
                claim_or_forward(&channel, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT),
                Startup::Primary(_)
            ));
        }

        #[test]
        fn an_unresponsive_owner_makes_the_sender_fall_back_in_time() {
            let channel = Channel::current(&unique_tag("hung")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&channel, inbox.clone()).unwrap().expect("owner");
            // The window took one batch, then stopped taking: hung.
            inbox.take();
            let request = encode_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\Windows")]).unwrap();
            let started = Instant::now();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT) {
                Startup::Standalone(why) => assert!(why.contains("not responding"), "{why}"),
                _ => panic!("a hung owner must not take the request"),
            }
            assert!(started.elapsed() < FORWARD_TIMEOUT + Duration::from_millis(500));
            assert!(inbox.take().is_empty(), "the withdrawn request must not open later");
            // A closing owner answers at once.
            inbox.close();
            let started = Instant::now();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT) {
                Startup::Standalone(why) => assert!(why.contains("closing"), "{why}"),
                _ => panic!("a closing owner must not take the request"),
            }
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn a_request_sent_while_the_owner_is_starting_is_accepted_once_taken() {
            let channel = Channel::current(&unique_tag("start")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&channel, inbox.clone()).unwrap().expect("owner");
            let request = encode_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\Users")]).unwrap();
            let sender = {
                let channel = channel.clone();
                std::thread::spawn(move || forward(&channel, &request, Instant::now() + FORWARD_TIMEOUT))
            };
            // The window takes its first batch only after UI_WAIT (a slow start).
            std::thread::sleep(UI_WAIT + Duration::from_millis(200));
            assert!(!sender.is_finished(), "the sender is answered only once the window took it");
            let (stop, window) = window(inbox.clone());
            assert!(sender.join().unwrap().is_ok());
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(window.join().unwrap(), vec![vec![PathBuf::from(r"C:\Users")]]);
        }

        #[test]
        fn an_owner_starting_past_the_timeout_or_closing_first_lets_the_sender_open_it_alone() {
            let channel = Channel::current(&unique_tag("slow")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&channel, inbox.clone()).unwrap().expect("owner");
            let request = encode_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\Users")]).unwrap();
            // Never takes anything: a start slower than the sender's timeout.
            let started = Instant::now();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, Duration::from_millis(1500)) {
                Startup::Standalone(why) => assert!(why.contains("not responding"), "{why}"),
                _ => panic!("a window that never took the request must not have it"),
            }
            assert!(started.elapsed() < Duration::from_millis(1500) + Duration::from_millis(300));
            assert!(inbox.take().is_empty(), "no double open: the sender opens it");

            let channel = Channel::current(&unique_tag("closing")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&channel, inbox.clone()).unwrap().expect("owner");
            let sender = {
                let channel = channel.clone();
                std::thread::spawn(move || {
                    claim_or_forward(&channel, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT)
                })
            };
            std::thread::sleep(Duration::from_millis(300));
            // Closed before its first frame took anything.
            inbox.close();
            match sender.join().unwrap() {
                Startup::Standalone(why) => assert!(why.contains("closing"), "{why}"),
                _ => panic!("a closing window must not have the request"),
            }
            assert!(inbox.take().is_empty());
        }

        #[test]
        fn an_owner_running_another_executable_is_refused_before_anything_is_sent() {
            let tag = unique_tag("exe");
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&Channel::current(&tag).unwrap(), inbox.clone())
                .unwrap()
                .expect("owner");
            let (stop, window) = window(inbox.clone());
            // The sender expects another executable than the one the owner (this test) runs.
            let other = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\cmd.exe");
            let sender = Channel::current(&tag).unwrap().expecting_exe(other);
            let request = encode_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\Users")]).unwrap();
            match claim_or_forward(&sender, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT) {
                Startup::Standalone(why) => assert!(why.contains("cmd.exe"), "{why}"),
                _ => panic!("an owner running another program must not get the request"),
            }
            std::thread::sleep(Duration::from_millis(200));
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(window.join().unwrap().is_empty(), "nothing may reach the impostor");
            // The same executable is accepted.
            let same = Channel::current(&tag).unwrap();
            assert!(same.check_exe(std::process::id()).is_ok());
        }

        #[test]
        fn simultaneous_launches_are_all_delivered_to_one_owner() {
            let channel = Channel::current(&unique_tag("race")).unwrap();
            let senders = 6;
            let barrier = Arc::new(std::sync::Barrier::new(senders));
            let inboxes: Vec<_> = (0..senders).map(|_| Arc::new(Inbox::new())).collect();
            // Every launch's window takes requests; only the owner's ever gets any.
            let windows: Vec<_> = inboxes.iter().map(|inbox| window(inbox.clone())).collect();
            let threads: Vec<_> = (0..senders)
                .map(|i| {
                    let (channel, barrier, inbox) = (channel.clone(), barrier.clone(), inboxes[i].clone());
                    std::thread::spawn(move || {
                        let request = encode_request(
                            Path::new(r"C:\"),
                            &[PathBuf::from(format!(r"C:\folder{i}"))],
                        )
                        .unwrap();
                        barrier.wait();
                        match claim_or_forward(&channel, inbox, &request, FORWARD_TIMEOUT) {
                            Startup::Primary(server) => Some(server),
                            Startup::Forwarded => None,
                            Startup::Standalone(why) => panic!("sender {i} fell back: {why}"),
                        }
                    })
                })
                .collect();
            let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            let owners: Vec<usize> = (0..senders).filter(|&i| results[i].is_some()).collect();
            assert_eq!(owners.len(), 1, "exactly one launch owns the pipe");
            let owner = owners[0];
            let mut received: Vec<PathBuf> = Vec::new();
            for (i, (stop, window)) in windows.into_iter().enumerate() {
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                let got: Vec<PathBuf> = window.join().unwrap().into_iter().flatten().collect();
                assert!(i == owner || got.is_empty(), "only the owner receives");
                received.extend(got);
            }
            received.sort();
            let mut expected: Vec<PathBuf> = (0..senders)
                .filter(|&i| i != owner)
                .map(|i| PathBuf::from(format!(r"C:\folder{i}")))
                .collect();
            expected.sort();
            assert_eq!(received, expected, "every forwarded path arrives exactly once");
        }

        #[test]
        fn invalid_payloads_are_rejected_and_never_queued() {
            let channel = Channel::current(&unique_tag("junk")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&channel, inbox.clone()).unwrap().expect("owner");
            assert_eq!(imp::raw_send(&channel, b"definitely not a request"), Some(Reply::Rejected));
            let mut wrong = encode_request(Path::new(r"C:\"), &[]).unwrap();
            wrong[4] = 9;
            assert_eq!(imp::raw_send(&channel, &wrong), Some(Reply::Rejected));
            // Over the pipe's message limit: the receiver sees more data than it accepts.
            assert_eq!(imp::raw_send(&channel, &vec![b'x'; MAX_PAYLOAD + 10]), Some(Reply::Rejected));
            assert!(inbox.take().is_empty());
        }

        #[test]
        fn launches_of_another_protocol_version_never_reach_each_other() {
            let tag = unique_tag("ver");
            let old = Channel::for_version(&tag, 1).unwrap();
            let old_inbox = Arc::new(Inbox::new());
            let _old = Server::claim(&old, old_inbox.clone()).unwrap().expect("v1 owner");
            let (stop, window) = window(old_inbox.clone());
            let new = Channel::current(&tag).unwrap();
            assert!(old.pipe_name().ends_with("-v1"), "{}", old.pipe_name());
            assert!(new.pipe_name().ends_with(&format!("-v{PROTOCOL_VERSION}")));
            let request = encode_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\Users")]).unwrap();
            // During an update overlap the new launch becomes a window of its own.
            match claim_or_forward(&new, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT) {
                Startup::Primary(_) => {}
                Startup::Forwarded => panic!("a v2 launch reached a v1 window"),
                Startup::Standalone(why) => panic!("the v2 launch should own its own pipe: {why}"),
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(window.join().unwrap().is_empty(), "the v1 window was contacted");
        }

        /// The documented trade-off: an "accepted" answer that arrives after the sender gave up
        /// leaves the launch open in both windows. Never lost, at worst one extra window.
        /// [`window`] for actions: every handoff, with its action.
        fn action_window(inbox: Arc<Inbox>) -> (Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<Vec<Handoff>>) {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::spawn(move || {
                let mut all = Vec::new();
                while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                    all.extend(inbox.take_handoffs());
                    std::thread::sleep(Duration::from_millis(10));
                }
                // A commit may still be on its way from the pipe thread.
                std::thread::sleep(Duration::from_millis(300));
                all.extend(inbox.take_handoffs());
                all
            });
            (stop, thread)
        }

        #[test]
        fn a_forwarded_action_reaches_the_owner_once_with_all_its_paths() {
            let channel = Channel::current(&unique_tag("act")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server = Server::claim(&channel, inbox.clone()).unwrap().expect("owner");
            let (stop, window) = action_window(inbox.clone());
            let paths: Vec<PathBuf> = (0..50).map(|i| PathBuf::from(format!(r"C:\sel\file {i}.jpg"))).collect();
            let request = encode_action_request(Path::new(r"C:\"), &paths, Some("add-zip")).unwrap();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, FORWARD_TIMEOUT) {
                Startup::Forwarded => {}
                Startup::Primary(_) => panic!("a second owner was allowed"),
                Startup::Standalone(why) => panic!("not forwarded: {why}"),
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                window.join().unwrap(),
                vec![Handoff {
                    action: Some("add-zip".into()),
                    paths
                }]
            );
        }

        #[test]
        fn a_late_accepted_answer_never_runs_an_action_twice() {
            let channel = Channel::current(&unique_tag("late-act")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server =
                Server::claim_with_reply_delay(&channel, inbox.clone(), Duration::from_secs(3))
                    .unwrap()
                    .expect("owner");
            let (stop, window) = action_window(inbox.clone());
            let request =
                encode_action_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\a.zip")], Some("extract-here"))
                    .unwrap();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, Duration::from_millis(1500)) {
                Startup::Standalone(why) => assert!(why.contains("in time"), "{why}"),
                _ => panic!("a sender past its deadline runs the action itself"),
            }
            // The receiver notices the missing commit once the sender's end is closed.
            std::thread::sleep(Duration::from_secs(3));
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                window.join().unwrap().is_empty(),
                "the window must not run an action its sender runs as well"
            );
        }

        #[test]
        fn a_late_accepted_answer_means_an_extra_window_never_a_lost_launch() {
            let channel = Channel::current(&unique_tag("late")).unwrap();
            let inbox = Arc::new(Inbox::new());
            let _server =
                Server::claim_with_reply_delay(&channel, inbox.clone(), Duration::from_secs(3))
                    .unwrap()
                    .expect("owner");
            let (stop, window) = window(inbox.clone());
            let request = encode_request(Path::new(r"C:\"), &[PathBuf::from(r"C:\Users")]).unwrap();
            match claim_or_forward(&channel, Arc::new(Inbox::new()), &request, Duration::from_millis(1500)) {
                Startup::Standalone(why) => assert!(why.contains("in time"), "{why}"),
                _ => panic!("a sender past its deadline must open its own window"),
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                window.join().unwrap(),
                vec![vec![PathBuf::from(r"C:\Users")]],
                "the running window opened it as well (the accepted, documented duplicate)"
            );
        }

        #[test]
        fn the_same_executable_is_recognised_through_path_aliases() {
            let exe = std::env::current_exe().unwrap();
            let plain = imp::path_key(&exe);
            let verbatim = PathBuf::from(format!(r"\\?\{}", exe.display()));
            assert_eq!(imp::path_key(&verbatim), plain);
            assert!(imp::same_file(&exe, &verbatim));
            let upper = PathBuf::from(exe.to_string_lossy().to_uppercase());
            assert_eq!(imp::path_key(&upper), plain);
            // An 8.3 alias, where the volume has short names.
            let wide = crate::to_wide(&exe);
            let mut short = vec![0u16; 1024];
            let len = unsafe {
                windows::Win32::Storage::FileSystem::GetShortPathNameW(
                    windows::core::PCWSTR(wide.as_ptr()),
                    Some(&mut short),
                )
            } as usize;
            if len > 0 && len < short.len() {
                let short = PathBuf::from(String::from_utf16_lossy(&short[..len]));
                if short != exe {
                    assert_eq!(imp::path_key(&short), plain, "{}", short.display());
                } else {
                    eprintln!("no 8.3 alias on this volume; skipped that case");
                }
            }
            let other = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\cmd.exe");
            assert!(!imp::same_file(&exe, &other));
            assert_ne!(imp::path_key(&other), plain);
        }

        #[test]
        fn bad_tags_are_refused() {
            for tag in ["", "a/b", "has space", &"x".repeat(49)] {
                assert!(Channel::current(tag).is_err(), "{tag:?}");
            }
        }
    }
}
