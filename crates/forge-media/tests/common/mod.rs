//! Programmatic fixtures: JPEGs with handcrafted EXIF APP1 segments, synthetic TIFF-based
//! RAW / CR3 / RAF containers. No binary test files are checked in.
#![allow(dead_code)]

use image::{ImageEncoder, Rgb, RgbImage};

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Structured test picture: gradient background + a few shapes (good for hashing and
/// sharpness tests) + mild deterministic noise.
pub fn pattern(w: u32, h: u32, seed: u32) -> RgbImage {
    let mut img = RgbImage::new(w, h);
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    for (x, y, p) in img.enumerate_pixels_mut() {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let n = (s & 15) as i32 - 8;
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        let mut v = [
            (fx * 200.0) as i32 + 20,
            (fy * 180.0) as i32 + 30,
            ((1.0 - fx) * 150.0) as i32 + 40,
        ];
        let (cx, cy) = (fx - 0.3 - seed as f32 * 0.05, fy - 0.4);
        if cx * cx + cy * cy < 0.04 {
            v = [230, 220, 40];
        }
        if fx > 0.6 && fx < 0.85 && fy > 0.55 && fy < 0.9 {
            v = [20, 30, 160];
        }
        if ((x / (w / 16).max(1)) + (y / (h / 12).max(1))).is_multiple_of(7) {
            v = [v[0] / 2, v[1] / 2, v[2] / 2];
        }
        *p = Rgb([
            (v[0] + n).clamp(0, 255) as u8,
            (v[1] + n).clamp(0, 255) as u8,
            (v[2] + n).clamp(0, 255) as u8,
        ]);
    }
    img
}

pub fn encode_jpeg(img: &RgbImage, q: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    buf
}

pub fn jpeg(w: u32, h: u32, seed: u32) -> Vec<u8> {
    encode_jpeg(&pattern(w, h, seed), 85)
}

pub fn encode_png(img: &RgbImage) -> Vec<u8> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    buf
}

/// Inserts an APP1 segment (EXIF or XMP payload) right after SOI.
pub fn insert_app1(jpeg: &[u8], payload: &[u8]) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    let len = (payload.len() + 2) as u16;
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

pub fn with_exif(jpeg: &[u8], tiff: &[u8]) -> Vec<u8> {
    let mut p = b"Exif\0\0".to_vec();
    p.extend_from_slice(tiff);
    insert_app1(jpeg, &p)
}

pub fn with_xmp(jpeg: &[u8], xmp: &str) -> Vec<u8> {
    let mut p = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    p.extend_from_slice(xmp.as_bytes());
    insert_app1(jpeg, &p)
}

// ---------------------------------------------------------------------------
// TIFF builder (little endian)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Ent {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    pub data: Vec<u8>,
}

pub fn ascii(tag: u16, s: &str) -> Ent {
    let mut d = s.as_bytes().to_vec();
    d.push(0);
    Ent {
        tag,
        typ: 2,
        count: d.len() as u32,
        data: d,
    }
}
pub fn short(tag: u16, v: u16) -> Ent {
    Ent {
        tag,
        typ: 3,
        count: 1,
        data: v.to_le_bytes().to_vec(),
    }
}
pub fn long(tag: u16, v: u32) -> Ent {
    Ent {
        tag,
        typ: 4,
        count: 1,
        data: v.to_le_bytes().to_vec(),
    }
}
pub fn longs(tag: u16, v: &[u32]) -> Ent {
    Ent {
        tag,
        typ: 4,
        count: v.len() as u32,
        data: v.iter().flat_map(|x| x.to_le_bytes()).collect(),
    }
}
pub fn rationals(tag: u16, v: &[(u32, u32)]) -> Ent {
    Ent {
        tag,
        typ: 5,
        count: v.len() as u32,
        data: v
            .iter()
            .flat_map(|(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes()))
            .collect(),
    }
}
pub fn rational(tag: u16, n: u32, d: u32) -> Ent {
    rationals(tag, &[(n, d)])
}
pub fn srational(tag: u16, n: i32, d: i32) -> Ent {
    Ent {
        tag,
        typ: 10,
        count: 1,
        data: n.to_le_bytes().into_iter().chain(d.to_le_bytes()).collect(),
    }
}
pub fn undefined(tag: u16, b: &[u8]) -> Ent {
    Ent {
        tag,
        typ: 7,
        count: b.len() as u32,
        data: b.to_vec(),
    }
}

