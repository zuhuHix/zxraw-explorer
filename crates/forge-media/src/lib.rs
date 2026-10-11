//! ZXraw Explorer media pipeline: still-image / RAW / video metadata, embedded RAW previews,
//! thumbnail generation with memory + disk caches, and culling analysis helpers.
//!
//! * [`image_meta`] — EXIF (kamadak-exif + own CR3/RAF/ORF/RW2 container handling) and XMP.
//! * [`raw_preview`] — largest/suitable embedded JPEG preview from RAW containers.
//! * [`xmp_write`] — merge ratings/labels/keywords into `.xmp` sidecars without touching other data.
//! * [`video`] — `ffprobe`/`ffmpeg` as external processes (timeouts, caps, no console).
//! * [`thumbs`] — [`ThumbnailService`] with bounded pools, generations and caches.
//! * [`analysis`] — sharpness, dHash/aHash, burst grouping.
//!
//! All parsers treat input as untrusted: reads are capped, offsets bounds-checked, loops
//! bounded; malformed input yields [`MediaError`], never a panic.

pub mod analysis;
mod bin_util;
pub mod error;
pub mod image_meta;
pub mod raw_preview;
pub mod thumbs;
pub mod video;
pub mod xmp_write;

pub use analysis::{
    BurstItem, Sharpness, ahash, dhash, group_bursts, hamming, hash_similarity, sharpness,
};
pub use error::{MediaError, Result};
pub use image_meta::{
    ExifTag, ImageFormat, ImageMetadata, XmpData, XmpSource, parse_xmp, read_image_metadata,
    read_orientation, read_xmp_sidecar,
};
pub use raw_preview::{PreviewPick, PreviewSource, RawPreview, extract_preview};
pub use thumbs::{
    DecodeLimits, RequestStatus, ThumbConfig, ThumbImage, ThumbRequest, ThumbResult, ThumbStats,
    ThumbnailService, cache_key, generate_thumbnail,
};
pub use video::{AudioStream, FfTools, FilmstripFrame, HdrKind, VideoMetadata, parse_ffprobe_json};
pub use xmp_write::{SidecarOutcome, SidecarUpdate, update_sidecar};
