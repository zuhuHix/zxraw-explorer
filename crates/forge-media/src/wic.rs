//! Windows Imaging Component fallback for installed still-image codecs.

use std::path::Path;

use crate::error::{MediaError, Result};

const DEFAULT_PIXEL_BUDGET: u64 = 200_000_000;

/// WIC containers used by generated test fixtures.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestContainer {
    Heif,
    Avif,
}

/// Decodes and scales a thumbnail without materialising a full-size pixel buffer.
pub fn decode_thumbnail(path: &Path, max_side: u32) -> Result<image::RgbaImage> {
    decode_thumbnail_within(path, max_side, DEFAULT_PIXEL_BUDGET)
}

/// [`decode_thumbnail`] that rejects sources over `max_pixels` (the caller's decode limits)
/// before the codec decodes anything.
#[cfg(windows)]
pub fn decode_thumbnail_within(
    path: &Path,
    max_side: u32,
    max_pixels: u64,
) -> Result<image::RgbaImage> {
    windows_impl::decode(path, Some(max_side.clamp(1, 4096)), max_pixels).map(|(image, _)| image)
}

#[cfg(not(windows))]
pub fn decode_thumbnail_within(
    _path: &Path,
    _max_side: u32,
    _max_pixels: u64,
) -> Result<image::RgbaImage> {
    Err(MediaError::Unsupported(
        "Windows Imaging Component is only available on Windows".into(),
    ))
}

/// Fully decodes `path` after rejecting dimensions over `max_pixels` before allocation.
#[cfg(windows)]
pub fn decode_full(path: &Path, max_pixels: u64) -> Result<image::RgbaImage> {
    windows_impl::decode(path, None, max_pixels).map(|(image, _)| image)
}

#[cfg(not(windows))]
pub fn decode_full(_path: &Path, _max_pixels: u64) -> Result<image::RgbaImage> {
    Err(MediaError::Unsupported(
        "Windows Imaging Component is only available on Windows".into(),
    ))
}

/// Returns oriented source dimensions without allocating its pixels.
#[cfg(windows)]
pub fn source_dimensions(path: &Path) -> Result<(u32, u32)> {
    windows_impl::source_dimensions(path)
}

#[cfg(not(windows))]
pub fn source_dimensions(_path: &Path) -> Result<(u32, u32)> {
    Err(MediaError::Unsupported(
        "Windows Imaging Component is only available on Windows".into(),
    ))
}

/// Encodes a small deterministic image through WIC for codec-gated tests.
#[doc(hidden)]
#[cfg(windows)]
pub fn encode_test_fixture(path: &Path, container: TestContainer) -> Result<()> {
    windows_impl::encode_test_fixture(path, container)
}

#[doc(hidden)]
#[cfg(not(windows))]
pub fn encode_test_fixture(_path: &Path, _container: TestContainer) -> Result<()> {
    Err(MediaError::Unsupported(
        "WIC fixture encoding is only available on Windows".into(),
    ))
}

