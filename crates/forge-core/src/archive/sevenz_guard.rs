//! Checks a 7z file's headers before `sevenz-rust2` parses them, so a hostile header can't
//! make it allocate or decode without bound:
//!
//! * the signature header's CRC, and the next header's offset, size (at most
//!   [`MAX_RAW_HEADER`]) and CRC;
//! * an encoded (compressed, possibly encrypted) header: its coders (at most
//!   [`MAX_HEADER_CODERS`], dictionary/model memory at most [`MAX_HEADER_DECODER_MEMORY`]) and
//!   declared size (at most [`MAX_DECODED_HEADER`]) are checked before it is decoded here;
//!   with AES, a password that can't decode it is [`CoreError::WrongPassword`] and no
//!   password is [`CoreError::PasswordRequired`];
//! * the decoded header is walked: pack streams, folders, coders per folder, streams per
//!   coder, sub-streams and files are counted against the listing limits before anything is
//!   allocated for them.
//!
//! The library reads the same bytes again afterwards; everything it allocates is then
//! bounded by these counts.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use aes::cipher::{BlockCipherDecrypt, KeyInit};
use sha2::Digest;
use zeroize::Zeroizing;

use super::Password;
use crate::error::{CoreError, Result};
use crate::paths;

const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
const SIGNATURE_HEADER_SIZE: u64 = 32;

/// Largest header read from the file (7-Zip writes a few bytes per entry).
pub(super) const MAX_RAW_HEADER: u64 = 64 << 20;
/// Largest decoded header: room for 100,000 entries with long names.
pub(super) const MAX_DECODED_HEADER: u64 = 128 << 20;
/// Memory the encoded header's decoder may need (7-Zip uses small LZMA dictionaries there).
pub(super) const MAX_HEADER_DECODER_MEMORY: u64 = 256 << 20;
pub(super) const MAX_HEADER_CODERS: usize = 4;
/// Coders of one folder (BCJ2 with its three LZMA streams is four).
pub(super) const MAX_CODERS: u64 = 8;
/// In/out streams of one coder (BCJ2 has four inputs).
const MAX_CODER_STREAMS: u64 = 4;
/// Largest coder properties (AES: 2 + 16 + 16).
const MAX_PROPS: u64 = 256;
/// 7-Zip's own limit for AES key derivation rounds (2^24).
const MAX_AES_CYCLES_POWER: u8 = 24;

const K_END: u8 = 0x00;
const K_HEADER: u8 = 0x01;
const K_ARCHIVE_PROPERTIES: u8 = 0x02;
const K_ADDITIONAL_STREAMS_INFO: u8 = 0x03;
const K_MAIN_STREAMS_INFO: u8 = 0x04;
const K_FILES_INFO: u8 = 0x05;
const K_PACK_INFO: u8 = 0x06;
const K_UNPACK_INFO: u8 = 0x07;
const K_SUB_STREAMS_INFO: u8 = 0x08;
const K_SIZE: u8 = 0x09;
const K_CRC: u8 = 0x0A;
const K_FOLDER: u8 = 0x0B;
const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
const K_NUM_UNPACK_STREAM: u8 = 0x0D;
const K_ENCODED_HEADER: u8 = 0x17;

pub(super) const ID_COPY: &[u8] = &[0x00];
pub(super) const ID_LZMA: &[u8] = &[0x03, 0x01, 0x01];
pub(super) const ID_LZMA2: &[u8] = &[0x21];
pub(super) const ID_PPMD: &[u8] = &[0x03, 0x04, 0x01];
pub(super) const ID_BZIP2: &[u8] = &[0x04, 0x02, 0x02];
pub(super) const ID_DEFLATE: &[u8] = &[0x04, 0x01, 0x08];
pub(super) const ID_AES: &[u8] = &[0x06, 0xF1, 0x07, 0x01];

/// What the checks found out.
#[derive(Debug, Clone, Copy)]
pub(super) struct HeaderInfo {
    /// The header (entry names) is encrypted; it decoded with the password given.
    pub encrypted: bool,
}

fn damaged(archive: &Path, what: &str) -> CoreError {
    CoreError::Invalid(format!(
        "{}: damaged or unsupported 7z archive ({what})",
        paths::display_path(archive)
    ))
}

