//! RAR archives (RAR 1.5 to 4.x, RAR5 and RAR7), read only, through the official UnRAR source
//! (built from the `unrar-ng-sys` crate). The UnRAR licence allows extracting, not creating,
//! RAR archives, so this backend lists, tests and extracts only.
//!
//! UnRAR never writes a file: every entry is decoded in its *test* mode (`RAR_TEST`) and its
//! data comes out through the `UCM_PROCESSDATA` callback, into the same writers as every
//! other format (names checked, folders pinned, temp file renamed when complete, MOTW).
//! UnRAR checks each file's own CRC32 or BLAKE2 as it decodes it; a mismatch fails the read
//! at its end, so the file is never published.
//!
//! UnRAR's API is blocking (`RARProcessFileW` returns once the whole file was decoded), so it
//! runs on a worker thread and hands its data over through a small bounded channel: decoding
//! waits while the consumer is busy, paused or slow, and stops when it cancels.
//!
//! Multi-volume sets are opened from their first volume (`x.part1.rar`, `x.rar` before
//! `x.r00`); opening another volume opens the set, or says that the first volume is
//! missing. A volume missing later on is reported by name.
//!
//! UnRAR licence (its paragraph 2, quoted as it requires): "UnRAR source code may be used in
//! any software to handle RAR archives without limitations free of charge, but cannot be used
//! to develop RAR (WinRAR) compatible archiver and to re-create RAR compression algorithm,
//! which is proprietary. Distribution of modified UnRAR source code in separate form or as a
//! part of other software is permitted, provided that full text of this paragraph, starting
//! from "UnRAR source code" words, is included in license, or in documentation if license is
//! not available, and in source code comments of resulting package." The full licence is in
//! `packaging/LICENSE-UNRAR.txt` and `packaging/THIRD_PARTY.md`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::c_int;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use unrar_ng_sys as ur;
use zeroize::Zeroizing;

use super::{
    ArchiveBackend, ArchiveEntry, ArchiveFormat, ArchiveListing, EntryEncryption, EntryVisitor,
    ExtractLimits, Password, PasswordProof, normalize_entry_name, validate_entry_name,
};
use crate::error::{CoreError, Result};
use crate::ops::JobCtx;
use crate::paths;

pub(super) static RAR: RarBackend = RarBackend;

pub(super) struct RarBackend;

const SIG4: &[u8] = b"Rar!\x1a\x07\x00";
const SIG5: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// Why a RAR archive isn't edited (`update`, `create`).
pub const READ_ONLY: &str = "RAR archives can be opened and extracted, not edited";

/// Most bytes handed over per message: UnRAR may flush a large part of its window at once.
const CHUNK: usize = 1 << 20;
/// Longest entry name read (UTF-16 units), the longest path Windows has.
const NAME_UNITS: usize = 32_768;

// RAR5 redirection types (`RedirType`).
const REDIR_UNIX_SYMLINK: u32 = 1;
const REDIR_WIN_SYMLINK: u32 = 2;
const REDIR_JUNCTION: u32 = 3;
const REDIR_HARDLINK: u32 = 4;
const REDIR_FILECOPY: u32 = 5;

const HOST_UNIX: u32 = 3;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Whether `head` starts like a RAR archive (RAR4 or RAR5 signature).
pub(super) fn sniff(head: &[u8]) -> bool {
    head.starts_with(SIG5) || head.starts_with(SIG4)
}

/// `.r00`...`.r99` (old-style volumes after `x.rar`).
pub(super) fn is_old_volume_ext(ext: &str) -> bool {
    let b = ext.as_bytes();
    b.len() == 3 && b[0] == b'r' && b[1].is_ascii_digit() && b[2].is_ascii_digit()
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

/// The name the first volume of the set `path` belongs to would have, from its name:
/// `x.part07.rar` -> `x.part01.rar`, `x.r03` -> `x.rar`. `None` when the name says nothing.
pub(super) fn first_volume_name(path: &Path) -> Option<PathBuf> {
    let name = paths::name_of(path);
    let lower = name.to_lowercase();
    if let Some(stem) = lower.strip_suffix(".rar")
        && let Some(at) = stem.rfind(".part")
    {
        let digits = &stem[at + 5..];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let first = format!("{}{:0width$}.rar", &name[..at + 5], 1, width = digits.len());
            return Some(path.with_file_name(first));
        }
        return None;
    }
    let ext = crate::model::ext_of(path);
    if is_old_volume_ext(&ext) {
        return Some(path.with_extension("rar"));
    }
    None
}

/// The archive to open for `path`: itself, or the first volume of its set. Encrypted
/// headers hide the volume flags without the password: the name says then.
fn resolve_first_volume(path: &Path, password: Option<&Password>) -> Result<PathBuf> {
    let flags = open_flags(path, password)?;
    if let Some(flags) = flags
        && (flags & ur::ROADF_VOLUME == 0 || flags & ur::ROADF_FIRSTVOLUME != 0)
    {
        return Ok(path.to_path_buf());
    }
    match first_volume_name(path) {
        // RAR 2 volumes don't mark the first one: `x.rar` is it.
        None => Ok(path.to_path_buf()),
        Some(first) if first == path => Ok(path.to_path_buf()),
        Some(first) if first.is_file() => Ok(first),
        // Unreadable flags: opening it says what it needs (a password).
        Some(_) if flags.is_none() => Ok(path.to_path_buf()),
        Some(first) => Err(CoreError::Invalid(format!(
            "{} is a later volume of a multi-volume RAR archive; its first volume, {}, is \
             needed to open it and is missing",
            paths::display_path(path),
            paths::name_of(&first)
        ))),
    }
}

