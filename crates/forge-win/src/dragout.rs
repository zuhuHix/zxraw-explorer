//! OLE drag-out of files to other applications.

use std::path::{Path, PathBuf};

use crate::{Result, WinError};

const DRAG_IMAGE_EDGE: u32 = 128;

/// Cached RGBA pixels supplied by the app for a drag preview.
#[derive(Clone, Copy)]
pub struct DragThumbnail<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

/// The final effect selected by the target application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropResult {
    None,
    Copy,
    Move,
    Link,
    Other(u32),
}

/// Returns the active HWND for the calling UI thread (eframe doesn't expose its native
/// window handle from `App::update`).
#[cfg(windows)]
pub fn active_window() -> Result<isize> {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    let hwnd = unsafe { GetActiveWindow() };
    if hwnd.0.is_null() {
        Err(WinError::Invalid(
            "there is no active window on this UI thread".into(),
        ))
    } else {
        Ok(hwnd.0 as isize)
    }
}

#[cfg(not(windows))]
pub fn active_window() -> Result<isize> {
    Err(WinError::Unsupported)
}

/// Runs the modal shell drag on the calling UI thread.
///
/// `allow_move` defaults to false at call sites because accepting MOVE in Explorer on the
/// same volume would otherwise move the user's originals. Copy and link remain available.
#[cfg(windows)]
pub fn drag_out(hwnd: isize, paths: &[PathBuf], allow_move: bool) -> Result<DropResult> {
    drag_out_with_thumbnail(hwnd, paths, allow_move, None)
}

/// Runs the modal shell drag and uses an already-cached app thumbnail when one is available.
#[cfg(windows)]
pub fn drag_out_with_thumbnail(
    hwnd: isize,
    paths: &[PathBuf],
    allow_move: bool,
    thumbnail: Option<DragThumbnail<'_>>,
) -> Result<DropResult> {
    use std::ffi::c_void;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Ole::{
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, IDropSource,
    };
    use windows::Win32::UI::Shell::SHDoDragDrop;

    if hwnd == 0 {
        return Err(WinError::Invalid(
            "drag-out needs a valid window handle".into(),
        ));
    }
    let _apartment = OleApartment::new()?;
    let pidls = parse_existing_paths(paths)?;
    let data = create_data_object(&pidls)?;
    // SHDoDragDrop remains responsible for the OLE source and effect negotiation. The helper
    // adds its bitmap to this shell data object; failure leaves the existing shell fallback in
    // place and must not prevent the file drag itself.
    let _drag_image = match initialize_drag_image(&data, &pidls[0], thumbnail) {
        Ok(image) => Some(image),
        Err(error) => {
            tracing::debug!("could not initialize file drag image: {error}");
            None
        }
    };
    let mut effects = DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0;
    if allow_move {
        effects |= DROPEFFECT_MOVE.0;
    }
    let result = unsafe {
        SHDoDragDrop(
            Some(HWND(hwnd as *mut c_void)),
            &data,
            None::<&IDropSource>,
            DROPEFFECT(effects),
        )
    }
    .map_err(|e| crate::api_err("SHDoDragDrop", e))?;

    Ok(drop_result(result.0))
}

#[cfg(not(windows))]
pub fn drag_out(_hwnd: isize, _paths: &[PathBuf], _allow_move: bool) -> Result<DropResult> {
    Err(WinError::Unsupported)
}

#[cfg(not(windows))]
pub fn drag_out_with_thumbnail(
    _hwnd: isize,
    _paths: &[PathBuf],
    _allow_move: bool,
    _thumbnail: Option<DragThumbnail<'_>>,
) -> Result<DropResult> {
    Err(WinError::Unsupported)
}

/// Builds the same shell data object used by [`drag_out`] and reads its CF_HDROP paths back.
/// This verifies the actual IDataObject contract without starting a modal external drag.
#[cfg(windows)]
pub fn data_object_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let _apartment = OleApartment::new()?;
    let pidls = parse_existing_paths(paths)?;
    let data = create_data_object(&pidls)?;
    read_hdrop_paths(&data)
}

#[cfg(not(windows))]
pub fn data_object_paths(_paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    Err(WinError::Unsupported)
}

