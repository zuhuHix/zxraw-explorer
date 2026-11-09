//! X11 drag and drop (XDND, protocol version 5) for the Linux build, on our own x11rb
//! connections: the drop target behind [`crate::drop_target`], a pointer query for hovers,
//! and the drag-out source.
//!
//! # Why not winit's
//!
//! winit 0.30 has its own XDND handler, but it only reports one `HoveredFile` at the enter
//! (no positions), always answers `XdndActionPrivate`, decodes the whole `text/uri-list` as
//! UTF-8 (one non-UTF-8 name loses the whole drop), rejects `file://localhost/` and
//! canonicalizes the paths (following symlinks). It has no drag-out at all.
//!
//! # Drop target: a proxy window
//!
//! A source sends its XDND client messages with `SendEvent` and an empty event mask, so the
//! X server delivers them to the client that created the target window: winit's connection,
//! not ours. XDND's `XdndProxy` redirects them. [`TargetThread::start`] creates an unmapped
//! `InputOnly` window on its own connection with `XdndProxy` (pointing at itself) and
//! `XdndAware` = 5, and sets `XdndProxy` on winit's toplevel (which already has
//! `XdndAware`). Sources then send every message of a drag over the toplevel to our proxy,
//! with the toplevel in the message's window field; winit never sees the drag, so nothing is
//! handled twice. A background thread serves the proxy:
//!
//! * `XdndEnter`: reads the offered types (`XdndTypeList` when there are more than three).
//!   Only drags offering `text/uri-list` are taken; anything else is refused without events,
//!   as on Windows.
//! * The first `XdndPosition` converts `XdndSelection` to `text/uri-list` (with the
//!   position's timestamp, `INCR` supported). When the files arrive the app gets
//!   [`DropEvent::Enter`]. Paths are percent-decoded byte by byte (non-UTF-8 names survive),
//!   `file://localhost/` is accepted and nothing is canonicalized.
//! * Every position is answered with `XdndStatus`: the app's latest
//!   [`set_answer`](crate::drop_target::DropTargetHandle::set_answer) (or the provisional
//!   answer before it gave one), limited to the source's `XdndActionList`, asking for more
//!   positions and setting no "quiet" rectangle. X positions carry no modifiers, so every
//!   position also reads Ctrl / Shift with `QueryPointer` for
//!   [`keys`](crate::drop_target::DropTargetHandle::keys), and wakes the app. A new answer
//!   from the app is sent to the source at once, so its cursor follows while the pointer
//!   rests.
//! * `XdndDrop` gives [`DropEvent::Drop`] and `XdndFinished`. A copy is reported as
//!   `XdndActionCopy`; a move as **`XdndActionPrivate`**: the app moves the files itself
//!   (like the optimized move on Windows), so a source that deletes its originals after a
//!   reported move can never race our move job.
//! * A source that dies (its window is destroyed), stops sending, or does not deliver the
//!   data in time ends the drag; a new `XdndEnter` replaces the current drag.
//!
//! # Drag-out source without grabs
//!
//! While the user holds the mouse button, winit's client holds the implicit pointer grab, so
//! a `GrabPointer` from our connection fails with `AlreadyGrabbed`. The source therefore
//! works without grabs: a background thread polls `QueryPointer` on the root (every 8 ms)
//! for the position, Button1 and the modifiers, and `QueryKeymap` for Escape. The window
//! under the pointer is found by descending from the root through `QueryPointer`'s `child`
//! to the first window with `XdndAware` (its `XdndProxy` honoured); our own toplevel is
//! never a target. Positions wait for the previous `XdndStatus` (or 500 ms). Releasing
//! Button1 over a window that accepted sends `XdndDrop` and serves `XdndSelection` until
//! `XdndFinished` (or 30 s); otherwise the drag ends with `XdndLeave` and
//! [`DropResult::None`]. The `DELETE` target is always refused: we never delete the
//! source files, whatever the target asks.
//!
//! The cursor: while the button is held, X shows the grab window's cursor (our toplevel's)
//! wherever the pointer is, and egui sets none outside the window. So [`DragCursors`], on a
//! connection of its own, sets a cursor-font glyph as our toplevel's cursor for the target's
//! answer (a hand when it accepts, a circle when it refuses, the plain arrow over no target)
//! and clears it when the drag ends, so winit's cursor applies again.
//!
//! Actions follow GTK's convention: Shift offers only Move, Ctrl+Shift only Link, Ctrl only
//! Copy; without modifiers Copy (suggested) and Link are offered, but not Move, so an
//! unmodified drag never moves the user's files (the Windows drag-out does not allow Move
//! either).
//!
//! The X11 tests run on a private Xvfb: `cargo test -p forge-win xdnd -- --ignored`.

