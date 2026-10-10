//! Video metadata and frame extraction through external `ffprobe` / `ffmpeg` processes.
//!
//! Why external processes (see docs/ARCHITECTURE.md): no LGPL/GPL linking, crash isolation,
//! hard timeouts. Processes are launched with argument vectors (never a shell string),
//! without a console window on Windows, with stdout/stderr size caps and a hard timeout
//! after which the child is killed. When the tools are not installed every function returns
//! [`MediaError::ToolMissing`].

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use forge_jobs::CancelToken;
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{MediaError, Result};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const PROBE_OUTPUT_CAP: usize = 4 * 1024 * 1024;
const STDERR_CAP: usize = 64 * 1024;

/// Located FFmpeg executables plus process limits.
#[derive(Debug, Clone)]
pub struct FfTools {
    pub ffprobe: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub probe_timeout: Duration,
    pub frame_timeout: Duration,
}

impl Default for FfTools {
    fn default() -> Self {
        Self {
            ffprobe: None,
            ffmpeg: None,
            probe_timeout: Duration::from_secs(15),
            frame_timeout: Duration::from_secs(30),
        }
    }
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn find_in(dirs: &[PathBuf], base: &str) -> Option<PathBuf> {
    let name = exe_name(base);
    dirs.iter().map(|d| d.join(&name)).find(|p| p.is_file())
}

impl FfTools {
    /// Looks for the tools on `PATH`.
    pub fn discover() -> Self {
        Self::discover_with(None)
    }

    /// Looks in a configured location first (a directory, or the path of either
    /// executable), then on `PATH`.
    pub fn discover_with(configured: Option<&Path>) -> Self {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(c) = configured {
            if c.is_file() {
                dirs.extend(c.parent().map(Path::to_path_buf));
            } else {
                dirs.push(c.to_path_buf());
                dirs.push(c.join("bin"));
            }
        }
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        Self::discover_in(&dirs)
    }

    /// Looks only in the given directories (no PATH lookup).
    pub fn discover_in(dirs: &[PathBuf]) -> Self {
        Self {
            ffprobe: find_in(dirs, "ffprobe"),
            ffmpeg: find_in(dirs, "ffmpeg"),
            ..Default::default()
        }
    }

    /// Process-wide default discovered from PATH once.
    pub fn global() -> &'static FfTools {
        static T: OnceLock<FfTools> = OnceLock::new();
        T.get_or_init(FfTools::discover)
    }

    pub fn has_ffprobe(&self) -> bool {
        self.ffprobe.is_some()
    }
    pub fn has_ffmpeg(&self) -> bool {
        self.ffmpeg.is_some()
    }

    fn ffprobe(&self) -> Result<&Path> {
        self.ffprobe
            .as_deref()
            .ok_or(MediaError::ToolMissing("ffprobe"))
    }
    fn ffmpeg(&self) -> Result<&Path> {
        self.ffmpeg
            .as_deref()
            .ok_or(MediaError::ToolMissing("ffmpeg"))
    }

    /// Runs `ffprobe -show_format -show_streams` and parses the result.
    pub fn probe(&self, path: &Path) -> Result<VideoMetadata> {
        let exe = self.ffprobe()?;
        if !path.is_file() {
            return Err(MediaError::NotFound(path.to_path_buf()));
        }
        let args: Vec<OsString> = vec![
            "-v".into(),
            "error".into(),
            "-print_format".into(),
            "json".into(),
            "-show_format".into(),
            "-show_streams".into(),
            file_arg(path),
        ];
        let out = run_tool(
            "ffprobe",
            exe,
            &args,
            self.probe_timeout,
            PROBE_OUTPUT_CAP,
            None,
        )?;
        let s = String::from_utf8_lossy(&out);
        parse_ffprobe_json(&s)
    }

    /// Decodes a single frame at `at_seconds`, scaled to fit `max_dim` (never upscaled).
    pub fn extract_frame(&self, path: &Path, at_seconds: f64, max_dim: u32) -> Result<RgbaImage> {
        self.extract_frame_cancellable(path, at_seconds, max_dim, None)
    }

