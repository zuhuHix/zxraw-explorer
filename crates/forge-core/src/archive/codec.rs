//! Whole-file compression streams: gzip, bzip2, xz, Zstandard, LZ4 frames and Brotli. They
//! wrap a tar (`.tar.gz`) or hold one file (`.gz`). Detection by signature (Brotli has none:
//! by name only), decoders with bounded memory, and encoders for the presets.
//!
//! Every decoder handles concatenated streams (gzip members, bzip2 streams, xz streams,
//! Zstandard and LZ4 frames) and checks the checksums the stream carries as it reads: gzip
//! CRC-32, bzip2 block CRCs, the xz check, the Zstandard and LZ4 content checksums when the
//! frame has one. A checksum is only known to match once the stream was read to its end.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Largest Zstandard window a decoder accepts (2^27 = 128 MiB, the `--long` default).
pub(crate) const ZSTD_WINDOW_LOG_MAX: u32 = 27;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    /// No compression (a plain `.tar`).
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    Lz4,
    Brotli,
}

impl Codec {
    /// The codecs that compress.
    pub const COMPRESSED: [Codec; 6] = [
        Codec::Gzip,
        Codec::Bzip2,
        Codec::Xz,
        Codec::Zstd,
        Codec::Lz4,
        Codec::Brotli,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Codec::None => "Stored",
            Codec::Gzip => "gzip",
            Codec::Bzip2 => "bzip2",
            Codec::Xz => "xz",
            Codec::Zstd => "Zstandard",
            Codec::Lz4 => "LZ4",
            Codec::Brotli => "Brotli",
        }
    }

    /// The extension of a single compressed file (no dot; `""` for none).
    pub fn extension(self) -> &'static str {
        match self {
            Codec::None => "",
            Codec::Gzip => "gz",
            Codec::Bzip2 => "bz2",
            Codec::Xz => "xz",
            Codec::Zstd => "zst",
            Codec::Lz4 => "lz4",
            Codec::Brotli => "br",
        }
    }

    /// The extension of a tar compressed with it (no dot).
    pub fn tar_extension(self) -> &'static str {
        match self {
            Codec::None => "tar",
            Codec::Gzip => "tar.gz",
            Codec::Bzip2 => "tar.bz2",
            Codec::Xz => "tar.xz",
            Codec::Zstd => "tar.zst",
            Codec::Lz4 => "tar.lz4",
            Codec::Brotli => "tar.br",
        }
    }

    /// Whether streams of this codec always carry a checksum (xz, Zstandard and LZ4 may not:
    /// see [`stream_has_checksum`]).
    pub fn checksum_name(self) -> Option<&'static str> {
        match self {
            Codec::Gzip => Some("CRC-32"),
            Codec::Bzip2 => Some("CRC-32 per block"),
            Codec::Xz => Some("xz check"),
            Codec::Zstd => Some("XXH64 content checksum"),
            Codec::Lz4 => Some("XXH32 content checksum"),
            Codec::None | Codec::Brotli => None,
        }
    }
}

/// The codec whose signature `head` starts with (Brotli has no signature).
pub(crate) fn sniff(head: &[u8]) -> Option<Codec> {
    if head.starts_with(&[0x1f, 0x8b, 0x08]) {
        Some(Codec::Gzip)
    } else if head.len() >= 4 && head.starts_with(b"BZh") && (b'1'..=b'9').contains(&head[3]) {
        Some(Codec::Bzip2)
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Some(Codec::Xz)
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some(Codec::Zstd)
    } else if head.starts_with(&[0x04, 0x22, 0x4d, 0x18])
        || head.starts_with(&[0x02, 0x21, 0x4c, 0x18])
    {
        Some(Codec::Lz4)
    } else {
        None
    }
}

/// The codec a file name says, and whether the name says it holds a tar: `x.tar.gz` and
/// `x.tgz` give `(Gzip, true)`, `x.gz` gives `(Gzip, false)`, `x.tar` gives `(None, true)`.
pub(crate) fn by_name(name: &str) -> Option<(Codec, bool)> {
    let lower = name.to_lowercase();
    let (stem, ext) = lower.rsplit_once('.')?;
    let tar_inside = stem.ends_with(".tar");
    let codec = match ext {
        "tar" => return Some((Codec::None, true)),
        "tgz" | "taz" => return Some((Codec::Gzip, true)),
        "tbz" | "tbz2" | "tb2" => return Some((Codec::Bzip2, true)),
        "txz" => return Some((Codec::Xz, true)),
        "tzst" => return Some((Codec::Zstd, true)),
        "tlz4" => return Some((Codec::Lz4, true)),
        "gz" => Codec::Gzip,
        "bz2" => Codec::Bzip2,
        "xz" => Codec::Xz,
        "zst" => Codec::Zstd,
        "lz4" => Codec::Lz4,
        "br" => Codec::Brotli,
        _ => return None,
    };
    Some((codec, tar_inside))
}