use std::{
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use x11rb::{
    connection::{Connection, RequestConnection},
    errors::ReplyError,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _,
            CreateWindowAux, EventMask, KeyButMask, PropMode, Property, SELECTION_NOTIFY_EVENT,
            SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::clipboard::ClipboardEffect;
use crate::clipboard::linux as formats;
use crate::dragout::{DragItem, DropResult};
use crate::drop_target::{
    self, DragInfo, DropEffect, DropEvent, EFFECT_COPY, EFFECT_LINK, EFFECT_MOVE, Shared,
};
use crate::{Result, WinError};

const NONE: u32 = x11rb::NONE;
const CURRENT_TIME: u32 = x11rb::CURRENT_TIME;
/// The XDND version we speak (and put in `XdndAware`).
const XDND_VERSION: u32 = 5;
/// The oldest version we accept from a source or target.
const MIN_VERSION: u32 = 3;

/// The target gives up on the data of a drag after this.
const DATA_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a drop waits for data that was not received during the hover.
const DROP_DATA_WAIT: Duration = Duration::from_secs(3);
/// A drag with no message for this long ends if no mouse button is held.
const IDLE_CHECK: Duration = Duration::from_secs(10);
/// The source polls the pointer this often.
const POLL: Duration = Duration::from_millis(8);
/// The source sends the next position without a status after this.
const STATUS_WAIT: Duration = Duration::from_millis(500);
/// How long the source serves its data after the drop, waiting for `XdndFinished`.
const FINISH_WAIT: Duration = Duration::from_secs(30);

const XK_ESCAPE: u32 = 0xff1b;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;

/// `$DISPLAY`, when set and not empty.
pub(crate) fn display_name() -> Option<String> {
    std::env::var("DISPLAY").ok().filter(|d| !d.is_empty())
}

fn api_err(op: &'static str, error: impl std::fmt::Display) -> WinError {
    WinError::Api {
        op,
        code: 0,
        message: error.to_string(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Rules (pure)
// ---------------------------------------------------------------------------------------------

/// An XDND action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Copy,
    Move,
    Link,
    Ask,
    Private,
}

/// The action a source suggests for the held modifiers.
fn action_for_keys(ctrl: bool, shift: bool) -> Action {
    match (ctrl, shift) {
        (true, true) => Action::Link,
        (false, true) => Action::Move,
        _ => Action::Copy,
    }
}

/// What our drag-out offers in `XdndActionList` while `action` is suggested: only the
/// chosen action with a modifier (GTK's convention), Copy and Link without one. Move is
/// never offered unless asked for with Shift.
fn offered_actions(action: Action) -> &'static [Action] {
    match action {
        Action::Move => &[Action::Move],
        Action::Link => &[Action::Link],
        _ => &[Action::Copy, Action::Link],
    }
}

/// Every action, for the test source (the target's answer is what is tested).
const ALL_ACTIONS: &[Action] = &[Action::Copy, Action::Move, Action::Link];

fn action_bit(action: Action) -> u32 {
    match action {
        Action::Copy => EFFECT_COPY,
        Action::Move => EFFECT_MOVE,
        Action::Link => EFFECT_LINK,
        Action::Ask | Action::Private => 0,
    }
}

/// The `DROPEFFECT`-style bits a source allows: its `XdndActionList`, or else the suggested
/// action. Copy is XDND's default action, which every source handles, so it is always
/// allowed when the source gives no list.
fn allowed_effects(list: &[Action], suggested: Option<Action>) -> u32 {
    let listed = list.iter().fold(0, |bits, &a| bits | action_bit(a));
    if listed != 0 {
        listed
    } else {
        suggested.map_or(0, action_bit) | EFFECT_COPY
    }
}

/// What an `XdndStatus` says for `effect`: `None` is "not accepted".
fn status_action(effect: DropEffect) -> Option<Action> {
    match effect {
        DropEffect::None => None,
        DropEffect::Copy => Some(Action::Copy),
        DropEffect::Move => Some(Action::Move),
    }
}

/// What `XdndFinished` tells the source for the `answered` drop: a move is
/// `XdndActionPrivate` (we move the files ourselves; the source must not delete them).
fn finished_action(answered: DropEffect) -> Option<Action> {
    match answered {
        DropEffect::None => None,
        DropEffect::Copy => Some(Action::Copy),
        DropEffect::Move => Some(Action::Private),
    }
}

/// What the drag-out cursor shows: what the target under the pointer last said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragFeedback {
    /// Over no XDND window (or over our own window).
    NoTarget,
    /// Over an XDND window that does not accept the drop (or has not answered yet).
    Refused,
    Copy,
    Move,
    Link,
}

/// The feedback for a target's last status. An accepted drop with no or an unknown action
/// counts as Copy (XDND's default).
fn feedback_of(accepted: bool, action: Option<Action>) -> DragFeedback {
    match (accepted, action) {
        (false, _) => DragFeedback::Refused,
        (true, Some(Action::Move)) => DragFeedback::Move,
        (true, Some(Action::Link)) => DragFeedback::Link,
        (true, _) => DragFeedback::Copy,
    }
}

/// The drag-out result for an accepted drop with `action` (`raw` is its atom, for
/// [`DropResult::Other`]).
fn drop_result_of(accepted: bool, action: Option<Action>, raw: Atom) -> DropResult {
    match (accepted, action) {
        (false, _) => DropResult::None,
        (true, Some(Action::Copy)) => DropResult::Copy,
        (true, Some(Action::Move)) => DropResult::Move,
        (true, Some(Action::Link)) => DropResult::Link,
        (true, None) if raw == NONE => DropResult::Copy,
        (true, _) => DropResult::Other(raw),
    }
}

/// What [`test_drop_files`] returns for the target's `XdndFinished`: our target reports a
/// move as Private, so Private is [`DropResult::Move`], and a plain `XdndActionMove` (which
/// our target must never send) is [`DropResult::Other`] with its atom.
fn test_result_of(accepted: bool, action: Option<Action>, raw: Atom) -> DropResult {
    match (accepted, action) {
        (false, _) => DropResult::None,
        (true, Some(Action::Copy)) => DropResult::Copy,
        (true, Some(Action::Private)) => DropResult::Move,
        (true, Some(Action::Link)) => DropResult::Link,
        (true, _) => DropResult::Other(raw),
    }
}

// ---------------------------------------------------------------------------------------------
// X11 plumbing
// ---------------------------------------------------------------------------------------------

macro_rules! atoms {
    ($($field:ident = $name:expr),* $(,)?) => {
        struct Atoms {
            $($field: Atom,)*
        }

        impl Atoms {
            /// Interns every atom in one round trip.
            fn intern(conn: &RustConnection) -> Result<Self> {
                let names: &[&str] = &[$($name),*];
                let cookies = names
                    .iter()
                    .map(|name| conn.intern_atom(false, name.as_bytes()))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| api_err("InternAtom", e))?;
                let mut atoms = cookies.into_iter().map(|cookie| {
                    cookie.reply().map(|r| r.atom).map_err(|e| api_err("InternAtom", e))
                });
                Ok(Self {
                    $($field: atoms.next().expect("one cookie per name")?,)*
                })
            }
        }
    };
}

atoms! {
    aware = "XdndAware",
    proxy = "XdndProxy",
    enter = "XdndEnter",
    position = "XdndPosition",
    status = "XdndStatus",
    leave = "XdndLeave",
    drop = "XdndDrop",
    finished = "XdndFinished",
    selection = "XdndSelection",
    type_list = "XdndTypeList",
    action_list = "XdndActionList",
    action_description = "XdndActionDescription",
    copy = "XdndActionCopy",
    move_ = "XdndActionMove",
    link = "XdndActionLink",
    ask = "XdndActionAsk",
    private = "XdndActionPrivate",
    uri_list = "text/uri-list",
    gnome = "x-special/gnome-copied-files",
    utf8_string = "UTF8_STRING",
    text_plain = "text/plain",
    text_plain_utf8 = "text/plain;charset=utf-8",
    targets = "TARGETS",
    timestamp = "TIMESTAMP",
    incr = "INCR",
    delete = "DELETE",
    data0 = "_ZXRAW_XDND_DATA0",
    data1 = "_ZXRAW_XDND_DATA1",
    time_prop = "_ZXRAW_XDND_TIME",
    stop = "_ZXRAW_XDND_STOP",
    wake = "_ZXRAW_XDND_WAKE",
}

impl Atoms {
    fn action(&self, action: Action) -> Atom {
        match action {
            Action::Copy => self.copy,
            Action::Move => self.move_,
            Action::Link => self.link,
            Action::Ask => self.ask,
            Action::Private => self.private,
        }
    }

    fn action_of(&self, atom: Atom) -> Option<Action> {
        [
            Action::Copy,
            Action::Move,
            Action::Link,
            Action::Ask,
            Action::Private,
        ]
        .into_iter()
        .find(|&a| atom != NONE && self.action(a) == atom)
    }
}

/// A connection to `display` and the root window of its default screen.
fn connect(display: &str) -> Result<(RustConnection, Window)> {
    let (conn, screen) =
        RustConnection::connect(Some(display)).map_err(|e| api_err("XOpenDisplay", e))?;
    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or_else(|| WinError::Invalid("X screen does not exist".to_string()))?
        .root;
    Ok((conn, root))
}

/// The next event, waiting until `deadline` (`None`: forever). `Ok(None)` on timeout.
fn next_event(conn: &RustConnection, deadline: Option<Instant>) -> Result<Option<Event>> {
    loop {
        if let Some(event) = conn
            .poll_for_event()
            .map_err(|e| api_err("XNextEvent", e))?
        {
            return Ok(Some(event));
        }
        conn.flush().map_err(|e| api_err("XFlush", e))?;
        let millis = match deadline {
            None => -1,
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32
            }
        };
        let mut pollfd = libc::pollfd {
            fd: conn.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if ready > 0 && pollfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // Whatever is left is read now; then the connection is gone.
            if let Some(event) = conn
                .poll_for_event()
                .map_err(|e| api_err("XNextEvent", e))?
            {
                return Ok(Some(event));
            }
            return Err(api_err("XNextEvent", "the X server closed the connection"));
        }
    }
}

/// The 32-bit values of `window`'s `property` of `type_` (`None` if unset or on error).
fn property32(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: Atom,
) -> Option<Vec<u32>> {
    let reply = conn
        .get_property(false, window, property, type_, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ != type_ || reply.format != 32 {
        return None;
    }
    Some(reply.value32()?.collect())
}

/// A 32-bit client message to `to`, with `window` in its window field.
fn send_message(conn: &RustConnection, to: Window, window: Window, type_: Atom, data: [u32; 5]) {
    let message = ClientMessageEvent::new(32, window, type_, data);
    let _ = conn.send_event(false, to, EventMask::NO_EVENT, message);
}

/// A server timestamp, from a zero-length append to our own window's property (the window
/// must select `PropertyChange`).
fn server_time(conn: &RustConnection, window: Window, atoms: &Atoms) -> Result<u32> {
    conn.change_property8(
        PropMode::APPEND,
        window,
        atoms.time_prop,
        AtomEnum::STRING,
        &[],
    )
    .map_err(|e| api_err("ChangeProperty", e))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while let Some(event) = next_event(conn, Some(deadline))? {
        if let Event::PropertyNotify(e) = event
            && e.window == window
            && e.atom == atoms.time_prop
        {
            return Ok(e.time);
        }
    }
    Err(api_err(
        "PropertyNotify",
        "the X server did not answer in time",
    ))
}

/// The keycode of `keysym`, if the keyboard has it.
fn keycode_of(conn: &RustConnection, keysym: u32) -> Option<u8> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let reply = conn
        .get_keyboard_mapping(min, max - min + 1)
        .ok()?
        .reply()
        .ok()?;
    let per = usize::from(reply.keysyms_per_keycode).max(1);
    let index = reply.keysyms.iter().position(|&sym| sym == keysym)?;
    u8::try_from(index / per).ok().map(|offset| min + offset)
}

