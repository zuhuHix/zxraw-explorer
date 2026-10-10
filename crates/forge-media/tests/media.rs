mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::*;
use forge_jobs::Priority;
use forge_media::raw_preview::{PreviewSource, extract_preview_from_bytes};
use forge_media::thumbs::DecodeLimits;
use forge_media::*;
use image::DynamicImage;

fn write(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    p
}

fn no_ff() -> FfTools {
    let empty = tempfile::tempdir().unwrap();
    FfTools::discover_in(&[empty.path().to_path_buf()])
}

// ---------------------------------------------------------------------------
// EXIF / XMP
// ---------------------------------------------------------------------------

#[test]
fn jpeg_exif_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let thumb = jpeg(160, 120, 1);
    let data = with_exif(&jpeg(640, 480, 1), &exif_tiff(6, Some(&thumb)));
    let p = write(dir.path(), "a.jpg", &data);
    let m = read_image_metadata(&p).unwrap();
    assert_eq!(m.format, Some(ImageFormat::Jpeg));
    assert_eq!((m.width, m.height), (Some(640), Some(480)));
    assert_eq!(m.display_dimensions(), Some((480, 640)));
    assert_eq!(m.make.as_deref(), Some("Canon"));
    assert_eq!(m.model.as_deref(), Some("Canon EOS R6"));
    assert_eq!(m.lens.as_deref(), Some("RF 35mm F1.8 MACRO IS STM"));
    assert_eq!(m.focal_length_mm, Some(35.0));
    assert_eq!(m.aperture, Some(2.8));
    assert_eq!(m.shutter.as_deref(), Some("1/250"));
    assert_eq!(m.iso, Some(400));
    assert_eq!(m.exposure_compensation, Some(-0.67));
    assert_eq!(
        m.date_taken.as_deref(),
        Some("2024-05-01T12:34:56.12+02:00")
    );
    assert_eq!(m.capture_timestamp, Some(1_714_559_696));
    assert_eq!(m.orientation, Some(6));
    assert_eq!(m.color_space.as_deref(), Some("sRGB"));
    assert_eq!(m.bit_depth, Some(8));
    let lat = m.gps_lat.unwrap();
    let lon = m.gps_lon.unwrap();
    assert!((lat - 38.71).abs() < 1e-6, "{lat}");
    assert!((lon + (9.0 + 8.0 / 60.0)).abs() < 1e-6, "{lon}");
    assert!(
        m.exif_tags
            .iter()
            .any(|t| t.name == "Make" && t.value.contains("Canon"))
    );
    assert!(m.exif_tags.iter().any(|t| t.name == "ExposureTime"));
    assert!(m.xmp.is_none());
}

#[test]
fn xmp_embedded_and_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let xmp = r#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF>
      <rdf:Description xmp:Rating="3" xmp:Label="Green">
      <dc:subject><rdf:Bag><rdf:li>wedding</rdf:li><rdf:li>ceremony</rdf:li></rdf:Bag></dc:subject>
      </rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;
    let p = write(dir.path(), "x.jpg", &with_xmp(&jpeg(64, 64, 2), xmp));
    let m = read_image_metadata(&p).unwrap();
    let x = m.xmp.unwrap();
    assert_eq!(x.rating, Some(3));
    assert_eq!(x.label.as_deref(), Some("Green"));
    assert_eq!(x.keywords, vec!["wedding", "ceremony"]);
    assert_eq!(x.source, XmpSource::Embedded);

    // Sidecar overrides the rating, embedded keywords survive the merge.
    std::fs::write(
        dir.path().join("x.xmp"),
        "<x:xmpmeta><rdf:Description xmp:Rating='5'/></x:xmpmeta>",
    )
    .unwrap();
    let m = read_image_metadata(&p).unwrap();
    let x = m.xmp.unwrap();
    assert_eq!(x.rating, Some(5));
    assert_eq!(x.source, XmpSource::Sidecar);
    assert_eq!(x.keywords, vec!["wedding", "ceremony"]);
}

#[test]
fn png_metadata_dimensions_and_depth() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "a.png", &encode_png(&pattern(300, 200, 3)));
    let m = read_image_metadata(&p).unwrap();
    assert_eq!(m.format, Some(ImageFormat::Png));
    assert_eq!(
        (m.width, m.height, m.bit_depth),
        (Some(300), Some(200), Some(8))
    );
}

// ---------------------------------------------------------------------------
// RAW
// ---------------------------------------------------------------------------