/// What a compressed file's headers say, read without decoding anything.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamInfo {
    /// Every stream or frame carries a checksum of its data (gzip and bzip2 always do; a file
    /// whose first frame is checked and the rest not doesn't count).
    pub checked: bool,
    /// The most memory a decoder of the file needs (xz dictionaries, Zstandard windows, LZ4
    /// blocks, the Brotli window, bzip2's and gzip's fixed tables).
    pub memory: u64,
    /// The decoded size, when every xz index or Zstandard frame states it.
    pub size: Option<u64>,
}

/// Fixed decoder memory: gzip (a 32 KiB window and buffers) and bzip2 (level 9 tables).
const GZIP_MEMORY: u64 = 256 << 10;
const BZIP2_MEMORY: u64 = 8 << 20;

fn bad_headers(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("damaged or unsupported {what} headers"),
    )
}

/// Most block headers read, over all frames or streams, before a file is decoded. Frame
/// headers alone don't say where the next frame starts (Zstandard and LZ4 frames don't
/// state their compressed length), so the walk goes block by block; millions of tiny
/// blocks would make it long, so it stops here. A real file with this many blocks is at
/// least hundreds of GB (Zstandard blocks are up to 128 KiB, LZ4 up to 4 MiB).
const MAX_HEADER_BLOCKS: u64 = 4_000_000;

#[cfg(test)]
thread_local! {
    /// Tests: a smaller [`MAX_HEADER_BLOCKS`].
    pub(crate) static HEADER_BLOCKS_CAP: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// The work a header walk may do: block headers read, counted across all frames.
struct Work {
    blocks: u64,
    max: u64,
    exceeded: bool,
}

impl Work {
    fn new() -> Self {
        #[cfg(test)]
        if let Some(max) = HEADER_BLOCKS_CAP.with(|c| c.get()) {
            return Self {
                blocks: 0,
                max,
                exceeded: false,
            };
        }
        Self {
            blocks: 0,
            max: MAX_HEADER_BLOCKS,
            exceeded: false,
        }
    }

    /// Count `n` more blocks; `None` (and `exceeded`) past the budget.
    fn blocks(&mut self, n: u64) -> Option<()> {
        self.blocks = self.blocks.saturating_add(n);
        if self.blocks > self.max {
            self.exceeded = true;
            return None;
        }
        Some(())
    }

    /// The error for a walk that stopped: too many blocks, or damaged headers.
    fn error(&self, what: &str) -> io::Error {
        if self.exceeded {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the {what} file has more than {} blocks; it is not read",
                    self.max
                ),
            )
        } else {
            bad_headers(what)
        }
    }
}

/// Read the headers of the compressed file at `path` (see [`StreamInfo`]). An error when they
/// can't be read, or ask for something the decoders refuse (a large-window Brotli stream).
pub(crate) fn stream_info(codec: Codec, path: &Path) -> io::Result<StreamInfo> {
    let fixed = |checked, memory| StreamInfo {
        checked,
        memory,
        size: None,
    };
    Ok(match codec {
        Codec::None => fixed(false, 0),
        Codec::Gzip => fixed(true, GZIP_MEMORY),
        Codec::Bzip2 => fixed(true, BZIP2_MEMORY),
        Codec::Xz => {
            let mut work = Work::new();
            let x = xz_streams(path, &mut work).ok_or_else(|| work.error("xz"))?;
            let size = std::fs::File::open(path)
                .ok()
                .and_then(|f| lzma_rust2::xz_uncompressed_size(BufReader::new(f)).ok());
            StreamInfo {
                checked: x.checked,
                memory: u64::from(lzma_rust2::lzma2_get_memory_usage(x.dict)) * 1024,
                size,
            }
        }
        Codec::Zstd => {
            let mut work = Work::new();
            let z = zstd_frames(path, &mut work).ok_or_else(|| work.error("Zstandard"))?;
            // The decoder's window limit is a power of two: the window rounded up to one.
            let log = zstd_window_log(z.window);
            if log > ZSTD_WINDOW_LOG_MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "a Zstandard window of {} MiB (at most 128 MiB is supported)",
                        z.window.div_ceil(1 << 20)
                    ),
                ));
            }
            StreamInfo {
                checked: z.checked,
                // The window, plus a block's buffers.
                memory: (1u64 << log).saturating_add(1 << 20),
                size: z.size,
            }
        }
        Codec::Lz4 => {
            let mut work = Work::new();
            let l = lz4_frames(path, &mut work).ok_or_else(|| work.error("LZ4"))?;
            // Compressed and decoded blocks, and a linked block's 64 KiB window.
            fixed(
                l.checked,
                l.block.saturating_mul(3).saturating_add(64 << 10),
            )
        }
        Codec::Brotli => {
            let mut head = [0u8; 2];
            let n = std::fs::File::open(path)?.read(&mut head)?;
            let window = brotli_window(&head[..n]).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "large-window Brotli streams are not supported",
                )
            })?;
            fixed(false, window.saturating_add(256 << 10))
        }
    })
}