#[cfg(windows)]
struct OleApartment;

#[cfg(windows)]
impl OleApartment {
    fn new() -> Result<Self> {
        use windows::Win32::System::Ole::OleInitialize;

        unsafe { OleInitialize(None) }.map_err(|e| crate::api_err("OleInitialize", e))?;
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for OleApartment {
    fn drop(&mut self) {
        use windows::Win32::System::Ole::OleUninitialize;

        unsafe { OleUninitialize() };
    }
}

#[cfg(windows)]
struct Pidl(*mut windows::Win32::UI::Shell::Common::ITEMIDLIST);

#[cfg(windows)]
impl Drop for Pidl {
    fn drop(&mut self) {
        use windows::Win32::System::Com::CoTaskMemFree;

        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

#[cfg(windows)]
fn parse_existing_paths(paths: &[PathBuf]) -> Result<Vec<Pidl>> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::UI::Shell::SHParseDisplayName;
    use windows::core::PCWSTR;

    if paths.is_empty() {
        return Err(WinError::Invalid("drag-out needs at least one path".into()));
    }
    paths
        .iter()
        .map(|path| {
            std::fs::metadata(path).map_err(|source| WinError::Io {
                op: "validate drag-out path",
                path: path.clone(),
                source,
            })?;
            let wide = path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            let mut pidl = std::ptr::null_mut();
            unsafe {
                SHParseDisplayName(
                    PCWSTR(wide.as_ptr()),
                    None::<&windows::Win32::System::Com::IBindCtx>,
                    &mut pidl,
                    0,
                    None,
                )
            }
            .map_err(|e| crate::api_err("SHParseDisplayName", e))?;
            if pidl.is_null() {
                return Err(WinError::Invalid(format!(
                    "the shell returned an empty item id for {}",
                    path.display()
                )));
            }
            Ok(Pidl(pidl))
        })
        .collect()
}

#[cfg(windows)]
fn create_data_object(pidls: &[Pidl]) -> Result<windows::Win32::System::Com::IDataObject> {
    use windows::Win32::System::Com::{IBindCtx, IDataObject};
    use windows::Win32::UI::Shell::{BHID_DataObject, SHCreateShellItemArrayFromIDLists};

    let pointers: Vec<_> = pidls.iter().map(|pidl| pidl.0.cast_const()).collect();
    let items = unsafe { SHCreateShellItemArrayFromIDLists(&pointers) }
        .map_err(|e| crate::api_err("SHCreateShellItemArrayFromIDLists", e))?;
    unsafe { items.BindToHandler::<_, IDataObject>(None::<&IBindCtx>, &BHID_DataObject) }
        .map_err(|e| crate::api_err("IShellItemArray::BindToHandler(BHID_DataObject)", e))
}

#[cfg(windows)]
struct DragPixels {
    width: u32,
    height: u32,
    /// Top-down BGRA pixels with premultiplied color channels.
    bgra: Vec<u8>,
}

#[cfg(windows)]
struct DragBitmap(windows::Win32::Graphics::Gdi::HBITMAP);

#[cfg(windows)]
impl Drop for DragBitmap {
    fn drop(&mut self) {
        use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};

        let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
    }
}

#[cfg(windows)]
fn initialize_drag_image(
    data: &windows::Win32::System::Com::IDataObject,
    first_item: &Pidl,
    thumbnail: Option<DragThumbnail<'_>>,
) -> Result<(DragBitmap, windows::Win32::UI::Shell::IDragSourceHelper2)> {
    use windows::Win32::Foundation::{COLORREF, POINT, SIZE};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        CLSID_DragDropHelper, DSH_ALLOWDROPDESCRIPTIONTEXT, IDragSourceHelper2, SHDRAGIMAGE,
    };

    let pixels = thumbnail
        .and_then(|image| build_drag_pixels(image.width, image.height, image.rgba).ok())
        .or_else(|| shell_icon_pixels(first_item).ok())
        .ok_or_else(|| WinError::Invalid("no image is available for the file drag".into()))?;
    let bitmap = create_drag_bitmap(&pixels)?;
    let helper: IDragSourceHelper2 =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| crate::api_err("CoCreateInstance(CLSID_DragDropHelper)", e))?;
    unsafe { helper.SetFlags(DSH_ALLOWDROPDESCRIPTIONTEXT.0 as u32) }
        .map_err(|e| crate::api_err("IDragSourceHelper2::SetFlags", e))?;
    let image = SHDRAGIMAGE {
        sizeDragImage: SIZE {
            cx: pixels.width as i32,
            cy: pixels.height as i32,
        },
        ptOffset: POINT {
            x: pixels.width as i32 / 2,
            y: pixels.height as i32 / 2,
        },
        hbmpDragImage: bitmap.0,
        crColorKey: COLORREF(0),
    };
    // InitializeFromBitmap premultiplies RGB itself. `create_drag_bitmap` therefore
    // unpremultiplies the tested BGRA buffer at the Win32 boundary before passing the HBITMAP.
    unsafe { helper.InitializeFromBitmap(&image, data) }
        .map_err(|e| crate::api_err("IDragSourceHelper::InitializeFromBitmap", e))?;
    Ok((bitmap, helper))
}

