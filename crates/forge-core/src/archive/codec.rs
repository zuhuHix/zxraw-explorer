//! Whole-file compression streams: gzip, bzip2, xz, Zstandard, LZ4 frames and Brotli. They
//! wrap a tar (`.tar.gz`) or hold one file (`.gz`). Detection by signature (Brotli has none:
//! by name only), decoders with bounded memory, and encoders for the presets.
//!
//! Every decoder handles concatenated streams (gzip members, bzip2 streams, xz streams,
//! Zstandard and LZ4 frames) and checks the checksums the stream carries as it reads: gzip
//! CRC-32, bzip2 block CRCs, the xz check, the Zstandard and LZ4 content checksums when the
//! frame has one. A checksum is only known to match once the stream was read to its end.

use std::io::{self, BufReader, Read, Write};
use std::num::NonZeroU64;

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

/// Whether the stream starting with `head` carries a checksum of its data (for xz,
/// Zstandard and LZ4 a flag of the first stream or frame says).
pub(crate) fn stream_has_checksum(codec: Codec, head: &[u8]) -> bool {
    match codec {
        Codec::Gzip | Codec::Bzip2 => true,
        // Stream flags: the second byte's low nibble is the check type (0 = none).
        Codec::Xz => head.get(7).is_some_and(|f| f & 0x0f != 0),
        // Frame header descriptor bit 2: Content_Checksum_flag.
        Codec::Zstd => head.get(4).is_some_and(|d| d & 0x04 != 0),
        // FLG bit 2: content checksum (legacy frames have none).
        Codec::Lz4 => {
            head.starts_with(&[0x04, 0x22, 0x4d, 0x18])
                && head.get(4).is_some_and(|f| f & 0x04 != 0)
        }
        Codec::None | Codec::Brotli => false,
    }
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
            let mut d = zstd::stream::read::Decoder::new(r)?;
            // The window is the decoder's memory: at most 128 MiB, less under a lower limit.
            let mut log = ZSTD_WINDOW_LOG_MAX;
            while log > 20 && (1u64 << log) > mem_limit {
                log -= 1;
            }
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