/// The smallest window log (power of two, at least Zstandard's 2^10) holding `window`.
pub(crate) fn zstd_window_log(window: u64) -> u32 {
    let log = 64 - window.saturating_sub(1).leading_zeros();
    log.max(10)
}

/// The largest window log a decoder under `limit` bytes may use: the power of two at or
/// below it (`None` below Zstandard's smallest window).
pub(crate) fn zstd_window_log_max(limit: u64) -> Option<u32> {
    let log = 63 - limit.max(1).leading_zeros();
    (log >= 10).then_some(log.min(ZSTD_WINDOW_LOG_MAX))
}

/// The window a Brotli stream declares in its first bits (RFC 7932, 9.1); `None` for a
/// large-window stream.
fn brotli_window(head: &[u8]) -> Option<u64> {
    let bits = u16::from(*head.first().unwrap_or(&0)) | u16::from(*head.get(1).unwrap_or(&0)) << 8;
    let wbits = if bits & 1 == 0 {
        16
    } else {
        match (bits >> 1) & 7 {
            0 => match (bits >> 4) & 7 {
                0 => 17,
                1 => return None, // large window
                m => 8 + u32::from(m),
            },
            n => 17 + u32::from(n),
        }
    };
    Some(1u64 << wbits)
}

/// What the frame headers of a Zstandard file say.
struct ZstdFrames {
    /// The total size, when every frame states its own.
    pub size: Option<u64>,
    /// Every frame has a content checksum.
    pub checked: bool,
    /// The largest window a frame needs.
    pub window: u64,
}

fn skip(f: &mut BufReader<File>, n: u64) -> Option<()> {
    f.seek_relative(i64::try_from(n).ok()?).ok()
}

fn read_n<const N: usize>(f: &mut BufReader<File>) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    f.read_exact(&mut b).ok()?;
    Some(b)
}

/// Walk a Zstandard file's frame and block headers (blocks are skipped by their sizes).
/// `None` when it isn't well-formed.
fn zstd_frames(path: &Path, work: &mut Work) -> Option<ZstdFrames> {
    let mut f = BufReader::new(File::open(path).ok()?);
    let len = f.get_ref().metadata().ok()?.len();
    let (mut pos, mut total, mut frames) = (0u64, Some(0u64), 0u32);
    let mut checked = true;
    let mut window = 0u64;
    while pos < len {
        frames += 1;
        if frames > 1_000_000 {
            return None;
        }
        let m = u32::from_le_bytes(read_n::<4>(&mut f)?);
        if (0x184d_2a50..=0x184d_2a5f).contains(&m) {
            let n = u64::from(u32::from_le_bytes(read_n::<4>(&mut f)?));
            skip(&mut f, n)?;
            pos = pos.checked_add(8 + n)?;
            continue;
        }
        if m != 0xfd2f_b528 {
            return None;
        }
        let fhd = read_n::<1>(&mut f)?[0];
        let single = fhd & 0x20 != 0;
        checked &= fhd & 0x04 != 0;
        let dict = [0u64, 1, 2, 4][usize::from(fhd & 3)];
        let fcs_len: u64 = match (fhd >> 6, single) {
            (0, false) => 0, // size not stated
            (0, true) => 1,
            (1, _) => 2,
            (2, _) => 4,
            _ => 8,
        };
        let frame_window = if single {
            None
        } else {
            // Window_Descriptor: exponent and mantissa.
            let wd = read_n::<1>(&mut f)?[0];
            let log = 10 + u32::from(wd >> 3);
            let base = 1u64 << log;
            Some(base + (base / 8) * u64::from(wd & 7))
        };
        skip(&mut f, dict)?;
        let mut fcs = [0u8; 8];
        f.read_exact(&mut fcs[..fcs_len as usize]).ok()?;
        let content = match fcs_len {
            0 => None,
            2 => Some(u64::from_le_bytes(fcs) + 256),
            _ => Some(u64::from_le_bytes(fcs)),
        };
        total = total.and_then(|t| t.checked_add(content?));
        // A single-segment frame's window is its whole content.
        window = window.max(frame_window.or(content)?);
        pos = pos.checked_add(5 + u64::from(!single) + dict + fcs_len)?;
        loop {
            work.blocks(1)?;
            let bh = read_n::<3>(&mut f)?;
            let h = u32::from(bh[0]) | u32::from(bh[1]) << 8 | u32::from(bh[2]) << 16;
            let stored = match (h >> 1) & 3 {
                0 | 2 => u64::from(h >> 3),
                1 => 1,
                _ => return None,
            };
            skip(&mut f, stored)?;
            pos = pos.checked_add(3 + stored)?;
            if h & 1 != 0 {
                break;
            }
        }
        if fhd & 0x04 != 0 {
            skip(&mut f, 4)?;
            pos = pos.checked_add(4)?;
        }
    }
    (pos == len).then_some(ZstdFrames {
        size: total,
        checked,
        window,
    })
}