// ---------------------------------------------------------------------------
// UnRAR sessions
// ---------------------------------------------------------------------------

fn wide(path: &Path) -> Vec<ur::WCHAR> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy()
            .chars()
            .map(|c| c as ur::WCHAR)
            .chain(std::iter::once(0))
            .collect()
    }
}

fn from_wide(units: &[ur::WCHAR]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    #[cfg(windows)]
    {
        String::from_utf16_lossy(&units[..end])
    }
    #[cfg(not(windows))]
    {
        units[..end]
            .iter()
            .map(|&u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
            .collect()
    }
}

/// What UnRAR's callback does with decoded data.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// Nothing is expected (listing).
    None,
    /// Hand it over as [`Event::Data`].
    Deliver,
    /// Drop it, sending [`Event::Tick`] so the consumer can pause or cancel.
    Discard,
}

/// State the callback works with (it gets a pointer to it as `UserData`). Only ever used
/// through shared references, so the pointer UnRAR holds stays valid while the owner reads
/// and sets it between calls.
struct Callback<'a> {
    password: Option<Zeroizing<Vec<ur::WCHAR>>>,
    password_asked: Cell<bool>,
    missing_volume: RefCell<Option<String>>,
    large_dict: Cell<Option<u64>>,
    sink: Cell<Sink>,
    /// Set by the consumer while it doesn't want the current entry's data any more.
    discard: Option<&'a AtomicBool>,
    /// Set by the consumer to stop decoding.
    abort: Option<&'a AtomicBool>,
    events: Option<Sender<Event>>,
}

impl Callback<'_> {
    fn new(password: Option<&Password>) -> Self {
        let password = password.filter(|p| !p.is_empty()).map(|p| {
            let text = String::from_utf8_lossy(p.as_bytes());
            #[cfg(windows)]
            let units: Vec<ur::WCHAR> = text.encode_utf16().collect();
            #[cfg(not(windows))]
            let units: Vec<ur::WCHAR> = text.chars().map(|c| c as ur::WCHAR).collect();
            Zeroizing::new(units)
        });
        Self {
            password,
            password_asked: Cell::new(false),
            missing_volume: RefCell::new(None),
            large_dict: Cell::new(None),
            sink: Cell::new(Sink::None),
            discard: None,
            abort: None,
            events: None,
        }
    }

    fn aborted(&self) -> bool {
        self.abort.is_some_and(|a| a.load(Ordering::Acquire))
    }

    fn data(&self, data: &[u8]) -> c_int {
        if self.aborted() {
            return -1;
        }
        let Some(events) = &self.events else {
            return 1;
        };
        let discard = self.sink.get() == Sink::Discard
            || self.discard.is_some_and(|d| d.load(Ordering::Acquire));
        if discard {
            return if events.send(Event::Tick).is_ok() {
                1
            } else {
                -1
            };
        }
        for part in data.chunks(CHUNK) {
            if self.aborted() || events.send(Event::Data(part.to_vec())).is_err() {
                return -1;
            }
        }
        1
    }
}

extern "C" fn callback(msg: ur::UINT, user: ur::LPARAM, p1: ur::LPARAM, p2: ur::LPARAM) -> c_int {
    // Never unwind into C++.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `user` is the `Callback` the session was opened with; it outlives the
        // session and is only used on the thread running it.
        let cb = unsafe { &*(user as *const Callback<'_>) };
        match msg {
            ur::UCM_PROCESSDATA => {
                if p1 == 0 || p2 <= 0 {
                    return 1;
                }
                // SAFETY: UnRAR passes `p2` decoded bytes at `p1`, valid during the call.
                let data = unsafe { std::slice::from_raw_parts(p1 as *const u8, p2 as usize) };
                cb.data(data)
            }
            ur::UCM_NEEDPASSWORDW => {
                cb.password_asked.set(true);
                match &cb.password {
                    Some(pw) if p1 != 0 && p2 > 1 => {
                        let n = pw.len().min(p2 as usize - 1);
                        // SAFETY: UnRAR passes a buffer of `p2` wide characters at `p1`.
                        let buf = unsafe {
                            std::slice::from_raw_parts_mut(p1 as *mut ur::WCHAR, p2 as usize)
                        };
                        buf[..n].copy_from_slice(&pw[..n]);
                        buf[n] = 0;
                        1
                    }
                    _ => -1,
                }
            }
            // The narrow version is asked only when the wide one gave nothing.
            ur::UCM_NEEDPASSWORD => -1,
            ur::UCM_CHANGEVOLUMEW => {
                if p2 == ur::RAR_VOL_ASK {
                    if p1 != 0 {
                        // SAFETY: a NUL-terminated wide name (UnRAR's buffer is 2048 units).
                        let units =
                            unsafe { std::slice::from_raw_parts(p1 as *const ur::WCHAR, 2048) };
                        *cb.missing_volume.borrow_mut() = Some(from_wide(units));
                    } else {
                        *cb.missing_volume.borrow_mut() = Some(String::new());
                    }
                    -1
                } else if cb.aborted() {
                    -1
                } else {
                    1
                }
            }
            ur::UCM_CHANGEVOLUME => {
                if p2 == ur::RAR_VOL_ASK {
                    cb.missing_volume
                        .borrow_mut()
                        .get_or_insert_with(String::new);
                    -1
                } else {
                    1
                }
            }
            ur::UCM_LARGEDICT => {
                // Beyond UnRAR's own limit (4 GiB): refused. Ours is checked before.
                cb.large_dict.set(Some((p1 as u64).saturating_mul(1024)));
                0
            }
            _ => 1,
        }
    }))
    .unwrap_or(-1)
}

