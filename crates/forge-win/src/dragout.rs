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

/// One file or folder to drag out, with what the caller already knows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragItem {
    pub path: PathBuf,
    /// Kind as the caller's listing showed it (`Some(true)` = folder). `None` means unknown;
    /// only then is a local path statted to give a clear error for a vanished item.
    pub is_dir: Option<bool>,
    /// The caller knows this path is on a network share (for example a mapped network drive,
    /// which only the drive type reveals). UNC paths are also recognized here lexically.
    pub on_network: bool,
}

impl DragItem {
    /// An item with no caller knowledge: its kind is unknown and only UNC paths count as
    /// network paths.
    pub fn unknown(path: PathBuf) -> Self {
        Self {
            path,
            is_dir: None,
            on_network: false,
        }
    }
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
/// The kind of each path is unknown, so local paths are statted; prefer [`drag_out_items`].
#[cfg(windows)]
pub fn drag_out_with_thumbnail(
    hwnd: isize,
    paths: &[PathBuf],
    allow_move: bool,
    thumbnail: Option<DragThumbnail<'_>>,
) -> Result<DropResult> {
    let items: Vec<_> = paths.iter().cloned().map(DragItem::unknown).collect();
    drag_out_items(hwnd, &items, allow_move, thumbnail)
}

/// Runs the modal shell drag for `items` on the calling UI thread, which it must not freeze
/// on a slow or offline share.
///
/// * **Network drags** (any item with `on_network` set, or any UNC path): nothing touches
///   the share before the drag starts. The data object is an empty shell data object given
///   only a `CF_HDROP` block built from the path strings. There is no `metadata`, no
///   `SHParseDisplayName` and no shell item array, since those resolve each path on the
///   server and block until Windows gives up. The drag image is the app's cached thumbnail
///   when one is supplied, or else a stock file or folder icon from the local system
///   resources; the shell is never asked for the remote item's icon. The drop target reads
///   the paths and does any remote work itself.
/// * **Local drags**: each path is parsed to a shell item id list and the data object
///   comes from a shell item array, so targets get the full set of shell formats. A path is
///   statted only when its kind is unknown (`is_dir == None`). Without a thumbnail, the
///   drag image is the shell icon of the first item.
#[cfg(windows)]
pub fn drag_out_items(
    hwnd: isize,
    items: &[DragItem],
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
    let (data, pidls) = build_data_object(items)?;
    // SHDoDragDrop remains responsible for the OLE source and effect negotiation. The helper
    // adds its bitmap to this shell data object; failure leaves the existing shell fallback in
    // place and must not prevent the file drag itself.
    let first_is_dir = items.first().and_then(|item| item.is_dir).unwrap_or(false);
    let _drag_image = match initialize_drag_image(&data, pidls.first(), first_is_dir, thumbnail) {
        Ok(helper) => Some(helper),
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

#[cfg(not(windows))]
pub fn drag_out_items(
    _hwnd: isize,
    _items: &[DragItem],
    _allow_move: bool,
    _thumbnail: Option<DragThumbnail<'_>>,
) -> Result<DropResult> {
    Err(WinError::Unsupported)
}

/// Builds the same shell data object used by [`drag_out`] and reads its CF_HDROP paths back.
/// This verifies the actual IDataObject contract without starting a modal external drag.
#[cfg(windows)]
pub fn data_object_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let items: Vec<_> = paths.iter().cloned().map(DragItem::unknown).collect();
    data_object_item_paths(&items)
}

/// Like [`data_object_paths`], for the data object [`drag_out_items`] would build.
#[cfg(windows)]
pub fn data_object_item_paths(items: &[DragItem]) -> Result<Vec<PathBuf>> {
    let _apartment = OleApartment::new()?;
    let (data, _pidls) = build_data_object(items)?;
    read_hdrop_paths(&data)
}

#[cfg(not(windows))]
pub fn data_object_paths(_paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    Err(WinError::Unsupported)
}

#[cfg(not(windows))]
pub fn data_object_item_paths(_items: &[DragItem]) -> Result<Vec<PathBuf>> {
    Err(WinError::Unsupported)
}

/// How the drag's data object is built.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataSource {
    /// Shell item array from parsed id lists (local paths only; resolves each path).
    ShellItems,
    /// `CF_HDROP` built from the path strings alone (no file system or network access).
    HdropOnly,
}

/// The pure decision of what `build_data_object` does with each item.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct DragPlan {
    source: DataSource,
    /// Whether item `i` is statted before the drag.
    stat: Vec<bool>,
}

