//! Reading tar streams: V7, ustar, GNU (long names and links, old sparse files) and POSIX
//! PAX (path, linkpath, size, mtime and the GNU sparse formats 0.0, 0.1 and 1.0). Names
//! are UTF-8, or code page 437 when they are not valid UTF-8.
//!
//! Written for untrusted input rather than taken from a crate, so every bound is ours: header
//! checksums are verified; octal and base-256 numbers are parsed with checked arithmetic;
//! a PAX header is at most [`MAX_PAX`] bytes, a GNU long name or link [`MAX_LONG_NAME`];
//! a member has at most [`MAX_META_HEADERS`] extension headers before it; a sparse map has
//! at most [`MAX_SPARSE_REGIONS`] regions, sorted, not overlapping, within the file's size
//! and adding up to the data stored. Anything else is an error for the whole archive.
//!
//! [`TarParser`] walks a stream member by member (headers only; data is skipped unless
//! read through [`TarParser::data`]) and tells each member's offsets in the uncompressed
//! stream, so a plain `.tar` can later be read by seeking straight to a member's data.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const BLOCK: u64 = 512;
/// Largest PAX extended header (local or global) read.
pub(crate) const MAX_PAX: u64 = 1024 * 1024;
/// Largest GNU long name or long link name read.
pub(crate) const MAX_LONG_NAME: u64 = 64 * 1024;
/// Most extension headers (PAX, GNU long names) before one member.
pub(crate) const MAX_META_HEADERS: usize = 32;
/// Most regions in one sparse file's map.
pub(crate) const MAX_SPARSE_REGIONS: usize = 100_000;
/// Most records in one PAX header.
const MAX_PAX_RECORDS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    Hardlink,
    CharDevice,
    BlockDevice,
    Fifo,
}

/// A sparse file: its data regions `(offset, length)` in the file, sorted and disjoint;
/// everything else reads as zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SparseMap {
    pub regions: Vec<(u64, u64)>,
}

/// One member, as its headers describe it.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    /// Its name as stored (`/`-separated, UTF-8 or else code page 437).
    pub name: String,
    pub kind: Kind,
    pub link: String,
    /// The file's size (for a sparse file, with its holes).
    pub size: u64,
    /// Data bytes stored in the archive after [`Member::data`] (for a sparse file, its
    /// regions only).
    pub stored: u64,
    /// Offsets in the (uncompressed) tar stream: its first header (extension headers
    /// included), its data, and just past its data's padding.
    pub start: u64,
    pub data: u64,
    pub end: u64,
    pub mtime: Option<SystemTime>,
    pub sparse: Option<Arc<SparseMap>>,
    /// Hash of its own 512-byte header, to recognise it when seeking back to it.
    pub header_hash: u64,
}

impl Member {
    pub(crate) fn is_file(&self) -> bool {
        self.kind == Kind::File
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the tar archive is truncated (it ends inside a member)",
    )
}

// ---------------------------------------------------------------------------
// Header fields
// ---------------------------------------------------------------------------

/// A NUL-terminated field.
fn cstr(field: &[u8]) -> &[u8] {
    match field.iter().position(|&b| b == 0) {
        Some(n) => &field[..n],
        None => field,
    }
}

/// Bytes of a name: UTF-8 when valid, else code page 437, which tar tools on Windows
/// (bsdtar, 7-Zip) write in ustar and V7 headers (every byte one character, so distinct
/// names stay distinct).
pub(crate) fn decode_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => super::cp437::decode(raw),
    }
}