#[test]
fn tiff_raw_previews_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let big = jpeg(1200, 800, 4);
    let small = jpeg(160, 120, 4);
    let nef = tiff_raw(42, 6, &big, &small);

    let p = extract_preview_from_bytes(&nef, PreviewPick::Largest).unwrap();
    assert_eq!((p.width, p.height, p.orientation), (1200, 800, 6));
    assert_eq!(p.source, PreviewSource::TiffJpegInterchange);
    assert_eq!(p.jpeg, big);
    let p = extract_preview_from_bytes(&nef, PreviewPick::AtLeast(128)).unwrap();
    assert_eq!((p.width, p.height), (160, 120));
    let p = extract_preview_from_bytes(&nef, PreviewPick::AtLeast(5000)).unwrap();
    assert_eq!((p.width, p.height), (1200, 800));

    let path = write(dir.path(), "DSC_0001.NEF", &nef);
    let m = read_image_metadata(&path).unwrap();
    assert_eq!(m.format, Some(ImageFormat::TiffRaw));
    assert_eq!(m.make.as_deref(), Some("NIKON CORPORATION"));
    assert_eq!(m.shutter.as_deref(), Some("1/250"));
    assert_eq!(m.iso, Some(400));
    assert_eq!((m.width, m.height), (Some(6000), Some(4000)));
    assert_eq!(m.bit_depth, Some(14));
    assert_eq!(m.orientation, Some(6));

    // Thumbnail from RAW: preview + orientation 6 → portrait.
    let t = generate_thumbnail(&path, 256, &DecodeLimits::default(), None, 5, None).unwrap();
    assert_eq!((t.width, t.height), (171, 256));
}

#[test]
fn orf_magic_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let orf = tiff_raw(0x4F52, 1, &jpeg(800, 600, 5), &jpeg(160, 120, 5));
    let p = extract_preview_from_bytes(&orf, PreviewPick::Largest).unwrap();
    assert_eq!((p.width, p.height), (800, 600));
    let path = write(dir.path(), "P1.ORF", &orf);
    let m = read_image_metadata(&path).unwrap();
    assert_eq!(m.iso, Some(400));
    assert_eq!(m.model.as_deref(), Some("NIKON Z 6_2"));
}

#[test]
fn scan_fallback_finds_maker_note_style_preview() {
    // TIFF RAW with no IFD-referenced preview; JPEG just sits in the file (ORF/PEF style).
    let mut t = TiffBuilder::new(42);
    let big = jpeg(1024, 683, 6);
    t.blob(&[0u8; 100]);
    t.blob(&big);
    let ifd0 = t.ifd(vec![ascii(0x010F, "PENTAX"), short(0x0112, 1)], 0);
    t.set_first_ifd(ifd0);
    let p = extract_preview_from_bytes(&t.buf, PreviewPick::Largest).unwrap();
    assert_eq!(p.source, PreviewSource::Scan);
    assert_eq!((p.width, p.height), (1024, 683));
    assert_eq!(p.jpeg, big);
}

#[test]
fn cr3_previews_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let thumb = jpeg(160, 120, 7);
    let prvw = jpeg(1620, 1080, 7);
    let full = jpeg(2400, 1600, 7);
    let cr3 = cr3(8, &thumb, &prvw, &full);

    let p = extract_preview_from_bytes(&cr3, PreviewPick::Largest).unwrap();
    assert_eq!(p.source, PreviewSource::Cr3Full);
    assert_eq!((p.width, p.height, p.orientation), (2400, 1600, 8));
    assert_eq!(p.jpeg, full);
    let p = extract_preview_from_bytes(&cr3, PreviewPick::AtLeast(256)).unwrap();
    assert_eq!(p.source, PreviewSource::Cr3Prvw);
    let p = extract_preview_from_bytes(&cr3, PreviewPick::AtLeast(100)).unwrap();
    assert_eq!(p.source, PreviewSource::Cr3Thumb);

    let path = write(dir.path(), "IMG_0001.CR3", &cr3);
    let m = read_image_metadata(&path).unwrap();
    assert_eq!(m.format, Some(ImageFormat::Cr3));
    assert_eq!(m.model.as_deref(), Some("Canon EOS R5"));
    assert_eq!(m.orientation, Some(8));
    assert_eq!(m.shutter.as_deref(), Some("1/250"));
    assert_eq!(m.aperture, Some(2.8));
    assert_eq!(
        m.date_taken.as_deref(),
        Some("2024-05-01T12:34:56.12+02:00")
    );
    assert!(m.gps_lat.is_some());
    assert_eq!((m.width, m.height), (Some(2400), Some(1600)));
}