/// TIFF byte buffer builder. Offsets are relative to the start of the buffer (TIFF header).
pub struct TiffBuilder {
    pub buf: Vec<u8>,
}

impl TiffBuilder {
    pub fn new(magic: u16) -> Self {
        let mut buf = b"II".to_vec();
        buf.extend_from_slice(&magic.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        Self { buf }
    }
    fn align(&mut self) {
        if self.buf.len() % 2 == 1 {
            self.buf.push(0);
        }
    }
    /// Appends raw data, returns its offset.
    pub fn blob(&mut self, b: &[u8]) -> u32 {
        self.align();
        let o = self.buf.len() as u32;
        self.buf.extend_from_slice(b);
        o
    }
    /// Appends an IFD, returns its offset.
    pub fn ifd(&mut self, mut entries: Vec<Ent>, next: u32) -> u32 {
        entries.sort_by_key(|e| e.tag);
        self.align();
        let off = self.buf.len();
        let n = entries.len();
        let mut data_pos = off + 2 + 12 * n + 4;
        let mut data = Vec::new();
        let mut table = Vec::new();
        table.extend_from_slice(&(n as u16).to_le_bytes());
        for e in &entries {
            table.extend_from_slice(&e.tag.to_le_bytes());
            table.extend_from_slice(&e.typ.to_le_bytes());
            table.extend_from_slice(&e.count.to_le_bytes());
            if e.data.len() <= 4 {
                let mut v = e.data.clone();
                v.resize(4, 0);
                table.extend_from_slice(&v);
            } else {
                table.extend_from_slice(&(data_pos as u32).to_le_bytes());
                data.extend_from_slice(&e.data);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
                data_pos = off + 2 + 12 * n + 4 + data.len();
            }
        }
        table.extend_from_slice(&next.to_le_bytes());
        self.buf.extend_from_slice(&table);
        self.buf.extend_from_slice(&data);
        off as u32
    }
    pub fn set_first_ifd(&mut self, off: u32) {
        self.buf[4..8].copy_from_slice(&off.to_le_bytes());
    }
}

/// Standard EXIF fixture values.
pub fn exif_entries() -> Vec<Ent> {
    vec![
        rational(0x829A, 1, 250),                   // ExposureTime
        rational(0x829D, 28, 10),                   // FNumber
        short(0x8827, 400),                         // ISO
        ascii(0x9003, "2024:05:01 12:34:56"),       // DateTimeOriginal
        ascii(0x9011, "+02:00"),                    // OffsetTimeOriginal
        ascii(0x9291, "12"),                        // SubSecTimeOriginal
        srational(0x9204, -2, 3),                   // ExposureBiasValue
        rational(0x920A, 350, 10),                  // FocalLength
        short(0xA001, 1),                           // ColorSpace sRGB
        ascii(0xA434, "RF 35mm F1.8 MACRO IS STM"), // LensModel
    ]
}

pub fn gps_entries() -> Vec<Ent> {
    vec![
        ascii(0x0001, "N"),
        rationals(0x0002, &[(38, 1), (42, 1), (3600, 100)]),
        ascii(0x0003, "W"),
        rationals(0x0004, &[(9, 1), (8, 1), (0, 1)]),
    ]
}

/// EXIF TIFF block for a JPEG APP1: IFD0 (Make/Model/Orientation, pointers) + EXIF + GPS +
/// optional IFD1 thumbnail.
pub fn exif_tiff(orientation: u16, thumb: Option<&[u8]>) -> Vec<u8> {
    let mut t = TiffBuilder::new(42);
    let exif = t.ifd(exif_entries(), 0);
    let gps = t.ifd(gps_entries(), 0);
    let ifd1 = thumb.map(|th| {
        let o = t.blob(th);
        t.ifd(
            vec![
                long(0x0201, o),
                long(0x0202, th.len() as u32),
                short(0x0103, 6),
            ],
            0,
        )
    });
    let ifd0 = t.ifd(
        vec![
            ascii(0x010F, "Canon"),
            ascii(0x0110, "Canon EOS R6"),
            short(0x0112, orientation),
            long(0x8769, exif),
            long(0x8825, gps),
        ],
        ifd1.unwrap_or(0),
    );
    t.set_first_ifd(ifd0);
    t.buf
}

/// A minimal lossless-JPEG (SOF3) header, as used for raw sensor data — must never be
/// chosen as a preview.
pub fn fake_lossless_jpeg() -> Vec<u8> {
    let mut v = vec![
        0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x0B, 14, 0x0F, 0xA0, 0x17, 0x70, 1, 1, 0x11, 0,
    ];
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0, 0]);
    v.resize(4096, 0x55);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