/// An open UnRAR handle, closed when dropped.
struct Session {
    handle: *const ur::Handle,
    flags: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a handle RAROpenArchiveEx returned, closed once.
        unsafe {
            ur::RARCloseArchive(self.handle);
        }
    }
}

/// Open `path` with UnRAR; `cb` must outlive the session and stay where it is.
fn open_session(path: &Path, mode: u32, cb: &Callback<'_>) -> Result<Session> {
    let name = wide(path);
    // SAFETY: zero is a valid value for every field (null pointers, no comment buffer).
    let mut data: ur::OpenArchiveDataEx = unsafe { std::mem::zeroed() };
    data.archive_name_w = name.as_ptr();
    data.open_mode = mode;
    data.callback = Some(callback);
    data.user_data = cb as *const Callback<'_> as ur::LPARAM;
    // SAFETY: `data` and `name` live through the call; UnRAR copies the name.
    let handle = unsafe { ur::RAROpenArchiveEx(&data) };
    let result = data.open_result as c_int;
    if handle.is_null() || result != ur::ERAR_SUCCESS {
        if !handle.is_null() {
            // SAFETY: opened above, not used after.
            unsafe {
                ur::RARCloseArchive(handle);
            }
        }
        let code = if result == ur::ERAR_SUCCESS {
            ur::ERAR_UNKNOWN
        } else {
            result
        };
        return Err(rar_error(path, code, cb, None, true));
    }
    Ok(Session {
        handle,
        flags: data.flags,
    })
}