struct Lz4Frames {
    /// Every frame has a content checksum or block checksums (legacy frames have none).
    checked: bool,
    /// The largest block size a frame declares.
    block: u64,
}

fn lz4_frames(path: &Path, work: &mut Work) -> Option<Lz4Frames> {
    let mut f = BufReader::new(File::open(path).ok()?);
    let len = f.get_ref().metadata().ok()?.len();
    let mut pos = 0u64;
    let mut frames = 0u32;
    let mut out = Lz4Frames {
        checked: true,
        block: 0,
    };
    while pos < len {
        frames += 1;
        if frames > 1_000_000 {
            return None;
        }
        let m = u32::from_le_bytes(read_n::<4>(&mut f)?);
        if (0x184d_2a50..=0x184d_2a5f).contains(&m) {
            let n = u64::from(u32::from_le_bytes(read_n::<4>(&mut f)?));
            skip(&mut f, n)?;
            pos = pos.checked_add(8 + n)?;
            continue;
        }
        if m == 0x184c_2102 {
            // Legacy frames: 8 MiB blocks, no checksum; they run to the end.
            out.checked = false;
            out.block = out.block.max(8 << 20);
            return Some(out);
        }
        if m != 0x184d_2204 {
            return None;
        }
        let [flg, bd] = read_n::<2>(&mut f)?;
        let block_sums = flg & 0x10 != 0;
        out.checked &= flg & 0x04 != 0 || block_sums;
        let block: u64 = match (bd >> 4) & 7 {
            4 => 64 << 10,
            5 => 256 << 10,
            6 => 1 << 20,
            7 => 4 << 20,
            _ => return None,
        };
        out.block = out.block.max(block);
        let extra = 1 + if flg & 0x08 != 0 { 8 } else { 0 } + if flg & 0x01 != 0 { 4 } else { 0 };
        skip(&mut f, extra)?;
        pos = pos.checked_add(6 + extra)?;
        loop {
            work.blocks(1)?;
            let size = u32::from_le_bytes(read_n::<4>(&mut f)?);
            pos = pos.checked_add(4)?;
            if size == 0 {
                break;
            }
            let n = u64::from(size & 0x7fff_ffff) + if block_sums { 4 } else { 0 };
            skip(&mut f, n)?;
            pos = pos.checked_add(n)?;
        }
        if flg & 0x04 != 0 {
            skip(&mut f, 4)?;
            pos = pos.checked_add(4)?;
        }
    }
    (pos == len).then_some(out)
}

struct XzStreams {
    /// Every stream has a check.
    checked: bool,
    /// The largest LZMA2 dictionary of any block.
    dict: u32,
}