/// NEF/DNG-like TIFF RAW: IFD0 = 160×120 thumbnail (JPEGInterchangeFormat), SubIFD0 = raw
/// (6000×4000, 14-bit, uncompressed-ish), SubIFD1 = big JPEG preview, SubIFD2 = lossless
/// JPEG strip (must be rejected). `magic` 42 for NEF/DNG, 0x4F52 for ORF.
pub fn tiff_raw(magic: u16, orientation: u16, big_preview: &[u8], small_preview: &[u8]) -> Vec<u8> {
    let mut t = TiffBuilder::new(magic);
    let big = t.blob(big_preview);
    let small = t.blob(small_preview);
    let ll = fake_lossless_jpeg();
    let llo = t.blob(&ll);
    let raw = t.blob(&[0u8; 64]);
    let exif = t.ifd(exif_entries(), 0);
    let sub_raw = t.ifd(
        vec![
            long(0x00FE, 0),
            long(0x0100, 6000),
            long(0x0101, 4000),
            short(0x0102, 14),
            short(0x0103, 34713),
            long(0x0111, raw),
            long(0x0117, 64),
        ],
        0,
    );
    let sub_prev = t.ifd(
        vec![
            long(0x00FE, 1),
            long(0x0201, big),
            long(0x0202, big_preview.len() as u32),
        ],
        0,
    );
    let sub_ll = t.ifd(
        vec![
            long(0x00FE, 1),
            short(0x0103, 7),
            long(0x0111, llo),
            long(0x0117, ll.len() as u32),
        ],
        0,
    );
    let ifd0 = t.ifd(
        vec![
            long(0x00FE, 1),
            long(0x0100, 160),
            long(0x0101, 120),
            ascii(0x010F, "NIKON CORPORATION"),
            ascii(0x0110, "NIKON Z 6_2"),
            short(0x0112, orientation),
            long(0x0201, small),
            long(0x0202, small_preview.len() as u32),
            longs(0x014A, &[sub_raw, sub_prev, sub_ll]),
            long(0x8769, exif),
        ],
        0,
    );
    t.set_first_ifd(ifd0);
    t.buf
}

// ---------------------------------------------------------------------------
// ISO-BMFF (CR3)
// ---------------------------------------------------------------------------

pub fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