#[cfg(windows)]
fn shell_icon_pixels(first_item: &Pidl) -> Result<DragPixels> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        BITMAP, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateShellItemArrayFromIDLists, SIIGBF, SIIGBF_ICONONLY,
        SIIGBF_SCALEUP,
    };
    use windows::core::Interface;

    let pointers = [first_item.0.cast_const()];
    let items = unsafe { SHCreateShellItemArrayFromIDLists(&pointers) }
        .map_err(|e| crate::api_err("SHCreateShellItemArrayFromIDLists(drag image)", e))?;
    let item = unsafe { items.GetItemAt(0) }
        .map_err(|e| crate::api_err("IShellItemArray::GetItemAt(drag image)", e))?;
    let factory: IShellItemImageFactory = item
        .cast()
        .map_err(|e| crate::api_err("IShellItem::cast(IShellItemImageFactory)", e))?;
    let hbitmap = unsafe {
        factory.GetImage(
            SIZE {
                cx: DRAG_IMAGE_EDGE as i32,
                cy: DRAG_IMAGE_EDGE as i32,
            },
            SIIGBF(SIIGBF_ICONONLY.0 | SIIGBF_SCALEUP.0),
        )
    }
    .map_err(|e| crate::api_err("IShellItemImageFactory::GetImage", e))?;
    let bitmap = DragBitmap(hbitmap);

    let mut info = unsafe { std::mem::zeroed::<BITMAP>() };
    let copied = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut info as *mut BITMAP).cast()),
        )
    };
    if copied != std::mem::size_of::<BITMAP>() as i32 || info.bmWidth <= 0 || info.bmHeight == 0 {
        return Err(WinError::Invalid(
            "the shell returned an invalid drag image bitmap".into(),
        ));
    }
    let width = info.bmWidth as u32;
    let height = info.bmHeight.unsigned_abs();
    let byte_count = pixel_byte_count(width, height)?;
    let mut dib_info = bitmap_info(width, height);
    let mut bgra = vec![0; byte_count];
    let hdc = unsafe { GetDC(None) };
    if hdc.0.is_null() {
        return Err(crate::last_io_err(
            "GetDC for shell drag image",
            Path::new("shell drag image"),
        ));
    }
    let copied_lines = unsafe {
        GetDIBits(
            hdc,
            bitmap.0,
            0,
            height,
            Some(bgra.as_mut_ptr().cast()),
            &mut dib_info,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(None, hdc) };
    if copied_lines != height as i32 {
        return Err(WinError::Invalid(
            "could not read the shell drag image bitmap".into(),
        ));
    }
    // Some shell icons are opaque GDI bitmaps whose unused alpha bytes are zero.
    if bgra.iter().skip(3).step_by(4).all(|alpha| *alpha == 0) {
        for alpha in bgra.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
    }
    let mut rgba = Vec::with_capacity(byte_count);
    for pixel in bgra.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    build_drag_pixels(width, height, &rgba)
}