fn key_down(keymap: &[u8; 32], keycode: Option<u8>) -> bool {
    keycode.is_some_and(|code| keymap[usize::from(code / 8)] & (1 << (code % 8)) != 0)
}

// ---------------------------------------------------------------------------------------------
// Pointer query
// ---------------------------------------------------------------------------------------------

/// The pointer as [`pointer`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerState {
    /// Position relative to the window's origin, in physical pixels.
    pub x: i32,
    pub y: i32,
    pub ctrl: bool,
    pub shift: bool,
    pub button1: bool,
}

/// The cached connection of [`pointer`] and the display it is on.
static POINTER: Mutex<Option<(String, RustConnection)>> = Mutex::new(None);

/// The pointer relative to `window` on `$DISPLAY`, with the modifiers and Button1, in one
/// round trip on a cached connection (reconnected after an error). `None` without `DISPLAY`,
/// for a window that does not exist, or when the pointer is on another screen.
pub fn pointer(window: u32) -> Option<PointerState> {
    pointer_on(&display_name()?, window)
}

fn pointer_on(display: &str, window: u32) -> Option<PointerState> {
    let mut cached = lock(&POINTER);
    for _ in 0..2 {
        if cached.as_ref().is_none_or(|(name, _)| name != display) {
            *cached = Some((display.to_string(), connect(display).ok()?.0));
        }
        let (_, conn) = cached.as_ref()?;
        let reply = conn
            .query_pointer(window)
            .map_err(ReplyError::from)
            .and_then(|cookie| cookie.reply());
        match reply {
            Ok(reply) if reply.same_screen => {
                return Some(PointerState {
                    x: reply.win_x.into(),
                    y: reply.win_y.into(),
                    ctrl: reply.mask.contains(KeyButMask::CONTROL),
                    shift: reply.mask.contains(KeyButMask::SHIFT),
                    button1: reply.mask.contains(KeyButMask::BUTTON1),
                });
            }
            Ok(_) | Err(ReplyError::X11Error(_)) => return None,
            // The connection broke: reconnect once.
            Err(_) => *cached = None,
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Drop target
// ---------------------------------------------------------------------------------------------

/// The XDND target thread of a [`crate::drop_target::DropTargetHandle`]. Dropping it removes
/// `XdndProxy` from the toplevel and stops the thread (which destroys the proxy window).
pub(crate) struct TargetThread {
    /// The handle's own connection, for notes to the thread and the cleanup.
    conn: RustConnection,
    toplevel: Window,
    proxy: Window,
    proxy_atom: Atom,
    stop_atom: Atom,
    wake_atom: Atom,
    done: mpsc::Receiver<()>,
}

impl TargetThread {
    /// Puts an XDND proxy of ours on `toplevel` and starts the thread serving it.
    pub(crate) fn start(
        display: &str,
        toplevel: Window,
        shared: Arc<Mutex<Shared>>,
        wake: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let (conn, root) = connect(display)?;
        let atoms = Atoms::intern(&conn)?;
        conn.get_window_attributes(toplevel)
            .map_err(|e| api_err("GetWindowAttributes", e))?
            .reply()
            .map_err(|e| WinError::Invalid(format!("X11 window {toplevel:#x}: {e}")))?;
        let proxy = conn.generate_id().map_err(|e| api_err("XGenerateId", e))?;
        conn.create_window(
            0,
            proxy,
            root,
            -100,
            -100,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| api_err("CreateWindow", e))?;
        conn.change_property32(
            PropMode::REPLACE,
            proxy,
            atoms.aware,
            AtomEnum::ATOM,
            &[XDND_VERSION],
        )
        .map_err(|e| api_err("ChangeProperty(XdndAware)", e))?;
        conn.change_property32(
            PropMode::REPLACE,
            proxy,
            atoms.proxy,
            AtomEnum::WINDOW,
            &[proxy],
        )
        .map_err(|e| api_err("ChangeProperty(XdndProxy)", e))?;
        conn.change_property32(
            PropMode::REPLACE,
            toplevel,
            atoms.proxy,
            AtomEnum::WINDOW,
            &[proxy],
        )
        .map_err(|e| api_err("ChangeProperty(XdndProxy)", e))?
        .check()
        .map_err(|e| api_err("ChangeProperty(XdndProxy)", e))?;

        let (handle_conn, _) = connect(display)?;
        let (proxy_atom, stop_atom, wake_atom) = (atoms.proxy, atoms.stop, atoms.wake);
        let (done_tx, done) = mpsc::channel();
        let mut target = Target {
            conn,
            atoms,
            root,
            toplevel,
            proxy,
            shared,
            wake,
            drag: None,
            generation: 0,
        };
        let spawned = std::thread::Builder::new()
            .name("zxraw-xdnd-target".to_string())
            .spawn(move || {
                target.run();
                drop(target);
                let _ = done_tx.send(());
            });
        let thread = Self {
            conn: handle_conn,
            toplevel,
            proxy,
            proxy_atom,
            stop_atom,
            wake_atom,
            done,
        };
        if let Err(e) = spawned {
            // Dropping `thread` removes the proxy property again.
            drop(thread);
            return Err(api_err("spawn(XDND target)", e));
        }
        Ok(thread)
    }

    /// The app's answer changed: the thread tells the source, if it differs from the last
    /// status.
    pub(crate) fn answer_changed(&self) {
        send_message(&self.conn, self.proxy, self.proxy, self.wake_atom, [0; 5]);
        let _ = self.conn.flush();
    }
}

impl Drop for TargetThread {
    fn drop(&mut self) {
        // Only our own proxy is removed: winit could have been given another one since.
        if property32(
            &self.conn,
            self.toplevel,
            self.proxy_atom,
            AtomEnum::WINDOW.into(),
        )
        .is_some_and(|value| value.first() == Some(&self.proxy))
        {
            let _ = self.conn.delete_property(self.toplevel, self.proxy_atom);
        }
        send_message(&self.conn, self.proxy, self.proxy, self.stop_atom, [0; 5]);
        // A round trip: the server handles both before this connection closes.
        let _ = self.conn.sync();
        let _ = self.done.recv_timeout(Duration::from_secs(1));
    }
}

/// The data of the drag being hovered.
enum Data {
    /// Not asked for yet.
    Wanted,
    Requested {
        time: u32,
        since: Instant,
        /// `Some` while an `INCR` transfer collects chunks.
        incr: Option<Vec<u8>>,
    },
    Received(Vec<PathBuf>),
    /// Refused, unreadable, late, or no local files.
    Failed,
}

/// The XDND drag over our window.
struct TargetDrag {
    source: Window,
    takes: bool,
    data: Data,
    /// Our property for this drag's data (alternating, so a late answer for the previous
    /// drag is told apart).
    property: Atom,
    /// What the source allows, as `DROPEFFECT` bits.
    allowed: u32,
    /// What our last status said; `None` before the first.
    status: Option<DropEffect>,
    last_message: Instant,
    /// The deadline for the data of a drop that is waiting for it.
    drop: Option<Instant>,
    /// The app got `Enter` for this drag.
    entered: bool,
}

struct Target {
    conn: RustConnection,
    atoms: Atoms,
    root: Window,
    toplevel: Window,
    proxy: Window,
    shared: Arc<Mutex<Shared>>,
    wake: Box<dyn Fn() + Send + Sync>,
    drag: Option<TargetDrag>,
    generation: u32,
}

impl Target {
    fn run(&mut self) {
        while let Ok(event) = next_event(&self.conn, self.deadline()) {
            if let Some(event) = event
                && !self.handle(event)
            {
                break;
            }
            self.tick();
            if self.conn.flush().is_err() {
                break;
            }
        }
        self.end_drag();
        let _ = self.conn.sync();
    }

    /// Handles one event; `false` to stop.
    fn handle(&mut self, event: Event) -> bool {
        let a = &self.atoms;
        match event {
            Event::ClientMessage(e) if e.format == 32 => {
                let l = e.data.as_data32();
                if e.type_ == a.stop && e.window == self.proxy {
                    return false;
                } else if e.type_ == a.wake {
                    self.answer_changed();
                } else if e.type_ == a.enter {
                    self.enter(l);
                } else if e.type_ == a.position {
                    self.position(l);
                } else if e.type_ == a.leave {
                    if self.drag.as_ref().is_some_and(|d| d.source == l[0]) {
                        self.end_drag();
                    }
                } else if e.type_ == a.drop {
                    self.dropped(l);
                }
            }
            Event::SelectionNotify(e) if e.requestor == self.proxy => self.selection_notify(e),
            Event::PropertyNotify(e)
                if e.window == self.proxy && e.state == Property::NEW_VALUE =>
            {
                self.incr_chunk(e.atom)
            }
            Event::DestroyNotify(e) if self.drag.as_ref().is_some_and(|d| d.source == e.window) => {
                self.end_drag();
            }
            _ => {}
        }
        true
    }

    fn deadline(&self) -> Option<Instant> {
        let drag = self.drag.as_ref()?;
        let mut deadline = drag.last_message + IDLE_CHECK;
        if let Data::Requested { since, .. } = drag.data {
            deadline = deadline.min(since + DATA_TIMEOUT);
        }
        if let Some(drop) = drag.drop {
            deadline = deadline.min(drop);
        }
        Some(deadline)
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if let Data::Requested { since, .. } = drag.data
            && now >= since + DATA_TIMEOUT
        {
            tracing::debug!("XDND: the drag source did not deliver text/uri-list in time");
            drag.data = Data::Failed;
        }
        if drag.drop.is_some_and(|deadline| now >= deadline) {
            self.finish_drop();
            return;
        }
        self.try_finish_drop();
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if drag.drop.is_none() && now >= drag.last_message + IDLE_CHECK {
            // A drag needs a held button: without one the source has gone quiet for good.
            let held = self
                .conn
                .query_pointer(self.root)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_none_or(|reply| {
                    reply.mask.intersects(
                        KeyButMask::BUTTON1
                            | KeyButMask::BUTTON2
                            | KeyButMask::BUTTON3
                            | KeyButMask::BUTTON4
                            | KeyButMask::BUTTON5,
                    )
                });
            if held {
                drag.last_message = now;
            } else {
                self.end_drag();
            }
        }
    }

    /// Ends the current drag (Leave for the app if it had Enter).
    fn end_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let mut shared = drop_target::lock(&self.shared);
        shared.dragging = false;
        if drag.entered {
            shared.events.push(DropEvent::Leave);
            drop(shared);
            (self.wake)();
        }
    }

    fn enter(&mut self, l: [u32; 5]) {
        self.end_drag();
        let (source, version) = (l[0], l[1] >> 24);
        if version < MIN_VERSION {
            tracing::debug!("XDND: ignored a drag of version {version}");
            return;
        }
        let types: Vec<Atom> = if l[1] & 1 != 0 {
            property32(
                &self.conn,
                source,
                self.atoms.type_list,
                AtomEnum::ATOM.into(),
            )
            .unwrap_or_default()
        } else {
            l[2..].iter().copied().filter(|&t| t != NONE).collect()
        };
        // DestroyNotify tells us when the source goes away mid-drag.
        let _ = self.conn.change_window_attributes(
            source,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        );
        self.generation = self.generation.wrapping_add(1);
        let property = if self.generation.is_multiple_of(2) {
            self.atoms.data0
        } else {
            self.atoms.data1
        };
        drop_target::lock(&self.shared).answered_by_app = false;
        self.drag = Some(TargetDrag {
            source,
            takes: types.contains(&self.atoms.uri_list),
            data: Data::Wanted,
            property,
            allowed: EFFECT_COPY,
            status: None,
            last_message: Instant::now(),
            drop: None,
            entered: false,
        });
    }

    /// Ctrl and Shift now, from `QueryPointer` (positions carry no modifiers).
    fn query_keys(&self) -> Option<(bool, bool)> {
        let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((
            reply.mask.contains(KeyButMask::CONTROL),
            reply.mask.contains(KeyButMask::SHIFT),
        ))
    }

    fn position(&mut self, l: [u32; 5]) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if drag.source != l[0] || drag.drop.is_some() {
            return;
        }
        drag.last_message = Instant::now();
        let (time, suggested) = (l[3], self.atoms.action_of(l[4]));
        // The action list (it may change with the modifiers) and the keys: one round trip.
        let list = self.conn.get_property(
            false,
            drag.source,
            self.atoms.action_list,
            AtomEnum::ATOM,
            0,
            64,
        );
        let pointer = self.conn.query_pointer(self.root);
        let list: Vec<Action> = list
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().map(|v| v.collect::<Vec<_>>()))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|atom| self.atoms.action_of(atom))
            .collect();
        let keys = pointer
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| {
                (
                    reply.mask.contains(KeyButMask::CONTROL),
                    reply.mask.contains(KeyButMask::SHIFT),
                )
            });
        drag.allowed = allowed_effects(&list, suggested);
        if let Some(keys) = keys {
            drop_target::lock(&self.shared).keys = keys;
        }
        if drag.takes && matches!(drag.data, Data::Wanted) {
            self.request_data(time);
        }
        let effect = self.effect();
        self.send_status(effect);
        (self.wake)();
    }

    /// The effect for the app's current answer (provisional before it gave one).
    fn effect(&self) -> DropEffect {
        let Some(drag) = &self.drag else {
            return DropEffect::None;
        };
        if !drag.takes || matches!(drag.data, Data::Failed) {
            return DropEffect::None;
        }
        let mut shared = drop_target::lock(&self.shared);
        if !shared.answered_by_app {
            let (ctrl, shift) = shared.keys;
            shared.answer = drop_target::provisional_answer(false, ctrl, shift);
        }
        drop_target::resolve_effect(shared.answer, drag.allowed, false)
    }

    fn send_status(&mut self, effect: DropEffect) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let action = status_action(effect);
        // Bit 0: accepted; bit 1: send positions everywhere (the rectangle stays empty).
        let flags = u32::from(action.is_some()) | 2;
        let data = [
            self.toplevel,
            flags,
            0,
            0,
            action.map_or(NONE, |a| self.atoms.action(a)),
        ];
        send_message(
            &self.conn,
            drag.source,
            drag.source,
            self.atoms.status,
            data,
        );
        drag.status = Some(effect);
    }

    fn answer_changed(&mut self) {
        let Some(drag) = &self.drag else {
            return;
        };
        if drag.status.is_none() || drag.drop.is_some() {
            return;
        }
        let effect = self.effect();
        if self.drag.as_ref().and_then(|d| d.status) != Some(effect) {
            self.send_status(effect);
        }
    }

    fn request_data(&mut self, time: u32) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let time = if time == 0 { CURRENT_TIME } else { time };
        let _ = self.conn.delete_property(self.proxy, drag.property);
        let _ = self.conn.convert_selection(
            self.proxy,
            self.atoms.selection,
            self.atoms.uri_list,
            drag.property,
            time,
        );
        drag.data = Data::Requested {
            time,
            since: Instant::now(),
            incr: None,
        };
    }

    fn selection_notify(&mut self, e: SelectionNotifyEvent) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let Data::Requested {
            time, incr: None, ..
        } = drag.data
        else {
            return;
        };
        if e.selection != self.atoms.selection || e.target != self.atoms.uri_list {
            return;
        }
        if e.property == NONE {
            // A refusal names no property; the time tells it apart from a late one.
            if time == CURRENT_TIME || e.time == CURRENT_TIME || e.time == time {
                drag.data = Data::Failed;
                self.try_finish_drop();
            }
            return;
        }
        if e.property != drag.property {
            return;
        }
        let Some((type_, bytes)) = self.take_property() else {
            if let Some(drag) = self.drag.as_mut() {
                drag.data = Data::Failed;
            }
            self.try_finish_drop();
            return;
        };
        if type_ == self.atoms.incr {
            if let Some(drag) = self.drag.as_mut() {
                // Deleting the property (done by the read) asks for the first chunk.
                drag.data = Data::Requested {
                    time,
                    since: Instant::now(),
                    incr: Some(Vec::new()),
                };
            }
            return;
        }
        self.received(&bytes);
    }

    /// Reads and deletes the drag's property: (type, bytes).
    fn take_property(&self) -> Option<(Atom, Vec<u8>)> {
        let drag = self.drag.as_ref()?;
        let reply = self
            .conn
            .get_property(
                true,
                self.proxy,
                drag.property,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )
            .ok()?
            .reply()
            .ok()?;
        Some((reply.type_, reply.value))
    }

    fn incr_chunk(&mut self, property: Atom) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        if property != drag.property || !matches!(drag.data, Data::Requested { incr: Some(_), .. })
        {
            return;
        }
        let chunk = self.take_property().map(|(_, bytes)| bytes);
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let Data::Requested {
            since,
            incr: Some(buffer),
            ..
        } = &mut drag.data
        else {
            return;
        };
        match chunk {
            Some(chunk) if chunk.is_empty() => {
                let bytes = std::mem::take(buffer);
                self.received(&bytes);
            }
            Some(chunk) => {
                buffer.extend_from_slice(&chunk);
                *since = Instant::now();
            }
            None => {
                drag.data = Data::Failed;
                self.try_finish_drop();
            }
        }
    }

    /// The `text/uri-list` arrived: Enter for the app (unless the drop is waiting for it).
    fn received(&mut self, bytes: &[u8]) {
        let files = formats::parse_uri_list(bytes);
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if files.is_empty() {
            drag.data = Data::Failed;
        } else {
            drag.data = Data::Received(files.clone());
            if drag.drop.is_none() {
                drag.entered = true;
                let mut shared = drop_target::lock(&self.shared);
                shared.dragging = true;
                shared.events.push(DropEvent::Enter(DragInfo {
                    files,
                    virtual_count: 0,
                }));
                drop(shared);
                (self.wake)();
            }
        }
        self.try_finish_drop();
    }

    fn dropped(&mut self, l: [u32; 5]) {
        let source = l[0];
        let Some(drag) = self.drag.as_mut().filter(|d| d.source == source) else {
            // A drop we know nothing about: let the source finish.
            send_message(
                &self.conn,
                source,
                source,
                self.atoms.finished,
                [self.toplevel, 0, NONE, 0, 0],
            );
            return;
        };
        drag.last_message = Instant::now();
        drag.drop = Some(Instant::now() + DROP_DATA_WAIT);
        if drag.takes && matches!(drag.data, Data::Wanted) {
            self.request_data(l[2]);
        }
        self.try_finish_drop();
    }

    /// Finishes a drop that is waiting, once its data is in (or will never come).
    fn try_finish_drop(&mut self) {
        let ready = self.drag.as_ref().is_some_and(|d| {
            d.drop.is_some() && !matches!(d.data, Data::Requested { .. } | Data::Wanted if d.takes)
        });
        if ready {
            self.finish_drop();
        }
    }

    fn finish_drop(&mut self) {
        let keys = self.query_keys();
        if let Some(keys) = keys {
            drop_target::lock(&self.shared).keys = keys;
        }
        let refused = self
            .drag
            .as_ref()
            .is_none_or(|d| d.status == Some(DropEffect::None));
        let effect = self.effect();
        let Some(drag) = self.drag.take() else {
            return;
        };
        let files = match drag.data {
            Data::Received(files) => files,
            _ => Vec::new(),
        };
        let answered = if files.is_empty() || refused {
            DropEffect::None
        } else {
            effect
        };
        let mut shared = drop_target::lock(&self.shared);
        shared.dragging = false;
        let (ctrl, shift) = shared.keys;
        let woke = if !files.is_empty() {
            if !drag.entered {
                shared.events.push(DropEvent::Enter(DragInfo {
                    files: files.clone(),
                    virtual_count: 0,
                }));
            }
            shared.events.push(DropEvent::Drop {
                info: DragInfo {
                    files,
                    virtual_count: 0,
                },
                virtual_source: None,
                ctrl,
                shift,
                answered,
            });
            true
        } else if drag.entered {
            shared.events.push(DropEvent::Leave);
            true
        } else {
            false
        };
        drop(shared);
        if woke {
            (self.wake)();
        }
        let action = finished_action(answered);
        let data = [
            self.toplevel,
            u32::from(action.is_some()),
            action.map_or(NONE, |a| self.atoms.action(a)),
            0,
            0,
        ];
        send_message(
            &self.conn,
            drag.source,
            drag.source,
            self.atoms.finished,
            data,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

/// An XDND window: where messages go (`deliver`, its proxy or itself) and the version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Site {
    window: Window,
    deliver: Window,
    version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Under {
    Nothing,
    Own,
    Site(Site),
}

/// What a source hears back.
enum Reply {
    Status {
        target: Window,
        accepted: bool,
        action: Atom,
    },
    Finished {
        target: Window,
        accepted: bool,
        action: Atom,
    },
    /// Another client took `XdndSelection`.
    Lost,
}

/// One INCR transfer of ours in progress.
struct OutTransfer {
    requestor: Window,
    property: Atom,
    target: Atom,
    bytes: Vec<u8>,
    offset: usize,
    started: Instant,
}

/// The source side: an unmapped window owning `XdndSelection`, its data and the messages.
struct Source {
    conn: RustConnection,
    atoms: Atoms,
    root: Window,
    window: Window,
    /// The server time we took the selection at, and when that was here.
    time0: u32,
    started: Instant,
    paths: Vec<PathBuf>,
    /// The suggested action (also what `x-special/gnome-copied-files` says).
    action: Action,
    offered: &'static [Action],
    chunk: usize,
    transfers: Vec<OutTransfer>,
}

impl Source {
    fn new(display: &str, paths: Vec<PathBuf>, offered: &'static [Action]) -> Result<Self> {
        if paths.is_empty() {
            return Err(WinError::Invalid("nothing to drag".to_string()));
        }
        for path in &paths {
            let bytes = path.as_os_str().as_bytes();
            if !path.is_absolute() || bytes.contains(&0) {
                return Err(WinError::Invalid(format!(
                    "not an absolute path: {}",
                    path.display()
                )));
            }
        }
        let (conn, root) = connect(display)?;
        let atoms = Atoms::intern(&conn)?;
        let window = conn.generate_id().map_err(|e| api_err("XGenerateId", e))?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| api_err("CreateWindow", e))?;
        let chunk = conn
            .maximum_request_bytes()
            .saturating_sub(1024)
            .clamp(4096, 256 * 1024);
        let mut source = Self {
            conn,
            atoms,
            root,
            window,
            time0: 0,
            started: Instant::now(),
            paths,
            action: Action::Copy,
            offered: &[],
            chunk,
            transfers: Vec::new(),
        };
        let types = source.types();
        source
            .conn
            .change_property32(
                PropMode::REPLACE,
                window,
                source.atoms.type_list,
                AtomEnum::ATOM,
                &types,
            )
            .map_err(|e| api_err("ChangeProperty(XdndTypeList)", e))?;
        source.set_offered(offered);
        source.time0 = server_time(&source.conn, window, &source.atoms)?;
        source.started = Instant::now();
        let a = &source.atoms;
        source
            .conn
            .set_selection_owner(window, a.selection, source.time0)
            .map_err(|e| api_err("SetSelectionOwner(XdndSelection)", e))?;
        let owner = source
            .conn
            .get_selection_owner(a.selection)
            .map_err(|e| api_err("GetSelectionOwner", e))?
            .reply()
            .map_err(|e| api_err("GetSelectionOwner", e))?
            .owner;
        if owner != window {
            return Err(api_err(
                "SetSelectionOwner(XdndSelection)",
                "another client took the selection at the same time",
            ));
        }
        Ok(source)
    }

    /// The offered types, most specific first.
    fn types(&self) -> Vec<Atom> {
        let a = &self.atoms;
        vec![
            a.uri_list,
            a.gnome,
            a.utf8_string,
            a.text_plain_utf8,
            a.text_plain,
        ]
    }

    fn set_offered(&mut self, offered: &'static [Action]) {
        if self.offered == offered {
            return;
        }
        self.offered = offered;
        let atoms: Vec<Atom> = offered.iter().map(|&a| self.atoms.action(a)).collect();
        let mut names = Vec::new();
        for action in offered {
            names.extend_from_slice(format!("{action:?}").as_bytes());
            names.push(0);
        }
        let a = &self.atoms;
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            self.window,
            a.action_list,
            AtomEnum::ATOM,
            &atoms,
        );
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            self.window,
            a.action_description,
            AtomEnum::STRING,
            &names,
        );
    }

    /// An estimate of the server time now (the selection time plus the time since).
    fn now(&self) -> u32 {
        let elapsed = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        self.time0.wrapping_add(elapsed)
    }

    fn send(&self, site: Site, type_: Atom, data: [u32; 5]) {
        send_message(&self.conn, site.deliver, site.window, type_, data);
        let _ = self.conn.flush();
    }

    fn enter(&self, site: Site) {
        let types = self.types();
        let more = u32::from(types.len() > 3);
        let mut data = [self.window, site.version << 24 | more, 0, 0, 0];
        for (slot, &atom) in data[2..].iter_mut().zip(&types) {
            *slot = atom;
        }
        self.send(site, self.atoms.enter, data);
    }

    fn position(&self, site: Site, x: i16, y: i16, action: Action) {
        let point = (u32::from(x as u16) << 16) | u32::from(y as u16);
        let data = [self.window, 0, point, self.now(), self.atoms.action(action)];
        self.send(site, self.atoms.position, data);
    }

    fn leave(&self, site: Site) {
        self.send(site, self.atoms.leave, [self.window, 0, 0, 0, 0]);
    }

    fn drop_on(&self, site: Site) {
        self.send(site, self.atoms.drop, [self.window, 0, self.now(), 0, 0]);
    }

    /// Answers selection requests and INCR progress; returns what concerns the drag.
    fn handle(&mut self, event: Event) -> Option<Reply> {
        let reply = match event {
            Event::SelectionRequest(e) => {
                self.answer(&e);
                None
            }
            Event::PropertyNotify(e) if e.state == Property::DELETE => {
                self.continue_transfer(e.window, e.atom);
                None
            }
            Event::SelectionClear(e)
                if e.owner == self.window && e.selection == self.atoms.selection =>
            {
                Some(Reply::Lost)
            }
            Event::ClientMessage(e) if e.format == 32 && e.window == self.window => {
                let l = e.data.as_data32();
                if e.type_ == self.atoms.status {
                    tracing::debug!(
                        "XDND source: status from {:#x}: flags {:#x}, action {}",
                        l[0],
                        l[1],
                        self.atom_name(l[4])
                    );
                    Some(Reply::Status {
                        target: l[0],
                        accepted: l[1] & 1 != 0,
                        action: l[4],
                    })
                } else if e.type_ == self.atoms.finished {
                    tracing::debug!(
                        "XDND source: finished from {:#x}: flags {:#x}, action {}",
                        l[0],
                        l[1],
                        self.atom_name(l[2])
                    );
                    Some(Reply::Finished {
                        target: l[0],
                        accepted: l[1] & 1 != 0,
                        action: l[2],
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        // A requestor that went away never deletes the property: forget it.
        self.transfers
            .retain(|t| t.started.elapsed() < Duration::from_secs(30));
        let _ = self.conn.flush();
        reply
    }

    fn payload(&self, target: Atom) -> Option<Vec<u8>> {
        let a = &self.atoms;
        if target == a.uri_list {
            Some(formats::uri_list(&self.paths))
        } else if target == a.gnome {
            let effect = if self.action == Action::Move {
                ClipboardEffect::Move
            } else {
                ClipboardEffect::Copy
            };
            Some(formats::gnome_copied_files(&self.paths, effect))
        } else if [a.utf8_string, a.text_plain, a.text_plain_utf8].contains(&target) {
            Some(formats::plain_text(&self.paths))
        } else {
            None
        }
    }

    /// An atom's name, for the debug log.
    fn atom_name(&self, atom: Atom) -> String {
        if atom == NONE {
            return "None".to_string();
        }
        self.conn
            .get_atom_name(atom)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map_or_else(
                || atom.to_string(),
                |reply| String::from_utf8_lossy(&reply.name).into_owned(),
            )
    }

    fn answer(&mut self, e: &SelectionRequestEvent) {
        tracing::debug!(
            "XDND source: {:#x} asks for {}",
            e.requestor,
            self.atom_name(e.target)
        );
        // Obsolete clients send no property: use the target as the property name.
        let property = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let a = &self.atoms;
        let answered = if e.selection != a.selection || e.owner != self.window {
            false
        } else if e.target == a.targets {
            let mut list = vec![a.targets, a.timestamp];
            list.extend(self.types());
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &list,
                )
                .is_ok()
        } else if e.target == a.timestamp {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[self.time0],
                )
                .is_ok()
        } else if e.target == a.delete {
            // We never delete the dragged files, whatever the target did with them.
            tracing::debug!("XDND: refused the target's DELETE request");
            false
        } else if let Some(bytes) = self.payload(e.target) {
            if bytes.len() > self.chunk {
                self.start_transfer(e.requestor, property, e.target, bytes)
            } else {
                self.conn
                    .change_property8(PropMode::REPLACE, e.requestor, property, e.target, &bytes)
                    .is_ok()
            }
        } else {
            false
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if answered { property } else { NONE },
        };
        let _ = self
            .conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, notify);
    }

    fn start_transfer(
        &mut self,
        requestor: Window,
        property: Atom,
        target: Atom,
        bytes: Vec<u8>,
    ) -> bool {
        let Ok(len) = u32::try_from(bytes.len()) else {
            return false;
        };
        let watched = self.conn.change_window_attributes(
            requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        );
        let announced = self.conn.change_property32(
            PropMode::REPLACE,
            requestor,
            property,
            self.atoms.incr,
            &[len],
        );
        if watched.is_err() || announced.is_err() {
            return false;
        }
        self.transfers
            .retain(|t| !(t.requestor == requestor && t.property == property));
        self.transfers.push(OutTransfer {
            requestor,
            property,
            target,
            bytes,
            offset: 0,
            started: Instant::now(),
        });
        true
    }

    /// The requestor deleted the property: send the next chunk, or the empty end marker.
    fn continue_transfer(&mut self, window: Window, property: Atom) {
        let Some(index) = self
            .transfers
            .iter()
            .position(|t| t.requestor == window && t.property == property)
        else {
            return;
        };
        let transfer = &mut self.transfers[index];
        let end = (transfer.offset + self.chunk).min(transfer.bytes.len());
        let chunk = &transfer.bytes[transfer.offset..end];
        let finished = chunk.is_empty();
        let _ =
            self.conn
                .change_property8(PropMode::APPEND, window, property, transfer.target, chunk);
        transfer.offset = end;
        if finished {
            self.transfers.remove(index);
        }
    }

    fn aware_version(&self, window: Window) -> Option<u32> {
        property32(&self.conn, window, self.atoms.aware, AtomEnum::ATOM.into())?
            .first()
            .copied()
    }

    /// The XDND site of `window` (which has `XdndAware` = `version`), its proxy honoured.
    fn site(&self, window: Window, version: u32) -> Option<Site> {
        if version < MIN_VERSION {
            return None;
        }
        let proxy_of = |w: Window| {
            property32(&self.conn, w, self.atoms.proxy, AtomEnum::WINDOW.into())?
                .first()
                .copied()
        };
        // A proxy counts only if it names itself (the spec's check against stale proxies).
        let deliver = proxy_of(window)
            .filter(|&proxy| proxy_of(proxy) == Some(proxy))
            .unwrap_or(window);
        Some(Site {
            window,
            deliver,
            version: version.min(XDND_VERSION),
        })
    }

    /// The XDND window under the pointer, descending from `root_child` (`QueryPointer`'s
    /// child of the root). Our own toplevel `own` (and everything in it) is never a target.
    fn under_pointer(&self, root_child: Window, own: Window) -> Under {
        let mut window = if root_child == NONE {
            self.root
        } else {
            root_child
        };
        for _ in 0..32 {
            if window == own {
                return Under::Own;
            }
            if let Some(version) = self.aware_version(window) {
                return self
                    .site(window, version)
                    .map_or(Under::Nothing, Under::Site);
            }
            if window == self.root {
                return Under::Nothing;
            }
            let child = self
                .conn
                .query_pointer(window)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or(NONE, |reply| reply.child);
            if child == NONE {
                return Under::Nothing;
            }
            window = child;
        }
        Under::Nothing
    }

    /// Pointer (root position, child of the root, mask) and keymap: one round trip.
    fn input(&self) -> Result<(i16, i16, Window, KeyButMask, [u8; 32])> {
        let pointer = self
            .conn
            .query_pointer(self.root)
            .map_err(|e| api_err("QueryPointer", e))?;
        let keymap = self
            .conn
            .query_keymap()
            .map_err(|e| api_err("QueryKeymap", e))?;
        let p = pointer.reply().map_err(|e| api_err("QueryPointer", e))?;
        let k = keymap.reply().map_err(|e| api_err("QueryKeymap", e))?;
        Ok((p.root_x, p.root_y, p.child, p.mask, k.keys))
    }
}

impl Drop for Source {
    /// Waits until the server has handled everything we sent: it may drop the unread
    /// requests of a client that hangs up (the last `XdndLeave` would be lost).
    fn drop(&mut self) {
        let _ = self.conn.sync();
    }
}

/// A drag-out started by [`start_drag_out`], run by a background thread.
pub struct DragOutSession {
    shared: Arc<SessionShared>,
}

#[derive(Default)]
struct SessionShared {
    state: Mutex<SessionState>,
    cancel: AtomicBool,
    /// The session value was dropped: a drag still in the air is cancelled.
    detached: AtomicBool,
}

struct SessionState {
    feedback: DragFeedback,
    result: Option<std::result::Result<DropResult, String>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            feedback: DragFeedback::NoTarget,
            result: None,
        }
    }
}