#[test]
fn raf_preview_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = TiffBuilder::new(42);
    let ex = t.ifd(exif_entries(), 0);
    let i0 = t.ifd(
        vec![
            ascii(0x010F, "FUJIFILM"),
            ascii(0x0110, "X-T5"),
            short(0x0112, 3),
            long(0x8769, ex),
        ],
        0,
    );
    t.set_first_ifd(i0);
    let j = with_exif(&jpeg(900, 600, 8), &t.buf);
    let raf = raf(&j);
    let p = extract_preview_from_bytes(&raf, PreviewPick::Largest).unwrap();
    assert_eq!((p.width, p.height, p.orientation), (900, 600, 3));
    assert_eq!(p.source, PreviewSource::RafHeader);
    let path = write(dir.path(), "DSCF0001.RAF", &raf);
    let m = read_image_metadata(&path).unwrap();
    assert_eq!(m.make.as_deref(), Some("FUJIFILM"));
    assert_eq!(m.iso, Some(400));
}

#[test]
fn lossless_jpeg_strip_never_chosen() {
    // Only a SOF3 strip available → no preview at all.
    let mut t = TiffBuilder::new(42);
    let ll = fake_lossless_jpeg();
    let o = t.blob(&ll);
    let i0 = t.ifd(
        vec![
            short(0x0103, 7),
            long(0x0111, o),
            long(0x0117, ll.len() as u32),
        ],
        0,
    );
    t.set_first_ifd(i0);
    assert!(extract_preview_from_bytes(&t.buf, PreviewPick::Largest).is_err());
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

#[test]
fn corrupt_and_truncated_inputs_never_panic() {
    let dir = tempfile::tempdir().unwrap();
    let jpg = with_exif(&jpeg(320, 240, 9), &exif_tiff(6, Some(&jpeg(160, 120, 9))));
    let nef = tiff_raw(42, 6, &jpeg(640, 480, 9), &jpeg(160, 120, 9));
    let cr3 = cr3(
        1,
        &jpeg(160, 120, 9),
        &jpeg(400, 300, 9),
        &jpeg(800, 600, 9),
    );
    let raf = raf(&jpeg(400, 300, 9));
    let limits = DecodeLimits::default();

    for (name, data) in [
        ("a.jpg", &jpg),
        ("a.nef", &nef),
        ("a.cr3", &cr3),
        ("a.raf", &raf),
    ] {
        // Truncations
        for cut in [
            0usize,
            1,
            2,
            7,
            8,
            16,
            64,
            200,
            data.len() / 3,
            data.len() / 2,
            data.len() - 1,
        ] {
            let d = &data[..cut.min(data.len())];
            let _ = extract_preview_from_bytes(d, PreviewPick::Largest);
            let p = write(dir.path(), name, d);
            let _ = read_image_metadata(&p);
            let _ = generate_thumbnail(&p, 128, &limits, None, 1, None);
        }
        // Random mutations (in memory: cheap)
        for seed in 0..300u64 {
            let d = mutate(data, seed, 1 + (seed as usize % 16));
            let _ = extract_preview_from_bytes(&d, PreviewPick::AtLeast(100));
        }
        // A few mutated files through the full metadata + thumbnail path.
        for seed in 0..25u64 {
            let d = mutate(data, seed * 7919, 4);
            let p = write(dir.path(), name, &d);
            let _ = read_image_metadata(&p);
            let _ = generate_thumbnail(&p, 96, &limits, None, 1, None);
        }
    }

    // Header-only JPEG and plain garbage must be errors.
    let p = write(dir.path(), "trunc.jpg", &jpg[..300]);
    assert!(generate_thumbnail(&p, 128, &limits, None, 1, None).is_err());
    let p = write(dir.path(), "garbage.jpg", b"this is not an image at all");
    assert!(generate_thumbnail(&p, 128, &limits, None, 1, None).is_err());
    let p = write(dir.path(), "garbage.nef", &[0x42; 4096]);
    assert!(generate_thumbnail(&p, 128, &limits, None, 1, None).is_err());
    assert!(read_image_metadata(&dir.path().join("missing.jpg")).is_err());
    let p = write(dir.path(), "empty.png", b"");
    assert!(generate_thumbnail(&p, 128, &limits, None, 1, None).is_err());
}

#[test]
fn huge_declared_dimensions_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let limits = DecodeLimits::default();
    let p = write(dir.path(), "bomb.png", &png_declaring(100_000, 100_000));
    let e = generate_thumbnail(&p, 128, &limits, None, 1, None).unwrap_err();
    assert!(matches!(e, MediaError::TooLarge { .. }), "{e:?}");

    // JPEG whose SOF claims 60000×60000.
    let mut j = jpeg(64, 64, 1);
    let sof = j.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    j[sof + 5..sof + 7].copy_from_slice(&60_000u16.to_be_bytes());
    j[sof + 7..sof + 9].copy_from_slice(&60_000u16.to_be_bytes());
    let p = write(dir.path(), "bomb.jpg", &j);
    let e = generate_thumbnail(&p, 128, &limits, None, 1, None).unwrap_err();
    assert!(matches!(e, MediaError::TooLarge { .. }), "{e:?}");

    // Custom lower limit applies to ordinary files too.
    let small = DecodeLimits {
        max_input_pixels: 1000,
        ..Default::default()
    };
    let p = write(dir.path(), "ok.png", &encode_png(&pattern(100, 100, 1)));
    assert!(matches!(
        generate_thumbnail(&p, 64, &small, None, 1, None),
        Err(MediaError::TooLarge { .. })
    ));
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

#[test]
fn jpeg_thumbnail_orientation_and_exif_thumb() {
    let dir = tempfile::tempdir().unwrap();
    let limits = DecodeLimits::default();
    // EXIF thumbnail is solid red; main image is the pattern. Requests ≤ 160 use the thumb.
    let red = encode_jpeg(
        &image::RgbImage::from_pixel(160, 120, image::Rgb([255, 0, 0])),
        90,
    );
    let data = with_exif(&jpeg(1600, 1200, 1), &exif_tiff(6, Some(&red)));
    let p = write(dir.path(), "o.jpg", &data);
    let t = generate_thumbnail(&p, 128, &limits, None, 1, None).unwrap();
    assert_eq!((t.width, t.height), (96, 128)); // rotated 90°
    let px = &t.rgba[(t.rgba.len() / 2) & !3..][..4];
    assert!(
        px[0] > 200 && px[1] < 60,
        "expected EXIF thumb (red), got {px:?}"
    );
    // Larger request decodes the main image.
    let t = generate_thumbnail(&p, 400, &limits, None, 1, None).unwrap();
    assert_eq!((t.width, t.height), (300, 400));
}

#[test]
fn other_formats_thumbnail() {
    let dir = tempfile::tempdir().unwrap();
    let limits = DecodeLimits::default();
    let img = DynamicImage::ImageRgb8(pattern(500, 250, 2));
    for ext in ["png", "tiff", "bmp", "gif", "webp"] {
        let p = dir.path().join(format!("i.{ext}"));
        img.save(&p).unwrap();
        let t = generate_thumbnail(&p, 100, &limits, None, 1, None).unwrap();
        assert_eq!((t.width, t.height), (100, 50), "{ext}");
    }
    let p = write(dir.path(), "x.heic", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic");
    assert!(matches!(
        generate_thumbnail(&p, 100, &limits, None, 1, None),
        Err(MediaError::Unsupported(_))
    ));
}

fn service(cache: Option<&Path>, threads: usize) -> ThumbnailService {
    ThumbnailService::new(ThumbConfig {
        cache_dir: cache.map(Path::to_path_buf),
        image_threads: threads,
        ff_tools: Some(no_ff()),
        ..Default::default()
    })
}

fn recv(s: &ThumbnailService) -> ThumbResult {
    s.results()
        .recv_timeout(Duration::from_secs(30))
        .expect("thumbnail result")
}

#[test]
fn cache_hit_miss_and_mtime_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let p = write(dir.path(), "c.jpg", &jpeg(800, 600, 3));
    let md = std::fs::metadata(&p).unwrap();
    let req = || ThumbRequest::new(&p, 128).stat(md.modified().ok(), md.len());

    let s = service(Some(&cache), 2);
    assert_eq!(s.request(req()), RequestStatus::Queued);
    let r = recv(&s);
    let img = r.image.unwrap();
    assert_eq!((img.width, img.height), (128, 96));
    assert_eq!(s.stats().generated, 1);
    // Memory hit is synchronous.
    assert!(matches!(s.request(req()), RequestStatus::MemoryHit(_)));
    assert!(
        s.get_cached(&p, 128, md.modified().ok(), md.len())
            .is_some()
    );
    drop(s);

    // Fresh service: disk hit, no decode.
    let s = service(Some(&cache), 2);
    assert_eq!(s.request(req()), RequestStatus::Queued);
    let r = recv(&s);
    let disk = r.image.unwrap();
    assert_eq!((disk.width, disk.height), (img.width, img.height));
    let mae: f64 = disk
        .rgba
        .iter()
        .zip(&img.rgba)
        .map(|(a, b)| (*a as f64 - *b as f64).abs())
        .sum::<f64>()
        / img.rgba.len() as f64;
    assert!(mae < 6.0, "cached JPEG differs too much: {mae}");
    let st = s.stats();
    assert_eq!((st.disk_hits, st.generated), (1, 0));

    // File changes (new mtime) → miss → regenerated.
    std::fs::write(&p, jpeg(800, 600, 4)).unwrap();
    let f = std::fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    drop(f);
    assert_eq!(s.request(ThumbRequest::new(&p, 128)), RequestStatus::Queued);
    let r = recv(&s);
    assert!(r.image.is_ok());
    assert_eq!(s.stats().generated, 1);

    // Corrupt cache file is discarded and regenerated.
    let s2 = service(Some(&cache), 1);
    for sub in std::fs::read_dir(&cache).unwrap() {
        for f in std::fs::read_dir(sub.unwrap().path()).unwrap() {
            std::fs::write(f.unwrap().path(), b"junk").unwrap();
        }
    }
    s2.request(ThumbRequest::new(&p, 128));
    assert!(recv(&s2).image.is_ok());
    assert_eq!(s2.stats().generated, 1);
    let (_, used) = s2.prune_disk_cache(0).unwrap();
    assert_eq!(used, 0);
}

#[test]
fn cancellation_drops_queued_requests() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<PathBuf> = (0..40)
        .map(|i| {
            write(
                dir.path(),
                &format!("f{i}.png"),
                &encode_png(&pattern(1200, 900, i)),
            )
        })
        .collect();
    let s = service(None, 1);
    for f in &files {
        let st = s.request(ThumbRequest::new(f, 64).generation(1));
        assert_eq!(st, RequestStatus::Queued);
    }
    // Navigate away.
    s.cancel_stale(2);
    assert_eq!(
        s.request(ThumbRequest::new(&files[0], 64).generation(1)),
        RequestStatus::Stale
    );
    let target = write(dir.path(), "new.png", &encode_png(&pattern(100, 100, 99)));
    assert_eq!(
        s.request(ThumbRequest::new(&target, 64).generation(2)),
        RequestStatus::Queued
    );
    let mut stale = 0;
    loop {
        let r = recv(&s);
        if r.generation == 1 {
            stale += 1;
            continue;
        }
        assert_eq!(r.path, target);
        assert!(r.image.is_ok());
        break;
    }
    // At most the one task that was already running when we navigated.
    assert!(stale <= 1, "stale results delivered: {stale}");
    assert!(s.stats().cancelled >= 38);
    let t0 = std::time::Instant::now();
    while !s.is_idle() && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(s.in_flight(), 0);
}