pub const CANON_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
pub const PRVW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// Synthetic CR3 following the documented layout (lclevy/canon_cr3).
pub fn cr3(orientation: u16, thumb: &[u8], prvw: &[u8], full: &[u8]) -> Vec<u8> {
    let build = |full_off: u64| -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
        // CMT1: TIFF IFD0
        let mut t1 = TiffBuilder::new(42);
        let i0 = t1.ifd(
            vec![
                ascii(0x010F, "Canon"),
                ascii(0x0110, "Canon EOS R5"),
                short(0x0112, orientation),
            ],
            0,
        );
        t1.set_first_ifd(i0);
        // CMT2: TIFF whose IFD0 holds EXIF-IFD tags
        let mut t2 = TiffBuilder::new(42);
        let i2 = t2.ifd(exif_entries(), 0);
        t2.set_first_ifd(i2);
        // CMT4: GPS
        let mut t4 = TiffBuilder::new(42);
        let i4 = t4.ifd(gps_entries(), 0);
        t4.set_first_ifd(i4);

        let mut thmb = vec![0u8, 0, 0, 0];
        thmb.extend_from_slice(&160u16.to_be_bytes());
        thmb.extend_from_slice(&120u16.to_be_bytes());
        thmb.extend_from_slice(&(thumb.len() as u32).to_be_bytes());
        thmb.extend_from_slice(&[0, 1, 0, 0]);
        thmb.extend_from_slice(thumb);

        let mut canon = CANON_UUID.to_vec();
        canon.extend(bx(b"CNCV", b"CanonCR3_001/00.09.00/00.00.00"));
        canon.extend(bx(b"CMT1", &t1.buf));
        canon.extend(bx(b"CMT2", &t2.buf));
        canon.extend(bx(b"CMT3", b"II*\0\x08\0\0\0\0\0"));
        canon.extend(bx(b"CMT4", &t4.buf));
        canon.extend(bx(b"THMB", &thmb));

        let mut stsz = vec![0u8; 4];
        stsz.extend_from_slice(&0u32.to_be_bytes());
        stsz.extend_from_slice(&1u32.to_be_bytes());
        stsz.extend_from_slice(&(full.len() as u32).to_be_bytes());
        let mut co64 = vec![0u8; 4];
        co64.extend_from_slice(&1u32.to_be_bytes());
        co64.extend_from_slice(&full_off.to_be_bytes());
        let stbl = bx(b"stbl", &[bx(b"stsz", &stsz), bx(b"co64", &co64)].concat());
        let trak = bx(b"trak", &bx(b"mdia", &bx(b"minf", &stbl)));
        let moov = bx(b"moov", &[bx(b"uuid", &canon), trak].concat());

        let mut pr = vec![0u8; 4];
        pr.extend_from_slice(&[0, 1]);
        pr.extend_from_slice(&1620u16.to_be_bytes());
        pr.extend_from_slice(&1080u16.to_be_bytes());
        pr.extend_from_slice(&[0, 1]);
        pr.extend_from_slice(&(prvw.len() as u32).to_be_bytes());
        pr.extend_from_slice(prvw);
        let mut pu = PRVW_UUID.to_vec();
        pu.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
        pu.extend(bx(b"PRVW", &pr));
        let prvw_box = bx(b"uuid", &pu);

        let mut out = [ftyp, moov, prvw_box].concat();
        out.extend(bx(b"mdat", full));
        out
    };
    let probe = build(0);
    let mdat_payload = probe.len() - full.len();
    build(mdat_payload as u64)
}

/// Synthetic Fujifilm RAF: fixed header with the JPEG offset/length at 0x54/0x58.
pub fn raf(jpeg: &[u8]) -> Vec<u8> {
    let mut d = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
    d.resize(0x100, 0);
    d[0x54..0x58].copy_from_slice(&0x100u32.to_be_bytes());
    d[0x58..0x5C].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
    d.extend_from_slice(jpeg);
    d.extend_from_slice(&[0u8; 256]); // "CFA" data
    d
}

/// Deterministic byte mutator for robustness tests.
pub fn mutate(data: &[u8], seed: u64, flips: usize) -> Vec<u8> {
    let mut d = data.to_vec();
    let mut s = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    for _ in 0..flips {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let i = (s >> 33) as usize % d.len().max(1);
        if let Some(b) = d.get_mut(i) {
            *b = (s >> 17) as u8;
        }
    }
    d
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// A PNG whose IHDR declares `w`×`h` (valid CRC) followed by a tiny IDAT/IEND.
pub fn png_declaring(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let chunk = |typ: &[u8], data: &[u8], out: &mut Vec<u8>| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = typ.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr, &mut out);
    chunk(
        b"IDAT",
        &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        &mut out,
    );
    chunk(b"IEND", &[], &mut out);
    out
}
