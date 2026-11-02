//! What a new or updated archive is written with: compression presets, methods, encryption
//! and the list of formats that are already compressed (stored as they are).

use serde::{Deserialize, Serialize};

use super::{ArchiveFormat, Password};

/// The choices of the compress dialog. For ZIP the first three write standard Deflate, which
/// every unzip tool (and Windows Explorer) opens; the advanced ones trade compatibility for
/// size. For 7z every preset is LZMA2 (see [`Preset::sevenzip`]); Ultra is the advanced one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preset {
    /// Deflate level 1.
    Fast,
    /// Deflate level 6.
    #[default]
    Balanced,
    /// Zopfli for files up to [`ZOPFLI_MAX_FILE`], Deflate level 9 beyond. Slow.
    Max,
    /// Zstandard (method 93), level 9. Advanced: needs 7-Zip 23+ or another recent tool.
    Zstd,
    /// Zstandard (method 93), level 19. Advanced and slow.
    Ultra,
}

/// Zopfli takes about a second per MiB; larger files in the Max preset use Deflate 9.
pub const ZOPFLI_MAX_FILE: u64 = 16 * 1024 * 1024;

impl Preset {
    pub const ALL: [Preset; 5] = [
        Preset::Fast,
        Preset::Balanced,
        Preset::Max,
        Preset::Zstd,
        Preset::Ultra,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "Fast",
            Preset::Balanced => "Balanced",
            Preset::Max => "Max (slow)",
            Preset::Zstd => "Zstandard",
            Preset::Ultra => "Ultra (Zstandard 19, slow)",
        }
    }

    /// Whether the preset belongs in the dialog's Advanced section.
    pub fn is_advanced(self) -> bool {
        matches!(self, Preset::Zstd | Preset::Ultra)
    }

    /// The presets offered for a format (7z has no Zstandard preset: 7-Zip can't read it).
    pub fn for_format(format: ArchiveFormat) -> &'static [Preset] {
        match format {
            ArchiveFormat::Zip => &Preset::ALL,
            ArchiveFormat::SevenZip => &[Preset::Fast, Preset::Balanced, Preset::Max, Preset::Ultra],
        }
    }

    /// The label for a format's dialog.
    pub fn label_for(self, format: ArchiveFormat) -> &'static str {
        match (format, self) {
            (ArchiveFormat::SevenZip, Preset::Ultra) => "Ultra (LZMA2, 256 MB dictionary)",
            (ArchiveFormat::SevenZip, Preset::Max) => "Max (slow)",
            _ => self.label(),
        }
    }

    /// Readers that can open the result, for a format's dialog.
    pub fn compatibility_for(self, format: ArchiveFormat) -> &'static str {
        match format {
            ArchiveFormat::Zip => self.compatibility(),
            ArchiveFormat::SevenZip => {
                "LZMA2 7z: opens in 7-Zip, NanaZip, WinRAR and Windows 11 Explorer (23H2+)."
            }
        }
    }

    /// A rough time and memory estimate for a format's dialog.
    pub fn estimate_for(self, format: ArchiveFormat) -> String {
        match format {
            ArchiveFormat::Zip => self.estimate().to_owned(),
            ArchiveFormat::SevenZip => {
                let info = self.sevenzip();
                let speed = match self.for_sevenzip() {
                    Preset::Fast => "Fast (~20 MB/s per core)",
                    Preset::Balanced => "Medium (~4 MB/s per core)",
                    Preset::Max => "Slow (~2 MB/s per core)",
                    _ => "Very slow (~1.5 MB/s per core)",
                };
                format!(
                    "{speed}; compressing needs ~{} MB per thread, opening ~{} MB",
                    info.compress_memory.div_ceil(1 << 20),
                    info.decompress_memory.div_ceil(1 << 20)
                )
            }
        }
    }

    /// The 7z preset this one stands for (Zstandard has none; it means Balanced).
    pub fn for_sevenzip(self) -> Preset {
        match self {
            Preset::Zstd => Preset::Balanced,
            p => p,
        }
    }

    /// LZMA2 settings and memory of the preset for 7z.
    pub fn sevenzip(self) -> SevenZipPresetInfo {
        let (level, dictionary): (u32, u32) = match self.for_sevenzip() {
            Preset::Fast => (3, 4 << 20),
            Preset::Balanced => (6, 16 << 20),
            Preset::Max => (9, 64 << 20),
            _ => (9, 256 << 20),
        };
        let mut lzma = lzma_rust2::LzmaOptions::with_preset(level);
        lzma.dict_size = dictionary;
        SevenZipPresetInfo {
            level,
            dictionary,
            compress_memory: u64::from(lzma.get_memory_usage()) * 1024,
            decompress_memory: u64::from(lzma_rust2::lzma2_get_memory_usage(dictionary)) * 1024,
            // Big enough for a good ratio, small enough that opening one file decodes at
            // most this much of its block.
            solid_block: (u64::from(dictionary) * 16).clamp(64 << 20, 2 << 30),
        }
    }

    /// Readers that can open the result, for the dialog.
    pub fn compatibility(self) -> &'static str {
        if self.is_advanced() {
            "Needs 7-Zip 23+ or another recent tool; Windows Explorer may not open it."
        } else {
            "Standard Deflate: opens in Windows Explorer and every unzip tool."
        }
    }

    /// A rough time and memory estimate for the dialog (per worker thread).
    pub fn estimate(self) -> &'static str {
        match self {
            Preset::Fast => "Very fast (~200 MB/s per core), little memory",
            Preset::Balanced => "Fast (~60 MB/s per core), little memory",
            Preset::Max => "Very slow (~1 MB/s per core for files up to 16 MB), ~100 MB memory",
            Preset::Zstd => "Fast (~80 MB/s per core), ~40 MB memory per core",
            Preset::Ultra => "Slow (~3 MB/s per core), ~250 MB memory per core",
        }
    }

    /// Method and level used for a file of `size` bytes.
    pub fn method_for(self, size: u64) -> (Method, Option<i64>) {
        match self {
            Preset::Fast => (Method::Deflate, Some(1)),
            Preset::Balanced => (Method::Deflate, Some(6)),
            Preset::Max if size <= ZOPFLI_MAX_FILE => (Method::Deflate, Some(24)),
            Preset::Max => (Method::Deflate, Some(9)),
            Preset::Zstd => (Method::Zstd, Some(9)),
            Preset::Ultra => (Method::Zstd, Some(19)),
        }
    }
}