fn too_big(archive: &Path, what: String) -> CoreError {
    CoreError::UnsafePath(format!("{}: {what}", paths::display_path(archive)))
}

/// Run the checks on the open archive `file`. `max_entries` bounds the entries (and so the
/// folders and streams) the header may declare.
pub(super) fn check(
    archive: &Path,
    file: &mut File,
    password: Option<&Password>,
    max_entries: usize,
) -> Result<HeaderInfo> {
    let io_err = |e| CoreError::io(archive, e);
    let len = file.seek(SeekFrom::End(0)).map_err(io_err)?;
    file.seek(SeekFrom::Start(0)).map_err(io_err)?;
    let mut sig = [0u8; SIGNATURE_HEADER_SIZE as usize];
    if len < SIGNATURE_HEADER_SIZE {
        return Err(damaged(archive, "too short"));
    }
    file.read_exact(&mut sig).map_err(io_err)?;
    if sig[..6] != SIGNATURE {
        return Err(damaged(archive, "not a 7z file"));
    }
    if sig[6] != 0 {
        return Err(damaged(archive, "unknown format version"));
    }
    let start_crc = u32::from_le_bytes(sig[8..12].try_into().unwrap());
    if start_crc == 0 && sig[12..].iter().all(|&b| b == 0) {
        // An archive whose writer never finished: 7-Zip searches for a header near the end;
        // we don't guess.
        return Err(damaged(
            archive,
            "incomplete archive: its header was never written",
        ));
    }
    if crc32(&sig[12..]) != start_crc {
        return Err(damaged(archive, "start header checksum mismatch"));
    }
    let next_offset = u64::from_le_bytes(sig[12..20].try_into().unwrap());
    let next_size = u64::from_le_bytes(sig[20..28].try_into().unwrap());
    let next_crc = u32::from_le_bytes(sig[28..32].try_into().unwrap());
    if next_size == 0 {
        // An empty archive.
        return Ok(HeaderInfo { encrypted: false });
    }
    if next_size > MAX_RAW_HEADER {
        return Err(too_big(
            archive,
            format!("7z header of {next_size} bytes is larger than allowed"),
        ));
    }
    let header_pos = SIGNATURE_HEADER_SIZE
        .checked_add(next_offset)
        .filter(|p| p.checked_add(next_size).is_some_and(|end| end <= len))
        .ok_or_else(|| damaged(archive, "header lies past the end of the file"))?;
    file.seek(SeekFrom::Start(header_pos)).map_err(io_err)?;
    let mut raw = vec![0u8; next_size as usize];
    file.read_exact(&mut raw).map_err(io_err)?;
    if crc32(&raw) != next_crc {
        return Err(damaged(archive, "header checksum mismatch"));
    }
    let limits = Limits::new(max_entries);
    let (header, encrypted) = if raw[0] == K_ENCODED_HEADER {
        let mut r = Cursor::new(&raw[1..]);
        let streams = read_streams_info(&mut r, &limits).map_err(|e| e.at(archive))?;
        let decoded = decode_header(archive, file, len, &streams, password)?;
        (decoded.data, decoded.encrypted)
    } else {
        (raw, false)
    };
    let mut r = Cursor::new(&header);
    // An encrypted header that passed its CRC is damaged here, not decrypted wrongly.
    let files = walk_header(&mut r, &limits).map_err(|e| e.at(archive))?;
    if files > max_entries as u64 {
        return Err(CoreError::Invalid(format!(
            "archive has {files} entries (listing limit {max_entries})"
        )));
    }
    Ok(HeaderInfo { encrypted })
}

// ---------------------------------------------------------------------------
// Header structures
// ---------------------------------------------------------------------------