/// Walk an xz file from its end: stream footers, indexes, stream headers and every block
/// header (concatenated streams and stream padding allowed).
fn xz_streams(path: &Path, work: &mut Work) -> Option<XzStreams> {
    let mut f = File::open(path).ok()?;
    let mut end = f.metadata().ok()?.len();
    let at = |f: &mut File, pos: u64, buf: &mut [u8]| -> Option<()> {
        f.seek(SeekFrom::Start(pos)).ok()?;
        f.read_exact(buf).ok()
    };
    let mut out = XzStreams {
        checked: true,
        dict: 0,
    };
    let mut streams = 0u32;
    while end > 0 {
        streams += 1;
        if streams > 100_000 {
            return None;
        }
        // Stream padding: groups of four zero bytes.
        let mut word = [0u8; 4];
        loop {
            at(&mut f, end.checked_sub(4)?, &mut word)?;
            if word != [0; 4] {
                break;
            }
            end -= 4;
        }
        let mut footer = [0u8; 12];
        at(&mut f, end.checked_sub(12)?, &mut footer)?;
        if &footer[10..12] != b"YZ" || footer[8] != 0 {
            return None;
        }
        out.checked &= footer[9] & 0x0f != 0;
        let backward = u64::from(u32::from_le_bytes(footer[4..8].try_into().ok()?));
        let index_size = (backward + 1) * 4;
        if index_size > 64 << 20 {
            return None;
        }
        let index_start = end.checked_sub(12 + index_size)?;
        let mut index = vec![0u8; index_size as usize];
        at(&mut f, index_start, &mut index)?;
        if index.first() != Some(&0) {
            return None;
        }
        // The records are read twice, never kept: once for the blocks' total size (where
        // the stream starts), once for each block's header. Their count is checked first,
        // against the work budget and what the index can hold (two bytes a record at least).
        let varint = |p: &mut usize| -> Option<u64> {
            let mut v = 0u64;
            for i in 0..9 {
                let b = *index.get(*p)?;
                *p += 1;
                v |= u64::from(b & 0x7f) << (7 * i);
                if b & 0x80 == 0 {
                    return Some(v);
                }
            }
            None
        };
        let mut p = 1usize;
        let records = varint(&mut p)?;
        if records > (index.len() as u64) / 2 {
            return None;
        }
        work.blocks(records)?;
        let first_record = p;
        let mut blocks = 0u64;
        for _ in 0..records {
            let unpadded = varint(&mut p)?;
            varint(&mut p)?;
            blocks = blocks.checked_add(unpadded.checked_add(3)? & !3)?;
        }
        let start = index_start.checked_sub(blocks)?.checked_sub(12)?;
        let mut header = [0u8; 12];
        at(&mut f, start, &mut header)?;
        if !header.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) || header[6..8] != footer[8..10]
        {
            return None;
        }
        // Each block's header: its filters, the LZMA2 one with its dictionary.
        let mut block_at = start + 12;
        let mut p = first_record;
        for _ in 0..records {
            let unpadded = varint(&mut p)?;
            varint(&mut p)?;
            out.dict = out.dict.max(xz_block_dict(&mut f, block_at)?);
            block_at = block_at.checked_add(unpadded.checked_add(3)? & !3)?;
        }
        end = start;
    }
    Some(out)
}

