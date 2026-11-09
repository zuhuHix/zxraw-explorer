//! Linux file clipboard: the X11 `CLIPBOARD` selection (x11rb) or, on Wayland compositors with
//! the wlr / ext data-control protocol, wl-clipboard-rs.
//!
//! # Formats
//!
//! A publish offers what Linux file managers (Nemo, Nautilus, Caja, Thunar, Dolphin) read:
//!
//! * `x-special/gnome-copied-files`: `copy` or `cut`, then one `file://` URI per line.
//! * `text/uri-list`: the URIs separated by CRLF.
//! * `application/x-kde-cutselection` = `1`, for a cut only (Dolphin's cut marker).
//! * For our own publishes (not [`super::set_files_without_text`]): the paths as plain text, one
//!   per line (`UTF8_STRING`, `text/plain;charset=utf-8`, `text/plain`, `STRING`), and the owner
//!   token, 8 little-endian bytes in the private target `application/x-zxraw-clipboard-owner`
//!   (the Windows "ZXraw Explorer Clipboard Owner" format).
//!
//! Paths are percent-encoded per RFC 3986 byte by byte (unreserved characters and `/` kept), so
//! paths that are not UTF-8 survive. Reading ignores URIs that are not local `file:` URIs.
//!
//! # Backends
//!
//! Picked per call: Wayland when `WAYLAND_DISPLAY` is set and the compositor offers data-control
//! (probed once per process); otherwise X11 (also XWayland) when `DISPLAY` is set; otherwise
//! every fallible function returns [`WinError::Unsupported`] and the others 0 / `false` / `None`.
//!
//! X11 ownership is an unmapped window holding `CLIPBOARD`, served by one background thread
//! (`TARGETS`, `TIMESTAMP`, `MULTIPLE`, every format, `INCR` for large values) until another
//! client takes the selection or we replace or clear it. Only one such thread runs per process.
//! While this process owns the selection, reads come from in-process state.
//!
//! # What is approximated
//!
//! * **Sequence number.** X11 has no clipboard sequence counter, and the XFixes extension is not
//!   enabled in this build. [`super::clipboard_sequence`] is a hash of (selection owner window,
//!   the owner's `TIMESTAMP` of acquiring the selection, an in-process counter bumped by every
//!   publish or clear of ours). A foreign app that re-copies takes the selection again with a new
//!   timestamp, so its copies are told apart; an owner that does not answer `TIMESTAMP` within
//!   250 ms counts as timestamp 0, and then two copies by the same window look alike. On Wayland
//!   the hash is over the offered MIME types, the owner token (if any) and the same counter, so
//!   two foreign copies offering the same types look alike.
//! * **Atomicity.** The Windows "if unchanged" / "if token" checks run inside one clipboard
//!   session. Here they are atomic against other threads of this process (one lock), but another
//!   client can still take the selection between the check and our write.
//! * **Lifetime.** X11 and Wayland selections live in the owning process: the content is gone
//!   when ZXraw Explorer exits, unless a clipboard manager kept a copy.
//!
//! The Wayland path is untested here (no compositor with data-control on the test machine); the
//! X11 path is tested against a private Xvfb (`cargo test -p forge-win clipboard:: -- --ignored`).

use std::{
    collections::HashSet,
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration,
};

use super::{ClipboardEffect, ClipboardRead, ClipboardToken};
use crate::{Result, WinError};

pub(super) const OWNER_TARGET: &str = "application/x-zxraw-clipboard-owner";
pub(super) const GNOME_TARGET: &str = "x-special/gnome-copied-files";
pub(super) const URI_LIST_TARGET: &str = "text/uri-list";
pub(super) const KDE_CUT_TARGET: &str = "application/x-kde-cutselection";
/// Text targets of our own publishes, most specific first.
pub(super) const TEXT_TARGETS: [&str; 4] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain",
    "STRING",
];
/// Targets that count as text when another app owns the clipboard.
const TEXT_LIKE_TARGETS: [&str; 5] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain",
    "STRING",
    "TEXT",
];

/// How long a read waits for the selection owner to answer.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a sequence check waits for the owner's `TIMESTAMP`.
const TIMESTAMP_TIMEOUT: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------------------------
// Payloads (pure)
// ---------------------------------------------------------------------------------------------

/// One publish: the file list and every target it is offered as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Publish {
    pub paths: Vec<PathBuf>,
    pub effect: ClipboardEffect,
    pub token: Option<ClipboardToken>,
    pub targets: Vec<(&'static str, Vec<u8>)>,
}

impl Publish {
    fn data(&self, target: &str) -> Option<&[u8]> {
        self.targets
            .iter()
            .find(|(name, _)| *name == target)
            .map(|(_, bytes)| bytes.as_slice())
    }

    fn has_text(&self) -> bool {
        self.data(TEXT_TARGETS[0]).is_some()
    }
}

/// Builds every target of a publish, validating `paths` first. `token` is `Some` for our own
/// publishes (text and owner marker included), `None` for what a file manager's Copy/Cut offers.
pub(super) fn build_publish(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    token: Option<ClipboardToken>,
) -> Result<Publish> {
    if paths.is_empty() {
        return Err(WinError::Invalid(
            "at least one file path is required".to_string(),
        ));
    }
    for path in paths {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(WinError::Invalid(format!(
                "path is empty or contains a NUL: {}",
                path.display()
            )));
        }
        if !path.is_absolute() {
            return Err(WinError::Invalid(format!(
                "path is not absolute: {}",
                path.display()
            )));
        }
    }
    let mut targets = Vec::new();
    // Our owner token first, as on Windows.
    if let Some(token) = token {
        targets.push((OWNER_TARGET, token_bytes(token).to_vec()));
    }
    targets.push((GNOME_TARGET, gnome_copied_files(paths, effect)));
    targets.push((URI_LIST_TARGET, uri_list(paths)));
    if effect == ClipboardEffect::Move {
        targets.push((KDE_CUT_TARGET, b"1".to_vec()));
    }
    if token.is_some() {
        let text = plain_text(paths);
        for target in TEXT_TARGETS {
            targets.push((target, text.clone()));
        }
    }
    Ok(Publish {
        paths: paths.to_vec(),
        effect,
        token,
        targets,
    })
}

pub(super) fn token_bytes(token: ClipboardToken) -> [u8; 8] {
    token.0.to_le_bytes()
}

/// The token in an owner-target value, `None` when it is too short to be one.
pub(super) fn parse_token(bytes: &[u8]) -> Option<ClipboardToken> {
    let raw: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(ClipboardToken(u64::from_le_bytes(raw)))
}

/// `file://` URI of an absolute path, every byte other than RFC 3986 unreserved and `/`
/// percent-encoded.
pub(super) fn encode_file_uri(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX[usize::from(byte >> 4)]));
            uri.push(char::from(HEX[usize::from(byte & 0xF)]));
        }
    }
    uri
}

/// The local path of a `file:` URI (`file:///p`, `file://localhost/p` or `file:/p`); `None` for
/// other schemes, remote hosts, and values that do not decode to an absolute path.
pub(super) fn decode_file_uri(uri: &str) -> Option<PathBuf> {
    let uri = uri.trim_matches(|c: char| c == '\r' || c == '\n' || c == ' ' || c == '\t');
    let (scheme, rest) = uri.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let path = if let Some(after) = rest.strip_prefix("//") {
        let slash = after.find('/')?;
        let host = &after[..slash];
        if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
            return None;
        }
        &after[slash..]
    } else {
        rest
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if !path.starts_with('/') {
        return None;
    }
    let bytes = percent_decode(path.as_bytes());
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                input.get(i + 1).copied().and_then(hex),
                input.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// `x-special/gnome-copied-files`: `copy` or `cut`, then the URIs, newline separated, with no
/// trailing newline.
pub(super) fn gnome_copied_files(paths: &[PathBuf], effect: ClipboardEffect) -> Vec<u8> {
    let mut text = String::from(match effect {
        ClipboardEffect::Copy => "copy",
        ClipboardEffect::Move => "cut",
    });
    for path in paths {
        text.push('\n');
        text.push_str(&encode_file_uri(path));
    }
    text.into_bytes()
}

/// `text/uri-list`: the URIs separated by CRLF.
pub(super) fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .map(|path| encode_file_uri(path))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

/// The paths as text, one per line (lossy for paths that are not UTF-8).
fn plain_text(paths: &[PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// Parses `x-special/gnome-copied-files`. `None` without a `copy`/`cut` header or local files.
pub(super) fn parse_gnome_copied_files(bytes: &[u8]) -> Option<(Vec<PathBuf>, ClipboardEffect)> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split('\n');
    let effect = match lines.next()?.trim_end_matches('\r') {
        "copy" => ClipboardEffect::Copy,
        "cut" => ClipboardEffect::Move,
        _ => return None,
    };
    let paths: Vec<PathBuf> = lines.filter_map(decode_file_uri).collect();
    (!paths.is_empty()).then_some((paths, effect))
}

/// Parses `text/uri-list` (CRLF or LF separated, `#` comments skipped), keeping local files.
pub(super) fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.starts_with('#'))
        .filter_map(decode_file_uri)
        .collect()
}