fn check_dimensions(width: u32, height: u32, limit: u64) -> Result<u64> {
    if width == 0 || height == 0 {
        return Err(MediaError::corrupt("WIC reported zero image dimensions"));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limit {
        return Err(MediaError::TooLarge {
            width,
            height,
            limit,
        });
    }
    Ok(pixels)
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use image::{DynamicImage, RgbaImage};
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatHeif, GUID_WICPixelFormat32bppRGBA,
        IWICBitmapCodecInfo, IWICBitmapEncoderInfo, IWICBitmapFrameDecode, IWICBitmapSource,
        IWICImagingFactory, IWICMetadataQueryReader, IWICPalette, WICBitmapDitherTypeNone,
        WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
        WICComponentEnumerateDefault, WICDecodeMetadataCacheOnLoad, WICEncoder,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        IPropertyBag2, PROPVARIANT, PropVariantClear, PropVariantToUInt32,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IEnumUnknown,
    };
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::core::{GUID, IUnknown, Interface, PCWSTR, w};

    use crate::error::{MediaError, Result};

    use super::{TestContainer, check_dimensions};

    const WINCODEC_ERR_COMPONENTNOTFOUND: i32 = 0x8898_2F50u32 as i32;

    /// The calling thread's COM apartment for the duration of a decode. A thread that is
    /// already single-threaded (e.g. one that used shell APIs) keeps its apartment: WIC
    /// works in either, and asking for MTA there fails with `RPC_E_CHANGED_MODE`.
    struct ComApartment {
        owned: bool,
    }

    impl ComApartment {
        fn new() -> Result<Self> {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr == RPC_E_CHANGED_MODE {
                return Ok(Self { owned: false });
            }
            hr.ok()
                .map_err(|e| MediaError::Decode(format!("initialize WIC COM apartment: {e}")))?;
            Ok(Self { owned: true })
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.owned {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct Source {
        frame: IWICBitmapFrameDecode,
        _decoder: windows::Win32::Graphics::Imaging::IWICBitmapDecoder,
        factory: IWICImagingFactory,
        width: u32,
        height: u32,
        orientation: u16,
        // Kept last so COM is uninitialized after the interfaces above are released.
        _apartment: ComApartment,
    }

    fn create_factory() -> Result<IWICImagingFactory> {
        unsafe {
            CoCreateInstance::<_, IWICImagingFactory>(
                &CLSID_WICImagingFactory,
                None,
                CLSCTX_INPROC_SERVER,
            )
        }
        .map_err(|e| MediaError::Decode(format!("create WIC imaging factory: {e}")))
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn is_heif_path(path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "heic" | "heif" | "avif"
        )
    }

    fn decoder_error(path: &Path, op: &str, error: windows::core::Error) -> MediaError {
        if error.code().0 == WINCODEC_ERR_COMPONENTNOTFOUND && is_heif_path(path) {
            return MediaError::Unsupported(
                "HEIC codec not installed (Microsoft HEIF Image Extensions)".into(),
            );
        }
        MediaError::Decode(format!("WIC {op} failed for {}: {error}", path.display()))
    }

    fn open_source(path: &Path) -> Result<Source> {
        std::fs::metadata(path).map_err(|e| MediaError::io(path, e))?;
        let apartment = ComApartment::new()?;
        let factory = create_factory()?;
        let filename = wide_path(path);
        let decoder = unsafe {
            factory.CreateDecoderFromFilename(
                PCWSTR(filename.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnLoad,
            )
        }
        .map_err(|e| decoder_error(path, "decoder creation", e))?;
        let frame =
            unsafe { decoder.GetFrame(0) }.map_err(|e| decoder_error(path, "frame access", e))?;
        let mut width = 0;
        let mut height = 0;
        unsafe { frame.GetSize(&mut width, &mut height) }
            .map_err(|e| decoder_error(path, "frame dimensions", e))?;
        if width == 0 || height == 0 {
            return Err(MediaError::corrupt("WIC reported zero image dimensions"));
        }
        let orientation = read_orientation(&frame).unwrap_or(1);
        Ok(Source {
            frame,
            _decoder: decoder,
            factory,
            width,
            height,
            orientation,
            _apartment: apartment,
        })
    }

    fn read_orientation(frame: &IWICBitmapFrameDecode) -> Option<u16> {
        let reader: IWICMetadataQueryReader = unsafe { frame.GetMetadataQueryReader() }.ok()?;
        for query in [
            w!("/ifd/{ushort=274}"),
            w!("/app1/ifd/{ushort=274}"),
            w!("/ifd/exif/{ushort=274}"),
        ] {
            let mut value = PROPVARIANT::default();
            let read = unsafe { reader.GetMetadataByName(query, &mut value) };
            if read.is_ok() {
                let orientation = unsafe { PropVariantToUInt32(&value) }.ok();
                let _ = unsafe { PropVariantClear(&mut value) };
                if let Some(value) = orientation.filter(|n| (1..=8).contains(n)) {
                    return Some(value as u16);
                }
            }
        }
        None
    }

    pub(super) fn source_dimensions(path: &Path) -> Result<(u32, u32)> {
        let source = open_source(path)?;
        let (width, height) = if matches!(source.orientation, 5..=8) {
            (source.height, source.width)
        } else {
            (source.width, source.height)
        };
        Ok((width, height))
    }

    pub(super) fn decode(
        path: &Path,
        max_side: Option<u32>,
        max_pixels: u64,
    ) -> Result<(RgbaImage, (u32, u32))> {
        let source = open_source(path)?;
        check_dimensions(source.width, source.height, max_pixels)?;
        let (width, height) = match max_side {
            Some(side) => scaled_dimensions(source.width, source.height, side.max(1)),
            None => (source.width, source.height),
        };
        let mut pixels = decode_pixels(&source, width, height)?;
        if let Some(orientation) = image::metadata::Orientation::from_exif(source.orientation as u8)
            && source.orientation != 1
        {
            let mut image = DynamicImage::ImageRgba8(pixels);
            image.apply_orientation(orientation);
            pixels = image.into_rgba8();
        }
        let natural = if matches!(source.orientation, 5..=8) {
            (source.height, source.width)
        } else {
            (source.width, source.height)
        };
        Ok((pixels, natural))
    }

    fn scaled_dimensions(width: u32, height: u32, max_side: u32) -> (u32, u32) {
        let long = width.max(height);
        if long <= max_side {
            return (width, height);
        }
        let scale = f64::from(max_side) / f64::from(long);
        (
            (f64::from(width) * scale).round().max(1.0) as u32,
            (f64::from(height) * scale).round().max(1.0) as u32,
        )
    }

    fn decode_pixels(source: &Source, width: u32, height: u32) -> Result<RgbaImage> {
        let bitmap_source = if (width, height) == (source.width, source.height) {
            source
                .frame
                .cast::<IWICBitmapSource>()
                .map_err(|e| MediaError::Decode(format!("query WIC frame source: {e}")))?
        } else {
            let scaler = unsafe { source.factory.CreateBitmapScaler() }
                .map_err(|e| MediaError::Decode(format!("create WIC bitmap scaler: {e}")))?;
            let frame_source = source
                .frame
                .cast::<IWICBitmapSource>()
                .map_err(|e| MediaError::Decode(format!("query WIC frame source: {e}")))?;
            unsafe {
                scaler.Initialize(&frame_source, width, height, WICBitmapInterpolationModeFant)
            }
            .map_err(|e| MediaError::Decode(format!("initialize WIC thumbnail scaler: {e}")))?;
            scaler
                .cast::<IWICBitmapSource>()
                .map_err(|e| MediaError::Decode(format!("query WIC scaled source: {e}")))?
        };
        let converter = unsafe { source.factory.CreateFormatConverter() }
            .map_err(|e| MediaError::Decode(format!("create WIC format converter: {e}")))?;
        unsafe {
            converter.Initialize(
                &bitmap_source,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        }
        .map_err(|e| MediaError::Decode(format!("initialize WIC RGBA converter: {e}")))?;

        let stride = width
            .checked_mul(4)
            .ok_or_else(|| MediaError::corrupt("WIC image row size overflow"))?;
        let byte_len = u64::from(stride) * u64::from(height);
        let byte_len = usize::try_from(byte_len)
            .map_err(|_| MediaError::corrupt("WIC image buffer is too large for this process"))?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(byte_len)
            .map_err(|e| MediaError::Decode(format!("cannot reserve WIC RGBA buffer: {e}")))?;
        rgba.resize(byte_len, 0);
        unsafe { converter.CopyPixels(std::ptr::null(), stride, &mut rgba) }
            .map_err(|e| MediaError::Decode(format!("copy WIC pixels: {e}")))?;
        RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| MediaError::corrupt("WIC returned an invalid RGBA buffer size"))
    }

    pub(super) fn encode_test_fixture(path: &Path, container: TestContainer) -> Result<()> {
        let _apartment = ComApartment::new()?;
        let factory = create_factory()?;
        let format = match container {
            TestContainer::Heif => GUID_ContainerFormatHeif,
            TestContainer::Avif => {
                encoder_format_for_extension(&factory, ".avif")?.ok_or_else(|| {
                    MediaError::Unsupported("AVIF encoder is not available through WIC".into())
                })?
            }
        };
        let encoder = unsafe { factory.CreateEncoder(&format, &GUID::zeroed()) }.map_err(|e| {
            if container == TestContainer::Heif && e.code().0 == WINCODEC_ERR_COMPONENTNOTFOUND {
                MediaError::Unsupported("HEIF WIC encoder is not installed".into())
            } else {
                MediaError::Decode(format!("create WIC test encoder: {e}"))
            }
        })?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| MediaError::io(path, e))?;
        let stream = unsafe { factory.CreateStream() }
            .map_err(|e| MediaError::Decode(format!("create WIC output stream: {e}")))?;
        let filename = wide_path(path);
        unsafe { stream.InitializeFromFilename(PCWSTR(filename.as_ptr()), GENERIC_WRITE.0) }
            .map_err(|e| MediaError::Decode(format!("open WIC test output: {e}")))?;
        unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }
            .map_err(|e| MediaError::Decode(format!("initialize WIC test encoder: {e}")))?;

        let mut frame = None;
        let mut options = None;
        unsafe { encoder.CreateNewFrame(&mut frame, &mut options) }
            .map_err(|e| MediaError::Decode(format!("create WIC test frame: {e}")))?;
        let frame =
            frame.ok_or_else(|| MediaError::Decode("WIC returned no encoder frame".into()))?;
        match options {
            Some(options) => unsafe { frame.Initialize(&options) },
            None => unsafe { frame.Initialize(None::<&IPropertyBag2>) },
        }
        .map_err(|e| MediaError::Decode(format!("initialize WIC test frame: {e}")))?;
        unsafe { frame.SetSize(64, 48) }
            .map_err(|e| MediaError::Decode(format!("set WIC test image dimensions: {e}")))?;
        let mut pixel_format = GUID_WICPixelFormat32bppRGBA;
        unsafe { frame.SetPixelFormat(&mut pixel_format) }
            .map_err(|e| MediaError::Decode(format!("select WIC test pixel format: {e}")))?;

        let mut pixels = Vec::with_capacity(64 * 48 * 4);
        for y in 0..48 {
            for x in 0..64 {
                pixels.extend_from_slice(&[
                    (x * 4) as u8,
                    (y * 5) as u8,
                    if x < 32 { 224 } else { 32 },
                    255,
                ]);
            }
        }
        let bitmap = unsafe {
            factory.CreateBitmapFromMemory(64, 48, &GUID_WICPixelFormat32bppRGBA, 64 * 4, &pixels)
        }
        .map_err(|e| MediaError::Decode(format!("create WIC test bitmap: {e}")))?;
        let converter = unsafe { factory.CreateFormatConverter() }
            .map_err(|e| MediaError::Decode(format!("create WIC test converter: {e}")))?;
        unsafe {
            converter.Initialize(
                &bitmap,
                &pixel_format,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        }
        .map_err(|e| MediaError::Decode(format!("convert WIC test bitmap: {e}")))?;
        unsafe { frame.WriteSource(&converter, std::ptr::null()) }
            .map_err(|e| MediaError::Decode(format!("write WIC test pixels: {e}")))?;
        unsafe { frame.Commit() }
            .map_err(|e| MediaError::Decode(format!("commit WIC test frame: {e}")))?;
        unsafe { encoder.Commit() }
            .map_err(|e| MediaError::Decode(format!("commit WIC test image: {e}")))
    }

    fn encoder_format_for_extension(
        factory: &IWICImagingFactory,
        extension: &str,
    ) -> Result<Option<GUID>> {
        let enumerator: IEnumUnknown = unsafe {
            factory.CreateComponentEnumerator(
                WICEncoder.0 as u32,
                WICComponentEnumerateDefault.0 as u32,
            )
        }
        .map_err(|e| MediaError::Decode(format!("enumerate WIC encoders: {e}")))?;
        loop {
            let mut item: [Option<IUnknown>; 1] = [None];
            let mut fetched = 0;
            let status = unsafe { enumerator.Next(&mut item, Some(&mut fetched)) };
            if status.is_err() || fetched == 0 {
                break;
            }
            let Some(item) = item[0].take() else {
                continue;
            };
            let Ok(info) = item.cast::<IWICBitmapEncoderInfo>() else {
                continue;
            };
            let codec: IWICBitmapCodecInfo = match info.cast() {
                Ok(codec) => codec,
                Err(_) => continue,
            };
            let mut extensions = [0u16; 2048];
            let mut actual = 0;
            if unsafe { codec.GetFileExtensions(&mut extensions, &mut actual) }.is_err() {
                continue;
            }
            let actual = (actual as usize).min(extensions.len());
            let text = String::from_utf16_lossy(&extensions[..actual]).to_ascii_lowercase();
            if text
                .split([',', ';', ' ', '\0'])
                .any(|found| found.trim() == extension)
            {
                return unsafe { codec.GetContainerFormat() }
                    .map(Some)
                    .map_err(|e| MediaError::Decode(format!("read WIC encoder format: {e}")));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{TestContainer, check_dimensions};

    #[test]
    fn huge_fabricated_frame_is_rejected_before_any_pixel_allocation() {
        let result = check_dimensions(u32::MAX, u32::MAX, 200_000_000);
        assert!(matches!(result, Err(crate::MediaError::TooLarge { .. })));
    }

    #[cfg(windows)]
    #[test]
    fn wic_encodes_and_decodes_available_heif_and_avif_fixtures() {
        let dir = tempfile::tempdir().unwrap();
        for (extension, container) in [("heic", TestContainer::Heif), ("avif", TestContainer::Avif)]
        {
            let path = dir.path().join(format!("generated.{extension}"));
            match super::encode_test_fixture(&path, container) {
                Ok(()) => {
                    let full = super::decode_full(&path, 10_000).unwrap();
                    assert_eq!(full.dimensions(), (64, 48));
                    let thumb = super::decode_thumbnail(&path, 16).unwrap();
                    assert!(thumb.width().max(thumb.height()) <= 16);
                }
                Err(crate::MediaError::Unsupported(message)) => {
                    eprintln!("skipped {extension} WIC fixture: {message}");
                }
                Err(error) => panic!("could not generate {extension} WIC fixture: {error}"),
            }
        }
    }
}