#[cfg(windows)]
fn build_drag_pixels(width: u32, height: u32, rgba: &[u8]) -> Result<DragPixels> {
    if width == 0 || height == 0 {
        return Err(WinError::Invalid(
            "drag thumbnail dimensions must be nonzero".into(),
        ));
    }
    let source_len = pixel_byte_count(width, height)?;
    if rgba.len() != source_len {
        return Err(WinError::Invalid(
            "drag thumbnail pixel data has the wrong length".into(),
        ));
    }

    let longest = width.max(height) as f64;
    let scale = DRAG_IMAGE_EDGE as f64 / longest;
    let out_width = ((width as f64 * scale).round() as u32).max(1);
    let out_height = ((height as f64 * scale).round() as u32).max(1);
    let mut bgra = vec![0; pixel_byte_count(out_width, out_height)?];
    for y in 0..out_height {
        let source_y = ((y as f64 + 0.5) * height as f64 / out_height as f64 - 0.5)
            .clamp(0.0, height.saturating_sub(1) as f64);
        for x in 0..out_width {
            let source_x = ((x as f64 + 0.5) * width as f64 / out_width as f64 - 0.5)
                .clamp(0.0, width.saturating_sub(1) as f64);
            let rgba_premultiplied =
                sample_premultiplied_rgba(rgba, width, height, source_x as f32, source_y as f32);
            let offset = ((y * out_width + x) * 4) as usize;
            bgra[offset..offset + 4].copy_from_slice(&[
                rgba_premultiplied[2],
                rgba_premultiplied[1],
                rgba_premultiplied[0],
                rgba_premultiplied[3],
            ]);
        }
    }
    Ok(DragPixels {
        width: out_width,
        height: out_height,
        bgra,
    })
}

#[cfg(windows)]
fn sample_premultiplied_rgba(rgba: &[u8], width: u32, height: u32, x: f32, y: f32) -> [u8; 4] {
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let offsets = [
        ((y0 * width + x0) * 4) as usize,
        ((y0 * width + x1) * 4) as usize,
        ((y1 * width + x0) * 4) as usize,
        ((y1 * width + x1) * 4) as usize,
    ];
    let weights = [
        (1.0 - fx) * (1.0 - fy),
        fx * (1.0 - fy),
        (1.0 - fx) * fy,
        fx * fy,
    ];
    let mut result = [0; 4];
    for channel in 0..4 {
        let value = offsets
            .iter()
            .zip(weights)
            .map(|(offset, weight)| {
                let alpha = rgba[offset + 3];
                let value = if channel == 3 {
                    alpha
                } else {
                    ((rgba[offset + channel] as u16 * alpha as u16 + 127) / 255) as u8
                };
                value as f32 * weight
            })
            .sum::<f32>();
        result[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    result
}

#[cfg(windows)]
fn create_drag_bitmap(pixels: &DragPixels) -> Result<DragBitmap> {
    use windows::Win32::Graphics::Gdi::{CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HGDIOBJ};

    let info = bitmap_info(pixels.width, pixels.height);
    let mut bits = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|e| crate::api_err("CreateDIBSection(drag image)", e))?;
    if bits.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        return Err(WinError::Invalid(
            "CreateDIBSection returned no drag image pixels".into(),
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), pixels.bgra.len()) };
    let source_pixels = pixels.bgra.as_chunks::<4>().0;
    let destination_pixels = bytes.as_chunks_mut::<4>().0;
    for (source, destination) in source_pixels.iter().zip(destination_pixels) {
        let alpha = source[3] as u16;
        for channel in 0..3 {
            // IDragSourceHelper::InitializeFromBitmap performs this multiplication itself, so
            // give it straight-alpha BGRA even though our scaled working buffer is premultiplied.
            let unpremultiplied = (source[channel] as u16 * 255 + alpha / 2)
                .checked_div(alpha)
                .unwrap_or(0);
            destination[channel] = unpremultiplied.min(255) as u8;
        }
        destination[3] = source[3];
    }
    Ok(DragBitmap(bitmap))
}

#[cfg(windows)]
fn bitmap_info(width: u32, height: u32) -> windows::Win32::Graphics::Gdi::BITMAPINFO {
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER};

    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(windows)]