/// The archive flags (`ROADF_*`) of `path`; `None` when its headers are encrypted and
/// `password` doesn't open them.
fn open_flags(path: &Path, password: Option<&Password>) -> Result<Option<u32>> {
    let cb = Callback::new(password);
    match open_session(path, ur::RAR_OM_LIST, &cb) {
        Ok(s) => Ok(Some(s.flags)),
        Err(CoreError::PasswordRequired(_) | CoreError::WrongPassword(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// One file header as UnRAR reported it.
#[derive(Debug, Clone)]
struct RawHeader {
    name: String,
    flags: u32,
    unp_size: u64,
    pack_size: u64,
    crc: u32,
    hash_type: u32,
    /// Bytes.
    dict: u64,
    redir_type: u32,
    host_os: u32,
    attr: u32,
    modified: Option<SystemTime>,
    method: u32,
    unp_ver: u32,
}

impl RawHeader {
    fn is_dir(&self) -> bool {
        self.flags & ur::RHDF_DIRECTORY != 0
    }
    fn is_link(&self) -> bool {
        matches!(
            self.redir_type,
            REDIR_UNIX_SYMLINK | REDIR_WIN_SYMLINK | REDIR_JUNCTION
        ) || (self.redir_type == 0
            && ((self.host_os == HOST_UNIX && self.attr & 0xF000 == 0xA000)
                || (self.host_os != HOST_UNIX && self.attr & FILE_ATTRIBUTE_REPARSE_POINT != 0)))
    }
    /// Whether UnRAR decodes this entry's data (links and references carry none of their
    /// own in RAR5; RAR4 Unix links are skipped, UnRAR keeps a solid stream going itself).
    fn has_data(&self) -> bool {
        !self.is_dir() && self.redir_type == 0 && !self.is_link()
    }
}

/// Boxed: the header struct is about 10 KiB.
struct HeaderBuf {
    data: Box<ur::HeaderDataEx>,
    name: Vec<ur::WCHAR>,
}

impl HeaderBuf {
    fn new() -> Self {
        // SAFETY: all-zero is valid for this plain C struct (null pointers, zero sizes).
        let data: Box<ur::HeaderDataEx> = unsafe { Box::new_zeroed().assume_init() };
        Self {
            data,
            name: vec![0; NAME_UNITS],
        }
    }

    /// Read the next file header: `Ok(None)` at the end of the archive.
    fn read(&mut self, s: &Session) -> std::result::Result<Option<RawHeader>, c_int> {
        let h = &mut *self.data;
        self.name.fill(0);
        h.file_name_ex = self.name.as_mut_ptr();
        h.file_name_ex_size = NAME_UNITS as u32;
        h.redir_name = std::ptr::null_mut();
        h.redir_name_size = 0;
        h.comment_buffer = std::ptr::null_mut();
        h.comment_buffer_size = 0;
        // SAFETY: a live handle and a header struct with valid buffers.
        let header: *mut ur::HeaderDataEx = &mut *self.data;
        let code = unsafe { ur::RARReadHeaderEx(s.handle, header) };
        match code {
            ur::ERAR_SUCCESS => {}
            ur::ERAR_END_ARCHIVE => return Ok(None),
            e => return Err(e),
        }
        let h = &*self.data;
        let mut name = from_wide(&self.name);
        if self.name[NAME_UNITS - 2] != 0 {
            // Filled to the end: cut short, so not the real name.
            name.push('\u{FFFD}');
        }
        let ticks = (u64::from(h.mtime_high) << 32) | u64::from(h.mtime_low);
        Ok(Some(RawHeader {
            name: normalize_entry_name(&name),
            flags: h.flags,
            unp_size: (u64::from(h.unp_size_high) << 32) | u64::from(h.unp_size),
            pack_size: (u64::from(h.pack_size_high) << 32) | u64::from(h.pack_size),
            crc: h.file_crc,
            hash_type: h.hash_type,
            dict: u64::from(h.dict_size).saturating_mul(1024),
            redir_type: h.redir_type,
            host_os: h.host_os,
            attr: h.file_attr,
            modified: filetime(ticks),
            method: h.method,
            unp_ver: h.unp_ver,
        }))
    }
}

/// FILETIME ticks (100 ns since 1601) as a time.
fn filetime(ticks: u64) -> Option<SystemTime> {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    if ticks == 0 {
        return None;
    }
    let secs = ticks / 10_000_000;
    let nanos = (ticks % 10_000_000) * 100;
    if secs >= EPOCH_DIFF {
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs - EPOCH_DIFF, nanos as u32))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(EPOCH_DIFF - secs))
    }
}

fn process(s: &Session, op: c_int) -> c_int {
    // SAFETY: a live handle; no destination: UnRAR tests or skips, it writes nothing.
    unsafe { ur::RARProcessFileW(s.handle, op, std::ptr::null_mut(), std::ptr::null_mut()) }
}

/// An UnRAR error code as an error for `path` (`entry`: the file it happened to).
fn rar_error(
    path: &Path,
    code: c_int,
    cb: &Callback<'_>,
    entry: Option<&str>,
    headers: bool,
) -> CoreError {
    let shown = paths::display_path(path);
    let what = entry.map_or_else(|| shown.clone(), |e| format!("{shown}: {e}"));
    if let Some(volume) = &*cb.missing_volume.borrow() {
        let name = if volume.is_empty() {
            "the next volume".to_owned()
        } else {
            paths::name_of(Path::new(volume))
        };
        return CoreError::Invalid(format!(
            "{shown}: a volume of this multi-volume archive is missing ({name})"
        ));
    }
    if let Some(dict) = cb.large_dict.get() {
        return CoreError::UnsafePath(format!(
            "{what}: needs a {} MiB dictionary, more than UnRAR allows",
            dict >> 20
        ));
    }
    if cb.aborted() {
        return CoreError::Cancelled;
    }
    match code {
        ur::ERAR_MISSING_PASSWORD => CoreError::PasswordRequired(what),
        ur::ERAR_BAD_PASSWORD => CoreError::WrongPassword(what),
        // Encrypted headers (RAR4 has no password check): a wrong password reads as garbage.
        ur::ERAR_BAD_DATA | ur::ERAR_BAD_ARCHIVE | ur::ERAR_UNKNOWN_FORMAT
            if headers && cb.password_asked.get() && cb.password.is_some() =>
        {
            CoreError::WrongPassword(what)
        }
        ur::ERAR_BAD_DATA if headers && cb.password_asked.get() => {
            CoreError::PasswordRequired(what)
        }
        ur::ERAR_BAD_DATA => {
            CoreError::Invalid(format!("{what}: damaged data (its checksum doesn't match)"))
        }
        ur::ERAR_BAD_ARCHIVE => {
            CoreError::Invalid(format!("{what}: not a RAR archive, or a damaged one"))
        }
        ur::ERAR_UNKNOWN_FORMAT => {
            CoreError::Unsupported(format!("{what}: this RAR format version is not supported"))
        }
        ur::ERAR_NO_MEMORY => CoreError::Invalid(format!("{what}: not enough memory to decode")),
        ur::ERAR_LARGE_DICT => CoreError::UnsafePath(format!(
            "{what}: its dictionary is larger than UnRAR allows"
        )),
        ur::ERAR_EREFERENCE => CoreError::Invalid(format!(
            "{what}: refers to another file of the archive that can't be read"
        )),
        ur::ERAR_EOPEN | ur::ERAR_EREAD => CoreError::io(
            path,
            io::Error::other(format!("can't read the archive (UnRAR error {code})")),
        ),
        _ => CoreError::Invalid(format!("{what}: UnRAR error {code}")),
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The directory of a set, with UnRAR's own view of each file.
struct Listed {
    archive_flags: u32,
    entries: Vec<ArchiveEntry>,
    raw: Vec<RawHeader>,
}

fn method_name(raw: &RawHeader) -> String {
    let level = match raw.method {
        0x30 => "Store",
        0x31 => "Fastest",
        0x32 => "Fast",
        0x33 => "Normal",
        0x34 => "Good",
        0x35 => "Best",
        _ => "",
    };
    let version = match raw.unp_ver {
        0..=29 => "RAR4",
        50 => "RAR5",
        _ => "RAR7",
    };
    if level.is_empty() {
        version.to_owned()
    } else {
        format!("{version} {level}")
    }
}

fn list_set(
    archive: &Path,
    max_entries: usize,
    password: Option<&Password>,
    ctx: Option<&JobCtx<'_>>,
) -> Result<Listed> {
    let first = resolve_first_volume(archive, password)?;
    let cb = Callback::new(password);
    let s = open_session(&first, ur::RAR_OM_LIST_INCSPLIT, &cb)?;
    let mut buf = HeaderBuf::new();
    let mut raw: Vec<RawHeader> = Vec::new();
    loop {
        if let Some(ctx) = ctx {
            ctx.cancel.check()?;
        }
        let h = match buf.read(&s) {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(code) => return Err(rar_error(archive, code, &cb, None, true)),
        };
        if h.flags & ur::RHDF_SPLITBEFORE != 0 {
            // The rest of the previous file, in the next volume.
            match raw.last_mut() {
                Some(prev) if prev.name == h.name => {
                    prev.pack_size = prev.pack_size.saturating_add(h.pack_size);
                    if h.flags & ur::RHDF_SPLITAFTER == 0 {
                        // RAR4 keeps the whole file's CRC in its last part.
                        prev.crc = h.crc;
                        prev.flags &= !ur::RHDF_SPLITAFTER;
                    }
                }
                _ => {
                    return Err(CoreError::Invalid(format!(
                        "{}: starts in the middle of a file; open the first volume of the set",
                        paths::display_path(archive)
                    )));
                }
            }
        } else {
            if raw.len() >= max_entries {
                return Err(CoreError::Invalid(format!(
                    "{}: more than {max_entries} entries",
                    paths::display_path(archive)
                )));
            }
            raw.push(h);
        }
        let code = process(&s, ur::RAR_SKIP);
        if code != ur::ERAR_SUCCESS {
            return Err(rar_error(archive, code, &cb, None, true));
        }
    }
    let archive_flags = s.flags;
    drop(s);
    if raw
        .last()
        .is_some_and(|r| r.flags & ur::RHDF_SPLITAFTER != 0)
    {
        return Err(CoreError::Invalid(format!(
            "{}: a volume of this multi-volume archive is missing (after the last one found)",
            paths::display_path(archive)
        )));
    }
    let entries = entries_of(&raw);
    Ok(Listed {
        archive_flags,
        entries,
        raw,
    })
}

fn entries_of(raw: &[RawHeader]) -> Vec<ArchiveEntry> {
    // Solid groups: a file flagged solid continues the previous one's stream.
    let mut out: Vec<ArchiveEntry> = Vec::with_capacity(raw.len());
    let mut group_start = 0usize;
    let mut offset = 0u64;
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (i, r) in raw.iter().enumerate() {
        let solid = r.flags & ur::RHDF_SOLID != 0 && i > 0;
        if !solid {
            if i > 0 {
                groups.push((group_start, i));
            }
            group_start = i;
            offset = 0;
        }
        let is_dir = r.is_dir();
        let mut name = r.name.clone();
        if is_dir && !name.ends_with('/') {
            name.push('/');
        }
        let encrypted = r.flags & ur::RHDF_ENCRYPTED != 0;
        let skip = if r.redir_type == REDIR_HARDLINK {
            Some("hard link entries are not extracted".to_owned())
        } else if r.redir_type == REDIR_FILECOPY {
            Some(
                "stored as a reference to an identical file of the archive; extract that file \
                 instead"
                    .to_owned(),
            )
        } else if !is_dir && r.unp_size >= i64::MAX as u64 {
            Some("its size is not recorded in the archive".to_owned())
        } else {
            None
        };
        let size = if r.has_data() && skip.is_none() {
            r.unp_size
        } else {
            0
        };
        out.push(ArchiveEntry {
            index: i,
            safe_name: validate_entry_name(&name).is_ok(),
            name,
            is_dir,
            is_symlink: r.is_link(),
            encrypted,
            encryption: encrypted.then_some(if r.unp_ver >= 50 {
                EntryEncryption::RarAes
            } else {
                EntryEncryption::Rar4Aes
            }),
            size,
            compressed_size: r.pack_size,
            modified: r.modified,
            crc32: if r.hash_type == ur::RAR_HASH_CRC32 {
                r.crc
            } else {
                0
            },
            method: method_name(r),
            solid: false,
            block_offset: offset,
            unchecked: r.has_data() && r.hash_type == ur::RAR_HASH_NONE && r.unp_ver >= 50,
            checked_by_block: false,
            block: None,
            block_end: 0,
            skip,
        });
        offset = offset.saturating_add(if r.has_data() { r.unp_size } else { 0 });
    }
    if !raw.is_empty() {
        groups.push((group_start, raw.len()));
    }
    // A solid file's own packed size can be tiny (its data was mostly in the dictionary
    // already): it gets its share of the group's, as a solid 7z block's files do.
    for (start, end) in groups {
        if end - start < 2 {
            out[start].block_offset = 0;
            continue;
        }
        let (unp, pack) = raw[start..end]
            .iter()
            .filter(|r| r.has_data())
            .fold((0u128, 0u128), |(u, p), r| {
                (u + u128::from(r.unp_size), p + u128::from(r.pack_size))
            });
        for e in &mut out[start..end] {
            e.solid = true;
            e.block = Some(start);
            if unp > 0 && e.size > 0 {
                e.compressed_size = (u128::from(e.size) * pack).div_ceil(unp) as u64;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reading entries
// ---------------------------------------------------------------------------

/// From the worker running UnRAR to the consumer.
enum Event {
    /// The next file header (continuation parts of split files are not sent).
    Header(Box<RawHeader>),
    /// Decoded data of the current entry.
    Data(Vec<u8>),
    /// Data of the current entry was decoded and dropped.
    Tick,
    /// The current entry is done: checksum matched, or why not.
    Done(Result<()>),
    /// The archive ended (or failed to open or read).
    End(Result<()>),
}

/// From the consumer to the worker, after each header.
enum Command {
    Skip,
    Test(Sink),
    Stop,
}

/// Runs UnRAR over the set from `first`, as the consumer commands.
fn worker(
    archive: &Path,
    first: &Path,
    password: Option<&Password>,
    events: Sender<Event>,
    commands: Receiver<Command>,
    discard: &AtomicBool,
    abort: &AtomicBool,
) {
    let mut cb = Callback::new(password);
    cb.discard = Some(discard);
    cb.abort = Some(abort);
    cb.events = Some(events.clone());
    let s = match open_session(first, ur::RAR_OM_EXTRACT, &cb) {
        Ok(s) => s,
        Err(e) => {
            let _ = events.send(Event::End(Err(e)));
            return;
        }
    };
    let mut buf = HeaderBuf::new();
    let end = loop {
        if cb.aborted() {
            break Err(CoreError::Cancelled);
        }
        let h = match buf.read(&s) {
            Ok(Some(h)) => h,
            Ok(None) => break Ok(()),
            Err(code) => break Err(rar_error(archive, code, &cb, None, true)),
        };
        if h.flags & ur::RHDF_SPLITBEFORE != 0 {
            // Reached past a split file that was skipped: its continuation.
            let code = process(&s, ur::RAR_SKIP);
            if code != ur::ERAR_SUCCESS {
                break Err(rar_error(archive, code, &cb, None, true));
            }
            continue;
        }
        let name = h.name.clone();
        if events.send(Event::Header(Box::new(h))).is_err() {
            return;
        }
        let Ok(command) = commands.recv() else {
            return;
        };
        let (op, sink) = match command {
            Command::Stop => return,
            Command::Skip => (ur::RAR_SKIP, Sink::None),
            Command::Test(sink) => (ur::RAR_TEST, sink),
        };
        cb.sink.set(sink);
        cb.password_asked.set(false);
        let code = process(&s, op);
        cb.sink.set(Sink::None);
        if op == ur::RAR_SKIP {
            if code != ur::ERAR_SUCCESS {
                break Err(rar_error(archive, code, &cb, Some(&name), false));
            }
            continue;
        }
        let done = if code == ur::ERAR_SUCCESS {
            Ok(())
        } else {
            Err(rar_error(archive, code, &cb, Some(&name), false))
        };
        // A missing volume or a cancel ends the whole run, not just this file.
        if cb.missing_volume.borrow().is_some() || cb.aborted() {
            break Err(done.err().unwrap_or(CoreError::Cancelled));
        }
        if events.send(Event::Done(done)).is_err() {
            return;
        }
    };
    let _ = events.send(Event::End(end));
}

/// The consumer's end of a run.
struct Run<'r> {
    events: &'r Receiver<Event>,
    commands: &'r Sender<Command>,
    discard: &'r AtomicBool,
}

impl Run<'_> {
    /// The next event, checking `ctx` for cancel (and waiting while paused) every so often.
    fn next(&self, ctx: Option<&JobCtx<'_>>) -> Result<Event> {
        loop {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(e) => return Ok(e),
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(ctx) = ctx {
                        ctx.cancel.check()?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CoreError::Invalid("the RAR decoder stopped".into()));
                }
            }
        }
    }

    fn command(&self, c: Command) -> Result<()> {
        self.commands
            .send(c)
            .map_err(|_| CoreError::Invalid("the RAR decoder stopped".into()))
    }

    /// Wait for the current entry's end, its data dropped.
    fn finish(&self, ctx: Option<&JobCtx<'_>>) -> Result<Result<()>> {
        self.discard.store(true, Ordering::Release);
        loop {
            match self.next(ctx)? {
                Event::Data(_) => {}
                Event::Tick => {
                    if let Some(ctx) = ctx {
                        ctx.cancel.check()?;
                    }
                }
                Event::Done(r) => return Ok(r),
                Event::End(r) => {
                    r?;
                    return Err(CoreError::Invalid("the RAR decoder stopped".into()));
                }
                Event::Header(_) => {
                    return Err(CoreError::Invalid("the RAR decoder got out of step".into()));
                }
            }
        }
    }
}

/// The decoded data of one entry, as the worker hands it over. Its end is reported only
/// once UnRAR checked the file's checksum: a mismatch is an error, never a clean end.
struct EntryReader<'r, 'a> {
    run: &'r Run<'a>,
    buf: Vec<u8>,
    pos: usize,
    /// The entry's outcome, once known.
    done: Option<Result<(), String>>,
    /// The run ended under it (a missing volume, a cancel).
    fatal: Option<CoreError>,
}

impl Read for EntryReader<'_, '_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = out.len().min(self.buf.len() - self.pos);
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if let Some(done) = &self.done {
                return match done {
                    Ok(()) => Ok(0),
                    Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.clone())),
                };
            }
            match self.run.events.recv() {
                Ok(Event::Data(d)) => {
                    self.buf = d;
                    self.pos = 0;
                }
                Ok(Event::Tick) => {}
                Ok(Event::Done(r)) => self.done = Some(r.map_err(|e| e.to_string())),
                Ok(Event::End(r)) => {
                    let e = r
                        .err()
                        .unwrap_or_else(|| CoreError::Invalid("the RAR decoder stopped".into()));
                    self.done = Some(Err(e.to_string()));
                    self.fatal = Some(e);
                }
                Ok(Event::Header(_)) | Err(_) => {
                    self.done = Some(Err("the RAR decoder stopped".into()));
                    self.fatal = Some(CoreError::Invalid("the RAR decoder stopped".into()));
                }
            }
        }
    }
}