impl DragOutSession {
    /// `None` while the drag runs; the result once it ended (the same value on every later
    /// call). After a drop it comes when the target sent `XdndFinished` (or after 30 s).
    pub fn poll(&self) -> Option<Result<DropResult>> {
        let state = lock(&self.shared.state);
        state.result.as_ref().map(|result| match result {
            Ok(result) => Ok(*result),
            Err(message) => Err(WinError::Api {
                op: "XDND drag",
                code: 0,
                message: message.clone(),
            }),
        })
    }

    /// What the window under the pointer last said, for the cursor.
    pub fn feedback(&self) -> DragFeedback {
        lock(&self.shared.state).feedback
    }

    /// Ends the drag (`XdndLeave`, result [`DropResult::None`]). Once the drop was sent it
    /// has no effect: the target may still be reading the data.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
    }
}

impl Drop for DragOutSession {
    /// A drag still in the air is cancelled; after a drop the thread keeps serving the data
    /// until the target finished.
    fn drop(&mut self) {
        self.shared.detached.store(true, Ordering::Release);
    }
}

/// Starts an XDND drag of `items` on `$DISPLAY`, while the user holds Button1 (the app calls
/// it when its own drag leaves the window). `window` is our toplevel: it is never a target.
/// The drag ends on release, Escape or [`DragOutSession::cancel`]; see the module docs.
pub fn start_drag_out(window: u32, items: &[DragItem]) -> Result<DragOutSession> {
    let display = display_name().ok_or(WinError::Unsupported)?;
    start_drag_out_on(&display, window, items)
}

