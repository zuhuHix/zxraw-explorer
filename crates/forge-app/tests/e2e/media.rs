//! End-to-end scenarios: media.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use forge_core::model::EntryKind;
use forge_core::query::{ColorLabel, PickState};
use forge_db::{Db, NewFile};

#[allow(unused_imports)]
use crate::*;

fn shot(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("zxfr-shots").join("media");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn gps_jpeg(out_of_range: bool, malformed_rational: bool) -> Vec<u8> {
    let make = b"OpenAI\0";
    let model = b"Camera X\0";
    let ifd0_offset = 8usize;
    let ifd0_len = 2 + 4 * 12 + 4;
    let make_offset = ifd0_offset + ifd0_len;
    let model_offset = make_offset + make.len();
    let exif_ifd_offset = model_offset + model.len();
    let exif_ifd_len = 2 + 12 + 4;
    let gps_ifd_offset = exif_ifd_offset + exif_ifd_len;
    let gps_data_offset = gps_ifd_offset + 2 + 8 * 12 + 4;
    let lat_offset = gps_data_offset;
    let lon_offset = lat_offset + 24;
    let altitude_offset = lon_offset + 24;
    let direction_offset = altitude_offset + 8;

    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&4u16.to_le_bytes());
    tiff_entry(&mut tiff, 0x010F, 2, make.len() as u32, make_offset as u32);
    tiff_entry(
        &mut tiff,
        0x0110,
        2,
        model.len() as u32,
        model_offset as u32,
    );
    tiff_entry(&mut tiff, 0x8769, 4, 1, exif_ifd_offset as u32);
    tiff_entry(&mut tiff, 0x8825, 4, 1, gps_ifd_offset as u32);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(make);
    tiff.extend_from_slice(model);

    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff_entry(&mut tiff, 0x8827, 3, 1, 200);
    tiff.extend_from_slice(&0u32.to_le_bytes());

    tiff.extend_from_slice(&8u16.to_le_bytes());
    tiff_entry_inline(&mut tiff, 0x0001, 2, 2, u32::from_le_bytes(*b"N\0\0\0"));
    tiff_entry(&mut tiff, 0x0002, 5, 3, lat_offset as u32);
    tiff_entry_inline(&mut tiff, 0x0003, 2, 2, u32::from_le_bytes(*b"W\0\0\0"));
    tiff_entry(&mut tiff, 0x0004, 5, 3, lon_offset as u32);
    tiff_entry_inline(&mut tiff, 0x0005, 1, 1, 0);
    tiff_entry(&mut tiff, 0x0006, 5, 1, altitude_offset as u32);
    tiff_entry_inline(&mut tiff, 0x0010, 2, 2, u32::from_le_bytes(*b"T\0\0\0"));
    tiff_entry(&mut tiff, 0x0011, 5, 1, direction_offset as u32);
    tiff.extend_from_slice(&0u32.to_le_bytes());

    let latitude_degrees = if out_of_range { 91 } else { 37 };
    tiff_rational(&mut tiff, latitude_degrees, 1);
    tiff_rational(&mut tiff, if out_of_range { 0 } else { 15 }, 1);
    tiff_rational(&mut tiff, 0, u32::from(!malformed_rational));
    tiff_rational(&mut tiff, 122, 1);
    tiff_rational(&mut tiff, 30, 1);
    tiff_rational(&mut tiff, 0, 1);
    tiff_rational(&mut tiff, 247, 2);
    tiff_rational(&mut tiff, 1081, 4);
    assert_eq!(tiff.len(), direction_offset + 8);

    let mut pixels = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut pixels)
        .encode_image(&image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(2, 2, image::Rgb([30, 80, 140])),
        ))
        .unwrap();
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(&tiff);
    let segment_len = u16::try_from(exif.len() + 2).unwrap();
    let mut jpeg = pixels[..2].to_vec();
    jpeg.extend_from_slice(&[0xFF, 0xE1]);
    jpeg.extend_from_slice(&segment_len.to_be_bytes());
    jpeg.extend_from_slice(&exif);
    jpeg.extend_from_slice(&pixels[2..]);
    jpeg
}