#[test]
fn dedupe_and_priority_and_bad_files() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = write(dir.path(), "big.png", &encode_png(&pattern(2000, 1500, 1)));
    let a = write(dir.path(), "a.png", &encode_png(&pattern(300, 200, 2)));
    let bad = write(dir.path(), "bad.jpg", b"\xFF\xD8\xFF\xE0garbage");
    let s = service(None, 1);
    s.request(ThumbRequest::new(&blocker, 64));
    assert_eq!(s.request(ThumbRequest::new(&a, 64)), RequestStatus::Queued);
    assert_eq!(
        s.request(ThumbRequest::new(&a, 64).priority(Priority::High)),
        RequestStatus::AlreadyInFlight
    );
    s.request(ThumbRequest::new(&bad, 64));
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(recv(&s));
    }
    assert!(
        s.results()
            .recv_timeout(Duration::from_millis(300))
            .is_err(),
        "duplicate result"
    );
    let bad_r = got.iter().find(|r| r.path == bad).unwrap();
    assert!(bad_r.image.is_err());
    assert!(got.iter().find(|r| r.path == a).unwrap().image.is_ok());
    // Worker survived the bad file.
    let b = write(dir.path(), "b.png", &encode_png(&pattern(50, 50, 3)));
    s.request(ThumbRequest::new(&b, 64));
    assert!(recv(&s).image.is_ok());
    assert_eq!(s.stats().errors, 1);
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