pub(crate) fn start_drag_out_on(
    display: &str,
    window: u32,
    items: &[DragItem],
) -> Result<DragOutSession> {
    let paths = items.iter().map(|item| item.path.clone()).collect();
    let source = Source::new(display, paths, offered_actions(Action::Copy))?;
    let cursor_display = display.to_string();
    let shared = Arc::new(SessionShared::default());
    let thread_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("zxraw-xdnd-source".to_string())
        .spawn(move || {
            let cursors = DragCursors::new(&cursor_display, window);
            let result = run_drag(source, window, &thread_shared, cursors.as_ref());
            if let Some(cursors) = &cursors {
                cursors.clear();
            }
            let mut state = lock(&thread_shared.state);
            state.feedback = DragFeedback::NoTarget;
            state.result = Some(result.map_err(|e| e.to_string()));
        })
        .map_err(|e| api_err("spawn(XDND source)", e))?;
    Ok(DragOutSession { shared })
}

/// The drag-out cursor: cursor-font glyphs set as our toplevel's cursor (see the module
/// docs). Errors are ignored: the cursor is only feedback.
struct DragCursors {
    conn: RustConnection,
    window: Window,
    /// Accepted, refused, no target.
    glyphs: [u32; 3],
    shown: Mutex<Option<DragFeedback>>,
}

