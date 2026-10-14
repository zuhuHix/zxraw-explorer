//! Which EXIF metadata an exported image carries.
//!
//! Exported pixels are re-encoded, so without this module an export has no metadata at all.
//! [`exif_for_export`] reads the source's EXIF and rebuilds a clean block for the encoder.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use exif::experimental::Writer;
use exif::{Context, Field, In, Reader, Tag, Value};
use serde::{Deserialize, Serialize};

/// JPEG APP1 segments hold at most 64 KiB, including the 6-byte `Exif\0\0` prefix.
const MAX_EXIF_BYTES: usize = 65_000;

/// Tags that describe the source's pixel layout, thumbnail or internal pointers. The pixels
/// are re-encoded and the writer synthesizes its own pointers, so these never carry over.
const LAYOUT_TAGS: [Tag; 25] = [
    Tag::ImageWidth,
    Tag::ImageLength,
    Tag::BitsPerSample,
    Tag::Compression,
    Tag::PhotometricInterpretation,
    Tag::Orientation,
    Tag::StripOffsets,
    Tag::RowsPerStrip,
    Tag::StripByteCounts,
    Tag::SamplesPerPixel,
    Tag::PlanarConfiguration,
    // TileWidth and TileLength have no constant in kamadak-exif; they are the tags 0x142/0x143.
    Tag(Context::Tiff, 0x142),
    Tag(Context::Tiff, 0x143),
    Tag::TileOffsets,
    Tag::TileByteCounts,
    Tag::JPEGInterchangeFormat,
    Tag::JPEGInterchangeFormatLength,
    Tag::YCbCrSubSampling,
    Tag::YCbCrPositioning,
    Tag::PixelXDimension,
    Tag::PixelYDimension,
    Tag::ExifIFDPointer,
    Tag::GPSInfoIFDPointer,
    Tag::InteropIFDPointer,
    Tag::MakerNote,
];

/// Free-text or identifying tags that the Clean policy removes.
const PRIVATE_TAGS: [Tag; 5] = [
    Tag::BodySerialNumber,
    Tag::LensSerialNumber,
    Tag::CameraOwnerName,
    Tag::ImageUniqueID,
    Tag::UserComment,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MetadataPolicy {
    /// No metadata in the output.
    None,
    /// Camera, exposure, date and copyright/artist; no GPS, serial numbers, owner names,
    /// maker notes or comments.
    #[default]
    Clean,
    /// Everything that can be carried over, GPS included.
    All,
}

impl MetadataPolicy {
    pub const ALL: [Self; 3] = [Self::None, Self::Clean, Self::All];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No metadata",
            Self::Clean => "Keep camera info and copyright (strip GPS and private data)",
            Self::All => "Keep all metadata (including GPS)",
        }
    }
}

/// The EXIF block (TIFF structure, without the `Exif\0\0` prefix) for an export of `source`
/// under `policy`, ready for `ImageEncoder::set_exif_metadata`. `None` when the policy is
/// [`MetadataPolicy::None`], the source has no readable EXIF, or nothing is left to keep.
pub fn exif_for_export(source: &Path, policy: MetadataPolicy) -> Option<Vec<u8>> {
    if policy == MetadataPolicy::None {
        return None;
    }
    let file = File::open(source).ok()?;
    let exif = Reader::new()
        .read_from_container(&mut BufReader::new(file))
        .ok()?;

    let fields: Vec<&Field> = exif.fields().filter(|f| keep(f, policy)).collect();
    if fields.is_empty() {
        return None;
    }

    let mut writer = Writer::new();
    for &field in &fields {
        writer.push_field(field);
    }
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out, exif.little_endian()).ok()?;
    let bytes = out.into_inner();
    (bytes.len() <= MAX_EXIF_BYTES).then_some(bytes)
}