fn read_set(
    archive: &Path,
    password: Option<&Password>,
    wanted: &dyn Fn(&ArchiveEntry) -> bool,
    ctx: Option<&JobCtx<'_>>,
    visit: &mut EntryVisitor<'_>,
) -> Result<()> {
    let listed = list_set(archive, usize::MAX, password, None)?;
    let want: HashSet<usize> = listed
        .entries
        .iter()
        .filter(|e| wanted(e))
        .map(|e| e.index)
        .collect();
    let Some(&last) = want.iter().max() else {
        return Ok(());
    };
    let first = resolve_first_volume(archive, password)?;
    let solid_archive = listed.archive_flags & ur::ROADF_SOLID != 0;
    let (ev_tx, ev_rx) = bounded::<Event>(4);
    let (cmd_tx, cmd_rx) = bounded::<Command>(1);
    let discard = AtomicBool::new(false);
    let abort = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("unrar".into())
            .spawn_scoped(scope, || {
                worker(archive, &first, password, ev_tx, cmd_rx, &discard, &abort)
            })
            .map_err(|e| CoreError::io(archive, e))?;
        let run = Run {
            events: &ev_rx,
            commands: &cmd_tx,
            discard: &discard,
        };
        let result = drive(
            &run,
            &listed,
            &want,
            last,
            solid_archive,
            archive,
            ctx,
            visit,
        );
        if result.is_err() {
            abort.store(true, Ordering::Release);
        } else {
            let _ = cmd_tx.send(Command::Stop);
        }
        drop(cmd_tx);
        drop(ev_rx);
        let _ = worker.join();
        result
    })
}

