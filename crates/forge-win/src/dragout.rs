//! OLE drag-out of files to other applications.

use std::path::{Path, PathBuf};

use crate::{Result, WinError};

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
    use super::data_object_paths;

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