/// Whether `field` from the source's primary image IFD survives `policy`.
fn keep(field: &Field, policy: MetadataPolicy) -> bool {
    let tag = field.tag;
    // Tags unknown to kamadak have no description; they are dropped rather than guessed at.
    if field.ifd_num != In::PRIMARY
        || tag.description().is_none()
        || LAYOUT_TAGS.contains(&tag)
        || matches!(field.value, Value::Unknown(..))
    {
        return false;
    }
    match tag.context() {
        Context::Tiff | Context::Exif => {}
        Context::Gps if policy == MetadataPolicy::All => {}
        _ => return false,
    }
    if policy == MetadataPolicy::Clean
        && (PRIVATE_TAGS.contains(&tag) || tag.to_string().contains("Serial"))
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use exif::experimental::Writer as TiffWriter;
    use exif::{Exif, Rational};
    use image::codecs::jpeg::JpegEncoder;
    use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage};

    fn ascii(s: &str) -> Value {
        Value::Ascii(vec![s.as_bytes().to_vec()])
    }

    fn dms() -> Value {
        Value::Rational(vec![
            Rational { num: 40, denom: 1 },
            Rational { num: 26, denom: 1 },
            Rational { num: 4620, denom: 100 },
        ])
    }

    fn primary(tag: Tag, value: Value) -> Field {
        Field { tag, ifd_num: In::PRIMARY, value }
    }

    /// A source image's EXIF: camera info, copyright, private tags and GPS.
    fn sample_fields() -> Vec<Field> {
        vec![
            primary(Tag::Make, ascii("TestCam")),
            primary(Tag::Model, ascii("T1")),
            primary(Tag::Orientation, Value::Short(vec![6])),
            primary(Tag::Copyright, ascii("© Jane Doe")),
            primary(Tag::Artist, ascii("Jane Doe")),
            primary(Tag::BodySerialNumber, ascii("SN123")),
            primary(Tag::CameraOwnerName, ascii("Jane")),
            primary(Tag::DateTimeOriginal, ascii("2024:05:01 12:00:00")),
            primary(Tag::ExposureTime, Value::Rational(vec![Rational { num: 1, denom: 125 }])),
            primary(Tag::GPSLatitudeRef, ascii("N")),
            primary(Tag::GPSLatitude, dms()),
            primary(Tag::GPSLongitudeRef, ascii("E")),
            primary(Tag::GPSLongitude, dms()),
        ]
    }

    fn tiff_bytes(fields: &[Field]) -> Vec<u8> {
        let mut writer = TiffWriter::new();
        for field in fields {
            writer.push_field(field);
        }
        let mut buf = Cursor::new(Vec::new());
        writer.write(&mut buf, true).unwrap();
        buf.into_inner()
    }

    /// Writes a small JPEG to `path`, carrying `exif` when given.
    fn write_jpeg(path: &Path, exif: Option<Vec<u8>>) {
        let img = RgbImage::from_pixel(16, 16, Rgb([120, 80, 200]));
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 85);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif).unwrap();
        }
        encoder
            .write_image(img.as_raw(), 16, 16, ExtendedColorType::Rgb8)
            .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn source_with_exif(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("source.jpg");
        write_jpeg(&path, Some(tiff_bytes(&sample_fields())));
        path
    }

    fn read_back(bytes: Vec<u8>) -> Exif {
        Reader::new().read_raw(bytes).unwrap()
    }

    fn ascii_of(exif: &Exif, tag: Tag) -> Option<Vec<u8>> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Ascii(values) => values.first().cloned(),
            _ => None,
        }
    }

    fn has_gps(exif: &Exif) -> bool {
        exif.fields().any(|f| f.tag.context() == Context::Gps)
    }

    #[test]
    fn labels_are_distinct() {
        let labels: Vec<_> = MetadataPolicy::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(labels[0], "No metadata");
        assert_ne!(labels[1], labels[2]);
    }

    #[test]
    fn clean_keeps_camera_info_and_copyright_only() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(dir.path());
        let exif = read_back(exif_for_export(&src, MetadataPolicy::Clean).unwrap());

        assert_eq!(ascii_of(&exif, Tag::Copyright).unwrap(), "© Jane Doe".as_bytes());
        assert_eq!(ascii_of(&exif, Tag::Artist).unwrap(), b"Jane Doe");
        assert_eq!(ascii_of(&exif, Tag::Make).unwrap(), b"TestCam");
        assert_eq!(ascii_of(&exif, Tag::DateTimeOriginal).unwrap(), b"2024:05:01 12:00:00");
        assert!(exif.get_field(Tag::ExposureTime, In::PRIMARY).is_some());
        assert!(!has_gps(&exif));
        assert!(exif.get_field(Tag::BodySerialNumber, In::PRIMARY).is_none());
        assert!(exif.get_field(Tag::CameraOwnerName, In::PRIMARY).is_none());
        assert!(exif.get_field(Tag::Orientation, In::PRIMARY).is_none());
    }

    #[test]
    fn all_keeps_gps_but_not_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(dir.path());
        let exif = read_back(exif_for_export(&src, MetadataPolicy::All).unwrap());

        assert!(exif.get_field(Tag::GPSLatitude, In::PRIMARY).is_some());
        assert!(exif.get_field(Tag::BodySerialNumber, In::PRIMARY).is_some());
        assert!(exif.get_field(Tag::Orientation, In::PRIMARY).is_none());
    }

    #[test]
    fn none_policy_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(dir.path());
        assert!(exif_for_export(&src, MetadataPolicy::None).is_none());
    }

    #[test]
    fn source_without_exif_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.png");
        RgbImage::from_pixel(4, 4, Rgb([1, 2, 3])).save(&path).unwrap();
        assert!(exif_for_export(&path, MetadataPolicy::Clean).is_none());
        assert!(exif_for_export(&path, MetadataPolicy::All).is_none());
    }

    #[test]
    fn missing_file_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jpg");
        assert!(exif_for_export(&path, MetadataPolicy::Clean).is_none());
    }

    #[test]
    fn clean_bytes_survive_the_jpeg_encoder() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_with_exif(dir.path());
        let block = exif_for_export(&src, MetadataPolicy::Clean).unwrap();

        let out = dir.path().join("out.jpg");
        write_jpeg(&out, Some(block));
        let exif = Reader::new()
            .read_from_container(&mut BufReader::new(File::open(&out).unwrap()))
            .unwrap();

        assert_eq!(ascii_of(&exif, Tag::Copyright).unwrap(), "© Jane Doe".as_bytes());
        assert!(!has_gps(&exif));
    }
}