impl DragCursors {
    /// `XC_hand2`, `XC_circle` and `XC_left_ptr` of the standard cursor font.
    const GLYPHS: [u16; 3] = [60, 24, 68];

    fn new(display: &str, window: Window) -> Option<Self> {
        let (conn, _) = RustConnection::connect(Some(display)).ok()?;
        let font = conn.generate_id().ok()?;
        conn.open_font(font, b"cursor").ok()?;
        let mut glyphs = [0; 3];
        for (slot, glyph) in glyphs.iter_mut().zip(Self::GLYPHS) {
            let cursor = conn.generate_id().ok()?;
            conn.create_glyph_cursor(
                cursor, font, font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff,
            )
            .ok()?;
            *slot = cursor;
        }
        let _ = conn.close_font(font);
        conn.flush().ok()?;
        Some(Self {
            conn,
            window,
            glyphs,
            shown: Mutex::new(None),
        })
    }

    fn show(&self, feedback: DragFeedback) {
        let mut shown = self.shown.lock().unwrap_or_else(|p| p.into_inner());
        if *shown == Some(feedback) {
            return;
        }
        *shown = Some(feedback);
        let glyph = match feedback {
            DragFeedback::Copy | DragFeedback::Move | DragFeedback::Link => self.glyphs[0],
            DragFeedback::Refused => self.glyphs[1],
            DragFeedback::NoTarget => self.glyphs[2],
        };
        self.set(glyph);
    }