fn tiff_entry(bytes: &mut Vec<u8>, tag: u16, kind: u16, count: u32, offset: u32) {
    tiff_entry_inline(bytes, tag, kind, count, offset);
}

fn tiff_entry_inline(bytes: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&kind.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn tiff_rational(bytes: &mut Vec<u8>, numerator: u32, denominator: u32) {
    bytes.extend_from_slice(&numerator.to_le_bytes());
    bytes.extend_from_slice(&denominator.to_le_bytes());
}

fn seed_library(profile: &Path, image: &Path) {
    let db_path = profile.join("data").join("forge.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = Db::open(&db_path).unwrap();
    let metadata = fs::metadata(image).unwrap();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64);
    let ids = db
        .upsert_files(&[NewFile {
            path: image.to_path_buf(),
            kind: EntryKind::File,
            size: metadata.len(),
            mtime: modified,
            ctime: None,
        }])
        .unwrap();
    db.set_rating(&ids, 4).unwrap();
    db.set_label(&ids, Some(ColorLabel::Green)).unwrap();
    db.set_pick(&ids, PickState::Pick).unwrap();
    db.add_tags(&ids, &["landscape", "summer"]).unwrap();
}

#[test]
fn gps_panel_shows_coordinates_and_invalid_fixture_without_opening_a_browser() {
    let sandbox = tempfile::tempdir().unwrap();
    let folder = sandbox.path().join("photos");
    write_file(&folder.join("gps.jpg"), &gps_jpeg(false, false));
    write_file(&folder.join("invalid.jpg"), &gps_jpeg(true, false));
    write_file(&folder.join("malformed.jpg"), &gps_jpeg(false, true));
    let profile = sandbox.path().join("profile");
    let screenshot = shot("gps-panel.png");
    let url = "https://www.openstreetmap.org/?mlat=37.250000&mlon=-122.500000#map=15/37.250000/-122.500000";
    let script = format!(
        "navigate {folder}\n\
         select gps.jpg\n\
         cmd TogglePreview\n\
         ui media gps assert 37.25,-122.5\n\
         pointer 1200 600\n\
         scroll 800\n\
         scroll 800\n\
         scroll 800\n\
         wait-frames 20\n\
         screenshot {screenshot}\n\
         ui media open-maps {url}\n\
         assert-toast Would open in maps: {url}\n\
         select invalid.jpg\n\
         ui media gps assert invalid\n\
         select malformed.jpg\n\
         ui media gps assert invalid\n\
         exit\n",
        folder = shown(&folder),
        screenshot = shown(&screenshot),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(screenshot.is_file(), "GPS panel screenshot was not saved");
}

#[test]
fn exports_csv_and_json_and_never_replaces_an_existing_output() {
    let sandbox = tempfile::tempdir().unwrap();
    let folder = sandbox.path().join("photos");
    let good = folder.join("gps.jpg");
    let invalid = folder.join("invalid.jpg");
    write_file(&good, &gps_jpeg(false, false));
    write_file(&invalid, &gps_jpeg(true, false));
    write_file(&folder.join("clip.mp4"), &noise(128, 17));
    let profile = sandbox.path().join("profile");
    seed_library(&profile, &good);

    let csv_path = sandbox.path().join("metadata.csv");
    let json_path = sandbox.path().join("metadata.json");
    let existing_path = sandbox.path().join("existing.csv");
    fs::write(&existing_path, b"keep the original bytes").unwrap();
    let screenshot = shot("export-window.png");
    let script = format!(
        "navigate {folder}\n\
         select gps.jpg, invalid.jpg, clip.mp4\n\
         cmd ExportMetadata\n\
         wait-frames 3\n\
         screenshot {screenshot}\n\
         dialog {csv_path}\n\
         ui media export start csv selection\n\
         ui media export wait 60\n\
         ui media export assert success\n\
         ui media export assert path {csv_path}\n\
         dialog {json_path}\n\
         ui media export start json selection\n\
         ui media export wait 60\n\
         ui media export assert success\n\
         ui media export assert path {json_path}\n\
         dialog {existing_path}\n\
         ui media export start csv selection\n\
         ui media export wait 60\n\
         ui media export assert failed\n\
         exit\n",
        folder = shown(&folder),
        screenshot = shown(&screenshot),
        csv_path = shown(&csv_path),
        json_path = shown(&json_path),
        existing_path = shown(&existing_path),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        screenshot.is_file(),
        "export window screenshot was not saved"
    );
    assert_eq!(
        fs::read(&existing_path).unwrap(),
        b"keep the original bytes"
    );

    let csv = fs::read(&csv_path).unwrap();
    assert!(csv.starts_with(&[0xEF, 0xBB, 0xBF]));
    let rows = parse_csv(&csv[3..]);
    let header = &rows[0];
    let good_row = rows
        .iter()
        .find(|row| row[header.iter().position(|field| field == "name").unwrap()] == "gps.jpg")
        .unwrap();
    let cell =
        |name: &str| good_row[header.iter().position(|field| field == name).unwrap()].as_str();
    assert_eq!(cell("width"), "2");
    assert_eq!(cell("height"), "2");
    assert_eq!(cell("camera_make"), "OpenAI");
    assert_eq!(cell("camera_model"), "Camera X");
    assert_eq!(cell("iso"), "200");
    assert_eq!(cell("gps_lat"), "37.25");
    // Numbers stay numbers: only text that could start a formula gets a leading '.
    assert_eq!(cell("gps_lon"), "-122.5");
    assert_eq!(cell("gps_alt_m"), "123.5");
    assert_eq!(cell("rating"), "4");
    assert_eq!(cell("label"), "green");
    assert_eq!(cell("pick"), "pick");
    assert_eq!(cell("tags"), "landscape; summer");
    let invalid_row = rows
        .iter()
        .find(|row| row[header.iter().position(|field| field == "name").unwrap()] == "invalid.jpg")
        .unwrap();
    assert_eq!(
        invalid_row[header.iter().position(|field| field == "gps_lat").unwrap()],
        ""
    );

    let json: serde_json::Value = serde_json::from_slice(&fs::read(&json_path).unwrap()).unwrap();
    assert_eq!(json["version"], 1);
    let records = json["records"].as_array().unwrap();
    let good_record = records
        .iter()
        .find(|record| record["name"] == "gps.jpg")
        .unwrap();
    assert_eq!(good_record["camera_make"], "OpenAI");
    assert_eq!(good_record["camera_model"], "Camera X");
    assert_eq!(good_record["iso"], 200);
    assert_eq!(good_record["gps_lat"], 37.25);
    assert_eq!(good_record["gps_lon"], -122.5);
    assert_eq!(good_record["gps_alt_m"], 123.5);
    assert_eq!(good_record["rating"], 4);
    assert_eq!(good_record["label"], "green");
    assert_eq!(good_record["pick"], "pick");
    assert_eq!(good_record["tags"][0], "landscape");
    assert_eq!(good_record["tags"][1], "summer");
    let video_record = records
        .iter()
        .find(|record| record["name"] == "clip.mp4")
        .unwrap();
    assert!(video_record["duration_seconds"].is_null());
    assert!(
        video_record["video_note"]
            .as_str()
            .unwrap()
            .contains("ffprobe is unavailable")
    );
    assert!(
        json["notes"][0]
            .as_str()
            .unwrap()
            .contains("ffprobe is unavailable")
    );
}

fn parse_csv(input: &[u8]) -> Vec<Vec<String>> {
    let text = std::str::from_utf8(input).unwrap();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if quoted {
            if character == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(character);
            }
        } else {
            match character {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut cell)),
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    row.push(std::mem::take(&mut cell));
                    rows.push(std::mem::take(&mut row));
                }
                '\n' => {
                    row.push(std::mem::take(&mut cell));
                    rows.push(std::mem::take(&mut row));
                }
                other => cell.push(other),
            }
        }
    }
    assert!(!quoted, "CSV ended inside a quoted field");
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}