/// The LZMA2 dictionary size in the xz block header at `pos`.
fn xz_block_dict(f: &mut File, pos: u64) -> Option<u32> {
    let mut first = [0u8; 1];
    f.seek(SeekFrom::Start(pos)).ok()?;
    f.read_exact(&mut first).ok()?;
    if first[0] == 0 {
        return None;
    }
    let len = (usize::from(first[0]) + 1) * 4;
    let mut h = vec![0u8; len - 1];
    f.read_exact(&mut h).ok()?;
    let flags = h[0];
    let mut p = 1usize;
    let varint = |p: &mut usize| -> Option<u64> {
        let mut v = 0u64;
        for i in 0..9 {
            let b = *h.get(*p)?;
            *p += 1;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    if flags & 0x40 != 0 {
        varint(&mut p)?;
    }
    if flags & 0x80 != 0 {
        varint(&mut p)?;
    }
    let mut dict = None;
    for _ in 0..(flags & 3) + 1 {
        let id = varint(&mut p)?;
        let props = usize::try_from(varint(&mut p)?).ok()?;
        let bytes = h.get(p..p.checked_add(props)?)?;
        if id == 0x21 {
            // LZMA2: one byte, the dictionary size.
            let b = *bytes.first()?;
            if b > 40 {
                return None;
            }
            dict = Some(if b == 40 {
                u32::MAX
            } else {
                (2 | (u32::from(b) & 1)) << (b / 2 + 11)
            });
        }
        p += props;
    }
    dict
}

/// A decoder of `codec` over `r`. `mem_limit` bounds the memory a stream may make it
/// allocate (xz dictionaries, Zstandard windows).
pub(crate) fn decoder<'a, R: Read + 'a>(
    codec: Codec,
    r: R,
    mem_limit: u64,
) -> io::Result<Box<dyn Read + 'a>> {
    Ok(match codec {
        Codec::None => Box::new(r),
        Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(BufReader::with_capacity(
            256 * 1024,
            r,
        ))),
        Codec::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(BufReader::with_capacity(
            256 * 1024,
            r,
        ))),
        Codec::Xz => {
            let kb = u32::try_from(mem_limit / 1024)
                .unwrap_or(u32::MAX - 1)
                .min(u32::MAX - 1);
            Box::new(lzma_rust2::XzReader::new_mem_limit(
                BufReader::with_capacity(256 * 1024, r),
                true,
                kb,
            ))
        }
        Codec::Zstd => {
            // The window is the decoder's memory: at most 128 MiB, less under a lower limit
            // (never raised to Zstandard's 1 KiB minimum: refused below it).
            let log = zstd_window_log_max(mem_limit).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the decoder memory limit is below Zstandard's smallest window",
                )
            })?;
            let mut d = zstd::stream::read::Decoder::new(r)?;
            d.window_log_max(log)?;
            Box::new(d)
        }
        // Blocks of at most 4 MiB, nothing else to bound.
        Codec::Lz4 => Box::new(lz4_flex::frame::FrameDecoder::new(
            BufReader::with_capacity(256 * 1024, r),
        )),
        // Standard Brotli windows are at most 16 MiB (large windows are refused).
        Codec::Brotli => Box::new(brotli::Decompressor::new(r, 64 * 1024)),
    })
}

/// How a stream is compressed: the codec's level and its options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamLevel {
    /// gzip/bzip2 1-9, xz 0-9, Zstandard 1-22, Brotli 0-11; LZ4 ignores it.
    pub level: i32,
    /// xz: the extreme variant (`-9e`). LZ4: linked blocks (a better ratio).
    pub extreme: bool,
    /// Zstandard: long-distance matching with a 128 MiB window (`--long=27`).
    pub long: bool,
    /// Compressing threads (xz and Zstandard; 1 for the others).
    pub threads: u32,
}

/// Memory all xz compressing threads may use together.
const XZ_MT_BUDGET: u64 = 1536 << 20;