/// A parse failure: damage, or something over a limit.
#[derive(Debug)]
pub(super) enum WalkError {
    Damaged(&'static str),
    TooBig(String),
}

impl WalkError {
    fn at(self, archive: &Path) -> CoreError {
        match self {
            WalkError::Damaged(what) => damaged(archive, what),
            WalkError::TooBig(what) => too_big(archive, what),
        }
    }
}

type Walk<T> = std::result::Result<T, WalkError>;

struct Limits {
    entries: u64,
    folders: u64,
    pack_streams: u64,
}

impl Limits {
    fn new(max_entries: usize) -> Self {
        let entries = max_entries as u64;
        Self {
            entries,
            // Every folder holds at least one stream of some entry; a few spare for the
            // encoded header's own.
            folders: entries.saturating_add(4),
            pack_streams: entries.saturating_add(4).saturating_mul(MAX_CODER_STREAMS),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CoderInfo {
    pub id: Vec<u8>,
    pub props: Vec<u8>,
    pub num_in: u64,
    pub num_out: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FolderInfo {
    pub coders: Vec<CoderInfo>,
    /// (in index, out index)
    pub bind_pairs: Vec<(u64, u64)>,
    pub packed: Vec<u64>,
    pub unpack_sizes: Vec<u64>,
    pub crc: Option<u32>,
}

#[derive(Debug, Default)]
pub(super) struct StreamsInfo {
    pub pack_pos: u64,
    pub pack_sizes: Vec<u64>,
    pub folders: Vec<FolderInfo>,
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn remaining(&self) -> u64 {
        (self.data.len() - self.pos) as u64
    }
    fn byte(&mut self) -> Walk<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or(WalkError::Damaged("header ends early"))?;
        self.pos += 1;
        Ok(b)
    }
    fn bytes(&mut self, n: u64) -> Walk<&'a [u8]> {
        if n > self.remaining() {
            return Err(WalkError::Damaged("header ends early"));
        }
        let s = &self.data[self.pos..self.pos + n as usize];
        self.pos += n as usize;
        Ok(s)
    }
    fn skip(&mut self, n: u64) -> Walk<()> {
        self.bytes(n).map(|_| ())
    }
    fn u32(&mut self) -> Walk<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    /// 7z's variable-length number.
    fn number(&mut self) -> Walk<u64> {
        let first = self.byte()?;
        let mut mask = 0x80u8;
        let mut value = 0u64;
        for i in 0..8 {
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Ok(value | (high << (8 * i)));
            }
            value |= u64::from(self.byte()?) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }
    /// A count no larger than `limit`, nor than the bytes left (each counted thing takes at
    /// least `min_bytes` of header, in eighths of a byte: bit vectors).
    fn count(&mut self, limit: u64, min_eighths: u64, what: &'static str) -> Walk<u64> {
        let n = self.number()?;
        if n > limit {
            return Err(WalkError::TooBig(format!(
                "7z header declares {n} {what} (limit {limit})"
            )));
        }
        if n.saturating_mul(min_eighths) > self.remaining().saturating_mul(8) {
            return Err(WalkError::Damaged("count larger than the header"));
        }
        Ok(n)
    }
    fn bits(&mut self, n: u64) -> Walk<Vec<bool>> {
        let bytes = self.bytes(n.div_ceil(8))?;
        Ok((0..n)
            .map(|i| bytes[(i / 8) as usize] & (0x80 >> (i % 8)) != 0)
            .collect())
    }
    /// A "defined" bit vector, all-defined when its first byte says so.
    fn defined(&mut self, n: u64) -> Walk<Vec<bool>> {
        if self.byte()? != 0 {
            Ok(vec![true; n as usize])
        } else {
            self.bits(n)
        }
    }
    fn digests(&mut self, n: u64) -> Walk<Vec<Option<u32>>> {
        let defined = self.defined(n)?;
        defined
            .into_iter()
            .map(|d| if d { self.u32().map(Some) } else { Ok(None) })
            .collect()
    }
}

fn read_pack_info(r: &mut Cursor<'_>, limits: &Limits, info: &mut StreamsInfo) -> Walk<()> {
    info.pack_pos = r.number()?;
    let n = r.count(limits.pack_streams, 8, "packed streams")?;
    loop {
        match r.byte()? {
            K_END => break,
            K_SIZE => {
                info.pack_sizes = (0..n).map(|_| r.number()).collect::<Walk<_>>()?;
            }
            K_CRC => {
                r.digests(n)?;
            }
            _ => return Err(WalkError::Damaged("unexpected property in pack info")),
        }
    }
    if info.pack_sizes.len() as u64 != n {
        return Err(WalkError::Damaged("pack sizes missing"));
    }
    Ok(())
}

fn read_folder(r: &mut Cursor<'_>) -> Walk<FolderInfo> {
    let num_coders = r.count(MAX_CODERS, 8, "coders in one folder")?;
    if num_coders == 0 {
        return Err(WalkError::Damaged("folder without coders"));
    }
    let mut folder = FolderInfo::default();
    let (mut total_in, mut total_out) = (0u64, 0u64);
    for _ in 0..num_coders {
        let flags = r.byte()?;
        if flags & 0x80 != 0 {
            return Err(WalkError::Damaged(
                "alternative coder methods are not supported",
            ));
        }
        let id = r.bytes(u64::from(flags & 0x0F))?.to_vec();
        let (num_in, num_out) = if flags & 0x10 != 0 {
            let i = r.count(MAX_CODER_STREAMS, 0, "streams of one coder")?;
            let o = r.count(MAX_CODER_STREAMS, 0, "streams of one coder")?;
            (i, o)
        } else {
            (1, 1)
        };
        if num_in == 0 || num_out != 1 {
            return Err(WalkError::Damaged(
                "coder with an unsupported stream layout",
            ));
        }
        let props = if flags & 0x20 != 0 {
            let n = r.count(MAX_PROPS, 8, "bytes of coder properties")?;
            r.bytes(n)?.to_vec()
        } else {
            Vec::new()
        };
        total_in += num_in;
        total_out += num_out;
        folder.coders.push(CoderInfo {
            id,
            props,
            num_in,
            num_out,
        });
    }
    let num_bind_pairs = total_out - 1;
    for _ in 0..num_bind_pairs {
        let i = r.number()?;
        let o = r.number()?;
        if i >= total_in || o >= total_out {
            return Err(WalkError::Damaged("bind pair out of range"));
        }
        folder.bind_pairs.push((i, o));
    }
    if total_in < num_bind_pairs {
        return Err(WalkError::Damaged("bad coder graph"));
    }
    let num_packed = total_in - num_bind_pairs;
    if num_packed == 1 {
        let free = (0..total_in)
            .find(|i| !folder.bind_pairs.iter().any(|(bi, _)| bi == i))
            .ok_or(WalkError::Damaged("bad coder graph"))?;
        folder.packed.push(free);
    } else {
        for _ in 0..num_packed {
            let i = r.number()?;
            if i >= total_in {
                return Err(WalkError::Damaged("packed stream out of range"));
            }
            folder.packed.push(i);
        }
    }
    // Exactly one output may be left unbound: the folder's result.
    let unbound = (0..total_out)
        .filter(|o| !folder.bind_pairs.iter().any(|(_, bo)| bo == o))
        .count();
    if unbound != 1 {
        return Err(WalkError::Damaged("bad coder graph"));
    }
    Ok(folder)
}

fn read_unpack_info(r: &mut Cursor<'_>, limits: &Limits, info: &mut StreamsInfo) -> Walk<()> {
    if r.byte()? != K_FOLDER {
        return Err(WalkError::Damaged("folder list missing"));
    }
    let n = r.count(limits.folders, 8, "folders")?;
    if r.byte()? != 0 {
        return Err(WalkError::Damaged("external folder data is not supported"));
    }
    for _ in 0..n {
        let f = read_folder(r)?;
        info.folders.push(f);
    }
    if r.byte()? != K_CODERS_UNPACK_SIZE {
        return Err(WalkError::Damaged("unpack sizes missing"));
    }
    for f in &mut info.folders {
        let outs: u64 = f.coders.iter().map(|c| c.num_out).sum();
        f.unpack_sizes = (0..outs).map(|_| r.number()).collect::<Walk<_>>()?;
    }
    loop {
        match r.byte()? {
            K_END => break,
            K_CRC => {
                let crcs = r.digests(n)?;
                for (f, c) in info.folders.iter_mut().zip(crcs) {
                    f.crc = c;
                }
            }
            _ => return Err(WalkError::Damaged("unexpected property in unpack info")),
        }
    }
    Ok(())
}

fn read_sub_streams_info(r: &mut Cursor<'_>, limits: &Limits, info: &StreamsInfo) -> Walk<()> {
    let mut per_folder = vec![1u64; info.folders.len()];
    let mut ty = r.byte()?;
    if ty == K_NUM_UNPACK_STREAM {
        let mut total = 0u64;
        for n in &mut per_folder {
            *n = r.count(limits.entries, 0, "streams in one folder")?;
            total += *n;
            if total > limits.entries {
                return Err(WalkError::TooBig(format!(
                    "7z header declares more than {} streams",
                    limits.entries
                )));
            }
        }
        ty = r.byte()?;
    }
    if ty == K_SIZE {
        for &n in &per_folder {
            for _ in 1..n.max(1) {
                r.number()?;
            }
        }
        ty = r.byte()?;
    }
    loop {
        match ty {
            K_END => return Ok(()),
            K_CRC => {
                let digests: u64 = per_folder
                    .iter()
                    .zip(&info.folders)
                    .map(|(&n, f)| if n == 1 && f.crc.is_some() { 0 } else { n })
                    .sum();
                r.digests(digests)?;
            }
            _ => {
                let n = r.number()?;
                r.skip(n)?;
            }
        }
        ty = r.byte()?;
    }
}

fn read_streams_info(r: &mut Cursor<'_>, limits: &Limits) -> Walk<StreamsInfo> {
    let mut info = StreamsInfo::default();
    loop {
        match r.byte()? {
            K_END => break,
            K_PACK_INFO => read_pack_info(r, limits, &mut info)?,
            K_UNPACK_INFO => read_unpack_info(r, limits, &mut info)?,
            K_SUB_STREAMS_INFO => read_sub_streams_info(r, limits, &info)?,
            _ => return Err(WalkError::Damaged("unexpected property in streams info")),
        }
    }
    Ok(info)
}

/// Walk a decoded header; returns the number of entries it declares.
fn walk_header(r: &mut Cursor<'_>, limits: &Limits) -> Walk<u64> {
    if r.byte()? != K_HEADER {
        return Err(WalkError::Damaged("no header"));
    }
    let mut ty = r.byte()?;
    if ty == K_ARCHIVE_PROPERTIES {
        loop {
            let prop = r.byte()?;
            if prop == K_END {
                break;
            }
            let n = r.number()?;
            r.skip(n)?;
        }
        ty = r.byte()?;
    }
    if ty == K_ADDITIONAL_STREAMS_INFO {
        return Err(WalkError::Damaged("additional streams are not supported"));
    }
    let mut folders = 0;
    if ty == K_MAIN_STREAMS_INFO {
        folders = read_streams_info(r, limits)?.folders.len();
        ty = r.byte()?;
    }
    let mut files = 0;
    if ty == K_FILES_INFO {
        // Each entry takes at least two bytes of name (its terminator).
        files = r.count(limits.entries, 16, "entries")?;
        loop {
            let prop = r.byte()?;
            if prop == K_END {
                break;
            }
            let n = r.number()?;
            r.skip(n)?;
        }
        ty = r.byte()?;
    } else if folders > 0 {
        return Err(WalkError::Damaged("streams without entries"));
    }
    if ty != K_END {
        return Err(WalkError::Damaged("header not terminated"));
    }
    Ok(files)
}

// ---------------------------------------------------------------------------
// Coders
// ---------------------------------------------------------------------------

/// LZMA2's dictionary size from its property byte.
pub(super) fn lzma2_dictionary(props: &[u8]) -> Option<u64> {
    let bits = u32::from(*props.first()?);
    match bits {
        0..=39 => Some(u64::from(2 | (bits & 1)) << (bits / 2 + 11)),
        40 => Some(0xFFFF_FFFF),
        _ => None,
    }
}

/// Memory a coder's decoder needs, from its properties (`None`: malformed). Filters and
/// unknown methods count as nothing (unknown ones fail when opened).
pub(super) fn decoder_memory(id: &[u8], props: &[u8]) -> Option<u64> {
    Some(match id {
        ID_LZMA => u64::from(u32::from_le_bytes(props.get(1..5)?.try_into().ok()?)),
        ID_LZMA2 => lzma2_dictionary(props)?,
        ID_PPMD => u64::from(u32::from_le_bytes(props.get(1..5)?.try_into().ok()?)),
        // 900k blocks, as `bzip2` allocates them.
        ID_BZIP2 => 8 << 20,
        ID_DEFLATE => 1 << 20,
        _ => 0,
    })
}

// ---------------------------------------------------------------------------
// Decoding the encoded header
// ---------------------------------------------------------------------------

struct Decoded {
    data: Vec<u8>,
    encrypted: bool,
}

/// The coders of a simple chain (every coder one in, one out), from the packed stream to
/// the result.
fn chain(folder: &FolderInfo) -> Option<Vec<usize>> {
    if folder.coders.iter().any(|c| c.num_in != 1) || folder.packed.len() != 1 {
        return None;
    }
    // With one in and one out per coder, coder i has in stream i and out stream i.
    let n = folder.coders.len() as u64;
    let mut order = Vec::new();
    let mut cur = folder.packed[0];
    loop {
        if cur >= n || order.len() as u64 >= n {
            return None;
        }
        order.push(cur as usize);
        // Who reads this coder's output?
        match folder.bind_pairs.iter().find(|(_, o)| *o == cur) {
            Some(&(i, _)) => cur = i,
            None => break,
        }
    }
    (order.len() as u64 == n).then_some(order)
}

fn decode_header(
    archive: &Path,
    file: &mut File,
    len: u64,
    streams: &StreamsInfo,
    password: Option<&Password>,
) -> Result<Decoded> {
    let [folder] = streams.folders.as_slice() else {
        return Err(damaged(archive, "encoded header must be one folder"));
    };
    if folder.coders.len() > MAX_HEADER_CODERS {
        return Err(too_big(archive, "too many coders in the 7z header".into()));
    }
    let encrypted = folder.coders.iter().any(|c| c.id == ID_AES);
    let mut memory = 0u64;
    for c in &folder.coders {
        let need = decoder_memory(&c.id, &c.props)
            .ok_or_else(|| damaged(archive, "bad coder properties in the header"))?;
        memory = memory.saturating_add(need);
    }
    if memory > MAX_HEADER_DECODER_MEMORY {
        return Err(too_big(
            archive,
            format!(
                "7z header needs {} MiB of decoder memory (limit {} MiB)",
                memory >> 20,
                MAX_HEADER_DECODER_MEMORY >> 20
            ),
        ));
    }
    if folder.unpack_sizes.iter().any(|&s| s > MAX_DECODED_HEADER) {
        return Err(too_big(
            archive,
            format!(
                "7z header expands to more than {} MiB",
                MAX_DECODED_HEADER >> 20
            ),
        ));
    }
    let order = chain(folder).ok_or_else(|| damaged(archive, "unsupported header coders"))?;
    if encrypted && password.is_none() {
        return Err(CoreError::PasswordRequired(paths::display_path(archive)));
    }
    let pack_size = *streams
        .pack_sizes
        .first()
        .ok_or_else(|| damaged(archive, "header has no packed stream"))?;
    let pack_pos = SIGNATURE_HEADER_SIZE
        .checked_add(streams.pack_pos)
        .filter(|p| p.checked_add(pack_size).is_some_and(|end| end <= len))
        .ok_or_else(|| damaged(archive, "header data lies past the end of the file"))?;
    if pack_size > MAX_RAW_HEADER {
        return Err(too_big(archive, "7z header is too large".into()));
    }
    file.seek(SeekFrom::Start(pack_pos))
        .map_err(|e| CoreError::io(archive, e))?;
    let mut data = vec![0u8; pack_size as usize];
    file.read_exact(&mut data)
        .map_err(|e| CoreError::io(archive, e))?;

    // A failure after decryption means the password is wrong (or the bytes are damaged,
    // which AES can't tell apart).
    let fail = |what: &'static str| {
        if encrypted {
            CoreError::WrongPassword(paths::display_path(archive))
        } else {
            damaged(archive, what)
        }
    };
    for &ci in &order {
        let coder = &folder.coders[ci];
        let out_size = folder.unpack_sizes[ci];
        data = match coder.id.as_slice() {
            ID_COPY => data,
            ID_AES => {
                let pw = password.expect("checked above");
                aes_decrypt(&coder.props, pw, data)
                    .map_err(|e| e.unwrap_or_else(|| damaged(archive, "bad AES properties")))?
            }
            ID_LZMA => {
                let props = coder
                    .props
                    .first()
                    .copied()
                    .ok_or_else(|| fail("bad LZMA"))?;
                let dict = decoder_memory(ID_LZMA, &coder.props).ok_or_else(|| fail("bad LZMA"))?;
                let reader = lzma_rust2::LzmaReader::new_with_props(
                    &data[..],
                    out_size,
                    props,
                    dict as u32,
                    None,
                )
                .map_err(|_| fail("bad LZMA header data"))?;
                let mut out = Vec::new();
                reader
                    .take(out_size)
                    .read_to_end(&mut out)
                    .map_err(|_| fail("bad LZMA header data"))?;
                out
            }
            _ => return Err(damaged(archive, "unsupported header coder")),
        };
        if ci != *order.last().unwrap() && data.len() as u64 > out_size {
            data.truncate(out_size as usize);
        }
    }
    let final_size = folder.unpack_sizes[*order.last().unwrap()];
    if data.len() as u64 != final_size {
        return Err(fail("header size mismatch"));
    }
    if let Some(crc) = folder.crc
        && crc32(&data) != crc
    {
        return Err(fail("header checksum mismatch"));
    }
    Ok(Decoded { data, encrypted })
}

/// The 7z AES-256 key of `password` for these coder properties (salt, rounds).
fn aes_key(props: &[u8], password: &Password) -> Option<(Zeroizing<[u8; 32]>, [u8; 16])> {
    let b0 = *props.first()?;
    let cycles = b0 & 0x3F;
    let (salt_len, iv_len) = if b0 & 0xC0 == 0 {
        (0, 0)
    } else {
        let b1 = *props.get(1)?;
        (
            usize::from((b0 >> 7) & 1) + usize::from(b1 >> 4),
            usize::from((b0 >> 6) & 1) + usize::from(b1 & 0x0F),
        )
    };
    let salt = props.get(2..2 + salt_len)?;
    let iv_bytes = props.get(2 + salt_len..2 + salt_len + iv_len)?;
    let mut iv = [0u8; 16];
    iv[..iv_len].copy_from_slice(iv_bytes);
    let text = std::str::from_utf8(password.as_bytes()).ok()?;
    let mut utf16 = Zeroizing::new(Vec::with_capacity(text.len() * 2));
    for u in text.encode_utf16() {
        utf16.extend_from_slice(&u.to_le_bytes());
    }
    let mut key = Zeroizing::new([0u8; 32]);
    if cycles == 0x3F {
        let n = salt.len().min(32);
        key[..n].copy_from_slice(&salt[..n]);
        let m = utf16.len().min(32 - n);
        key[n..n + m].copy_from_slice(&utf16[..m]);
    } else {
        if cycles > MAX_AES_CYCLES_POWER {
            return None;
        }
        let mut sha = sha2::Sha256::new();
        let mut counter = [0u8; 8];
        for _ in 0..(1u64 << cycles) {
            sha.update(salt);
            sha.update(&utf16[..]);
            sha.update(counter);
            for b in &mut counter {
                *b = b.wrapping_add(1);
                if *b != 0 {
                    break;
                }
            }
        }
        key.copy_from_slice(&sha.finalize());
    }
    Some((key, iv))
}

/// AES-256-CBC decryption of a 7z AES stream. `Err(None)`: malformed properties.
fn aes_decrypt(
    props: &[u8],
    password: &Password,
    mut data: Vec<u8>,
) -> std::result::Result<Vec<u8>, Option<CoreError>> {
    let (key, iv) = aes_key(props, password).ok_or(None)?;
    // The encrypted stream is whole blocks; anything after the last one is ignored.
    data.truncate(data.len() / 16 * 16);
    let cipher = aes::Aes256Dec::new_from_slice(&key[..]).map_err(|_| None)?;
    let mut prev = iv;
    for chunk in data.chunks_mut(16) {
        let saved: [u8; 16] = chunk.try_into().unwrap();
        let mut block = aes::cipher::Block::<aes::Aes256Dec>::default();
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
        for (b, p) in chunk.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        prev = saved;
    }
    Ok(data)
}

pub(super) fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}