/// LZMA2 settings of a preset for 7z, with the memory they need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SevenZipPresetInfo {
    /// LZMA2 level, 0-9.
    pub level: u32,
    /// Dictionary size in bytes.
    pub dictionary: u32,
    /// Memory one compressing thread needs.
    pub compress_memory: u64,
    /// Memory needed to open (decompress) the archive.
    pub decompress_memory: u64,
    /// Default largest solid block (bytes of input).
    pub solid_block: u64,
}

/// Compression methods that can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Method {
    Stored,
    /// Levels 1-9 use zlib-rs; 10-264 use Zopfli.
    Deflate,
    /// Method 93.
    Zstd,
    Bzip2,
    Xz,
}

impl Method {
    pub(crate) fn zip(self) -> zip::CompressionMethod {
        match self {
            Method::Stored => zip::CompressionMethod::Stored,
            Method::Deflate => zip::CompressionMethod::Deflated,
            Method::Zstd => zip::CompressionMethod::Zstd,
            Method::Bzip2 => zip::CompressionMethod::Bzip2,
            Method::Xz => zip::CompressionMethod::Xz,
        }
    }
}

/// How new entries are encrypted.
#[derive(Debug, Clone, Default)]
pub enum Encryption {
    #[default]
    None,
    /// AES-256 (WinZip AE-2): the default for new encrypted archives.
    Aes256(Password),
    /// The original ZIP encryption: legacy compatibility only, it is weak.
    ZipCryptoLegacy(Password),
}

impl Encryption {
    pub fn is_none(&self) -> bool {
        matches!(self, Encryption::None)
    }

    pub fn password(&self) -> Option<&Password> {
        match self {
            Encryption::None => None,
            Encryption::Aes256(p) | Encryption::ZipCryptoLegacy(p) => Some(p),
        }
    }
}