#[cfg(windows)]
fn plan_drag(items: &[DragItem]) -> Result<DragPlan> {
    if items.is_empty() {
        return Err(WinError::Invalid("drag-out needs at least one path".into()));
    }
    if items
        .iter()
        .any(|item| item.on_network || is_unc_path(&item.path))
    {
        return Ok(DragPlan {
            source: DataSource::HdropOnly,
            stat: vec![false; items.len()],
        });
    }
    Ok(DragPlan {
        source: DataSource::ShellItems,
        stat: items.iter().map(|item| item.is_dir.is_none()).collect(),
    })
}

/// Lexical: `\\server\share\...` or `\\?\UNC\server\share\...`.
#[cfg(windows)]
fn is_unc_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

/// Bytes of a wide-character `DROPFILES` block for `paths`: the 20-byte header followed by
/// NUL-terminated UTF-16 paths and a final NUL. Pure: it never looks at the files.
#[cfg(windows)]
fn hdrop_bytes(paths: &[&Path]) -> Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;

    const HEADER: u32 = 20;
    if paths.is_empty() {
        return Err(WinError::Invalid("drag-out needs at least one path".into()));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&HEADER.to_le_bytes()); // pFiles
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bytes.extend_from_slice(&0i32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1i32.to_le_bytes()); // fWide
    for path in paths {
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.is_empty() || wide.contains(&0) {
            return Err(WinError::Invalid(format!(
                "cannot drag out the path {}",
                path.display()
            )));
        }
        for unit in wide.into_iter().chain(Some(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    Ok(bytes)
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

/// Builds the drag's data object following [`plan_drag`]. The returned id lists are empty
/// for an HDROP-only object.
#[cfg(windows)]
fn build_data_object(
    items: &[DragItem],
) -> Result<(windows::Win32::System::Com::IDataObject, Vec<Pidl>)> {
    let plan = plan_drag(items)?;
    match plan.source {
        DataSource::HdropOnly => {
            let paths: Vec<&Path> = items.iter().map(|item| item.path.as_path()).collect();
            Ok((create_hdrop_data_object(&paths)?, Vec::new()))
        }
        DataSource::ShellItems => {
            let pidls = parse_paths(items, &plan.stat)?;
            Ok((create_data_object(&pidls)?, pidls))
        }
    }
}

#[cfg(windows)]
fn parse_paths(items: &[DragItem], stat: &[bool]) -> Result<Vec<Pidl>> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::UI::Shell::SHParseDisplayName;
    use windows::core::PCWSTR;

    items
        .iter()
        .zip(stat)
        .map(|(item, &stat)| {
            let path = &item.path;
            if stat {
                std::fs::metadata(path).map_err(|source| WinError::Io {
                    op: "validate drag-out path",
                    path: path.clone(),
                    source,
                })?;
            }
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

/// An empty shell data object (no id lists, so nothing is resolved) given a `CF_HDROP`
/// block built from the path strings. The shell object also stores the drag image helper's
/// private formats, which a hand-written `IDataObject` would have to support itself.
#[cfg(windows)]
fn create_hdrop_data_object(paths: &[&Path]) -> Result<windows::Win32::System::Com::IDataObject> {
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::Com::{
        DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::SHCreateDataObject;

    let bytes = hdrop_bytes(paths)?;
    let data: IDataObject = unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) }
        .map_err(|e| crate::api_err("SHCreateDataObject", e))?;
    let global = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }
        .map_err(|e| crate::api_err("GlobalAlloc(CF_HDROP)", e))?;
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        let error = crate::last_io_err("GlobalLock CF_HDROP", Path::new("CF_HDROP"));
        let _ = unsafe { GlobalFree(Some(global)) };
        return Err(error);
    }
    {
        let _locked = LockedGlobal(global);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len()) };
    }
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: global },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    };
    // With fRelease = TRUE the data object owns the block once SetData succeeds.
    if let Err(e) = unsafe { data.SetData(&format, &medium, true) } {
        let _ = unsafe { GlobalFree(Some(global)) };
        return Err(crate::api_err("IDataObject::SetData(CF_HDROP)", e));
    }
    Ok(data)
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
    first_item: Option<&Pidl>,
    first_is_dir: bool,
    thumbnail: Option<DragThumbnail<'_>>,
) -> Result<windows::Win32::UI::Shell::IDragSourceHelper2> {
    use windows::Win32::Foundation::{COLORREF, POINT, SIZE};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        CLSID_DragDropHelper, DSH_ALLOWDROPDESCRIPTIONTEXT, IDragSourceHelper2, SHDRAGIMAGE,
    };

    // Without an id list (network drags) the shell is not asked for the item's icon, which
    // would resolve it on the server; a local stock icon stands in instead.
    let pixels = thumbnail
        .and_then(|image| build_drag_pixels(image.width, image.height, image.rgba).ok())
        .or_else(|| match first_item {
            Some(pidl) => shell_icon_pixels(pidl).ok(),
            None => stock_icon_pixels(first_is_dir).ok(),
        })
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
    // On success the helper owns the bitmap and frees it when the drag ends; deleting it here
    // too would free a handle Windows may already have reused. On failure `bitmap` drops above.
    std::mem::forget(bitmap);
    Ok(helper)
}