/// A numeric field: octal digits (leading spaces, ending at a space or NUL) or GNU
/// base-256 (first byte `0x80` positive, `0xff` negative). Checked: overflow is an error.
pub(crate) fn parse_number(field: &[u8]) -> Result<i128, ()> {
    let Some(&first) = field.first() else {
        return Ok(0);
    };
    if first & 0x80 != 0 {
        let negative = first == 0xff;
        if !negative && first != 0x80 {
            return Err(());
        }
        let mut v: i128 = 0;
        for &b in &field[1..] {
            let b = if negative { !b } else { b };
            v = v
                .checked_mul(256)
                .ok_or(())?
                .checked_add(i128::from(b))
                .ok_or(())?;
            if v > i128::from(u64::MAX) {
                return Err(());
            }
        }
        return Ok(if negative { -(v + 1) } else { v });
    }
    let mut digits = field
        .iter()
        .skip_while(|&&b| b == b' ' || b == 0)
        .peekable();
    let mut v: i128 = 0;
    for &b in digits.by_ref() {
        match b {
            b'0'..=b'7' => {
                v = v
                    .checked_mul(8)
                    .ok_or(())?
                    .checked_add(i128::from(b - b'0'))
                    .ok_or(())?;
                if v > i128::from(u64::MAX) {
                    return Err(());
                }
            }
            b' ' | 0 => break,
            _ => return Err(()),
        }
    }
    // Only spaces and NULs may follow.
    if digits.any(|&b| b != b' ' && b != 0) {
        return Err(());
    }
    Ok(v)
}

fn unsigned(field: &[u8], what: &str) -> io::Result<u64> {
    match parse_number(field) {
        Ok(v) if v >= 0 => Ok(v as u64),
        _ => Err(bad(format!("tar header: invalid {what} field"))),
    }
}

fn time_from_secs(secs: i128, nanos: u32) -> Option<SystemTime> {
    let s = i64::try_from(secs).ok()?;
    if s >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(s as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(s.unsigned_abs()))?
            .checked_add(Duration::from_nanos(u64::from(nanos)))
    }
}

/// Whether `h` is a header whose checksum matches (unsigned or signed byte sums, as old
/// tars wrote either).
pub(crate) fn checksum_ok(h: &[u8; 512]) -> bool {
    let Ok(stored) = parse_number(&h[148..156]) else {
        return false;
    };
    let mut unsigned: i128 = 0;
    let mut signed: i128 = 0;
    for (i, &b) in h.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += i128::from(b);
        signed += i128::from(b as i8);
    }
    stored == unsigned || stored == signed
}

/// Whether the first 512 bytes of a file are a tar header (checksum matches, and it is not
/// all zeros).
pub(crate) fn looks_like_tar(head: &[u8]) -> bool {
    let Some(h) = head.get(..512) else {
        return false;
    };
    let h: &[u8; 512] = h.try_into().expect("512 bytes");
    h.iter().any(|&b| b != 0) && checksum_ok(h)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Magic {
    V7,
    Ustar,
    Gnu,
}

fn magic(h: &[u8; 512]) -> Magic {
    match &h[257..265] {
        b"ustar\x0000" => Magic::Ustar,
        b"ustar  \x00" => Magic::Gnu,
        m if m.starts_with(b"ustar") => Magic::Ustar,
        _ => Magic::V7,
    }
}

// ---------------------------------------------------------------------------
// PAX
// ---------------------------------------------------------------------------

/// The records of a PAX header (`"<len> <key>=<value>\n"`), checked: each length is
/// decimal, counts itself and fits; keys are UTF-8.
pub(crate) fn parse_pax(data: &[u8]) -> io::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        // NUL padding after the last record ends it.
        if data[pos] == 0 {
            break;
        }
        if out.len() >= MAX_PAX_RECORDS {
            return Err(bad("PAX header: too many records"));
        }
        let rest = &data[pos..];
        let space = rest
            .iter()
            .take(21)
            .position(|&b| b == b' ')
            .ok_or_else(|| bad("PAX header: bad record length"))?;
        let len_text = std::str::from_utf8(&rest[..space])
            .ok()
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| bad("PAX header: bad record length"))?;
        let len: usize = len_text
            .parse()
            .map_err(|_| bad("PAX header: bad record length"))?;
        if len <= space + 1 || len > rest.len() || rest[len - 1] != b'\n' {
            return Err(bad("PAX header: bad record length"));
        }
        let record = &rest[space + 1..len - 1];
        let eq = record
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| bad("PAX header: record without '='"))?;
        let key = std::str::from_utf8(&record[..eq])
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| bad("PAX header: bad key"))?;
        out.push((key.to_owned(), record[eq + 1..].to_vec()));
        pos += len;
    }
    Ok(out)
}