#[test]
fn video_functions_report_tool_missing() {
    let dir = tempfile::tempdir().unwrap();
    let v = write(dir.path(), "clip.mp4", b"\0\0\0\x18ftypisom");
    let ff = no_ff();
    assert!(!ff.has_ffmpeg() && !ff.has_ffprobe());
    assert!(matches!(
        ff.probe(&v),
        Err(MediaError::ToolMissing("ffprobe"))
    ));
    assert!(matches!(
        ff.extract_frame(&v, 1.0, 256),
        Err(MediaError::ToolMissing("ffmpeg"))
    ));
    assert!(matches!(
        ff.representative_frame(&v, 256, 5, None),
        Err(MediaError::ToolMissing("ffmpeg"))
    ));
    assert!(matches!(
        ff.filmstrip(&v, 8, 128, None),
        Err(MediaError::ToolMissing(_))
    ));
    let e = generate_thumbnail(&v, 128, &DecodeLimits::default(), Some(&ff), 5, None).unwrap_err();
    assert!(e.is_tool_missing());

    // Through the service as well.
    let s = service(None, 1);
    s.request(ThumbRequest::new(&v, 128));
    assert!(matches!(recv(&s).image, Err(MediaError::ToolMissing(_))));

    // Global (PATH) discovery: ffmpeg is not installed on the dev machine.
    if !FfTools::global().has_ffprobe() {
        assert!(matches!(video::probe(&v), Err(MediaError::ToolMissing(_))));
        assert!(matches!(
            video::representative_frame(&v, 64),
            Err(MediaError::ToolMissing(_))
        ));
    }
}