fn pixel_byte_count(width: u32, height: u32) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| WinError::Invalid("drag image dimensions overflow".into()))
}

#[cfg(windows)]
struct Medium(windows::Win32::System::Com::STGMEDIUM);

#[cfg(windows)]
impl Drop for Medium {
    fn drop(&mut self) {
        use windows::Win32::System::Ole::ReleaseStgMedium;

        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

#[cfg(windows)]
struct LockedGlobal(windows::Win32::Foundation::HGLOBAL);

#[cfg(windows)]
impl Drop for LockedGlobal {
    fn drop(&mut self) {
        use windows::Win32::System::Memory::GlobalUnlock;

        let _ = unsafe { GlobalUnlock(self.0) };
    }
}

#[cfg(windows)]
fn read_hdrop_paths(data: &windows::Win32::System::Com::IDataObject) -> Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;

    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = Medium(
        unsafe { data.GetData(&format) }.map_err(|e| crate::api_err("IDataObject::GetData", e))?,
    );
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err(WinError::Invalid(
            "CF_HDROP did not use a global-memory medium".into(),
        ));
    }
    let global = unsafe { medium.0.u.hGlobal };
    if unsafe { GlobalSize(global) } == 0 {
        return Err(WinError::Invalid(
            "CF_HDROP has an empty global block".into(),
        ));
    }
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        return Err(crate::last_io_err(
            "GlobalLock CF_HDROP",
            Path::new("CF_HDROP"),
        ));
    }
    let _locked = LockedGlobal(global);
    let hdrop = HDROP(ptr);
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    if count == 0 {
        return Err(WinError::Invalid("CF_HDROP contains no paths".into()));
    }
    let mut result = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { DragQueryFileW(hdrop, index, None) } as usize;
        let mut wide = vec![0; length.saturating_add(1)];
        let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut wide)) } as usize;
        if copied == 0 || copied > length {
            return Err(WinError::Invalid(
                "DragQueryFileW returned an invalid path length".into(),
            ));
        }
        result.push(PathBuf::from(std::ffi::OsString::from_wide(
            &wide[..copied],
        )));
    }
    Ok(result)
}

#[cfg(windows)]
fn drop_result(value: u32) -> DropResult {
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};

    match value {
        x if x == DROPEFFECT_COPY.0 => DropResult::Copy,
        x if x == DROPEFFECT_MOVE.0 => DropResult::Move,
        x if x == DROPEFFECT_LINK.0 => DropResult::Link,
        0 => DropResult::None,
        other => DropResult::Other(other),
    }
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::{bitmap_info, build_drag_pixels, data_object_paths};

    #[test]
    fn drag_dib_fits_within_128_pixels_and_uses_a_top_down_32_bit_header() {
        let image =
            build_drag_pixels(3, 1, &[10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255]).unwrap();
        let info = bitmap_info(image.width, image.height);

        assert_eq!(image.width, 128);
        assert!(image.height > 0 && image.height <= 128);
        assert_eq!(info.bmiHeader.biWidth, image.width as i32);
        assert_eq!(info.bmiHeader.biHeight, -(image.height as i32));
        assert_eq!(info.bmiHeader.biBitCount, 32);
    }

    #[test]
    fn drag_dib_pixels_are_premultiplied_bgra_in_top_down_row_order() {
        let rgba = [
            200, 100, 50, 128, 200, 100, 50, 128, // red-tinted top row
            0, 0, 255, 64, 0, 0, 255, 64, // blue bottom row
        ];
        let image = build_drag_pixels(2, 2, &rgba).unwrap();

        assert_eq!(&image.bgra[..4], &[25, 50, 100, 128]);
        let bottom = ((image.height - 1) * image.width * 4) as usize;
        assert_eq!(&image.bgra[bottom..bottom + 4], &[64, 0, 0, 64]);
    }

    #[test]
    fn shell_data_object_round_trips_unicode_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            dir.path().join("photo with spaces.jpg"),
            dir.path().join("München 漢字.png"),
        ];
        for path in &paths {
            std::fs::write(path, b"test").unwrap();
        }
        assert_eq!(data_object_paths(&paths).unwrap(), paths);
    }
}