/// Dolphin's cut marker: `1` (perhaps NUL or newline terminated) is a cut.
pub(super) fn is_kde_cut(bytes: &[u8]) -> bool {
    bytes.first() == Some(&b'1')
}

/// The file list of a clipboard whose targets `has` reports, fetching values with `fetch`.
fn read_file_targets(
    has: impl Fn(&str) -> bool,
    mut fetch: impl FnMut(&str) -> Result<Option<Vec<u8>>>,
) -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
    if has(GNOME_TARGET)
        && let Some(bytes) = fetch(GNOME_TARGET)?
        && let Some(files) = parse_gnome_copied_files(&bytes)
    {
        return Ok(Some(files));
    }
    if has(URI_LIST_TARGET)
        && let Some(bytes) = fetch(URI_LIST_TARGET)?
    {
        let paths = parse_uri_list(&bytes);
        if !paths.is_empty() {
            let cut = has(KDE_CUT_TARGET) && fetch(KDE_CUT_TARGET)?.is_some_and(|b| is_kde_cut(&b));
            let effect = if cut {
                ClipboardEffect::Move
            } else {
                ClipboardEffect::Copy
            };
            return Ok(Some((paths, effect)));
        }
    }
    Ok(None)
}

/// 32-bit FNV-1a, never zero.
fn sequence_hash(parts: &[&[u8]]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for part in parts {
        for &byte in *part {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        // Separator, so ("ab", "c") and ("a", "bc") differ.
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

// ---------------------------------------------------------------------------------------------
// Operations, shared by both backends
// ---------------------------------------------------------------------------------------------

/// One look at, and possibly one change of, the clipboard, with this process's lock held.
trait Backend {
    fn sequence(&mut self) -> Result<u32>;
    fn token(&mut self) -> Result<Option<ClipboardToken>>;
    fn files(&mut self) -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>>;
    fn has_text(&mut self) -> Result<bool>;
    fn publish(&mut self, publish: Publish) -> Result<()>;
    fn clear(&mut self) -> Result<()>;
}

/// Process-wide clipboard state: the X11 owner (if any) and the change counter.
struct State {
    owner: Option<x11::OwnerHandle>,
    counter: u32,
}

static STATE: Mutex<State> = Mutex::new(State {
    owner: None,
    counter: 0,
});

fn lock_state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Debug, PartialEq, Eq)]
enum Display {
    X11(String),
    Wayland,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn display() -> Option<Display> {
    static WAYLAND_DATA_CONTROL: OnceLock<bool> = OnceLock::new();
    pick_display(
        env_nonempty("WAYLAND_DISPLAY"),
        env_nonempty("DISPLAY"),
        || *WAYLAND_DATA_CONTROL.get_or_init(wayland::supported),
    )
}

/// Wayland when it is there and has data-control, else X11 (perhaps XWayland), else nothing.
fn pick_display(
    wayland_display: Option<String>,
    x11_display: Option<String>,
    data_control: impl FnOnce() -> bool,
) -> Option<Display> {
    if wayland_display.is_some() && data_control() {
        return Some(Display::Wayland);
    }
    x11_display.map(Display::X11)
}

fn with_backend<T>(op: impl FnOnce(&mut dyn Backend) -> Result<T>) -> Result<T> {
    match display() {
        None => Err(WinError::Unsupported),
        Some(Display::X11(name)) => with_x11(&name, op),
        Some(Display::Wayland) => {
            let mut state = lock_state();
            op(&mut wayland::WlBackend::new(&mut state))
        }
    }
}

/// Runs `op` against the X display `name` (tests pass their own Xvfb here).
fn with_x11<T>(name: &str, op: impl FnOnce(&mut dyn Backend) -> Result<T>) -> Result<T> {
    let mut state = lock_state();
    let mut backend = x11::X11Backend::open(&mut state, name)?;
    op(&mut backend)
}

fn op_set_if_unchanged(b: &mut dyn Backend, publish: Publish, baseline: u32) -> Result<bool> {
    if b.sequence()? != baseline && !b.token()?.is_some_and(ClipboardToken::is_from_this_process) {
        return Ok(false);
    }
    b.publish(publish)?;
    Ok(true)
}

fn op_read(b: &mut dyn Backend) -> Result<ClipboardRead> {
    Ok(ClipboardRead {
        files: b.files()?,
        owner: b.token()?,
        sequence: b.sequence()?,
    })
}

fn op_clear_if_owned_files(
    b: &mut dyn Backend,
    paths: &[PathBuf],
    expected_sequence: Option<u32>,
) -> Result<Option<u32>> {
    if paths.is_empty() {
        return Ok(None);
    }
    if let Some(expected) = expected_sequence
        && b.sequence()? != expected
    {
        return Ok(None);
    }
    match b.files()? {
        Some((current, _)) if current == paths => {
            b.clear()?;
            Ok(Some(b.sequence()?))
        }
        _ => Ok(None),
    }
}

fn op_clear_if_token(b: &mut dyn Backend, token: ClipboardToken) -> Result<bool> {
    if b.token()? != Some(token) {
        return Ok(false);
    }
    b.clear()?;
    Ok(true)
}

fn op_replace_if_token(
    b: &mut dyn Backend,
    publish: Option<Publish>,
    token: ClipboardToken,
) -> Result<Option<ClipboardToken>> {
    if b.token()? != Some(token) {
        return Ok(None);
    }
    match publish {
        None => {
            b.clear()?;
            Ok(None)
        }
        Some(publish) => {
            let new_token = publish.token;
            b.publish(publish)?;
            Ok(new_token)
        }
    }
}

// Entry points for `clipboard.rs`. Paths are validated before anything touches the clipboard.

pub(super) fn set_files(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    token: Option<ClipboardToken>,
) -> Result<()> {
    let publish = build_publish(paths, effect, token)?;
    with_backend(|b| b.publish(publish))
}

pub(super) fn set_files_if_unchanged(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    baseline: u32,
    token: ClipboardToken,
) -> Result<bool> {
    let publish = build_publish(paths, effect, Some(token))?;
    with_backend(|b| op_set_if_unchanged(b, publish, baseline))
}

pub(super) fn get_files_and_owner() -> Result<ClipboardRead> {
    with_backend(op_read)
}

pub(super) fn clipboard_sequence() -> u32 {
    with_backend(|b| b.sequence()).unwrap_or(0)
}

pub(super) fn clipboard_has_text() -> bool {
    with_backend(|b| b.has_text()).unwrap_or(false)
}

pub(super) fn clear_if_owned_files(
    paths: &[PathBuf],
    expected_sequence: Option<u32>,
) -> Option<u32> {
    if paths.is_empty() {
        return None;
    }
    with_backend(|b| op_clear_if_owned_files(b, paths, expected_sequence))
        .ok()
        .flatten()
}

pub(super) fn clear_if_token(token: ClipboardToken) -> Result<bool> {
    with_backend(|b| op_clear_if_token(b, token))
}

pub(super) fn replace_files_if_token(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    token: ClipboardToken,
    new_token: ClipboardToken,
) -> Result<Option<ClipboardToken>> {
    let publish = if paths.is_empty() {
        None
    } else {
        Some(build_publish(paths, effect, Some(new_token))?)
    };
    with_backend(|b| op_replace_if_token(b, publish, token))
}

fn api_err(op: &'static str, error: impl std::fmt::Display) -> WinError {
    WinError::Api {
        op,
        code: 0,
        message: error.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// X11
// ---------------------------------------------------------------------------------------------

mod x11 {
    use std::{
        os::fd::AsRawFd,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use x11rb::{
        connection::{Connection, RequestConnection},
        protocol::{
            Event,
            xproto::{
                Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _,
                CreateWindowAux, EventMask, PropMode, Property, SELECTION_NOTIFY_EVENT,
                SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
            },
        },
        rust_connection::RustConnection,
        wrapper::ConnectionExt as _,
    };

    use super::*;

    const CURRENT_TIME: u32 = x11rb::CURRENT_TIME;
    const NONE: u32 = x11rb::NONE;

    /// Format targets this module knows by name.
    const FORMAT_NAMES: [&str; 9] = [
        OWNER_TARGET,
        GNOME_TARGET,
        URI_LIST_TARGET,
        KDE_CUT_TARGET,
        "UTF8_STRING",
        "text/plain;charset=utf-8",
        "text/plain",
        "STRING",
        "TEXT",
    ];

    pub(super) struct Atoms {
        pub clipboard: Atom,
        pub targets: Atom,
        pub timestamp: Atom,
        pub multiple: Atom,
        pub atom_pair: Atom,
        pub incr: Atom,
        /// Our property for receiving conversions and getting a server timestamp.
        pub prop: Atom,
        /// Client message type that asks an owner thread to stop.
        pub stop: Atom,
        formats: Vec<(&'static str, Atom)>,
    }

    impl Atoms {
        pub fn intern(conn: &RustConnection) -> Result<Self> {
            let fixed = [
                "CLIPBOARD",
                "TARGETS",
                "TIMESTAMP",
                "MULTIPLE",
                "ATOM_PAIR",
                "INCR",
                "_ZXRAW_CLIPBOARD_PROPERTY",
                "_ZXRAW_CLIPBOARD_STOP",
            ];
            // Send every request first, then collect: one round trip.
            let cookies = fixed
                .iter()
                .chain(FORMAT_NAMES.iter())
                .map(|name| conn.intern_atom(false, name.as_bytes()))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| api_err("InternAtom", e))?;
            let atoms = cookies
                .into_iter()
                .map(|cookie| cookie.reply().map(|reply| reply.atom))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| api_err("InternAtom", e))?;
            Ok(Self {
                clipboard: atoms[0],
                targets: atoms[1],
                timestamp: atoms[2],
                multiple: atoms[3],
                atom_pair: atoms[4],
                incr: atoms[5],
                prop: atoms[6],
                stop: atoms[7],
                formats: FORMAT_NAMES
                    .iter()
                    .copied()
                    .zip(atoms[fixed.len()..].iter().copied())
                    .collect(),
            })
        }

        pub fn format(&self, name: &str) -> Atom {
            self.formats
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, atom)| *atom)
                .expect("format target interned")
        }
    }

    fn connect(display: &str) -> Result<(RustConnection, usize)> {
        RustConnection::connect(Some(display)).map_err(|e| api_err("XOpenDisplay", e))
    }

    /// An unmapped 1x1 window that receives property changes.
    fn create_window(conn: &RustConnection, screen: usize) -> Result<Window> {
        let root = conn
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| WinError::Invalid("X screen does not exist".to_string()))?
            .root;
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
        Ok(window)
    }

    /// Waits until `deadline` for an event `wanted` accepts; other events are dropped.
    fn wait_event(
        conn: &RustConnection,
        deadline: Instant,
        mut wanted: impl FnMut(&Event) -> bool,
    ) -> Result<Option<Event>> {
        conn.flush().map_err(|e| api_err("XFlush", e))?;
        loop {
            while let Some(event) = conn
                .poll_for_event()
                .map_err(|e| api_err("XNextEvent", e))?
            {
                if wanted(&event) {
                    return Ok(Some(event));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let millis = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32;
            let mut pollfd = libc::pollfd {
                fd: conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            unsafe { libc::poll(&mut pollfd, 1, millis) };
        }
    }

    /// A server timestamp, from a zero-length append to our own window's property.
    fn server_time(conn: &RustConnection, window: Window, atoms: &Atoms) -> Result<u32> {
        conn.change_property8(PropMode::APPEND, window, atoms.prop, AtomEnum::STRING, &[])
            .map_err(|e| api_err("ChangeProperty", e))?;
        match wait_event(
            conn,
            Instant::now() + READ_TIMEOUT,
            |event| matches!(event, Event::PropertyNotify(e) if e.window == window && e.atom == atoms.prop),
        )? {
            Some(Event::PropertyNotify(e)) => Ok(e.time),
            _ => Err(api_err(
                "PropertyNotify",
                "the X server did not answer in time",
            )),
        }
    }

    /// A connection with a window for asking the selection owner for its data.
    pub(super) struct Session {
        conn: RustConnection,
        pub atoms: Atoms,
        window: Window,
    }

    impl Session {
        pub fn open(display: &str) -> Result<Self> {
            let (conn, screen) = connect(display)?;
            let atoms = Atoms::intern(&conn)?;
            let window = create_window(&conn, screen)?;
            Ok(Self {
                conn,
                atoms,
                window,
            })
        }

        pub fn owner(&self) -> Result<Window> {
            self.conn
                .get_selection_owner(self.atoms.clipboard)
                .map_err(|e| api_err("GetSelectionOwner", e))?
                .reply()
                .map(|reply| reply.owner)
                .map_err(|e| api_err("GetSelectionOwner", e))
        }

        /// Converts the selection to `target`: `Ok(None)` when the owner refuses, `Err` when it
        /// does not answer within `timeout`.
        pub fn convert(&self, target: Atom, timeout: Duration) -> Result<Option<Vec<u8>>> {
            let (conn, atoms, window) = (&self.conn, &self.atoms, self.window);
            let deadline = Instant::now() + timeout;
            conn.delete_property(window, atoms.prop)
                .map_err(|e| api_err("DeleteProperty", e))?;
            conn.convert_selection(window, atoms.clipboard, target, atoms.prop, CURRENT_TIME)
                .map_err(|e| api_err("ConvertSelection", e))?;
            let notify = wait_event(conn, deadline, |event| {
                matches!(event, Event::SelectionNotify(e)
                    if e.requestor == window && e.selection == atoms.clipboard)
            })?;
            let Some(Event::SelectionNotify(notify)) = notify else {
                return Err(api_err(
                    "ConvertSelection(CLIPBOARD)",
                    "the clipboard owner did not answer in time",
                ));
            };
            if notify.property == NONE {
                return Ok(None);
            }
            let reply = self.take_property()?;
            if reply.0 != atoms.incr {
                return Ok(Some(reply.1));
            }
            // INCR: deleting the property (done by the read) asks for each chunk in turn; a
            // zero-length chunk ends the transfer.
            let mut data = Vec::new();
            loop {
                let chunk_deadline = Instant::now() + timeout;
                let event = wait_event(conn, chunk_deadline, |event| {
                    matches!(event, Event::PropertyNotify(e)
                        if e.window == window && e.atom == atoms.prop
                            && e.state == Property::NEW_VALUE)
                })?;
                if event.is_none() {
                    return Err(api_err(
                        "ConvertSelection(INCR)",
                        "the clipboard owner stopped sending",
                    ));
                }
                let (_, chunk) = self.take_property()?;
                if chunk.is_empty() {
                    return Ok(Some(data));
                }
                data.extend_from_slice(&chunk);
            }
        }

        /// Reads and deletes our property: (type, bytes).
        fn take_property(&self) -> Result<(Atom, Vec<u8>)> {
            let reply = self
                .conn
                .get_property(
                    true,
                    self.window,
                    self.atoms.prop,
                    AtomEnum::ANY,
                    0,
                    u32::MAX / 4,
                )
                .map_err(|e| api_err("GetProperty", e))?
                .reply()
                .map_err(|e| api_err("GetProperty", e))?;
            Ok((reply.type_, reply.value))
        }

        pub fn targets(&self) -> Result<Vec<Atom>> {
            Ok(self
                .convert(self.atoms.targets, READ_TIMEOUT)?
                .map(|bytes| atoms_from_bytes(&bytes))
                .unwrap_or_default())
        }

        /// The owner's `TIMESTAMP`, or `None` if it does not give one quickly.
        pub fn owner_timestamp(&self) -> Option<u32> {
            let bytes = self
                .convert(self.atoms.timestamp, TIMESTAMP_TIMEOUT)
                .ok()
                .flatten()?;
            Some(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
        }

        /// Gives up the selection for whoever owns it, if nobody took it after `time`.
        pub fn disown(&self, time: u32) -> Result<()> {
            self.conn
                .set_selection_owner(NONE, self.atoms.clipboard, time)
                .map_err(|e| api_err("SetSelectionOwner(None)", e))?;
            self.conn.sync().map_err(|e| api_err("XSync", e))
        }
    }

    fn atoms_from_bytes(bytes: &[u8]) -> Vec<Atom> {
        let (chunks, _) = bytes.as_chunks::<4>();
        chunks
            .iter()
            .map(|chunk| u32::from_ne_bytes(*chunk))
            .collect()
    }

    /// A running owner thread and what it serves.
    pub(super) struct OwnerHandle {
        pub display: String,
        pub window: Window,
        pub timestamp: u32,
        pub publish: Arc<Publish>,
        conn: Arc<RustConnection>,
        stop_atom: Atom,
        clipboard_atom: Atom,
        pub alive: Arc<AtomicBool>,
        done: mpsc::Receiver<()>,
    }

    impl OwnerHandle {
        /// Stops the owner thread (without giving up the selection first).
        pub fn stop(self) {
            if self.alive.load(Ordering::Acquire) {
                let message = ClientMessageEvent::new(32, self.window, self.stop_atom, [0_u32; 5]);
                let _ = self
                    .conn
                    .send_event(false, self.window, EventMask::NO_EVENT, message);
                let _ = self.conn.flush();
            }
            let _ = self.done.recv_timeout(Duration::from_secs(1));
        }

        /// Gives up the selection (only if nobody took it since we did) and stops the thread.
        pub fn release(self) {
            let _ = self
                .conn
                .set_selection_owner(NONE, self.clipboard_atom, self.timestamp);
            let _ = self.conn.flush();
            self.stop();
        }
    }

    /// Takes `CLIPBOARD` on `display` for `publish` and starts the thread that serves it.
    /// `before_start` runs after the selection is ours and before the thread starts (it stops
    /// the previous owner thread, so only one runs at a time).
    pub(super) fn acquire(
        display: &str,
        publish: Arc<Publish>,
        before_start: impl FnOnce(),
    ) -> Result<OwnerHandle> {
        let (conn, screen) = connect(display)?;
        let atoms = Atoms::intern(&conn)?;
        let window = create_window(&conn, screen)?;
        let timestamp = server_time(&conn, window, &atoms)?;
        conn.set_selection_owner(window, atoms.clipboard, timestamp)
            .map_err(|e| api_err("SetSelectionOwner(CLIPBOARD)", e))?;
        let owner = conn
            .get_selection_owner(atoms.clipboard)
            .map_err(|e| api_err("GetSelectionOwner", e))?
            .reply()
            .map_err(|e| api_err("GetSelectionOwner", e))?
            .owner;
        if owner != window {
            return Err(api_err(
                "SetSelectionOwner(CLIPBOARD)",
                "another client took the clipboard at the same time",
            ));
        }
        before_start();

        let conn = Arc::new(conn);
        let alive = Arc::new(AtomicBool::new(true));
        let (done_tx, done) = mpsc::channel();
        let data = publish
            .targets
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (atoms.format(name), index))
            .collect();
        let (stop_atom, clipboard_atom) = (atoms.stop, atoms.clipboard);
        let mut server = Server {
            chunk: chunk_limit(&conn),
            conn: Arc::clone(&conn),
            atoms,
            window,
            timestamp,
            publish: Arc::clone(&publish),
            data,
            transfers: Vec::new(),
        };
        let thread_alive = Arc::clone(&alive);
        let spawned = std::thread::Builder::new()
            .name("zxraw-clipboard-owner".to_string())
            .spawn(move || {
                server.run();
                thread_alive.store(false, Ordering::Release);
                let _ = server.conn.destroy_window(server.window);
                let _ = server.conn.flush();
                let _ = done_tx.send(());
            });
        if let Err(e) = spawned {
            let _ = conn.set_selection_owner(NONE, clipboard_atom, timestamp);
            let _ = conn.flush();
            return Err(api_err("spawn(clipboard owner)", e));
        }
        Ok(OwnerHandle {
            display: display.to_string(),
            window,
            timestamp,
            publish,
            conn,
            stop_atom,
            clipboard_atom,
            alive,
            done,
        })
    }

    #[cfg(test)]
    pub(super) static TEST_CHUNK: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// Largest value written in one request; larger values go out with INCR.
    fn chunk_limit(conn: &RustConnection) -> usize {
        #[cfg(test)]
        {
            let chunk = TEST_CHUNK.load(Ordering::Relaxed);
            if chunk != 0 {
                return chunk;
            }
        }
        conn.maximum_request_bytes()
            .saturating_sub(1024)
            .clamp(4096, 256 * 1024)
    }

    /// One INCR transfer in progress.
    struct Incr {
        requestor: Window,
        property: Atom,
        target: Atom,
        index: usize,
        offset: usize,
        started: Instant,
    }

    struct Server {
        conn: Arc<RustConnection>,
        atoms: Atoms,
        window: Window,
        timestamp: u32,
        publish: Arc<Publish>,
        /// Target atom, index into `publish.targets`.
        data: Vec<(Atom, usize)>,
        chunk: usize,
        transfers: Vec<Incr>,
    }

    impl Server {
        fn run(&mut self) {
            loop {
                let Ok(event) = self.conn.wait_for_event() else {
                    return;
                };
                match event {
                    Event::SelectionClear(e)
                        if e.owner == self.window && e.selection == self.atoms.clipboard =>
                    {
                        return;
                    }
                    Event::ClientMessage(e)
                        if e.window == self.window && e.type_ == self.atoms.stop =>
                    {
                        return;
                    }
                    Event::SelectionRequest(e) => self.answer(&e),
                    Event::PropertyNotify(e) if e.state == Property::DELETE => {
                        self.continue_incr(e.window, e.atom)
                    }
                    _ => {}
                }
                // A requestor that went away never deletes the property: forget it.
                self.transfers
                    .retain(|t| t.started.elapsed() < Duration::from_secs(30));
                let _ = self.conn.flush();
            }
        }

        fn bytes(&self, target: Atom) -> Option<(usize, &[u8])> {
            self.data
                .iter()
                .find(|(atom, _)| *atom == target)
                .map(|&(_, index)| (index, self.publish.targets[index].1.as_slice()))
        }

        fn target_list(&self) -> Vec<Atom> {
            let mut list = vec![
                self.atoms.targets,
                self.atoms.timestamp,
                self.atoms.multiple,
            ];
            list.extend(self.data.iter().map(|(atom, _)| *atom));
            list
        }

        /// Writes `target` to `property` without INCR. `false` if it is not ours or too large.
        fn write_small(&self, requestor: Window, property: Atom, target: Atom) -> bool {
            if target == self.atoms.targets {
                self.conn
                    .change_property32(
                        PropMode::REPLACE,
                        requestor,
                        property,
                        AtomEnum::ATOM,
                        &self.target_list(),
                    )
                    .is_ok()
            } else if target == self.atoms.timestamp {
                self.conn
                    .change_property32(
                        PropMode::REPLACE,
                        requestor,
                        property,
                        AtomEnum::INTEGER,
                        &[self.timestamp],
                    )
                    .is_ok()
            } else if let Some((_, bytes)) = self.bytes(target) {
                bytes.len() <= self.chunk
                    && self
                        .conn
                        .change_property8(PropMode::REPLACE, requestor, property, target, bytes)
                        .is_ok()
            } else {
                false
            }
        }

        fn answer(&mut self, e: &SelectionRequestEvent) {
            // Obsolete clients send no property: use the target as the property name.
            let property = if e.property == NONE {
                e.target
            } else {
                e.property
            };
            let ours = e.selection == self.atoms.clipboard
                && e.owner == self.window
                && (e.time == CURRENT_TIME || e.time >= self.timestamp);
            let answered = if !ours {
                false
            } else if e.target == self.atoms.multiple {
                e.property != NONE && self.answer_multiple(e.requestor, e.property)
            } else if let Some((index, bytes)) = self.bytes(e.target)
                && bytes.len() > self.chunk
            {
                self.start_incr(e.requestor, property, e.target, index, bytes.len())
            } else {
                self.write_small(e.requestor, property, e.target)
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

        /// `MULTIPLE`: a list of (target, property) pairs; each one we cannot write (or that
        /// would need INCR) gets its property replaced by None.
        fn answer_multiple(&self, requestor: Window, property: Atom) -> bool {
            let Ok(reply) = self
                .conn
                .get_property(false, requestor, property, AtomEnum::ANY, 0, u32::MAX / 4)
                .map_err(drop)
                .and_then(|cookie| cookie.reply().map_err(drop))
            else {
                return false;
            };
            let mut pairs = atoms_from_bytes(&reply.value);
            if !pairs.len().is_multiple_of(2) {
                return false;
            }
            for pair in pairs.as_chunks_mut::<2>().0 {
                if pair[1] == NONE || !self.write_small(requestor, pair[1], pair[0]) {
                    pair[1] = NONE;
                }
            }
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    self.atoms.atom_pair,
                    &pairs,
                )
                .is_ok()
        }

        fn start_incr(
            &mut self,
            requestor: Window,
            property: Atom,
            target: Atom,
            index: usize,
            len: usize,
        ) -> bool {
            let Ok(len) = u32::try_from(len) else {
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
            self.transfers.push(Incr {
                requestor,
                property,
                target,
                index,
                offset: 0,
                started: Instant::now(),
            });
            true
        }

        /// The requestor deleted the property: send the next chunk, or the empty end marker.
        fn continue_incr(&mut self, window: Window, property: Atom) {
            let Some(position) = self
                .transfers
                .iter()
                .position(|t| t.requestor == window && t.property == property)
            else {
                return;
            };
            let transfer = &mut self.transfers[position];
            let bytes = &self.publish.targets[transfer.index].1;
            let end = (transfer.offset + self.chunk).min(bytes.len());
            let chunk = &bytes[transfer.offset..end];
            let finished = chunk.is_empty();
            let _ = self.conn.change_property8(
                PropMode::APPEND,
                window,
                property,
                transfer.target,
                chunk,
            );
            transfer.offset = end;
            transfer.started = Instant::now();
            if finished {
                self.transfers.remove(position);
            }
        }
    }

    /// What one look found: the owner, and our publish if we are it.
    struct View {
        owner: Window,
        timestamp: u32,
        ours: Option<Arc<Publish>>,
        targets: Option<Vec<Atom>>,
    }

    pub(super) struct X11Backend<'a> {
        state: &'a mut State,
        display: String,
        session: Session,
        view: Option<View>,
    }

    impl<'a> X11Backend<'a> {
        pub fn open(state: &'a mut State, display: &str) -> Result<Self> {
            Ok(Self {
                state,
                display: display.to_string(),
                session: Session::open(display)?,
                view: None,
            })
        }

        fn view(&mut self) -> Result<&mut View> {
            if self.view.is_none() {
                let owner = self.session.owner()?;
                let ours = self.state.owner.as_ref().filter(|handle| {
                    handle.display == self.display
                        && handle.window == owner
                        && handle.alive.load(Ordering::Acquire)
                });
                let view = if owner == NONE {
                    View {
                        owner,
                        timestamp: 0,
                        ours: None,
                        targets: Some(Vec::new()),
                    }
                } else if let Some(handle) = ours {
                    View {
                        owner,
                        timestamp: handle.timestamp,
                        ours: Some(Arc::clone(&handle.publish)),
                        targets: None,
                    }
                } else {
                    View {
                        owner,
                        timestamp: self.session.owner_timestamp().unwrap_or(0),
                        ours: None,
                        targets: None,
                    }
                };
                self.view = Some(view);
            }
            Ok(self.view.as_mut().expect("view just set"))
        }

        /// Our publish, or the foreign owner's target list.
        fn look(&mut self) -> Result<(Option<Arc<Publish>>, Vec<Atom>)> {
            let view = self.view()?;
            if let Some(ours) = &view.ours {
                return Ok((Some(Arc::clone(ours)), Vec::new()));
            }
            if view.targets.is_none() {
                let targets = self.session.targets()?;
                self.view.as_mut().expect("view").targets = Some(targets);
            }
            Ok((
                None,
                self.view
                    .as_ref()
                    .expect("view")
                    .targets
                    .clone()
                    .unwrap_or_default(),
            ))
        }

        fn has(&self, targets: &[Atom], name: &str) -> bool {
            targets.contains(&self.session.atoms.format(name))
        }

        fn fetch(&self, name: &str) -> Result<Option<Vec<u8>>> {
            self.session
                .convert(self.session.atoms.format(name), READ_TIMEOUT)
        }
    }

    impl Backend for X11Backend<'_> {
        fn sequence(&mut self) -> Result<u32> {
            let counter = self.state.counter;
            let view = self.view()?;
            Ok(sequence_hash(&[
                &view.owner.to_le_bytes(),
                &view.timestamp.to_le_bytes(),
                &counter.to_le_bytes(),
            ]))
        }

        fn token(&mut self) -> Result<Option<ClipboardToken>> {
            let (ours, targets) = self.look()?;
            if let Some(ours) = ours {
                return Ok(ours.token);
            }
            if !self.has(&targets, OWNER_TARGET) {
                return Ok(None);
            }
            Ok(self
                .fetch(OWNER_TARGET)?
                .and_then(|bytes| parse_token(&bytes)))
        }

        fn files(&mut self) -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
            let (ours, targets) = self.look()?;
            if let Some(ours) = ours {
                return Ok(Some((ours.paths.clone(), ours.effect)));
            }
            read_file_targets(|name| self.has(&targets, name), |name| self.fetch(name))
        }

        fn has_text(&mut self) -> Result<bool> {
            let (ours, targets) = self.look()?;
            if let Some(ours) = ours {
                return Ok(ours.has_text());
            }
            Ok(TEXT_LIKE_TARGETS
                .iter()
                .any(|name| self.has(&targets, name)))
        }

        fn publish(&mut self, publish: Publish) -> Result<()> {
            let previous = &mut self.state.owner;
            let handle = acquire(&self.display, Arc::new(publish), || {
                if let Some(old) = previous.take() {
                    old.stop();
                }
            })?;
            self.state.owner = Some(handle);
            self.state.counter = self.state.counter.wrapping_add(1);
            self.view = None;
            Ok(())
        }

        fn clear(&mut self) -> Result<()> {
            let view = self.view()?;
            let (owner, timestamp, ours) = (view.owner, view.timestamp, view.ours.is_some());
            if ours {
                if let Some(handle) = self.state.owner.take() {
                    handle.release();
                }
            } else if owner != NONE {
                let time = if timestamp == 0 {
                    CURRENT_TIME
                } else {
                    timestamp
                };
                self.session.disown(time)?;
            }
            self.state.counter = self.state.counter.wrapping_add(1);
            self.view = None;
            Ok(())
        }
    }

    /// Test helpers: another client on the same display.
    #[cfg(test)]
    pub(super) mod testing {
        use super::*;

        /// Serves `publish` from a second connection, as another app's Copy/Cut would. Not
        /// registered as ours.
        pub fn foreign_owner(display: &str, publish: Publish) -> OwnerHandle {
            acquire(display, Arc::new(publish), || {}).expect("foreign owner")
        }

        /// Takes the selection from a bare connection that never answers requests.
        pub fn steal_selection(display: &str) -> (RustConnection, Window) {
            let (conn, screen) = connect(display).unwrap();
            let atoms = Atoms::intern(&conn).unwrap();
            let window = create_window(&conn, screen).unwrap();
            let time = server_time(&conn, window, &atoms).unwrap();
            conn.set_selection_owner(window, atoms.clipboard, time)
                .unwrap();
            conn.sync().unwrap();
            (conn, window)
        }

        pub fn convert_named(session: &Session, name: &str) -> Option<Vec<u8>> {
            session
                .convert(session.atoms.format(name), READ_TIMEOUT)
                .unwrap()
        }

        /// Asks for `names` in one `MULTIPLE` request; `None` for each refused target.
        pub fn convert_multiple(session: &Session, names: &[&str]) -> Vec<Option<Vec<u8>>> {
            let conn = &session.conn;
            let mut pairs = Vec::new();
            let mut props = Vec::new();
            for (i, name) in names.iter().enumerate() {
                let prop = conn
                    .intern_atom(false, format!("_ZXRAW_TEST_MULTI_{i}").as_bytes())
                    .unwrap()
                    .reply()
                    .unwrap()
                    .atom;
                pairs.extend([session.atoms.format(name), prop]);
                props.push(prop);
            }
            conn.change_property32(
                PropMode::REPLACE,
                session.window,
                session.atoms.prop,
                session.atoms.atom_pair,
                &pairs,
            )
            .unwrap();
            conn.convert_selection(
                session.window,
                session.atoms.clipboard,
                session.atoms.multiple,
                session.atoms.prop,
                CURRENT_TIME,
            )
            .unwrap();
            let event = wait_event(conn, Instant::now() + READ_TIMEOUT, |event| {
                matches!(event, Event::SelectionNotify(_))
            })
            .unwrap();
            assert!(matches!(event, Some(Event::SelectionNotify(e)) if e.property != NONE));
            let (_, written) = session.take_property().unwrap();
            let written = atoms_from_bytes(&written);
            props
                .iter()
                .enumerate()
                .map(|(i, &prop)| {
                    if written[2 * i + 1] == NONE {
                        return None;
                    }
                    let reply = conn
                        .get_property(true, session.window, prop, AtomEnum::ANY, 0, u32::MAX / 4)
                        .unwrap()
                        .reply()
                        .unwrap();
                    Some(reply.value)
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Wayland (data-control through wl-clipboard-rs). Untested: no compositor on the test machine.
// ---------------------------------------------------------------------------------------------

mod wayland {
    use std::io::Read;

    use wl_clipboard_rs::{copy, paste};

    use super::*;

    /// Whether the compositor offers data-control (false sends us to X11 / XWayland).
    pub(super) fn supported() -> bool {
        !matches!(
            paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified),
            Err(paste::Error::MissingProtocol { .. } | paste::Error::WaylandConnection(_))
        )
    }

    fn empty(error: &paste::Error) -> bool {
        matches!(
            error,
            paste::Error::NoSeats | paste::Error::ClipboardEmpty | paste::Error::NoMimeType
        )
    }

    pub(super) struct WlBackend<'a> {
        state: &'a mut State,
        types: Option<HashSet<String>>,
    }

    impl<'a> WlBackend<'a> {
        pub fn new(state: &'a mut State) -> Self {
            Self { state, types: None }
        }

        fn types(&mut self) -> Result<&HashSet<String>> {
            if self.types.is_none() {
                let types = match paste::get_mime_types(
                    paste::ClipboardType::Regular,
                    paste::Seat::Unspecified,
                ) {
                    Ok(types) => types,
                    Err(e) if empty(&e) => HashSet::new(),
                    Err(e) => return Err(api_err("wl-paste (types)", e)),
                };
                self.types = Some(types);
            }
            Ok(self.types.as_ref().expect("types just set"))
        }

        /// Reads one MIME type, giving up after [`READ_TIMEOUT`] (the reading thread is then
        /// left to finish on its own).
        fn fetch(name: &str) -> Result<Option<Vec<u8>>> {
            let (tx, rx) = std::sync::mpsc::channel();
            let name = name.to_string();
            std::thread::Builder::new()
                .name("zxraw-wl-paste".to_string())
                .spawn(move || {
                    let result = paste::get_contents(
                        paste::ClipboardType::Regular,
                        paste::Seat::Unspecified,
                        paste::MimeType::Specific(&name),
                    )
                    .map_err(|e| (empty(&e), e.to_string()))
                    .and_then(|(mut pipe, _)| {
                        let mut bytes = Vec::new();
                        pipe.read_to_end(&mut bytes)
                            .map(|_| bytes)
                            .map_err(|e| (false, e.to_string()))
                    });
                    let _ = tx.send(result);
                })
                .map_err(|e| api_err("spawn(wl-paste)", e))?;
            match rx.recv_timeout(READ_TIMEOUT) {
                Ok(Ok(bytes)) => Ok(Some(bytes)),
                Ok(Err((true, _))) => Ok(None),
                Ok(Err((false, message))) => Err(api_err("wl-paste", message)),
                Err(_) => Err(api_err(
                    "wl-paste",
                    "the clipboard owner did not answer in time",
                )),
            }
        }
    }

    impl Backend for WlBackend<'_> {
        fn sequence(&mut self) -> Result<u32> {
            let mut types: Vec<String> = self.types()?.iter().cloned().collect();
            types.sort();
            let token = self.token()?.map(token_bytes).unwrap_or_default();
            let joined = types.join("\n");
            Ok(sequence_hash(&[
                joined.as_bytes(),
                &token,
                &self.state.counter.to_le_bytes(),
            ]))
        }

        fn token(&mut self) -> Result<Option<ClipboardToken>> {
            if !self.types()?.contains(OWNER_TARGET) {
                return Ok(None);
            }
            Ok(Self::fetch(OWNER_TARGET)?.and_then(|bytes| parse_token(&bytes)))
        }

        fn files(&mut self) -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
            let types = self.types()?.clone();
            read_file_targets(|name| types.contains(name), Self::fetch)
        }

        fn has_text(&mut self) -> Result<bool> {
            let types = self.types()?;
            Ok(TEXT_LIKE_TARGETS.iter().any(|name| types.contains(*name)))
        }

        fn publish(&mut self, publish: Publish) -> Result<()> {
            let sources = publish
                .targets
                .into_iter()
                .map(|(name, bytes)| copy::MimeSource {
                    source: copy::Source::Bytes(bytes.into_boxed_slice()),
                    mime_type: copy::MimeType::Specific(name.to_string()),
                })
                .collect();
            let mut options = copy::Options::new();
            options
                .clipboard(copy::ClipboardType::Regular)
                .omit_additional_text_mime_types(true);
            // Serves from its own thread until another client copies.
            options
                .copy_multi(sources)
                .map_err(|e| api_err("wl-copy", e))?;
            self.state.counter = self.state.counter.wrapping_add(1);
            self.types = None;
            Ok(())
        }

        fn clear(&mut self) -> Result<()> {
            copy::clear(copy::ClipboardType::Regular, copy::Seat::All)
                .map_err(|e| api_err("wl-copy --clear", e))?;
            self.state.counter = self.state.counter.wrapping_add(1);
            self.types = None;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn uri_encoding_round_trips_spaces_percent_unicode_and_non_utf8() {
        let non_utf8 = PathBuf::from(OsString::from_vec(b"/tmp/bad\xff\xfe name".to_vec()));
        let cases = [
            p("/home/user/My Photos/a b.jpg"),
            p("/tmp/100% done/#1?x=y&z"),
            p("/data/日本語/résumé.txt"),
            p("/a/-._~/plain"),
            non_utf8,
        ];
        for path in &cases {
            let uri = encode_file_uri(path);
            assert!(uri.is_ascii(), "{uri}");
            assert!(uri.starts_with("file:///"), "{uri}");
            assert!(!uri.contains(' ') && !uri.contains('#') && !uri.contains('?'));
            assert_eq!(decode_file_uri(&uri).as_ref(), Some(path), "{uri}");
        }
        assert_eq!(
            encode_file_uri(&p("/My Photos/100%/é")),
            "file:///My%20Photos/100%25/%C3%A9"
        );
        assert_eq!(encode_file_uri(&cases[4]), "file:///tmp/bad%FF%FE%20name");
    }

    #[test]
    fn decoding_accepts_local_file_uris_only() {
        assert_eq!(decode_file_uri("file:///a%20b"), Some(p("/a b")));
        assert_eq!(decode_file_uri("FILE://localhost/x"), Some(p("/x")));
        assert_eq!(decode_file_uri("file:/kde/style"), Some(p("/kde/style")));
        assert_eq!(decode_file_uri("file:///lower%c3%a9"), Some(p("/loweré")));
        assert_eq!(
            decode_file_uri("file:///bad%zzescape"),
            Some(p("/bad%zzescape"))
        );
        assert_eq!(decode_file_uri("file:///frag#ment"), Some(p("/frag")));
        assert_eq!(decode_file_uri("file://server/share/x"), None);
        assert_eq!(decode_file_uri("https://example.com/x"), None);
        assert_eq!(decode_file_uri("smb://server/x"), None);
        assert_eq!(decode_file_uri("file:///nul%00byte"), None);
        assert_eq!(decode_file_uri("file:relative"), None);
        assert_eq!(decode_file_uri(""), None);
    }

    #[test]
    fn gnome_copied_files_for_copy_and_cut() {
        let paths = [p("/a/one.txt"), p("/b/two words.txt")];
        assert_eq!(
            gnome_copied_files(&paths, ClipboardEffect::Copy),
            b"copy\nfile:///a/one.txt\nfile:///b/two%20words.txt"
        );
        assert_eq!(
            gnome_copied_files(&paths, ClipboardEffect::Move),
            b"cut\nfile:///a/one.txt\nfile:///b/two%20words.txt"
        );
        for effect in [ClipboardEffect::Copy, ClipboardEffect::Move] {
            assert_eq!(
                parse_gnome_copied_files(&gnome_copied_files(&paths, effect)),
                Some((paths.to_vec(), effect))
            );
        }
        // CRLF and a trailing newline from other writers, non-file URIs skipped.
        assert_eq!(
            parse_gnome_copied_files(b"cut\r\nfile:///x\r\nhttp://y/z\r\n"),
            Some((vec![p("/x")], ClipboardEffect::Move))
        );
        assert_eq!(parse_gnome_copied_files(b"move\nfile:///x"), None);
        assert_eq!(parse_gnome_copied_files(b"copy\n"), None);
        assert_eq!(parse_gnome_copied_files(b""), None);
    }

    #[test]
    fn uri_list_uses_crlf_and_parses_comments_and_lf() {
        let paths = [p("/a"), p("/b c")];
        assert_eq!(uri_list(&paths), b"file:///a\r\nfile:///b%20c");
        assert_eq!(parse_uri_list(&uri_list(&paths)), paths);
        assert_eq!(
            parse_uri_list(b"# comment\nfile:///a\nhttps://x/y\r\nfile:///b%20c\r\n"),
            paths
        );
        assert!(parse_uri_list(b"").is_empty());
    }

    #[test]
    fn publish_targets_text_kde_marker_and_owner_token() {
        let token = ClipboardToken::from_raw(0x0102_0304_0506_0708);
        let paths = vec![p("/x/a.txt"), p("/y/b.txt")];

        let cut = build_publish(&paths, ClipboardEffect::Move, Some(token)).unwrap();
        let names: Vec<&str> = cut.targets.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                OWNER_TARGET,
                GNOME_TARGET,
                URI_LIST_TARGET,
                KDE_CUT_TARGET,
                "UTF8_STRING",
                "text/plain;charset=utf-8",
                "text/plain",
                "STRING"
            ]
        );
        assert_eq!(
            cut.data(OWNER_TARGET).unwrap(),
            [8, 7, 6, 5, 4, 3, 2, 1],
            "8 little-endian bytes"
        );
        assert_eq!(parse_token(cut.data(OWNER_TARGET).unwrap()), Some(token));
        assert_eq!(parse_token(&[1, 2, 3]), None);
        assert_eq!(cut.data(KDE_CUT_TARGET).unwrap(), b"1");
        assert!(is_kde_cut(b"1") && !is_kde_cut(b"0") && !is_kde_cut(b""));
        assert_eq!(cut.data("UTF8_STRING").unwrap(), b"/x/a.txt\n/y/b.txt");
        assert!(cut.has_text());

        let copy = build_publish(&paths, ClipboardEffect::Copy, Some(token)).unwrap();
        assert!(copy.data(KDE_CUT_TARGET).is_none());

        // What a file manager offers: no text, no owner token.
        let theirs = build_publish(&paths, ClipboardEffect::Move, None).unwrap();
        let names: Vec<&str> = theirs.targets.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, [GNOME_TARGET, URI_LIST_TARGET, KDE_CUT_TARGET]);
        assert!(!theirs.has_text());
    }

    #[test]
    fn publish_rejects_empty_nul_and_relative_paths() {
        let bad = [
            vec![],
            vec![p("")],
            vec![p("relative/x")],
            vec![PathBuf::from(OsString::from_vec(b"/a\0b".to_vec()))],
        ];
        for paths in bad {
            assert!(matches!(
                build_publish(&paths, ClipboardEffect::Copy, None),
                Err(WinError::Invalid(_))
            ));
        }
    }

    #[test]
    fn reading_targets_prefers_gnome_then_uri_list_with_kde_marker() {
        let paths = vec![p("/x")];
        let theirs = build_publish(&paths, ClipboardEffect::Move, None).unwrap();
        let read = |only: &[&str]| {
            read_file_targets(
                |name| only.contains(&name),
                |name| Ok(theirs.data(name).map(<[u8]>::to_vec)),
            )
            .unwrap()
        };
        assert_eq!(
            read(&[GNOME_TARGET, URI_LIST_TARGET]),
            Some((paths.clone(), ClipboardEffect::Move))
        );
        assert_eq!(
            read(&[URI_LIST_TARGET, KDE_CUT_TARGET]),
            Some((paths.clone(), ClipboardEffect::Move))
        );
        assert_eq!(
            read(&[URI_LIST_TARGET]),
            Some((paths.clone(), ClipboardEffect::Copy))
        );
        assert_eq!(read(&["UTF8_STRING"]), None);
    }

    #[test]
    fn display_choice_prefers_wayland_data_control_then_x11_then_nothing() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            pick_display(some("wayland-0"), some(":0"), || true),
            Some(Display::Wayland)
        );
        // No data-control (GNOME): XWayland.
        assert_eq!(
            pick_display(some("wayland-0"), some(":0"), || false),
            Some(Display::X11(":0".to_string()))
        );
        assert_eq!(pick_display(some("wayland-0"), None, || false), None);
        assert_eq!(
            pick_display(None, some(":1"), || panic!("not probed without Wayland")),
            Some(Display::X11(":1".to_string()))
        );
        assert_eq!(pick_display(None, None, || true), None);
    }

    #[test]
    fn sequence_hash_is_nonzero_and_order_sensitive() {
        assert_ne!(sequence_hash(&[b"ab", b"c"]), sequence_hash(&[b"a", b"bc"]));
        assert_ne!(
            sequence_hash(&[&1_u32.to_le_bytes()]),
            sequence_hash(&[&2_u32.to_le_bytes()])
        );
        assert_ne!(sequence_hash(&[]), 0);
    }

    // ---- Real X11 clipboard, on a private Xvfb (never the user's display) ----

    /// A private Xvfb, killed when dropped. Holds the lock that serializes these tests (they
    /// share the process-wide owner state).
    struct Xvfb {
        child: std::process::Child,
        display: String,
        _lock: MutexGuard<'static, ()>,
    }

    static XVFB_LOCK: Mutex<()> = Mutex::new(());

    impl Xvfb {
        fn start() -> Self {
            use std::process::{Command, Stdio};
            let lock = XVFB_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let base = 150 + std::process::id() % 400;
            for number in base..base + 50 {
                if Path::new(&format!("/tmp/.X{number}-lock")).exists()
                    || Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
                {
                    continue;
                }
                let display = format!(":{number}");
                let Ok(mut child) = Command::new("/usr/bin/Xvfb")
                    // -noreset: no server reset (refusing connections) when a client leaves.
                    .args([
                        display.as_str(),
                        "-nolisten",
                        "tcp",
                        "-noreset",
                        "-screen",
                        "0",
                        "64x64x24",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    panic!("could not start /usr/bin/Xvfb");
                };
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    if child.try_wait().ok().flatten().is_some() {
                        break; // Display taken after all: try the next one.
                    }
                    if x11rb::rust_connection::RustConnection::connect(Some(&display)).is_ok() {
                        return Self {
                            child,
                            display,
                            _lock: lock,
                        };
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                stop_xvfb(&mut child);
            }
            panic!("no free X display number for Xvfb");
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            // Stop our owner thread first, so it does not outlive the server.
            if let Some(handle) = lock_state().owner.take() {
                handle.stop();
            }
            x11::TEST_CHUNK.store(0, std::sync::atomic::Ordering::Relaxed);
            stop_xvfb(&mut self.child);
        }
    }

    /// SIGTERM, so Xvfb removes its lock file and socket; SIGKILL if it does not exit in 5 s.
    fn stop_xvfb(child: &mut std::process::Child) {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: plain kill(2) on the child we spawned and have not reaped.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn set(x: &Xvfb, paths: &[PathBuf], effect: ClipboardEffect) -> ClipboardToken {
        let token = super::super::next_token();
        let publish = build_publish(paths, effect, Some(token)).unwrap();
        with_x11(&x.display, |b| b.publish(publish)).unwrap();
        token
    }

    fn read(x: &Xvfb) -> ClipboardRead {
        with_x11(&x.display, op_read).unwrap()
    }

    fn sequence(x: &Xvfb) -> u32 {
        with_x11(&x.display, |b| b.sequence()).unwrap()
    }

    #[test]
    #[ignore = "starts Xvfb"]
    fn x11_set_then_get_round_trips_copy_and_cut() {
        let x = Xvfb::start();
        let paths = vec![p("/tmp/zx clip/a.txt"), p("/tmp/zx clip/日本.txt")];
        let empty = read(&x);
        assert_eq!((empty.files, empty.owner), (None, None));
        assert!(!with_x11(&x.display, |b| b.has_text()).unwrap());

        for effect in [ClipboardEffect::Copy, ClipboardEffect::Move] {
            let before = sequence(&x);
            let token = set(&x, &paths, effect);
            let read = read(&x);
            assert_eq!(read.files, Some((paths.clone(), effect)));
            assert_eq!(read.owner, Some(token));
            assert!(token.is_from_this_process());
            assert_ne!(read.sequence, before);
            assert_eq!(read.sequence, sequence(&x), "stable while nothing changes");
            assert!(with_x11(&x.display, |b| b.has_text()).unwrap());

            // Another client sees every format over the X protocol.
            let session = x11::Session::open(&x.display).unwrap();
            let gnome = x11::testing::convert_named(&session, GNOME_TARGET).unwrap();
            assert_eq!(
                parse_gnome_copied_files(&gnome),
                Some((paths.clone(), effect))
            );
            let uris = x11::testing::convert_named(&session, URI_LIST_TARGET).unwrap();
            assert_eq!(uris, uri_list(&paths));
            assert_eq!(
                x11::testing::convert_named(&session, KDE_CUT_TARGET).is_some(),
                effect == ClipboardEffect::Move
            );
            assert_eq!(
                x11::testing::convert_named(&session, "UTF8_STRING").unwrap(),
                "/tmp/zx clip/a.txt\n/tmp/zx clip/日本.txt".as_bytes()
            );
            let owner = x11::testing::convert_named(&session, OWNER_TARGET).unwrap();
            assert_eq!(parse_token(&owner), Some(token));
            let targets = session.targets().unwrap();
            assert!(targets.contains(&session.atoms.format(GNOME_TARGET)));
            assert!(targets.contains(&session.atoms.targets));
            assert!(session.owner_timestamp().is_some());
        }
    }

    #[test]
    #[ignore = "starts Xvfb"]
    fn x11_large_values_go_out_with_incr_and_multiple_works() {
        let x = Xvfb::start();
        x11::TEST_CHUNK.store(64, std::sync::atomic::Ordering::Relaxed);
        let paths: Vec<PathBuf> = (0..50)
            .map(|i| p(&format!("/tmp/incr test/file number {i}.raw")))
            .collect();
        set(&x, &paths, ClipboardEffect::Copy);
        let session = x11::Session::open(&x.display).unwrap();
        let uris = x11::testing::convert_named(&session, URI_LIST_TARGET).unwrap();
        assert!(uris.len() > 64 * 10);
        assert_eq!(uris, uri_list(&paths));

        // MULTIPLE: small targets written, ones needing INCR (and unknown ones) refused.
        let got = x11::testing::convert_multiple(
            &session,
            &[OWNER_TARGET, URI_LIST_TARGET, KDE_CUT_TARGET],
        );
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_deref().map(<[u8]>::len), Some(8));
        assert_eq!(got[1], None, "too large for MULTIPLE");
        assert_eq!(got[2], None, "copy has no cut marker");
    }

    #[test]
    #[ignore = "starts Xvfb"]
    fn x11_token_checks_clear_and_replace_only_our_publish() {
        let x = Xvfb::start();
        let a = p("/tmp/owner-a.txt");
        let b = p("/tmp/owner-b.txt");
        let stale = set(&x, std::slice::from_ref(&a), ClipboardEffect::Copy);
        let current = set(&x, &[a.clone(), b.clone()], ClipboardEffect::Copy);
        assert_ne!(stale, current);

        assert!(!with_x11(&x.display, |be| op_clear_if_token(be, stale)).unwrap());
        assert!(read(&x).files.is_some());

        // Replace with a stale token: nothing changes.
        let new_token = super::super::next_token();
        let publish = build_publish(
            std::slice::from_ref(&a),
            ClipboardEffect::Move,
            Some(new_token),
        )
        .unwrap();
        assert_eq!(
            with_x11(&x.display, |be| op_replace_if_token(
                be,
                Some(publish),
                stale
            ))
            .unwrap(),
            None
        );
        assert_eq!(read(&x).owner, Some(current));

        // Replace with the current token.
        let publish = build_publish(
            std::slice::from_ref(&b),
            ClipboardEffect::Move,
            Some(new_token),
        )
        .unwrap();
        assert_eq!(
            with_x11(&x.display, |be| op_replace_if_token(
                be,
                Some(publish),
                current
            ))
            .unwrap(),
            Some(new_token)
        );
        let after = read(&x);
        assert_eq!(after.files, Some((vec![b.clone()], ClipboardEffect::Move)));
        assert_eq!(after.owner, Some(new_token));

        // Empty paths clear.
        assert_eq!(
            with_x11(&x.display, |be| op_replace_if_token(be, None, new_token)).unwrap(),
            None
        );
        assert_eq!(read(&x).files, None);

        // clear_if_token with the current token clears; owned-files cleanup too.
        let token = set(&x, std::slice::from_ref(&a), ClipboardEffect::Copy);
        assert!(with_x11(&x.display, |be| op_clear_if_token(be, token)).unwrap());
        assert_eq!(read(&x).files, None);
        set(&x, std::slice::from_ref(&a), ClipboardEffect::Move);
        let seq = sequence(&x);
        let wrong = with_x11(&x.display, |be| {
            op_clear_if_owned_files(be, std::slice::from_ref(&b), Some(seq))
        });
        assert_eq!(wrong.unwrap(), None);
        let cleared = with_x11(&x.display, |be| {
            op_clear_if_owned_files(be, std::slice::from_ref(&a), Some(seq))
        })
        .unwrap();
        assert_eq!(cleared, Some(sequence(&x)));
        assert_eq!(read(&x).files, None);
    }

    #[test]
    #[ignore = "starts Xvfb"]
    fn x11_set_if_unchanged_refuses_after_another_apps_copy() {
        let x = Xvfb::start();
        let ours = vec![p("/tmp/ours.txt")];
        let theirs = vec![p("/tmp/their file.txt")];
        let baseline = sequence(&x);
        let publish = |paths: &[PathBuf]| {
            build_publish(
                paths,
                ClipboardEffect::Move,
                Some(super::super::next_token()),
            )
            .unwrap()
        };
        // Unchanged since the baseline, or changed only by us: written.
        assert!(
            with_x11(&x.display, |b| op_set_if_unchanged(
                b,
                publish(&ours),
                baseline
            ))
            .unwrap()
        );
        assert!(
            with_x11(&x.display, |b| op_set_if_unchanged(
                b,
                publish(&ours),
                baseline
            ))
            .unwrap()
        );

        // Another app copies (a second connection serving a file manager's formats).
        let foreign = x11::testing::foreign_owner(
            &x.display,
            build_publish(&theirs, ClipboardEffect::Copy, None).unwrap(),
        );
        assert!(
            !with_x11(&x.display, |b| op_set_if_unchanged(
                b,
                publish(&ours),
                baseline
            ))
            .unwrap()
        );
        let read = read(&x);
        assert_eq!(read.files, Some((theirs.clone(), ClipboardEffect::Copy)));
        assert_eq!(read.owner, None);
        assert!(!with_x11(&x.display, |b| b.has_text()).unwrap());
        // An unchanged foreign clipboard keeps its sequence.
        assert_eq!(read.sequence, sequence(&x));
        foreign.stop();
    }

    #[test]
    #[ignore = "starts Xvfb"]
    fn x11_ownership_ends_when_another_client_takes_the_selection() {
        let x = Xvfb::start();
        let paths = vec![p("/tmp/owned.txt")];
        let token = set(&x, &paths, ClipboardEffect::Copy);
        let alive = Arc::clone(&lock_state().owner.as_ref().unwrap().alive);
        let before = sequence(&x);

        // An identical list from another app is not ours.
        let foreign = x11::testing::foreign_owner(
            &x.display,
            build_publish(&paths, ClipboardEffect::Move, None).unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while alive.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "owner thread kept running"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let read = read(&x);
        assert_eq!(read.files, Some((paths.clone(), ClipboardEffect::Move)));
        assert_eq!(read.owner, None);
        assert_ne!(read.sequence, before);
        assert!(!with_x11(&x.display, |b| op_clear_if_token(b, token)).unwrap());
        assert_eq!(
            with_x11(&x.display, op_read).unwrap().files,
            Some((paths.clone(), ClipboardEffect::Move))
        );
        // Clearing another app's matching list (cut cleanup) works too.
        let seq = sequence(&x);
        let cleared = with_x11(&x.display, |b| {
            op_clear_if_owned_files(b, &paths, Some(seq))
        })
        .unwrap();
        assert!(cleared.is_some());
        assert_eq!(read_owner(&x), x11rb::NONE);
        foreign.stop();

        // A client that takes the selection and never answers: our thread still stops, and
        // reads report the timeout as an error rather than hanging.
        let token = set(&x, &paths, ClipboardEffect::Copy);
        let alive = Arc::clone(&lock_state().owner.as_ref().unwrap().alive);
        let (_conn, window) = x11::testing::steal_selection(&x.display);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while alive.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "owner thread kept running"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(read_owner(&x), window);
        assert!(with_x11(&x.display, |b| op_clear_if_token(b, token)).is_err());
    }

    fn read_owner(x: &Xvfb) -> u32 {
        x11::Session::open(&x.display).unwrap().owner().unwrap()
    }
}