const PORTRAIT_IPHONE: &str = r#"{
 "streams": [
  {"index":0,"codec_name":"hevc","codec_long_name":"H.265 / HEVC","profile":"Main 10","codec_type":"video",
   "width":1920,"height":1080,"pix_fmt":"yuv420p10le","color_range":"tv","color_space":"bt2020nc",
   "color_transfer":"arib-std-b67","color_primaries":"bt2020","r_frame_rate":"30/1","avg_frame_rate":"30/1",
   "duration":"12.345000","bit_rate":"9000000","nb_frames":"370",
   "disposition":{"attached_pic":0},
   "tags":{"creation_time":"2024-05-01T10:00:00.000000Z"},
   "side_data_list":[{"side_data_type":"Display Matrix","displaymatrix":"...","rotation":-90}]},
  {"index":1,"codec_name":"aac","codec_type":"audio","sample_rate":"44100","channels":2,"channel_layout":"stereo","bit_rate":"128000","tags":{"language":"und"}},
  {"index":2,"codec_type":"data","tags":{"timecode":"01:00:00:00"}}
 ],
 "format": {"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"12.345000","bit_rate":"9200000",
  "tags":{"com.apple.quicktime.make":"Apple","com.apple.quicktime.model":"iPhone 15 Pro","creation_time":"2024-05-01T10:00:00.000000Z"}}
}"#;

#[test]
fn ffprobe_portrait_hlg() {
    let m = parse_ffprobe_json(PORTRAIT_IPHONE).unwrap();
    assert!(m.has_video);
    assert_eq!(m.rotation, 90);
    assert_eq!((m.width, m.height), (Some(1080), Some(1920)));
    assert_eq!((m.coded_width, m.coded_height), (Some(1920), Some(1080)));
    assert_eq!(m.codec.as_deref(), Some("hevc"));
    assert_eq!(m.profile.as_deref(), Some("Main 10"));
    assert_eq!(m.bit_depth, Some(10));
    assert_eq!(m.chroma_subsampling.as_deref(), Some("4:2:0"));
    assert_eq!(m.hdr, Some(HdrKind::Hlg));
    assert_eq!(m.fps, Some(30.0));
    assert!(!m.vfr);
    assert_eq!(m.duration_s, Some(12.345));
    assert_eq!(m.bitrate, Some(9_200_000));
    assert_eq!(m.frame_count, Some(370));
    assert_eq!(m.make.as_deref(), Some("Apple"));
    assert_eq!(m.model.as_deref(), Some("iPhone 15 Pro"));
    assert_eq!(m.timecode.as_deref(), Some("01:00:00:00"));
    assert_eq!(
        m.creation_time.as_deref(),
        Some("2024-05-01T10:00:00.000000Z")
    );
    assert_eq!(m.audio.len(), 1);
    let a = &m.audio[0];
    assert_eq!((a.channels, a.sample_rate), (Some(2), Some(44100)));
    assert_eq!(a.codec.as_deref(), Some("aac"));
    assert_eq!(a.language, None);
}