#[cfg(windows)]
fn shell_icon_pixels(first_item: &Pidl) -> Result<DragPixels> {
    use windows::Win32::Foundation::SIZE;
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
    bitmap_pixels(&DragBitmap(hbitmap))
}

/// A stock file or folder icon from the local system resources. It names no path, so it
/// cannot touch a share.
#[cfg(windows)]
fn stock_icon_pixels(is_dir: bool) -> Result<DragPixels> {
    use windows::Win32::UI::Shell::{
        SHDefExtractIconW, SHGSI_ICONLOCATION, SHGetStockIconInfo, SHSTOCKICONINFO,
        SIID_DOCNOASSOC, SIID_FOLDER,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
    use windows::core::PCWSTR;

    let mut info = SHSTOCKICONINFO {
        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
        ..Default::default()
    };
    let id = if is_dir { SIID_FOLDER } else { SIID_DOCNOASSOC };
    unsafe { SHGetStockIconInfo(id, SHGSI_ICONLOCATION, &mut info) }
        .map_err(|e| crate::api_err("SHGetStockIconInfo", e))?;
    let mut icon = HICON::default();
    unsafe {
        SHDefExtractIconW(
            PCWSTR(info.szPath.as_ptr()),
            info.iIcon,
            0,
            Some(&mut icon),
            None,
            DRAG_IMAGE_EDGE,
        )
    }
    .ok()
    .map_err(|e| crate::api_err("SHDefExtractIconW", e))?;
    if icon.0.is_null() {
        return Err(WinError::Invalid("the stock drag icon is empty".into()));
    }
    let mut parts = ICONINFO::default();
    let got = unsafe { GetIconInfo(icon, &mut parts) };
    let _ = unsafe { DestroyIcon(icon) };
    got.map_err(|e| crate::api_err("GetIconInfo", e))?;
    let _mask = DragBitmap(parts.hbmMask);
    if parts.hbmColor.0.is_null() {
        return Err(WinError::Invalid(
            "the stock drag icon has no color bitmap".into(),
        ));
    }
    bitmap_pixels(&DragBitmap(parts.hbmColor))
}

/// Reads a GDI bitmap into drag pixels.
#[cfg(windows)]
fn bitmap_pixels(bitmap: &DragBitmap) -> Result<DragPixels> {
    use windows::Win32::Graphics::Gdi::{
        BITMAP, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC,
    };

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
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{
        DataSource, DragItem, bitmap_info, build_drag_pixels, data_object_item_paths,
        data_object_paths, hdrop_bytes, is_unc_path, plan_drag, stock_icon_pixels,
    };

    fn item(path: &str, is_dir: Option<bool>, on_network: bool) -> DragItem {
        DragItem {
            path: PathBuf::from(path),
            is_dir,
            on_network,
        }
    }

    #[test]
    fn network_paths_take_the_no_probe_branch() {
        // UNC is recognized lexically even when the caller did not flag it.
        for path in [r"\\server\share\a.jpg", r"\\?\UNC\server\share\a.jpg"] {
            let plan = plan_drag(&[item(path, None, false)]).unwrap();
            assert_eq!(plan.source, DataSource::HdropOnly, "{path}");
            assert_eq!(plan.stat, vec![false]);
        }
        // A mapped drive is only known to the caller; one network item switches the whole
        // drag, and nothing is statted even when the kind is unknown.
        let plan = plan_drag(&[
            item(r"C:\local.jpg", None, false),
            item(r"Z:\remote.jpg", None, true),
        ])
        .unwrap();
        assert_eq!(plan.source, DataSource::HdropOnly);
        assert_eq!(plan.stat, vec![false, false]);
    }

    #[test]
    fn known_kinds_are_not_statted() {
        let plan = plan_drag(&[
            item(r"C:\photos\a.jpg", Some(false), false),
            item(r"C:\photos\album", Some(true), false),
            item(r"C:\photos\unknown.jpg", None, false),
        ])
        .unwrap();
        assert_eq!(plan.source, DataSource::ShellItems);
        assert_eq!(plan.stat, vec![false, false, true]);
        assert!(plan_drag(&[]).is_err());
    }

    #[test]
    fn unc_detection_is_lexical_and_ignores_local_verbatim_paths() {
        assert!(is_unc_path(Path::new(r"\\nas\media\x")));
        assert!(!is_unc_path(Path::new(r"\\?\C:\x")));
        assert!(!is_unc_path(Path::new(r"C:\x")));
    }

    #[test]
    fn hdrop_block_is_a_wide_dropfiles_with_double_nul_terminator() {
        let bytes = hdrop_bytes(&[Path::new(r"\\s\x\a"), Path::new("b")]).unwrap();
        assert_eq!(&bytes[..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1i32.to_le_bytes());
        let wide: Vec<u16> = bytes[20..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let expected: Vec<u16> = "\\\\s\\x\\a\0b\0\0".encode_utf16().collect();
        assert_eq!(wide, expected);
        assert!(hdrop_bytes(&[]).is_err());
    }

    #[test]
    fn unroutable_share_builds_its_data_object_without_waiting() {
        // TEST-NET-1 is never routable: any probe of it would block for seconds.
        let paths = vec![
            PathBuf::from(r"\\192.0.2.1\x\a.jpg"),
            PathBuf::from(r"\\192.0.2.1\x\folder"),
        ];
        let started = Instant::now();
        let read_back = data_object_paths(&paths).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(read_back, paths);
        assert!(elapsed < Duration::from_millis(250), "took {elapsed:?}");
    }

    #[test]
    fn flagged_network_items_are_neither_statted_nor_parsed() {
        // A mapped-drive path the caller flagged is never statted or parsed, so even a path
        // that does not exist builds (the drop target reports it, as Explorer would).
        let path = r"C:\zx-missing-drag-test\nope.jpg";
        assert_eq!(
            data_object_item_paths(&[item(path, Some(false), true)]).unwrap(),
            vec![PathBuf::from(path)]
        );
    }

    #[test]
    fn known_kind_local_items_skip_the_stat_but_still_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, b"x").unwrap();
        let items = [
            DragItem {
                path: file.clone(),
                is_dir: Some(false),
                on_network: false,
            },
            DragItem {
                path: dir.path().to_path_buf(),
                is_dir: Some(true),
                on_network: false,
            },
        ];
        assert_eq!(
            data_object_item_paths(&items).unwrap(),
            vec![file, dir.path().to_path_buf()]
        );
    }

    #[test]
    fn stock_icons_give_a_drag_image_for_files_and_folders() {
        for is_dir in [false, true] {
            let pixels = stock_icon_pixels(is_dir).unwrap();
            assert!(pixels.width == 128 || pixels.height == 128);
            assert!(
                pixels
                    .bgra
                    .iter()
                    .skip(3)
                    .step_by(4)
                    .any(|alpha| *alpha > 0)
            );
        }
    }

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