    /// Back to no cursor of ours (winit sets its own again on its next change).
    fn clear(&self) {
        self.set(NONE);
    }

    fn set(&self, cursor: u32) {
        let _ = self.conn.change_window_attributes(
            self.window,
            &ChangeWindowAttributesAux::new().cursor(cursor),
        );
        let _ = self.conn.flush();
    }
}

/// The status a source has from its current target.
#[derive(Clone, Copy, Default)]
struct Status {
    accepted: bool,
    action: Atom,
}

fn run_drag(
    mut source: Source,
    own: Window,
    shared: &SessionShared,
    cursors: Option<&DragCursors>,
) -> Result<DropResult> {
    let escape = keycode_of(&source.conn, XK_ESCAPE);
    let set_feedback = |feedback| {
        lock(&shared.state).feedback = feedback;
        if let Some(cursors) = cursors {
            cursors.show(feedback);
        }
    };
    let mut site: Option<Site> = None;
    let mut status = Status::default();
    // When the last position went out without a status yet.
    let mut awaiting: Option<Instant> = None;
    let mut sent: Option<(i16, i16, Action)> = None;
    let mut cache: Option<(Window, Under, Instant)> = None;
    let mut tick = Instant::now();

    let handle = |source: &mut Source,
                  event: Event,
                  site: Option<Site>,
                  status: &mut Status,
                  awaiting: &mut Option<Instant>|
     -> Option<Reply> {
        match source.handle(event) {
            Some(Reply::Status {
                target,
                accepted,
                action,
            }) if site.is_some_and(|s| target == s.window || target == s.deliver) => {
                *status = Status { accepted, action };
                *awaiting = None;
                None
            }
            other => other,
        }
    };

    // Hover.
    let (x, y) = loop {
        while let Some(event) = next_event(&source.conn, Some(tick))? {
            if let Some(Reply::Lost) = handle(&mut source, event, site, &mut status, &mut awaiting)
            {
                if let Some(site) = site {
                    source.leave(site);
                }
                return Ok(DropResult::None);
            }
        }
        tick = Instant::now() + POLL;
        let (x, y, root_child, mask, keymap) = source.input()?;
        let cancelled =
            shared.cancel.load(Ordering::Acquire) || shared.detached.load(Ordering::Acquire);
        if cancelled || key_down(&keymap, escape) {
            if let Some(site) = site {
                source.leave(site);
            }
            return Ok(DropResult::None);
        }
        if !mask.contains(KeyButMask::BUTTON1) {
            break (x, y);
        }
        let under = match cache {
            Some((child, under, at))
                if child == root_child && at.elapsed() < Duration::from_millis(250) =>
            {
                under
            }
            _ => {
                let under = source.under_pointer(root_child, own);
                cache = Some((root_child, under, Instant::now()));
                under
            }
        };
        let new_site = match under {
            Under::Site(site) => Some(site),
            _ => None,
        };
        if new_site != site {
            if let Some(old) = site {
                source.leave(old);
            }
            if let Some(new) = new_site {
                source.enter(new);
            }
            site = new_site;
            status = Status::default();
            awaiting = None;
            sent = None;
        }
        let action = action_for_keys(
            mask.contains(KeyButMask::CONTROL),
            mask.contains(KeyButMask::SHIFT),
        );
        if action != source.action {
            source.action = action;
            source.set_offered(offered_actions(action));
        }
        if let Some(site) = site {
            let stale = awaiting.is_some_and(|at| at.elapsed() > STATUS_WAIT);
            if sent != Some((x, y, action)) && (awaiting.is_none() || stale) {
                source.position(site, x, y, action);
                sent = Some((x, y, action));
                awaiting = Some(Instant::now());
            }
        }
        set_feedback(match under {
            Under::Site(_) => feedback_of(status.accepted, source.atoms.action_of(status.action)),
            _ => DragFeedback::NoTarget,
        });
    };

    // Released.
    let Some(site) = site else {
        return Ok(DropResult::None);
    };
    if sent.is_some_and(|(sx, sy, _)| (sx, sy) != (x, y)) || awaiting.is_some() {
        // The release point is where the drop happens: tell the target, and wait for its
        // answer (briefly).
        if awaiting.is_none() {
            source.position(site, x, y, source.action);
        }
        let deadline = Instant::now() + STATUS_WAIT;
        awaiting = Some(Instant::now());
        while awaiting.is_some() {
            let Some(event) = next_event(&source.conn, Some(deadline))? else {
                break;
            };
            if let Some(Reply::Lost) =
                handle(&mut source, event, Some(site), &mut status, &mut awaiting)
            {
                break;
            }
        }
    }
    if !status.accepted {
        source.leave(site);
        return Ok(DropResult::None);
    }
    source.drop_on(site);
    let deadline = Instant::now() + FINISH_WAIT;
    let mut awaiting = None;
    while let Some(event) = next_event(&source.conn, Some(deadline))? {
        match handle(&mut source, event, Some(site), &mut status, &mut awaiting) {
            Some(Reply::Finished {
                target,
                accepted,
                action,
            }) if target == site.window || target == site.deliver => {
                return Ok(if site.version >= 5 {
                    let action = if accepted && action == NONE {
                        status.action
                    } else {
                        action
                    };
                    drop_result_of(accepted, source.atoms.action_of(action), action)
                } else {
                    drop_result_of(
                        status.accepted,
                        source.atoms.action_of(status.action),
                        status.action,
                    )
                });
            }
            // Someone else took the selection: the target can no longer read ours.
            Some(Reply::Lost) => break,
            _ => {}
        }
    }
    tracing::debug!("XDND: the drop target sent no XdndFinished");
    Ok(drop_result_of(
        status.accepted,
        source.atoms.action_of(status.action),
        status.action,
    ))
}