    pub fn extract_frame_cancellable(
        &self,
        path: &Path,
        at_seconds: f64,
        max_dim: u32,
        cancel: Option<&CancelToken>,
    ) -> Result<RgbaImage> {
        let exe = self.ffmpeg()?;
        if !path.is_file() {
            return Err(MediaError::NotFound(path.to_path_buf()));
        }
        let max_dim = max_dim.clamp(16, 4096);
        let t = if at_seconds.is_finite() && at_seconds > 0.0 {
            at_seconds
        } else {
            0.0
        };
        let vf = format!(
            "scale=w=min(iw\\,{max_dim}):h=min(ih\\,{max_dim}):force_original_aspect_ratio=decrease:flags=bilinear"
        );
        let args: Vec<OsString> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
            "-ss".into(),
            format!("{t:.3}").into(),
            "-i".into(),
            file_arg(path),
            "-map".into(),
            "0:v:0".into(),
            "-frames:v".into(),
            "1".into(),
            "-an".into(),
            "-sn".into(),
            "-dn".into(),
            "-vf".into(),
            vf.into(),
            "-pix_fmt".into(),
            "bgr24".into(),
            "-c:v".into(),
            "bmp".into(),
            "-f".into(),
            "image2pipe".into(),
            "-".into(),
        ];
        let cap = (max_dim as usize * max_dim as usize * 3) + 1024 * 1024;
        let out = run_tool("ffmpeg", exe, &args, self.frame_timeout, cap, cancel)?;
        if out.is_empty() {
            return Err(MediaError::Decode(format!("no video frame at {t:.2}s")));
        }
        let img = image::load_from_memory_with_format(&out, image::ImageFormat::Bmp)?;
        Ok(img.to_rgba8())
    }

    /// Picks a "representative" frame: samples `samples` frames between 10 % and 90 % of
    /// the duration and returns the one with the highest luminance variance that is not
    /// near-black or near-white (fades, slates, lens caps).
    pub fn representative_frame(
        &self,
        path: &Path,
        max_dim: u32,
        samples: usize,
        cancel: Option<&CancelToken>,
    ) -> Result<RgbaImage> {
        self.ffmpeg()?;
        let duration = self
            .probe(path)
            .ok()
            .and_then(|m| m.duration_s)
            .filter(|d| *d > 0.0);
        let times: Vec<f64> = match duration {
            Some(d) => {
                let n = samples.clamp(1, 15);
                (0..n)
                    .map(|i| {
                        let f = if n == 1 {
                            0.5
                        } else {
                            0.1 + 0.8 * i as f64 / (n - 1) as f64
                        };
                        d * f
                    })
                    .collect()
            }
            None => vec![1.0, 0.0],
        };
        let mut best: Option<(f64, RgbaImage)> = None;
        let mut last_err = None;
        for t in times {
            if let Some(c) = cancel {
                c.check()?;
            }
            match self.extract_frame_cancellable(path, t, max_dim, cancel) {
                Ok(img) => {
                    let s = frame_score(&img);
                    if best.as_ref().is_none_or(|(b, _)| s > *b) {
                        best = Some((s, img));
                    }
                }
                Err(e @ (MediaError::Cancelled | MediaError::ToolMissing(_))) => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        best.map(|(_, i)| i)
            .ok_or_else(|| last_err.unwrap_or(MediaError::Decode("no frames".into())))
    }

    /// `n` frames evenly spaced across the video (frame i at (i + 0.5) / n of the duration).
    pub fn filmstrip(
        &self,
        path: &Path,
        n: usize,
        max_dim: u32,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<FilmstripFrame>> {
        self.ffmpeg()?;
        let meta = self.probe(path)?;
        let d = meta
            .duration_s
            .filter(|d| *d > 0.0)
            .ok_or_else(|| MediaError::Decode("unknown duration".into()))?;
        let n = n.clamp(1, 64);
        let mut out = Vec::with_capacity(n);
        let mut last_err = None;
        for i in 0..n {
            if let Some(c) = cancel {
                c.check()?;
            }
            let t = d * (i as f64 + 0.5) / n as f64;
            match self.extract_frame_cancellable(path, t, max_dim, cancel) {
                Ok(image) => out.push(FilmstripFrame {
                    at_seconds: t,
                    image,
                }),
                Err(e @ MediaError::Cancelled) => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        if out.is_empty() {
            return Err(last_err.unwrap_or(MediaError::Decode("no frames".into())));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct FilmstripFrame {
    pub at_seconds: f64,
    pub image: RgbaImage,
}

/// Convenience wrappers using [`FfTools::global`].
pub fn probe(path: &Path) -> Result<VideoMetadata> {
    FfTools::global().probe(path)
}
pub fn extract_frame(path: &Path, at_seconds: f64, max_dim: u32) -> Result<RgbaImage> {
    FfTools::global().extract_frame(path, at_seconds, max_dim)
}
pub fn representative_frame(path: &Path, max_dim: u32) -> Result<RgbaImage> {
    FfTools::global().representative_frame(path, max_dim, 5, None)
}
pub fn filmstrip(path: &Path, n: usize, max_dim: u32) -> Result<Vec<FilmstripFrame>> {
    FfTools::global().filmstrip(path, n, max_dim, None)
}

/// `file:` prefix forces the file protocol so a crafted name can never select another
/// FFmpeg protocol or be read as an option.
fn file_arg(path: &Path) -> OsString {
    let mut s = OsString::from("file:");
    s.push(path.as_os_str());
    s
}

/// Luminance statistics → score. Near-black/near-white frames are heavily penalised.
pub(crate) fn frame_score(img: &RgbaImage) -> f64 {
    let (mean, var) = luma_stats(img);
    if !(20.0..=235.0).contains(&mean) {
        var * 0.01
    } else {
        var
    }
}

pub(crate) fn luma_stats(img: &RgbaImage) -> (f64, f64) {
    let n = (img.width() as u64 * img.height() as u64).max(1) as f64;
    let mut sum = 0f64;
    let mut sq = 0f64;
    for p in img.pixels() {
        let y = 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
        sum += y;
        sq += y * y;
    }
    let mean = sum / n;
    (mean, (sq / n - mean * mean).max(0.0))
}

// ---------------------------------------------------------------------------
// Process runner
// ---------------------------------------------------------------------------

/// Runs an external tool with an argument vector, a hard timeout (child killed) and an
/// output cap. Returns stdout.
pub(crate) fn run_tool(
    tool: &'static str,
    exe: &Path,
    args: &[OsString],
    timeout: Duration,
    stdout_cap: usize,
    cancel: Option<&CancelToken>,
) -> Result<Vec<u8>> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = CREATE_NO_WINDOW;
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => MediaError::ToolMissing(tool),
        _ => MediaError::io(exe, e),
    })?;

    let over = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let over2 = over.clone();
    let out_t = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() + n > stdout_cap {
                            over2.store(true, Ordering::Release);
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        buf
    });
    let err_t = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            let mut chunk = [0u8; 8 * 1024];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = STDERR_CAP.saturating_sub(buf.len());
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        if over.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_t.join();
            let _ = err_t.join();
            return Err(MediaError::OutputTooLarge {
                what: tool,
                limit: stdout_cap,
            });
        }
        if cancel.is_some_and(|c| c.is_cancelled()) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_t.join();
            let _ = err_t.join();
            return Err(MediaError::Cancelled);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_t.join();
            let _ = err_t.join();
            return Err(MediaError::Timeout {
                tool,
                secs: timeout.as_secs_f64(),
            });
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                return Err(MediaError::io(exe, e));
            }
        }
    };
    let out = out_t.join().unwrap_or_default();
    let err = err_t.join().unwrap_or_default();
    if over.load(Ordering::Acquire) {
        return Err(MediaError::OutputTooLarge {
            what: tool,
            limit: stdout_cap,
        });
    }
    if !status.success() {
        let s = String::from_utf8_lossy(&err);
        let s = s.trim();
        let tail: String = s
            .char_indices()
            .rev()
            .nth(500)
            .map(|(i, _)| s[i..].to_string())
            .unwrap_or_else(|| s.to_string());
        return Err(MediaError::ToolFailed {
            tool,
            status: status.to_string(),
            stderr: tail,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ffprobe JSON
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HdrKind {
    /// SMPTE ST 2084 (HDR10 / HDR10+ / Dolby Vision base layers).
    Pq,
    /// ARIB STD-B67 Hybrid Log-Gamma.
    Hlg,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioStream {
    pub codec: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
    pub bitrate: Option<u64>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VideoMetadata {
    pub container: Option<String>,
    pub duration_s: Option<f64>,
    /// Display dimensions (rotation applied: portrait phone video reports portrait).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Dimensions as encoded.
    pub coded_width: Option<u32>,
    pub coded_height: Option<u32>,
    /// Clockwise display rotation in degrees (0, 90, 180, 270).
    pub rotation: u32,
    /// Best frame-rate estimate (average rate for VFR, nominal otherwise).
    pub fps: Option<f64>,
    pub r_frame_rate: Option<f64>,
    pub avg_frame_rate: Option<f64>,
    /// Variable frame rate suspected (`r_frame_rate` and `avg_frame_rate` differ > 1 %).
    pub vfr: bool,
    pub frame_count: Option<u64>,
    pub codec: Option<String>,
    pub codec_long: Option<String>,
    pub profile: Option<String>,
    pub pix_fmt: Option<String>,
    pub bit_depth: Option<u32>,
    /// "4:2:0", "4:2:2", "4:4:4", "mono", …
    pub chroma_subsampling: Option<String>,
    /// Overall bitrate (bits/s).
    pub bitrate: Option<u64>,
    pub video_bitrate: Option<u64>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_space: Option<String>,
    pub color_range: Option<String>,
    pub hdr: Option<HdrKind>,
    pub dolby_vision: bool,
    pub timecode: Option<String>,
    pub creation_time: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub audio: Vec<AudioStream>,
    pub has_video: bool,
}

fn s_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty() && t != "unknown" && t != "N/A").then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
fn f_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}
fn u_of(v: &Value) -> Option<u64> {
    f_of(v).filter(|x| *x >= 0.0 && *x < 1e18).map(|x| x as u64)
}

/// "30000/1001" → 29.97; "0/0" → None.
pub fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let n: f64 = n.trim().parse().ok()?;
    let d: f64 = d.trim().parse().ok()?;
    if d == 0.0 || n <= 0.0 {
        return None;
    }
    let r = n / d;
    (r.is_finite() && r < 10_000.0).then_some(r)
}

/// pix_fmt → (bits per component, chroma subsampling).
pub fn pix_fmt_info(p: &str) -> (Option<u32>, Option<&'static str>) {
    let p = p.trim().to_ascii_lowercase();
    let base = p
        .strip_suffix("le")
        .or_else(|| p.strip_suffix("be"))
        .unwrap_or(&p);
    let trailing_bits = |s: &str, default: u32| -> u32 {
        let digits: String = s
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        digits
            .parse()
            .ok()
            .filter(|b| (1..=32).contains(b))
            .unwrap_or(default)
    };
    let chroma_of = |s: &str| -> Option<&'static str> {
        if s.contains("420") {
            Some("4:2:0")
        } else if s.contains("422") {
            Some("4:2:2")
        } else if s.contains("444") {
            Some("4:4:4")
        } else if s.contains("440") {
            Some("4:4:0")
        } else if s.contains("411") {
            Some("4:1:1")
        } else if s.contains("410") {
            Some("4:1:0")
        } else {
            None
        }
    };
    if let Some(rest) = base
        .strip_prefix("yuvj")
        .or_else(|| base.strip_prefix("yuva"))
        .or_else(|| base.strip_prefix("yuv"))
    {
        let chroma = chroma_of(rest);
        // e.g. "420p10" → after 'p'
        let bits = rest
            .split_once('p')
            .map(|(_, b)| trailing_bits(b, 8))
            .unwrap_or(8);
        return (Some(bits), chroma);
    }
    if base.starts_with("gbr") {
        return (Some(trailing_bits(base, 8)), Some("4:4:4"));
    }
    if base.starts_with("gray") || base.starts_with("ya") {
        return (Some(trailing_bits(base, 8)), Some("mono"));
    }
    match base {
        "nv12" | "nv21" => return (Some(8), Some("4:2:0")),
        "nv16" => return (Some(8), Some("4:2:2")),
        "nv20" => return (Some(10), Some("4:2:2")),
        "nv24" | "nv42" => return (Some(8), Some("4:4:4")),
        "v210" | "y210" => return (Some(10), Some("4:2:2")),
        "yuyv422" | "uyvy422" | "yvyu422" => return (Some(8), Some("4:2:2")),
        "rgb48" | "bgr48" | "rgba64" | "bgra64" => return (Some(16), Some("4:4:4")),
        "rgb24" | "bgr24" | "rgba" | "bgra" | "argb" | "abgr" | "rgb0" | "bgr0" | "0rgb"
        | "0bgr" => return (Some(8), Some("4:4:4")),
        "x2rgb10" | "x2bgr10" => return (Some(10), Some("4:4:4")),
        _ => {}
    }
    if let Some(rest) = base.strip_prefix('p')
        && rest.len() == 3
        && rest.bytes().all(|b| b.is_ascii_digit())
    {
        let chroma = match &rest[..1] {
            "0" => Some("4:2:0"),
            "2" => Some("4:2:2"),
            "4" => Some("4:4:4"),
            _ => None,
        };
        let bits: u32 = rest[1..].parse().unwrap_or(0);
        return ((bits > 0).then_some(bits), chroma);
    }
    (None, None)
}

fn tag_ci(tags: Option<&Value>, keys: &[&str]) -> Option<String> {
    let obj = tags?.as_object()?;
    for k in keys {
        if let Some((_, v)) = obj.iter().find(|(kk, _)| kk.eq_ignore_ascii_case(k))
            && let Some(s) = s_of(v)
        {
            return Some(s);
        }
    }
    None
}

/// Parses `ffprobe -print_format json -show_format -show_streams` output.
pub fn parse_ffprobe_json(json: &str) -> Result<VideoMetadata> {
    let root: Value = serde_json::from_str(json)
        .map_err(|e| MediaError::corrupt(format!("ffprobe JSON: {e}")))?;
    let fmt = root.get("format");
    let streams: &[Value] = root
        .get("streams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let ftags = fmt.and_then(|f| f.get("tags"));

    let mut m = VideoMetadata {
        container: fmt.and_then(|f| f.get("format_name")).and_then(s_of),
        duration_s: fmt
            .and_then(|f| f.get("duration"))
            .and_then(f_of)
            .filter(|d| *d >= 0.0),
        bitrate: fmt.and_then(|f| f.get("bit_rate")).and_then(u_of),
        creation_time: tag_ci(
            ftags,
            &["creation_time", "com.apple.quicktime.creationdate"],
        ),
        timecode: tag_ci(ftags, &["timecode"]),
        make: tag_ci(
            ftags,
            &[
                "com.apple.quicktime.make",
                "make",
                "com.android.manufacturer",
            ],
        ),
        model: tag_ci(
            ftags,
            &["com.apple.quicktime.model", "model", "com.android.model"],
        ),
        ..Default::default()
    };

    let is_attached_pic = |s: &Value| {
        s.get("disposition")
            .and_then(|d| d.get("attached_pic"))
            .and_then(Value::as_i64)
            == Some(1)
    };

    let video = streams.iter().find(|s| {
        s.get("codec_type").and_then(Value::as_str) == Some("video") && !is_attached_pic(s)
    });
    if let Some(v) = video {
        m.has_video = true;
        let g = |k: &str| v.get(k).and_then(s_of);
        m.codec = g("codec_name");
        m.codec_long = g("codec_long_name");
        m.profile = g("profile");
        m.pix_fmt = g("pix_fmt");
        m.color_primaries = g("color_primaries");
        m.color_transfer = g("color_transfer");
        m.color_space = g("color_space");
        m.color_range = g("color_range");
        m.video_bitrate = v.get("bit_rate").and_then(u_of);
        m.frame_count = v.get("nb_frames").and_then(u_of).filter(|n| *n > 0);
        m.coded_width = v
            .get("width")
            .and_then(u_of)
            .map(|x| x as u32)
            .filter(|x| *x > 0);
        m.coded_height = v
            .get("height")
            .and_then(u_of)
            .map(|x| x as u32)
            .filter(|x| *x > 0);
        if m.duration_s.is_none() {
            m.duration_s = v.get("duration").and_then(f_of).filter(|d| *d >= 0.0);
        }
        if let Some(pf) = &m.pix_fmt {
            let (b, c) = pix_fmt_info(pf);
            m.bit_depth = b;
            m.chroma_subsampling = c.map(str::to_string);
        }
        if m.bit_depth.is_none() {
            m.bit_depth = v
                .get("bits_per_raw_sample")
                .and_then(u_of)
                .map(|b| b as u32)
                .filter(|b| (1..=32).contains(b));
        }

        // Rotation: display matrix side data (counter-clockwise degrees) or legacy tag
        // (clockwise degrees).
        let mut rot_cw: Option<f64> = None;
        if let Some(sd) = v.get("side_data_list").and_then(Value::as_array) {
            for d in sd {
                if let Some(r) = d.get("rotation").and_then(f_of) {
                    rot_cw = Some(-r);
                }
                let t = d
                    .get("side_data_type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if t.contains("DOVI") || t.contains("Dolby Vision") {
                    m.dolby_vision = true;
                }
            }
        }
        let vtags = v.get("tags");
        if rot_cw.is_none() {
            rot_cw = tag_ci(vtags, &["rotate"]).and_then(|s| s.parse::<f64>().ok());
        }
        if let Some(r) = rot_cw.filter(|r| r.is_finite()) {
            let q = ((r / 90.0).round() as i64).rem_euclid(4);
            m.rotation = (q * 90) as u32;
        }
        let swap = m.rotation % 180 == 90;
        (m.width, m.height) = if swap {
            (m.coded_height, m.coded_width)
        } else {
            (m.coded_width, m.coded_height)
        };

        m.r_frame_rate = v
            .get("r_frame_rate")
            .and_then(Value::as_str)
            .and_then(parse_rate);
        m.avg_frame_rate = v
            .get("avg_frame_rate")
            .and_then(Value::as_str)
            .and_then(parse_rate);
        m.vfr = match (m.r_frame_rate, m.avg_frame_rate) {
            (Some(r), Some(a)) => (r - a).abs() / r.max(a) > 0.01,
            _ => false,
        };
        m.fps = if m.vfr {
            m.avg_frame_rate
        } else {
            m.r_frame_rate.or(m.avg_frame_rate)
        };

        m.hdr = match m.color_transfer.as_deref() {
            Some("smpte2084") => Some(HdrKind::Pq),
            Some("arib-std-b67") => Some(HdrKind::Hlg),
            _ => None,
        };
        if m.timecode.is_none() {
            m.timecode = tag_ci(vtags, &["timecode"]);
        }
        if m.creation_time.is_none() {
            m.creation_time = tag_ci(vtags, &["creation_time"]);
        }
    }
    if m.timecode.is_none() {
        m.timecode = streams
            .iter()
            .find_map(|s| tag_ci(s.get("tags"), &["timecode"]));
    }

    for a in streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio"))
        .take(64)
    {
        m.audio.push(AudioStream {
            codec: a.get("codec_name").and_then(s_of),
            channels: a.get("channels").and_then(u_of).map(|c| c as u32),
            channel_layout: a.get("channel_layout").and_then(s_of),
            sample_rate: a.get("sample_rate").and_then(u_of).map(|c| c as u32),
            bitrate: a.get("bit_rate").and_then(u_of),
            language: tag_ci(a.get("tags"), &["language"]).filter(|l| l != "und"),
        });
    }
    Ok(m)
}

/// Format seconds as `H:MM:SS` / `M:SS`.
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let t = secs.round() as u64;
    let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn sys(exe: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root).join("System32").join(exe)
    }
    fn a(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn timeout_kills_child() {
        let t0 = Instant::now();
        let r = run_tool(
            "ffmpeg",
            &sys("PING.EXE"),
            &a(&["-n", "30", "127.0.0.1"]),
            Duration::from_millis(300),
            1 << 20,
            None,
        );
        assert!(matches!(r, Err(MediaError::Timeout { .. })), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_cap_enforced() {
        let r = run_tool(
            "ffprobe",
            &sys("cmd.exe"),
            &a(&["/C", "dir", "/s", r"C:\Windows\System32"]),
            Duration::from_secs(20),
            1000,
            None,
        );
        assert!(matches!(r, Err(MediaError::OutputTooLarge { .. })), "{r:?}");
    }

    #[test]
    fn cancel_and_failure_and_missing() {
        let c = CancelToken::new();
        c.cancel();
        let r = run_tool(
            "ffmpeg",
            &sys("PING.EXE"),
            &a(&["-n", "30", "127.0.0.1"]),
            Duration::from_secs(20),
            1 << 20,
            Some(&c),
        );
        assert!(matches!(r, Err(MediaError::Cancelled)), "{r:?}");
        let r = run_tool(
            "ffmpeg",
            &sys("cmd.exe"),
            &a(&["/C", "echo oops 1>&2 & exit 3"]),
            Duration::from_secs(10),
            1 << 20,
            None,
        );
        match r {
            Err(MediaError::ToolFailed { stderr, .. }) => assert!(stderr.contains("oops")),
            other => panic!("{other:?}"),
        }
        let r = run_tool(
            "ffprobe",
            Path::new(r"C:\definitely\not\here\ffprobe.exe"),
            &[],
            Duration::from_secs(1),
            1,
            None,
        );
        assert!(
            matches!(r, Err(MediaError::ToolMissing("ffprobe"))),
            "{r:?}"
        );
        let ok = run_tool(
            "ffmpeg",
            &sys("cmd.exe"),
            &a(&["/C", "echo hi"]),
            Duration::from_secs(10),
            100,
            None,
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&ok).trim(), "hi");
    }
}