#[allow(clippy::too_many_arguments)]
fn drive(
    run: &Run<'_>,
    listed: &Listed,
    want: &HashSet<usize>,
    last: usize,
    solid_archive: bool,
    archive: &Path,
    ctx: Option<&JobCtx<'_>>,
    visit: &mut EntryVisitor<'_>,
) -> Result<()> {
    let changed = || {
        CoreError::Invalid(format!(
            "{}: the archive changed while it was read",
            paths::display_path(archive)
        ))
    };
    let mut index = 0usize;
    loop {
        let header = match run.next(ctx)? {
            Event::Header(h) => h,
            Event::End(r) => {
                r?;
                // Ended before the last wanted entry: the archive changed since its listing.
                return if index > last { Ok(()) } else { Err(changed()) };
            }
            _ => return Err(CoreError::Invalid("the RAR decoder got out of step".into())),
        };
        let i = index;
        index += 1;
        let (Some(entry), Some(raw)) = (listed.entries.get(i), listed.raw.get(i)) else {
            return Err(changed());
        };
        if header.name != raw.name || header.unp_size != raw.unp_size {
            return Err(changed());
        }
        if !want.contains(&i) {
            if i > last {
                return Ok(());
            }
            // A solid stream is decoded through every file to the wanted ones: tested and
            // dropped here (UnRAR's skip would decode it too, without a way to cancel).
            if solid_archive && raw.has_data() {
                run.discard.store(true, Ordering::Release);
                run.command(Command::Test(Sink::Discard))?;
                // A damaged file before the wanted ones shows up in their own results.
                let _ = run.finish(ctx)?;
            } else {
                run.command(Command::Skip)?;
            }
            continue;
        }
        if let Some(ctx) = ctx {
            ctx.cancel.check()?;
        }
        if !raw.has_data() || entry.skip.is_some() {
            run.command(Command::Skip)?;
            visit(entry, Ok(&mut io::empty()))?;
            continue;
        }
        run.discard.store(false, Ordering::Release);
        run.command(Command::Test(Sink::Deliver))?;
        // The first data, or the outcome: a wrong password shows before any data.
        let first = run.next(ctx)?;
        match first {
            Event::Data(d) => {
                let mut reader = EntryReader {
                    run,
                    buf: d,
                    pos: 0,
                    done: None,
                    fatal: None,
                };
                let visited = visit(entry, Ok(&mut reader));
                if let Some(e) = reader.fatal.take() {
                    return Err(e);
                }
                let finished = reader.done.is_some();
                visited?;
                if !finished {
                    let _ = run.finish(ctx)?;
                }
            }
            Event::Tick => {
                // Only in discard mode; not expected here.
                let _ = run.finish(ctx)?;
                visit(
                    entry,
                    Err(CoreError::Invalid("the RAR decoder got out of step".into())),
                )?;
            }
            Event::Done(Ok(())) => visit(entry, Ok(&mut io::empty()))?,
            Event::Done(Err(e)) => visit(entry, Err(e))?,
            Event::End(r) => {
                r?;
                return Err(changed());
            }
            Event::Header(_) => {
                return Err(CoreError::Invalid("the RAR decoder got out of step".into()));
            }
        }
        if i == last {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

impl ArchiveBackend for RarBackend {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Rar
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rar"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        sniff(head)
    }

    fn list(
        &self,
        archive: &Path,
        max_entries: usize,
        password: Option<&Password>,
    ) -> Result<ArchiveListing> {
        self.list_progress(
            archive,
            &ExtractLimits {
                max_entries,
                ..Default::default()
            },
            password,
            None,
        )
    }

    fn list_progress(
        &self,
        archive: &Path,
        limits: &ExtractLimits,
        password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
    ) -> Result<ArchiveListing> {
        let listed = list_set(archive, limits.max_entries, password, ctx)?;
        Ok(ArchiveListing {
            format: ArchiveFormat::Rar,
            entries: listed.entries,
            comment: String::new(),
        })
    }

    fn preflight(
        &self,
        archive: &Path,
        indices: &[usize],
        password: Option<&Password>,
        limits: &ExtractLimits,
    ) -> Result<()> {
        let listed = list_set(archive, limits.max_entries, password, None)?;
        let Some(&last) = indices.iter().max() else {
            return Ok(());
        };
        let wanted: HashSet<usize> = indices.iter().copied().collect();
        let solid = listed.archive_flags & ur::ROADF_SOLID != 0;
        // A solid stream is decoded from its start: every file up to the last wanted one
        // sets up its decoder.
        for (i, raw) in listed.raw.iter().enumerate().take(last + 1) {
            if !raw.has_data() || !(solid || wanted.contains(&i)) {
                continue;
            }
            if raw.dict > limits.max_decoder_memory {
                return Err(CoreError::UnsafePath(format!(
                    "{}: needs {} MiB of decoder memory (limit {} MiB)",
                    raw.name,
                    raw.dict >> 20,
                    limits.max_decoder_memory >> 20
                )));
            }
        }
        Ok(())
    }

    fn read_entries(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()> {
        read_set(archive, password, wanted, ctx, visit)
    }

    fn check_password(
        &self,
        archive: &Path,
        entries: &[&ArchiveEntry],
        password: Option<&Password>,
    ) -> Result<PasswordProof> {
        // The cheapest encrypted file, decoded to its end: RAR5 refuses a wrong password
        // from its check value, a RAR4 file only fails its CRC.
        let Some(probe) = entries
            .iter()
            .filter(|e| e.encrypted && !e.is_dir && e.skip.is_none() && !e.is_symlink)
            .min_by_key(|e| e.decode_cost())
        else {
            return Ok(PasswordProof::Proven);
        };
        let index = probe.index;
        let rar4 = probe.encryption == Some(EntryEncryption::Rar4Aes);
        let mut outcome: Result<()> = Ok(());
        read_set(
            archive,
            password,
            &|e| e.index == index,
            None,
            &mut |_, reader| {
                match reader {
                    Err(e) => outcome = Err(e),
                    Ok(r) if rar4 => {
                        if let Err(e) = io::copy(r, &mut io::sink()) {
                            outcome = Err(CoreError::WrongPassword(format!(
                                "{} (or the file is damaged: {e})",
                                paths::display_path(archive)
                            )));
                        }
                    }
                    Ok(_) => {}
                }
                Ok(())
            },
        )?;
        match outcome {
            Ok(()) => Ok(PasswordProof::Proven),
            Err(e @ (CoreError::PasswordRequired(_) | CoreError::WrongPassword(_))) => Err(e),
            // Damaged rather than refused: each file is checked as it is extracted.
            Err(_) => Ok(PasswordProof::Proven),
        }
    }
}