/// An encoder of a whole stream. [`Encoder::finish`] writes the stream's end; dropping it
/// without finishing leaves an incomplete stream.
pub(crate) enum Encoder<W: Write> {
    Plain(W),
    Gzip(flate2::write::GzEncoder<W>),
    Bzip2(bzip2::write::BzEncoder<W>),
    Xz(lzma_rust2::XzWriter<W>),
    XzMt(lzma_rust2::XzWriterMt<W>),
    Zstd(zstd::stream::write::Encoder<'static, W>),
    Lz4(lz4_flex::frame::FrameEncoder<W>),
    Brotli(Box<brotli::CompressorWriter<W>>),
}

/// The xz options of a level: presets 0-9, `extreme` like `xz -9e`.
pub(crate) fn xz_options(level: i32, extreme: bool) -> lzma_rust2::XzOptions {
    let mut o = lzma_rust2::XzOptions::with_preset(level.clamp(0, 9) as u32);
    if extreme {
        // xz's extreme presets search harder: the longest matches, a deeper match finder.
        o.lzma_options.nice_len = 273;
        o.lzma_options.depth_limit = 512;
    }
    o
}

/// Memory one xz compressing thread of `level` needs (bytes).
pub(crate) fn xz_thread_memory(level: i32, extreme: bool) -> u64 {
    let o = xz_options(level, extreme);
    // The encoder's own estimate, plus the block it buffers (three dictionaries).
    u64::from(o.lzma_options.get_memory_usage()) * 1024 + 3 * u64::from(o.lzma_options.dict_size)
}

/// The xz threads `wanted` threads may use within [`XZ_MT_BUDGET`].
pub(crate) fn xz_threads(level: i32, extreme: bool, wanted: u32) -> u32 {
    let per = xz_thread_memory(level, extreme).max(1);
    let fit = (XZ_MT_BUDGET / per).clamp(1, 64) as u32;
    wanted.clamp(1, fit)
}

/// Memory a Zstandard compressor of `level` needs, roughly (bytes; per thread).
pub(crate) fn zstd_memory(level: i32, long: bool) -> u64 {
    let window: u64 = if long {
        1 << ZSTD_WINDOW_LOG_MAX
    } else {
        match level {
            ..=3 => 1 << 21,
            4..=9 => 1 << 23,
            10..=19 => 1 << 23,
            _ => 1 << 27,
        }
    };
    // Window, hash and chain tables (more for the higher levels).
    match level {
        ..=3 => window * 3,
        4..=15 => window * 5,
        _ => window * 9,
    }
}

impl<W: Write> Encoder<W> {
    /// `gzip_name` goes in a gzip header (FNAME); nothing else stores a name.
    pub(crate) fn new(
        codec: Codec,
        l: StreamLevel,
        w: W,
        gzip_name: Option<&str>,
    ) -> io::Result<Self> {
        Ok(match codec {
            Codec::None => Encoder::Plain(w),
            Codec::Gzip => {
                // No name and no time in the header: the same input gives the same bytes.
                let level = flate2::Compression::new(l.level.clamp(1, 9) as u32);
                let mut b = flate2::GzBuilder::new().mtime(0);
                if let Some(name) = gzip_name {
                    b = b.filename(name);
                }
                Encoder::Gzip(b.write(w, level))
            }
            Codec::Bzip2 => Encoder::Bzip2(bzip2::write::BzEncoder::new(
                w,
                bzip2::Compression::new(l.level.clamp(1, 9) as u32),
            )),
            Codec::Xz => {
                let mut o = xz_options(l.level, l.extreme);
                let threads = xz_threads(l.level, l.extreme, l.threads);
                if threads > 1 {
                    // Blocks of three dictionaries, as `xz -T` makes them.
                    let block = (3 * u64::from(o.lzma_options.dict_size)).max(1 << 20);
                    o.set_block_size(NonZeroU64::new(block));
                    Encoder::XzMt(lzma_rust2::XzWriterMt::new(w, o, threads)?)
                } else {
                    Encoder::Xz(lzma_rust2::XzWriter::new(w, o)?)
                }
            }
            Codec::Zstd => {
                let mut e = zstd::stream::write::Encoder::new(w, l.level.clamp(1, 22))?;
                e.include_checksum(true)?;
                if l.long {
                    e.long_distance_matching(true)?;
                    e.window_log(ZSTD_WINDOW_LOG_MAX)?;
                }
                if l.threads > 1 {
                    e.multithread(l.threads)?;
                }
                Encoder::Zstd(e)
            }
            Codec::Lz4 => {
                let info = lz4_flex::frame::FrameInfo::new()
                    .content_checksum(true)
                    .block_size(lz4_flex::frame::BlockSize::Max4MB)
                    .block_mode(if l.extreme {
                        lz4_flex::frame::BlockMode::Linked
                    } else {
                        lz4_flex::frame::BlockMode::Independent
                    });
                Encoder::Lz4(lz4_flex::frame::FrameEncoder::with_frame_info(info, w))
            }
            Codec::Brotli => Encoder::Brotli(Box::new(brotli::CompressorWriter::new(
                w,
                1 << 20,
                l.level.clamp(0, 11) as u32,
                // 16 MiB window: the largest standard decoders accept.
                24,
            ))),
        })
    }

    /// Write the end of the stream and return the writer. Brotli doesn't report errors of
    /// its last write here: the writer must remember them (see `tar_writer::Out`).
    pub(crate) fn finish(self) -> io::Result<W> {
        match self {
            Encoder::Plain(w) => Ok(w),
            Encoder::Gzip(e) => e.finish(),
            Encoder::Bzip2(e) => e.finish(),
            Encoder::Xz(e) => e.finish(),
            Encoder::XzMt(e) => e.finish(),
            Encoder::Zstd(e) => e.finish(),
            Encoder::Lz4(e) => e.finish().map_err(io::Error::other),
            Encoder::Brotli(e) => Ok(e.into_inner()),
        }
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Encoder::Plain(w) => w.write(buf),
            Encoder::Gzip(e) => e.write(buf),
            Encoder::Bzip2(e) => e.write(buf),
            Encoder::Xz(e) => e.write(buf),
            Encoder::XzMt(e) => e.write(buf),
            Encoder::Zstd(e) => e.write(buf),
            Encoder::Lz4(e) => e.write(buf),
            Encoder::Brotli(e) => e.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Encoder::Plain(w) => w.flush(),
            Encoder::Gzip(e) => e.flush(),
            Encoder::Bzip2(e) => e.flush(),
            Encoder::Xz(e) => e.flush(),
            Encoder::XzMt(e) => e.flush(),
            Encoder::Zstd(e) => e.flush(),
            Encoder::Lz4(e) => e.flush(),
            Encoder::Brotli(e) => e.flush(),
        }
    }
}