fn pax_u64(v: &[u8], what: &str) -> io::Result<u64> {
    std::str::from_utf8(v)
        .ok()
        .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|t| t.parse::<u64>().ok())
        .ok_or_else(|| bad(format!("PAX header: invalid {what}")))
}

/// A PAX time: decimal seconds, maybe negative, maybe with a fraction.
fn pax_time(v: &[u8]) -> Option<SystemTime> {
    let t = std::str::from_utf8(v).ok()?;
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || whole.len() > 18 {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: i128 = whole.parse().ok()?;
    let mut nanos: u32 = 0;
    for (i, b) in frac.bytes().take(9).enumerate() {
        nanos += u32::from(b - b'0') * 10u32.pow(8 - i as u32);
    }
    if neg {
        // -1.5 is 1.5 seconds before the epoch.
        let t = time_from_secs(-secs, 0)?;
        t.checked_sub(Duration::from_nanos(u64::from(nanos)))
    } else {
        time_from_secs(secs, nanos)
    }
}

/// What extension headers said about the member that follows them.
#[derive(Default)]
struct Pending {
    path: Option<String>,
    link: Option<String>,
    size: Option<u64>,
    mtime: Option<SystemTime>,
    gnu_long_name: Option<String>,
    gnu_long_link: Option<String>,
    /// GNU sparse in PAX: the version, the real size, the real name, the map (0.0 and 0.1).
    sparse_major: Option<u64>,
    sparse_minor: Option<u64>,
    sparse_size: Option<u64>,
    sparse_name: Option<String>,
    sparse_map: Vec<u64>,
    sparse_pending_offset: Option<u64>,
    headers: usize,
}

impl Pending {
    fn apply_pax(&mut self, records: Vec<(String, Vec<u8>)>) -> io::Result<()> {
        // `hdrcharset=BINARY` marks names that are not UTF-8.
        for (key, value) in records {
            match key.as_str() {
                "path" => self.path = Some(decode_name(&value)),
                "linkpath" => self.link = Some(decode_name(&value)),
                "size" => self.size = Some(pax_u64(&value, "size")?),
                "mtime" => self.mtime = pax_time(&value),
                "GNU.sparse.major" => self.sparse_major = Some(pax_u64(&value, "sparse version")?),
                "GNU.sparse.minor" => self.sparse_minor = Some(pax_u64(&value, "sparse version")?),
                "GNU.sparse.size" | "GNU.sparse.realsize" => {
                    self.sparse_size = Some(pax_u64(&value, "sparse size")?)
                }
                "GNU.sparse.name" => self.sparse_name = Some(decode_name(&value)),
                "GNU.sparse.numblocks" => {
                    let n = pax_u64(&value, "sparse map")?;
                    if n > MAX_SPARSE_REGIONS as u64 {
                        return Err(bad("sparse file: too many regions"));
                    }
                    self.sparse_major.get_or_insert(0);
                }
                // Version 0.0: offset/numbytes pairs, in order.
                "GNU.sparse.offset" => {
                    self.sparse_pending_offset = Some(pax_u64(&value, "sparse map")?);
                    self.sparse_major.get_or_insert(0);
                }
                "GNU.sparse.numbytes" => {
                    let off = self
                        .sparse_pending_offset
                        .take()
                        .ok_or_else(|| bad("sparse file: length without offset"))?;
                    self.push_sparse(off, pax_u64(&value, "sparse map")?)?;
                }
                // Version 0.1: one "offset,length,offset,length..." list.
                "GNU.sparse.map" => {
                    self.sparse_major.get_or_insert(0);
                    self.sparse_minor.get_or_insert(1);
                    let text =
                        std::str::from_utf8(&value).map_err(|_| bad("sparse file: bad map"))?;
                    let nums: Vec<&str> = text.split(',').filter(|s| !s.is_empty()).collect();
                    if !nums.len().is_multiple_of(2) || nums.len() / 2 > MAX_SPARSE_REGIONS {
                        return Err(bad("sparse file: bad map"));
                    }
                    for pair in nums.chunks(2) {
                        let off = pax_u64(pair[0].as_bytes(), "sparse map")?;
                        let len = pax_u64(pair[1].as_bytes(), "sparse map")?;
                        self.push_sparse(off, len)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn push_sparse(&mut self, off: u64, len: u64) -> io::Result<()> {
        if self.sparse_map.len() / 2 >= MAX_SPARSE_REGIONS {
            return Err(bad("sparse file: too many regions"));
        }
        self.sparse_map.push(off);
        self.sparse_map.push(len);
        Ok(())
    }
}

/// Check a sparse map against the file's size and the bytes stored: regions sorted, not
/// overlapping, within `size`, adding up to `stored`. Zero-length regions are dropped.
fn checked_map(pairs: Vec<(u64, u64)>, size: u64, stored: u64) -> io::Result<SparseMap> {
    let mut regions = Vec::with_capacity(pairs.len());
    let mut end = 0u64;
    let mut total = 0u64;
    for (off, len) in pairs {
        let region_end = off
            .checked_add(len)
            .ok_or_else(|| bad("sparse file: region out of range"))?;
        if off < end || region_end > size {
            return Err(bad("sparse file: regions overlap or exceed the file"));
        }
        end = region_end;
        total = total
            .checked_add(len)
            .ok_or_else(|| bad("sparse file: region out of range"))?;
        if len > 0 {
            regions.push((off, len));
        }
    }
    if total != stored {
        return Err(bad("sparse file: the map doesn't match the data stored"));
    }
    Ok(SparseMap { regions })
}

// ---------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------

/// What a tar is read from: a reader that may be able to skip without reading.
pub(crate) trait Source: Read {
    /// Skip `n` bytes without reading them, when the source can (a file); `false` when it
    /// can't (a decoder), and they are read instead.
    fn skip_fast(&mut self, _n: u64) -> io::Result<bool> {
        Ok(false)
    }
}

impl Source for &[u8] {}
impl Source for Box<dyn Read + '_> {}

/// A plain `.tar` file: skips by seeking, and knows its length so a member that would end
/// past it is reported truncated rather than skipped into the void.
pub(crate) struct SeekFile {
    inner: io::BufReader<std::fs::File>,
    pos: u64,
    len: u64,
}

impl SeekFile {
    pub(crate) fn open(path: &std::path::Path, at: u64) -> io::Result<Self> {
        use std::io::Seek;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut inner = io::BufReader::with_capacity(64 * 1024, file);
        inner.seek(io::SeekFrom::Start(at))?;
        Ok(Self {
            inner,
            pos: at,
            len,
        })
    }
}

impl Read for SeekFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Source for SeekFile {
    fn skip_fast(&mut self, n: u64) -> io::Result<bool> {
        let to = self.pos.checked_add(n).ok_or_else(truncated)?;
        if to > self.len {
            return Err(truncated());
        }
        let delta = i64::try_from(n).map_err(|_| truncated())?;
        self.inner.seek_relative(delta)?;
        self.pos = to;
        Ok(true)
    }
}

/// Counts the bytes read from the inner reader.
pub(crate) struct Counted<R> {
    pub inner: R,
    pub pos: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Walks a tar stream member by member.
pub(crate) struct TarParser<R> {
    r: Counted<R>,
    /// Data bytes of the current member not read yet, then its padding.
    left: u64,
    pad: u64,
    finished: bool,
    members: usize,
    max_members: usize,
    /// Raw bytes of the headers of the member just returned (for copying it as is).
    pub(crate) raw_headers: Option<Vec<u8>>,
}

impl<R: Source> TarParser<R> {
    pub(crate) fn new(r: R, max_members: usize) -> Self {
        Self::at(r, 0, max_members)
    }

    /// A parser of a stream already at offset `pos` (seeked to a member's first header).
    pub(crate) fn at(r: R, pos: u64, max_members: usize) -> Self {
        Self {
            r: Counted { inner: r, pos },
            left: 0,
            pad: 0,
            finished: false,
            members: 0,
            max_members,
            raw_headers: None,
        }
    }

    /// Keep the raw header bytes of each member in [`TarParser::raw_headers`].
    pub(crate) fn keep_raw_headers(&mut self) {
        self.raw_headers = Some(Vec::new());
    }

    /// The position in the stream.
    pub(crate) fn pos(&self) -> u64 {
        self.r.pos
    }

    pub(crate) fn into_inner(self) -> R {
        self.r.inner
    }

    pub(crate) fn inner_mut(&mut self) -> &mut R {
        &mut self.r.inner
    }

    fn read_block(&mut self, buf: &mut [u8; 512]) -> io::Result<bool> {
        let mut n = 0;
        while n < 512 {
            match self.r.read(&mut buf[n..]) {
                Ok(0) if n == 0 => return Ok(false),
                Ok(0) => return Err(truncated()),
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if let Some(raw) = &mut self.raw_headers {
            raw.extend_from_slice(buf);
        }
        Ok(true)
    }

    fn skip(&mut self, mut n: u64) -> io::Result<()> {
        if n > 0 && self.r.inner.skip_fast(n)? {
            self.r.pos += n;
            return Ok(());
        }
        let mut buf = [0u8; 16 * 1024];
        while n > 0 {
            let want = n.min(buf.len() as u64) as usize;
            match self.r.read(&mut buf[..want]) {
                Ok(0) => return Err(truncated()),
                Ok(k) => n -= k as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Read a whole extension header's data (at most `max` bytes), with its padding.
    fn read_meta(&mut self, size: u64, max: u64, what: &str) -> io::Result<Vec<u8>> {
        if size > max {
            return Err(bad(format!("tar: {what} is too large ({size} bytes)")));
        }
        let mut data = vec![0u8; size as usize];
        self.r.read_exact(&mut data).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                truncated()
            } else {
                e
            }
        })?;
        let pad = padding(size);
        let mut p = vec![0u8; pad as usize];
        self.r.read_exact(&mut p).map_err(|_| truncated())?;
        if let Some(raw) = &mut self.raw_headers {
            raw.extend_from_slice(&data);
            raw.extend_from_slice(&p);
        }
        Ok(data)
    }

    /// Skip what is left of the current member's data.
    pub(crate) fn skip_rest(&mut self) -> io::Result<()> {
        let n = self.left.saturating_add(self.pad);
        self.left = 0;
        self.pad = 0;
        self.skip(n)
    }

    /// The current member's data (its stored bytes; see [`Member::stored`]). Reading past
    /// them gives end of file.
    pub(crate) fn data(&mut self) -> DataReader<'_, R> {
        DataReader { p: self }
    }

    /// The next member, its headers checked; `None` at the end of the archive (a zero
    /// block, or the end of the stream between members).
    pub(crate) fn next_member(&mut self) -> io::Result<Option<Member>> {
        if self.finished {
            return Ok(None);
        }
        self.skip_rest()?;
        if let Some(raw) = &mut self.raw_headers {
            raw.clear();
        }
        let start = self.r.pos;
        let mut pending = Pending::default();
        let mut h = [0u8; 512];
        loop {
            let header = self.r.pos;
            if !self.read_block(&mut h)? {
                if pending.headers > 0 {
                    return Err(truncated());
                }
                self.finished = true;
                return Ok(None);
            }
            if h.iter().all(|&b| b == 0) {
                if pending.headers > 0 {
                    return Err(bad("tar: extension headers without a member"));
                }
                self.finished = true;
                return Ok(None);
            }
            if !checksum_ok(&h) {
                return Err(bad(format!(
                    "tar: damaged header at offset {header} (checksum doesn't match)"
                )));
            }
            let flag = h[156];
            let size = unsigned(&h[124..136], "size")?;
            match flag {
                b'x' | b'X' | b'g' | b'L' | b'K' => {
                    pending.headers += 1;
                    if pending.headers > MAX_META_HEADERS {
                        return Err(bad("tar: too many extension headers before a member"));
                    }
                    match flag {
                        b'L' | b'K' => {
                            let data = self.read_meta(size, MAX_LONG_NAME, "a GNU long name")?;
                            let name = decode_name(cstr(&data));
                            if flag == b'L' {
                                pending.gnu_long_name = Some(name);
                            } else {
                                pending.gnu_long_link = Some(name);
                            }
                        }
                        b'g' => {
                            // Global headers are checked but not applied (nothing in them
                            // changes where or what a member extracts to).
                            let data = self.read_meta(size, MAX_PAX, "a PAX header")?;
                            parse_pax(&data)?;
                        }
                        _ => {
                            let data = self.read_meta(size, MAX_PAX, "a PAX header")?;
                            pending.apply_pax(parse_pax(&data)?)?;
                        }
                    }
                    continue;
                }
                b'V' => {
                    // A volume label: not a member (its data kept with the headers, so a
                    // member copied as it is stays whole).
                    self.read_meta(size, MAX_LONG_NAME, "a volume label")?;
                    continue;
                }
                b'M' => return Err(bad("multi-volume tar archives are not supported")),
                _ => {}
            }
            self.members += 1;
            if self.members > self.max_members {
                return Err(bad(format!(
                    "the archive has more than {} entries",
                    self.max_members
                )));
            }
            return self.finish_member(&h, start, size, pending).map(Some);
        }
    }

    fn finish_member(
        &mut self,
        h: &[u8; 512],
        start: u64,
        header_size: u64,
        mut p: Pending,
    ) -> io::Result<Member> {
        let m = magic(h);
        let flag = h[156];
        let mut name = match (&p.path, &p.gnu_long_name) {
            (Some(path), _) => path.clone(),
            (None, Some(long)) => long.clone(),
            (None, None) => {
                let base = decode_name(cstr(&h[0..100]));
                let prefix = cstr(&h[345..500]);
                if m == Magic::Ustar && !prefix.is_empty() {
                    format!("{}/{base}", decode_name(prefix))
                } else {
                    base
                }
            }
        };
        let link = p
            .link
            .clone()
            .or_else(|| p.gnu_long_link.clone())
            .unwrap_or_else(|| decode_name(cstr(&h[157..257])));
        let mtime = p.mtime.or_else(|| {
            parse_number(&h[136..148])
                .ok()
                .and_then(|s| time_from_secs(s, 0))
        });
        let mut kind = match flag {
            b'1' => Kind::Hardlink,
            b'2' => Kind::Symlink,
            b'3' => Kind::CharDevice,
            b'4' => Kind::BlockDevice,
            b'5' | b'D' => Kind::Dir,
            b'6' => Kind::Fifo,
            // '0', NUL, '7' (contiguous), 'S' (GNU sparse) and, as POSIX says, any type
            // not known.
            _ => Kind::File,
        };
        if kind == Kind::File && name.ends_with('/') && matches!(flag, 0 | b'0') {
            kind = Kind::Dir; // V7 directories
        }
        // Data stored after the header: the PAX size wins over the header's.
        let mut stored = p.size.unwrap_or(header_size);
        // Links, devices, FIFOs and folders carry no data (GNU dumpdirs do: skipped).
        if matches!(
            kind,
            Kind::Symlink | Kind::CharDevice | Kind::BlockDevice | Kind::Fifo
        ) || (kind == Kind::Dir && flag != b'D')
        {
            stored = 0;
        }
        let mut size = if kind == Kind::File { stored } else { 0 };
        let mut sparse = None;
        let mut map_bytes = 0u64;
        if flag == b'S' {
            // Old GNU sparse: four regions in the header, more in extension blocks.
            let real = unsigned(&h[483..495], "sparse size")?;
            let mut pairs = Vec::new();
            let take = |entries: &[u8], pairs: &mut Vec<(u64, u64)>| -> io::Result<()> {
                for e in entries.chunks(24) {
                    if e[0] == 0 {
                        break;
                    }
                    if pairs.len() >= MAX_SPARSE_REGIONS {
                        return Err(bad("sparse file: too many regions"));
                    }
                    pairs.push((
                        unsigned(&e[..12], "sparse offset")?,
                        unsigned(&e[12..24], "sparse length")?,
                    ));
                }
                Ok(())
            };
            take(&h[386..482], &mut pairs)?;
            let mut extended = h[482] != 0;
            let mut ext = [0u8; 512];
            while extended {
                if !self.read_block(&mut ext)? {
                    return Err(truncated());
                }
                take(&ext[..504], &mut pairs)?;
                extended = ext[504] != 0;
            }
            sparse = Some(checked_map(pairs, real, stored)?);
            size = real;
        } else if let Some(major) = p.sparse_major.filter(|_| kind == Kind::File) {
            let real = p
                .sparse_size
                .ok_or_else(|| bad("sparse file: no real size"))?;
            if let Some(real_name) = p.sparse_name.take() {
                name = real_name;
            }
            let pairs: Vec<(u64, u64)> = if major == 1 {
                if p.sparse_minor.unwrap_or(0) != 0 {
                    return Err(bad("sparse file: unknown sparse format version"));
                }
                // 1.0: the map leads the data, as decimal lines, padded to a block.
                let (pairs, used) = self.read_sparse_map_1_0(stored)?;
                map_bytes = used;
                pairs
            } else if major == 0 {
                p.sparse_map.chunks(2).map(|c| (c[0], c[1])).collect()
            } else {
                return Err(bad("sparse file: unknown sparse format version"));
            };
            let dense = stored - map_bytes;
            sparse = Some(checked_map(pairs, real, dense)?);
            size = real;
        }
        let data = self.r.pos;
        let dense = stored - map_bytes;
        let end = data
            .checked_add(dense)
            .and_then(|e| e.checked_add(padding(stored)))
            .ok_or_else(|| bad("tar: size out of range"))?;
        self.left = dense;
        self.pad = padding(stored);
        Ok(Member {
            name,
            kind,
            link,
            size,
            stored: dense,
            start,
            data,
            end,
            mtime,
            sparse: sparse.map(Arc::new),
            header_hash: xxhash_rust::xxh3::xxh3_64(h),
        })
    }

    /// Read a sparse 1.0 map from the start of the data: the count, then offset and length
    /// lines; it takes whole blocks. Returns the pairs and the bytes the map took.
    fn read_sparse_map_1_0(&mut self, stored: u64) -> io::Result<(Vec<(u64, u64)>, u64)> {
        let mut text: Vec<u8> = Vec::new();
        let mut used = 0u64;
        let mut numbers: Vec<u64> = Vec::new();
        let mut want: Option<u64> = None;
        let mut block = [0u8; 512];
        loop {
            if let Some(n) = want
                && numbers.len() as u64 == n * 2 + 1
            {
                break;
            }
            if used >= stored || used >= MAX_PAX {
                return Err(bad("sparse file: bad map"));
            }
            let mut n = 0;
            while n < 512 {
                match self.r.read(&mut block[n..]) {
                    Ok(0) => return Err(truncated()),
                    Ok(k) => n += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if let Some(raw) = &mut self.raw_headers {
                raw.extend_from_slice(&block);
            }
            used += BLOCK;
            for &b in &block {
                if want.is_some_and(|n| numbers.len() as u64 == n * 2 + 1) {
                    break;
                }
                if b == b'\n' {
                    let v = pax_u64(&text, "sparse map")?;
                    text.clear();
                    if numbers.is_empty() {
                        if v > MAX_SPARSE_REGIONS as u64 {
                            return Err(bad("sparse file: too many regions"));
                        }
                        want = Some(v);
                    }
                    numbers.push(v);
                } else {
                    if text.len() >= 20 {
                        return Err(bad("sparse file: bad map"));
                    }
                    text.push(b);
                }
            }
        }
        if used > stored {
            return Err(bad("sparse file: bad map"));
        }
        let pairs = numbers[1..].chunks(2).map(|c| (c[0], c[1])).collect();
        Ok((pairs, used))
    }
}

/// Bytes of padding after `size` bytes of data.
pub(crate) fn padding(size: u64) -> u64 {
    (BLOCK - size % BLOCK) % BLOCK
}

/// The stored data of the parser's current member.
pub(crate) struct DataReader<'a, R> {
    p: &'a mut TarParser<R>,
}

impl<R: Source> Read for DataReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.p.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.p.left) as usize;
        let n = self.p.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(truncated());
        }
        self.p.left -= n as u64;
        Ok(n)
    }
}

/// A sparse file's contents from its stored regions: zeros in the holes.
pub(crate) struct SparseReader<R> {
    inner: R,
    map: Arc<SparseMap>,
    size: u64,
    pos: u64,
    region: usize,
}

impl<R: Read> SparseReader<R> {
    pub(crate) fn new(inner: R, map: Arc<SparseMap>, size: u64) -> Self {
        Self {
            inner,
            map,
            size,
            pos: 0,
            region: 0,
        }
    }
}

impl<R: Read> Read for SparseReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let next = self.map.regions.get(self.region).copied();
        let n = match next {
            Some((off, len)) if self.pos >= off => {
                // Inside the region.
                let left = off + len - self.pos;
                let want = (buf.len() as u64).min(left) as usize;
                let n = self.inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(truncated());
                }
                if n as u64 == left {
                    self.region += 1;
                }
                n
            }
            _ => {
                // A hole up to the next region (or the end).
                let until = next.map_or(self.size, |(off, _)| off);
                let want = (buf.len() as u64).min(until - self.pos) as usize;
                buf[..want].fill(0);
                want
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_parse_checked() {
        assert_eq!(parse_number(b"0000644\0"), Ok(0o644));
        assert_eq!(parse_number(b"  12 \0\0"), Ok(0o12));
        assert_eq!(parse_number(b"\0\0\0"), Ok(0));
        assert_eq!(parse_number(b"0009"), Err(()));
        assert_eq!(parse_number(b"12 3"), Err(()));
        // Base-256, positive and negative.
        let mut f = [0u8; 12];
        f[0] = 0x80;
        f[11] = 5;
        assert_eq!(parse_number(&f), Ok(5));
        let f = [0xffu8; 12];
        assert_eq!(parse_number(&f), Ok(-1));
        // Too big for 64 bits.
        let mut f = [0xffu8; 12];
        f[0] = 0x80;
        assert_eq!(parse_number(&f), Err(()));
        assert_eq!(parse_number(b"7777777777777777777777777"), Err(()));
        let mut f = [0u8; 12];
        f[0] = 0x81;
        assert_eq!(parse_number(&f), Err(()));
    }

    #[test]
    fn pax_records_are_bounded_and_checked() {
        let ok = b"30 mtime=1700000000.123456789\n20 path=dir/f\xc3\xa9.txt\n";
        let r = parse_pax(ok).unwrap();
        assert_eq!(r[0].0, "mtime");
        assert_eq!(r[1].1, "dir/fé.txt".as_bytes());
        for bad in [
            &b"99 path=x\n"[..],
            b"5 path=x\n",
            b"x path=x\n",
            b"10 pathxxx\n",
            b"11 path=xy",
            b"100000000000000000000000 path=x\n",
        ] {
            assert!(
                parse_pax(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(pax_time(b"-1.5").unwrap() < UNIX_EPOCH);
        assert_eq!(
            pax_time(b"12.5"),
            Some(UNIX_EPOCH + Duration::from_millis(12_500))
        );
        assert_eq!(pax_time(b"abc"), None);
    }

    #[test]
    fn sparse_maps_must_fit() {
        assert!(checked_map(vec![(0, 10), (100, 5)], 200, 15).is_ok());
        assert!(checked_map(vec![(0, 10), (5, 5)], 200, 15).is_err());
        assert!(checked_map(vec![(190, 20)], 200, 20).is_err());
        assert!(checked_map(vec![(0, 10)], 200, 11).is_err());
        assert!(checked_map(vec![(u64::MAX, 2)], u64::MAX, 2).is_err());
        let map = Arc::new(checked_map(vec![(2, 3), (8, 2)], 12, 5).unwrap());
        let mut out = Vec::new();
        SparseReader::new(&b"abcde"[..], map, 12)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"\0\0abc\0\0\0de\0\0");
    }
}