// ---------------------------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------------------------

/// For e2e tests: drops `paths` on the toplevel `window` at window-relative physical
/// `(x, y)` on `$DISPLAY`, as a minimal blocking XDND source, and returns what the target's
/// `XdndFinished` reported. With `ctrl` / `shift`, Control_L / Shift_L are held through
/// XTEST for the drag (so the target's `QueryPointer` sees them) and released afterwards.
///
/// Positions go out every 50 ms (each after the previous status) until one is accepted
/// (at most 10 s; the app needs some frames to answer), and for 300 ms more so the status
/// reflects the app's real answer for that place; then the drop. The result is mapped for
/// our own target, which reports a move as `XdndActionPrivate`: Private is
/// [`DropResult::Move`], Copy is [`DropResult::Copy`], a plain `XdndActionMove` (which it
/// must never send) is [`DropResult::Other`] with the atom, and a refused drop (or none) is
/// [`DropResult::None`].
#[doc(hidden)]
pub fn test_drop_files(
    window: u32,
    x: i32,
    y: i32,
    paths: &[PathBuf],
    ctrl: bool,
    shift: bool,
) -> Result<DropResult> {
    let display = display_name().ok_or(WinError::Unsupported)?;
    drop_files_on(&display, window, x, y, paths, ctrl, shift)
}

pub(crate) fn drop_files_on(
    display: &str,
    window: u32,
    x: i32,
    y: i32,
    paths: &[PathBuf],
    ctrl: bool,
    shift: bool,
) -> Result<DropResult> {
    use x11rb::protocol::xtest::ConnectionExt as _;

    const KEY_PRESS: u8 = 2;
    const KEY_RELEASE: u8 = 3;
    let mut source = Source::new(display, paths.to_vec(), ALL_ACTIONS)?;
    source.action = action_for_keys(ctrl, shift);
    let mut keys = Vec::new();
    for (held, keysym) in [(ctrl, XK_CONTROL_L), (shift, XK_SHIFT_L)] {
        if held {
            keys.push(keycode_of(&source.conn, keysym).ok_or_else(|| {
                WinError::Invalid(format!("the keyboard has no keysym {keysym:#x}"))
            })?);
        }
    }
    let fake = |source: &Source, kind: u8, code: u8| {
        source
            .conn
            .xtest_fake_input(kind, code, CURRENT_TIME, source.root, 0, 0, 0)
            .map_err(|e| api_err("XTestFakeInput", e))
            .map(drop)
    };
    let mut pressed = Vec::new();
    let mut result = Ok(());
    for &code in &keys {
        result = fake(&source, KEY_PRESS, code);
        if result.is_err() {
            break;
        }
        pressed.push(code);
    }
    let result = result
        .and_then(|()| source.conn.sync().map_err(|e| api_err("XSync", e)))
        .and_then(|()| test_drop(&mut source, window, x, y));
    for &code in pressed.iter().rev() {
        let _ = fake(&source, KEY_RELEASE, code);
    }
    let _ = source.conn.sync();
    result
}

fn test_drop(source: &mut Source, window: u32, x: i32, y: i32) -> Result<DropResult> {
    let version = source
        .aware_version(window)
        .ok_or_else(|| WinError::Invalid(format!("window {window:#x} is not XdndAware")))?;
    let site = source
        .site(window, version)
        .ok_or_else(|| WinError::Invalid(format!("window {window:#x} speaks XDND {version}")))?;
    let clamp = |v: i32| v.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
    let point = source
        .conn
        .translate_coordinates(window, source.root, clamp(x), clamp(y))
        .map_err(|e| api_err("TranslateCoordinates", e))?
        .reply()
        .map_err(|e| api_err("TranslateCoordinates", e))?;
    let (rx, ry) = (point.dst_x, point.dst_y);
    let action = source.action;
    source.enter(site);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = Status::default();
    let mut accepted_at: Option<Instant> = None;
    let mut next_position = Instant::now();
    let mut awaiting: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if now >= deadline {
            source.leave(site);
            return Ok(DropResult::None);
        }
        if status.accepted
            && accepted_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(300))
        {
            break;
        }
        let stale = awaiting.is_some_and(|at| at.elapsed() > Duration::from_secs(1));
        if now >= next_position && (awaiting.is_none() || stale) {
            source.position(site, rx, ry, action);
            awaiting = Some(now);
            next_position = now + Duration::from_millis(50);
        }
        let wait = next_position
            .max(now + Duration::from_millis(5))
            .min(deadline);
        while let Some(event) = next_event(&source.conn, Some(wait))? {
            if let Some(Reply::Status {
                target,
                accepted,
                action,
            }) = source.handle(event)
                && (target == site.window || target == site.deliver)
            {
                status = Status { accepted, action };
                awaiting = None;
                if accepted && accepted_at.is_none() {
                    accepted_at = Some(Instant::now());
                }
            }
        }
    }
    source.drop_on(site);
    let deadline = Instant::now() + FINISH_WAIT;
    while let Some(event) = next_event(&source.conn, Some(deadline))? {
        if let Some(Reply::Finished {
            target,
            accepted,
            action,
        }) = source.handle(event)
            && (target == site.window || target == site.deliver)
        {
            return Ok(test_result_of(
                accepted,
                source.atoms.action_of(action),
                action,
            ));
        }
    }
    Err(api_err(
        "XdndFinished",
        "the drop target did not finish the drop",
    ))
}

#[cfg(test)]
#[path = "xdnd_linux_tests.rs"]
mod tests;