/// The name a gzip header stores (FNAME), if any and if it is a plain file name: at most
/// 255 bytes, no path, decoded as UTF-8 or else Latin-1 (what the format says). Reads only
/// the first member's header from `head`.
pub(crate) fn gzip_name(head: &[u8]) -> Option<String> {
    const FEXTRA: u8 = 4;
    const FNAME: u8 = 8;
    if !head.starts_with(&[0x1f, 0x8b, 0x08]) || head.len() < 10 {
        return None;
    }
    let flags = head[3];
    if flags & FNAME == 0 {
        return None;
    }
    let mut pos = 10usize;
    if flags & FEXTRA != 0 {
        let len = u16::from_le_bytes([*head.get(10)?, *head.get(11)?]) as usize;
        pos = pos.checked_add(2)?.checked_add(len)?;
    }
    let rest = head.get(pos..)?;
    let end = rest.iter().take(256).position(|&b| b == 0)?;
    let raw = &rest[..end];
    let name = match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => raw.iter().map(|&b| char::from(b)).collect(),
    };
    // Only the last component of whatever path was stored, and only a valid name.
    let name = name.rsplit(['/', '\\']).next().unwrap_or("").to_owned();
    crate::paths::validate_file_name(&name).ok()?;
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(codec: Codec, l: StreamLevel, data: &[u8]) -> Vec<u8> {
        let mut e = Encoder::new(codec, l, Vec::new(), None).unwrap();
        e.write_all(data).unwrap();
        let out = e.finish().unwrap();
        if codec != Codec::None && codec != Codec::Brotli {
            assert_eq!(sniff(&out), Some(codec), "{codec:?}");
        }
        let mut back = Vec::new();
        decoder(codec, &out[..], 1 << 30)
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        back
    }

    #[test]
    fn every_codec_round_trips_and_is_recognised() {
        let data: Vec<u8> = (0..300_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        for codec in Codec::COMPRESSED {
            for (level, extreme, long, threads) in [
                (1, false, false, 1),
                (6, true, false, 2),
                (19, false, true, 2),
            ] {
                let l = StreamLevel {
                    level: if codec == Codec::Zstd {
                        level
                    } else {
                        level.min(9)
                    },
                    extreme,
                    long,
                    threads,
                };
                assert_eq!(roundtrip(codec, l, &data), data, "{codec:?} {l:?}");
            }
        }
    }

    #[test]
    fn names_say_codec_and_tar() {
        assert_eq!(by_name("a.tar.gz"), Some((Codec::Gzip, true)));
        assert_eq!(by_name("a.TGZ"), Some((Codec::Gzip, true)));
        assert_eq!(by_name("a.gz"), Some((Codec::Gzip, false)));
        assert_eq!(by_name("a.tar"), Some((Codec::None, true)));
        assert_eq!(by_name("a.tar.zst"), Some((Codec::Zstd, true)));
        assert_eq!(by_name("a.br"), Some((Codec::Brotli, false)));
        assert_eq!(by_name("a.txt"), None);
    }

    #[test]
    fn gzip_names_are_plain_file_names_or_nothing() {
        let header = |name: &[u8]| {
            let mut h = vec![0x1f, 0x8b, 8, 8, 0, 0, 0, 0, 0, 3];
            h.extend_from_slice(name);
            h.push(0);
            h
        };
        assert_eq!(
            gzip_name(&header(b"report.txt")).as_deref(),
            Some("report.txt")
        );
        assert_eq!(
            gzip_name(&header(b"../../x/evil.txt")).as_deref(),
            Some("evil.txt")
        );
        assert_eq!(
            gzip_name(&header(b"C:\\Windows\\x.dll")).as_deref(),
            Some("x.dll")
        );
        assert_eq!(gzip_name(&header(b"..")), None);
        assert_eq!(gzip_name(&header(b"CON")), None);
        assert_eq!(gzip_name(&header(b"a:b")), None);
        assert_eq!(gzip_name(&header(b"caf\xe9")).as_deref(), Some("café"));
        assert_eq!(gzip_name(&header(&[b'a'; 300])), None);
    }
}