/// Options for creating or updating an archive.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub preset: Preset,
    /// Overrides the preset's method and level for every compressed file (API and tests).
    pub method: Option<(Method, Option<i64>)>,
    /// Store files whose format is already compressed (see [`is_precompressed`]).
    pub auto_store: bool,
    pub encryption: Encryption,
    /// Worker threads compressing small files in parallel (`0`: one per core, at most 8).
    /// For 7z, the LZMA2 threads of a block (fewer when their memory would be too much).
    pub threads: usize,
    /// 7z: compress files together in solid blocks (smaller; opening one file decodes its
    /// block up to it). Already-compressed files always get blocks of their own.
    pub solid: bool,
    /// 7z: the largest solid block, in bytes of input (`None`: the preset's default).
    pub solid_block_size: Option<u64>,
    /// 7z with a password: encrypt the file names too, so listing needs the password.
    pub encrypt_names: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            preset: Preset::Balanced,
            method: None,
            auto_store: true,
            encryption: Encryption::None,
            threads: 0,
            solid: true,
            solid_block_size: None,
            encrypt_names: true,
        }
    }
}

impl CreateOptions {
    pub(crate) fn method_for(&self, ext: &str, size: u64) -> (Method, Option<i64>) {
        if self.auto_store && is_precompressed(ext) {
            return (Method::Stored, None);
        }
        self.method.unwrap_or_else(|| self.preset.method_for(size))
    }

    pub(crate) fn worker_threads(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism()
                .map_or(2, |n| n.get())
                .clamp(1, 8),
            n => n.clamp(1, 32),
        }
    }
}

/// Formats that are already compressed: storing them is faster and barely larger.
const PRECOMPRESSED: &[&str] = &[
    // images
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "webp", "heic", "heif", "avif", "jxl", "cr3",
    // video
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "3gp", "mts", "m2ts", "insv",
    // audio
    "mp3", "aac", "m4a", "ogg", "oga", "opus", "flac", "wma",
    // archives and compressed streams
    "zip", "7z", "rar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "tzst", "lz4", "br", "lzma",
    "cab", "zipx", "apk", "jar", // documents that are zip containers
    "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub",
];

/// Whether a file with this (lowercase, dot-less) extension is already compressed.
pub fn is_precompressed(ext: &str) -> bool {
    PRECOMPRESSED.contains(&ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_map_to_standard_deflate_unless_advanced() {
        assert_eq!(Preset::Fast.method_for(10), (Method::Deflate, Some(1)));
        assert_eq!(Preset::Balanced.method_for(10), (Method::Deflate, Some(6)));
        assert_eq!(Preset::Max.method_for(10), (Method::Deflate, Some(24)));
        assert_eq!(
            Preset::Max.method_for(ZOPFLI_MAX_FILE + 1),
            (Method::Deflate, Some(9))
        );
        assert_eq!(Preset::Ultra.method_for(10).0, Method::Zstd);
        assert!(Preset::Zstd.is_advanced() && !Preset::Max.is_advanced());
        assert_eq!(Preset::default(), Preset::Balanced);
    }

    #[test]
    fn sevenzip_presets_grow_and_state_their_memory() {
        let presets = Preset::for_format(ArchiveFormat::SevenZip);
        assert!(!presets.contains(&Preset::Zstd));
        let infos: Vec<_> = presets.iter().map(|p| p.sevenzip()).collect();
        for w in infos.windows(2) {
            assert!(w[0].dictionary < w[1].dictionary);
            assert!(w[0].compress_memory < w[1].compress_memory);
        }
        let balanced = Preset::Balanced.sevenzip();
        assert_eq!(balanced.dictionary, 16 << 20);
        assert!(balanced.decompress_memory >= 16 << 20 && balanced.decompress_memory < 20 << 20);
        assert!(Preset::Max.sevenzip().compress_memory < 1 << 30);
        assert!(Preset::Ultra.is_advanced());
        assert_eq!(Preset::Zstd.for_sevenzip(), Preset::Balanced);
        assert!(Preset::Fast.estimate_for(ArchiveFormat::SevenZip).contains("MB"));
    }

    #[test]
    fn precompressed_files_are_stored_unless_turned_off() {
        let mut o = CreateOptions::default();
        assert_eq!(o.method_for("jpg", 10).0, Method::Stored);
        assert_eq!(o.method_for("txt", 10).0, Method::Deflate);
        o.auto_store = false;
        assert_eq!(o.method_for("jpg", 10).0, Method::Deflate);
        for ext in [
            "png", "heic", "webp", "mp4", "mkv", "mov", "mp3", "aac", "flac", "zip",
        ] {
            assert!(is_precompressed(ext), "{ext}");
        }
        for ext in ["7z", "rar", "gz", "xz", "zst"] {
            assert!(is_precompressed(ext), "{ext}");
        }
    }
}
