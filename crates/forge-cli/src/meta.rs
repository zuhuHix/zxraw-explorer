//! `zxraw-cli meta`: image (EXIF/XMP) or video (ffprobe) metadata for one file.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;
use forge_core::model::{MediaKind, ext_of, media_kind_for_ext};
use forge_media::video::{FfTools, format_duration};
use forge_media::{ImageMetadata, VideoMetadata};

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS, EXIT_USAGE};

#[derive(Args)]
pub struct MetaArgs {
    /// Image (JPEG, PNG, TIFF, WebP, GIF, BMP, RAW) or video file.
    pub file: PathBuf,
    /// Print the metadata as JSON.
    #[arg(long)]
    pub json: bool,
    /// Path to ffprobe (or its folder). Defaults to searching PATH.
    #[arg(long, value_name = "PATH")]
    pub ffprobe: Option<PathBuf>,
}

pub fn run(args: MetaArgs) -> Result<u8> {
    util::require_exists(&args.file)?;
    if !args.file.is_file() {
        bail!("{} is not a file", args.file.display());
    }
    let ext = ext_of(&args.file);
    match media_kind_for_ext(&ext) {
        MediaKind::Image | MediaKind::Raw => {
            let meta = match forge_media::read_image_metadata(&args.file) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("could not read image metadata: {e}");
                    return Ok(EXIT_PROBLEMS);
                }
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                print_image(&meta);
            }
            Ok(EXIT_OK)
        }
        MediaKind::Video => {
            let tools = FfTools::discover_with(args.ffprobe.as_deref());
            if !tools.has_ffprobe() {
                eprintln!(
                    "ffprobe is not installed, so video metadata is unavailable.\n\
                     Install FFmpeg and add it to PATH, or pass --ffprobe <path>."
                );
                return Ok(EXIT_USAGE);
            }
            let meta = match tools.probe(&args.file) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("ffprobe failed: {e}");
                    return Ok(EXIT_PROBLEMS);
                }
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                print_video(&meta);
            }
            Ok(EXIT_OK)
        }
        _ => {
            eprintln!("no metadata reader for .{ext} files");
            Ok(EXIT_USAGE)
        }
    }
}

fn opt<T: std::fmt::Display>(label: &str, v: Option<T>) {
    if let Some(v) = v {
        println!("{label:<14} {v}");
    }
}

fn print_image(m: &ImageMetadata) {
    opt("format", m.format.map(|f| format!("{f:?}")));
    if let (Some(w), Some(h)) = (m.width, m.height) {
        opt("dimensions", Some(format!("{w} x {h}")));
    }
    opt("camera", camera(m.make.as_deref(), m.model.as_deref()));
    opt("lens", m.lens.as_deref());
    opt("focal length", m.focal_length_mm.map(|f| format!("{f} mm")));
    opt("aperture", m.aperture.map(|a| format!("f/{a}")));
    opt("shutter", m.shutter.as_deref());
    opt("iso", m.iso);
    opt("taken", m.date_taken.as_deref());
    opt("orientation", m.orientation);
    opt("color space", m.color_space.as_deref());
    opt("bit depth", m.bit_depth);
    if let (Some(lat), Some(lon)) = (m.gps_lat, m.gps_lon) {
        opt("gps", Some(format!("{lat:.6}, {lon:.6}")));
    }
    if let Some(x) = &m.xmp {
        opt("xmp rating", x.rating);
        opt("xmp label", x.label.as_deref());
        if !x.keywords.is_empty() {
            opt("keywords", Some(x.keywords.join(", ")));
        }
    }
}

fn print_video(m: &VideoMetadata) {
    opt("container", m.container.as_deref());
    opt("duration", m.duration_s.map(format_duration));
    if let (Some(w), Some(h)) = (m.width, m.height) {
        opt("dimensions", Some(format!("{w} x {h}")));
    }
    opt("codec", m.codec.as_deref());
    opt("fps", m.fps.map(|f| format!("{f:.3}")));
    opt("pixel format", m.pix_fmt.as_deref());
    opt("bit depth", m.bit_depth);
    opt("bitrate", m.bitrate.map(|b| format!("{} kb/s", b / 1000)));
    opt("hdr", m.hdr.map(|h| format!("{h:?}")));
    opt("created", m.creation_time.as_deref());
    opt("camera", camera(m.make.as_deref(), m.model.as_deref()));
    for a in &m.audio {
        println!(
            "{:<14} {} {} ch {} Hz",
            "audio",
            a.codec.as_deref().unwrap_or("?"),
            a.channels
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            a.sample_rate
                .map(|r| r.to_string())
                .unwrap_or_else(|| "?".into())
        );
    }
}

fn camera(make: Option<&str>, model: Option<&str>) -> Option<String> {
    let s = model.or(make)?.trim();
    (!s.is_empty()).then(|| s.to_string())
}