#[test]
fn ffprobe_vfr_pq_and_legacy_rotate() {
    let j = r#"{"streams":[{"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,
        "pix_fmt":"yuv422p12le","color_transfer":"smpte2084","color_primaries":"bt2020",
        "r_frame_rate":"60/1","avg_frame_rate":"2850/100","tags":{"rotate":"270","timecode":"10:11:12:13"},
        "side_data_list":[{"side_data_type":"DOVI configuration record"}]},
        {"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1},"width":320,"height":240}],
        "format":{"duration":"N/A","tags":{"make":"DJI"}}}"#;
    let m = parse_ffprobe_json(j).unwrap();
    assert!(m.vfr);
    assert_eq!(m.fps, Some(28.5));
    assert_eq!(m.r_frame_rate, Some(60.0));
    assert_eq!(m.hdr, Some(HdrKind::Pq));
    assert!(m.dolby_vision);
    assert_eq!(m.rotation, 270);
    assert_eq!((m.width, m.height), (Some(2160), Some(3840)));
    assert_eq!(m.bit_depth, Some(12));
    assert_eq!(m.chroma_subsampling.as_deref(), Some("4:2:2"));
    assert_eq!(m.duration_s, None);
    assert_eq!(m.timecode.as_deref(), Some("10:11:12:13"));
    assert_eq!(m.make.as_deref(), Some("DJI"));
    assert_eq!(m.codec.as_deref(), Some("hevc"));

    // NTSC rate is not VFR.
    let j = r#"{"streams":[{"codec_type":"video","r_frame_rate":"30000/1001","avg_frame_rate":"29970/1000","pix_fmt":"nv12"}]}"#;
    let m = parse_ffprobe_json(j).unwrap();
    assert!(!m.vfr);
    assert!((m.fps.unwrap() - 29.97).abs() < 0.01);
    assert_eq!(m.bit_depth, Some(8));

    // Degenerate inputs.
    assert!(parse_ffprobe_json("not json").is_err());
    let m = parse_ffprobe_json("{}").unwrap();
    assert!(!m.has_video);
    let m = parse_ffprobe_json(r#"{"streams":[{"codec_type":"video","width":"x","r_frame_rate":"0/0","side_data_list":[{"rotation":"NaN"}]}]}"#).unwrap();
    assert_eq!((m.width, m.fps, m.rotation), (None, None, 0));
}

#[test]
fn pix_fmt_table() {
    use forge_media::video::pix_fmt_info;
    assert_eq!(pix_fmt_info("yuv420p"), (Some(8), Some("4:2:0")));
    assert_eq!(pix_fmt_info("yuvj422p"), (Some(8), Some("4:2:2")));
    assert_eq!(pix_fmt_info("yuv444p16le"), (Some(16), Some("4:4:4")));
    assert_eq!(pix_fmt_info("p010le"), (Some(10), Some("4:2:0")));
    assert_eq!(pix_fmt_info("p210le"), (Some(10), Some("4:2:2")));
    assert_eq!(pix_fmt_info("gbrp10le"), (Some(10), Some("4:4:4")));
    assert_eq!(pix_fmt_info("rgb24"), (Some(8), Some("4:4:4")));
    assert_eq!(pix_fmt_info("gray"), (Some(8), Some("mono")));
    assert_eq!(pix_fmt_info("weird"), (None, None));
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

#[test]
fn dhash_matches_resized_copies() {
    let base = DynamicImage::ImageRgb8(pattern(800, 600, 1));
    let small = base.resize_exact(400, 300, image::imageops::FilterType::Lanczos3);
    let big = base.resize_exact(1600, 1200, image::imageops::FilterType::Triangle);
    let recompressed = image::load_from_memory(&encode_jpeg(&base.to_rgb8(), 60)).unwrap();
    let other = DynamicImage::ImageRgb8(pattern(800, 600, 5)).fliph();

    let h = dhash(&base);
    for (name, img) in [("small", &small), ("big", &big), ("jpeg60", &recompressed)] {
        let d = hamming(h, dhash(img));
        assert!(d <= 6, "{name}: dhash distance {d}");
        let da = hamming(ahash(&base), ahash(img));
        assert!(da <= 6, "{name}: ahash distance {da}");
    }
    let d = hamming(h, dhash(&other));
    assert!(d >= 16, "different image too similar: {d}");
}

#[test]
fn sharpness_lower_when_blurred() {
    let sharp = DynamicImage::ImageRgb8(pattern(1024, 768, 2));
    let blurred = sharp.blur(3.0);
    let s1 = sharpness(&sharp);
    let s2 = sharpness(&blurred);
    assert!(
        s1.laplacian_variance > s2.laplacian_variance * 2.0,
        "{s1:?} vs {s2:?}"
    );
    assert!(s1.score > s2.score);
    assert!((0.0..=1.0).contains(&s1.score));
}

// ---------------------------------------------------------------------------
// Structured EXIF/XMP locators in other containers
// ---------------------------------------------------------------------------

const XMP4: &str =
    "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmp:Rating='4'/></x:xmpmeta>";

fn png_chunk(typ: &[u8], data: &[u8]) -> Vec<u8> {
    let mut c = typ.to_vec();
    c.extend_from_slice(data);
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
    out
}

#[test]
fn png_exif_and_xmp_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let png = encode_png(&pattern(120, 80, 1));
    // Insert after the IHDR chunk (8 sig + 25 IHDR).
    let mut itxt = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    itxt.extend_from_slice(XMP4.as_bytes());
    let mut d = png[..33].to_vec();
    d.extend(png_chunk(b"eXIf", &exif_tiff(3, None)));
    d.extend_from_slice(&png[33..png.len() - 12]);
    d.extend(png_chunk(b"iTXt", &itxt)); // after IDAT is legal for iTXt
    d.extend_from_slice(&png[png.len() - 12..]);
    let p = write(dir.path(), "m.png", &d);
    let m = read_image_metadata(&p).unwrap();
    assert_eq!(m.orientation, Some(3));
    assert_eq!(m.iso, Some(400));
    assert_eq!(m.xmp.and_then(|x| x.rating), Some(4));
    assert_eq!((m.width, m.height), (Some(120), Some(80)));
}

#[test]
fn webp_exif_and_xmp_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let p0 = dir.path().join("base.webp");
    DynamicImage::ImageRgb8(pattern(64, 48, 2))
        .save(&p0)
        .unwrap();
    let mut d = std::fs::read(&p0).unwrap();
    let mut ex = b"Exif\0\0".to_vec();
    ex.extend(exif_tiff(8, None));
    for (fourcc, data) in [(b"EXIF", ex), (b"XMP ", XMP4.as_bytes().to_vec())] {
        d.extend_from_slice(fourcc);
        d.extend_from_slice(&(data.len() as u32).to_le_bytes());
        d.extend_from_slice(&data);
        if data.len() % 2 == 1 {
            d.push(0);
        }
    }
    let riff = (d.len() - 8) as u32;
    d[4..8].copy_from_slice(&riff.to_le_bytes());
    let p = write(dir.path(), "m.webp", &d);
    let m = read_image_metadata(&p).unwrap();
    assert_eq!(m.orientation, Some(8));
    assert_eq!(m.make.as_deref(), Some("Canon"));
    assert_eq!(m.xmp.and_then(|x| x.rating), Some(4));
}

#[test]
fn tiff_tag_700_and_cr3_xmp_box() {
    let dir = tempfile::tempdir().unwrap();
    // DNG-like TIFF: XMP placed beyond the 1 MB fallback scan window.
    let mut t = TiffBuilder::new(42);
    t.blob(&vec![0u8; 1_200_000]);
    let x = t.blob(XMP4.as_bytes());
    let i0 = t.ifd(
        vec![
            ascii(0x010F, "Adobe"),
            Ent {
                tag: 0x02BC,
                typ: 7,
                count: XMP4.len() as u32,
                data: vec![],
            },
        ],
        0,
    );
    // Patch the XMP entry to point at the blob (builder can't express external offsets).
    let n = u16::from_le_bytes([t.buf[i0 as usize], t.buf[i0 as usize + 1]]) as usize;
    for k in 0..n {
        let e = i0 as usize + 2 + 12 * k;
        if u16::from_le_bytes([t.buf[e], t.buf[e + 1]]) == 0x02BC {
            t.buf[e + 8..e + 12].copy_from_slice(&x.to_le_bytes());
        }
    }
    t.set_first_ifd(i0);
    let p = write(dir.path(), "x.dng", &t.buf);
    let m = read_image_metadata(&p).unwrap();
    assert_eq!(m.xmp.and_then(|x| x.rating), Some(4));

    let mut c = cr3(
        1,
        &jpeg(160, 120, 1),
        &jpeg(320, 240, 1),
        &jpeg(640, 480, 1),
    );
    let mut payload = vec![
        0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf,
        0xac,
    ];
    payload.extend_from_slice(XMP4.as_bytes());
    c.extend(bx(b"uuid", &payload));
    let p = write(dir.path(), "x.cr3", &c);
    let m = read_image_metadata(&p).unwrap();
    assert_eq!(m.xmp.and_then(|x| x.rating), Some(4));
}
