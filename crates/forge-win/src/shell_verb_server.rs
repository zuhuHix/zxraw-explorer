//! The COM local server behind the ZXraw menu's `DropTarget` verbs ([`crate::archive_menu`]).
//!
//! Explorer runs a `DropTarget` verb by creating the verb's COM class and dropping the whole
//! selection on it (`IDropTarget::DragEnter`, then `Drop`). The class is registered per user
//! with `LocalServer32 = "zxraw.exe" --shell-verb <name>`, so COM starts ZXraw Explorer with
//! `--shell-verb <name> -Embedding`. [`serve_drop`] registers the class object, pumps
//! messages until Explorer dropped the items (or the wait ran out), and returns their paths.
//! The caller then handles them like `zxraw.exe --<name> <paths>`: handed to the running
//! window over the instance pipe, or run in a new window.

use std::path::PathBuf;
use std::time::Duration;

use crate::archive_menu::MenuVerb;
use crate::{Result, WinError};

/// Whether `args` (without the program name) are a COM activation of a menu verb: the
/// [`crate::archive_menu::SHELL_VERB_SWITCH`] and a verb name, then COM's `-Embedding`.
pub fn verb_of_activation(args: &[std::ffi::OsString]) -> Option<MenuVerb> {
    let mut args = args.iter().filter_map(|a| a.to_str());
    if args.next()? != crate::archive_menu::SHELL_VERB_SWITCH {
        return None;
    }
    let verb = MenuVerb::from_name(args.next()?)?;
    let embedding = args.next()?;
    let embedding = embedding.trim_start_matches(['-', '/']);
    (embedding.eq_ignore_ascii_case("Embedding") && args.next().is_none()).then_some(verb)
}

/// Shows `text` in a modal error box titled "ZXraw Explorer", for a menu process that has no
/// window of its own. Returns when the user closes it.
pub fn show_error(text: &str) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
        };
        use windows::core::HSTRING;
        // SAFETY: plain call with owned strings and no owner window.
        unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(text),
                &HSTRING::from("ZXraw Explorer"),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = text;
}

/// What Explorer dropped on a menu command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The selected items' file system paths.
    pub paths: Vec<PathBuf>,
    /// Display names of selected items that have no file system path (a library, a phone,
    /// "This PC"). The command must not run on the rest alone.
    pub unresolved: Vec<String>,
}

impl Dropped {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.unresolved.is_empty()
    }
}

/// Serves `verb`'s class until Explorer dropped the selection on it, at most `wait`.
/// Returns what was dropped (empty when nothing came). Initializes COM on this thread for
/// the duration and leaves it uninitialized again.
pub fn serve_drop(verb: MenuVerb, wait: Duration) -> Result<Dropped> {
    #[cfg(windows)]
    {
        imp::serve(verb, wait)
    }
    #[cfg(not(windows))]
    {
        let _ = (verb, wait);
        Err(WinError::Unsupported)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, CloseHandle, HANDLE, POINTL};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent};
    use windows::core::PCWSTR;

    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoInitializeEx, CoRegisterClassObject,
        CoRevokeClassObject, CoTaskMemFree, CoUninitialize, IClassFactory, IClassFactory_Impl,
        IDataObject, REGCLS_SINGLEUSE, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Ole::{CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, IDropTarget, IDropTarget_Impl};
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::{
        IShellItemArray, SHCreateShellItemArrayFromDataObject, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        TranslateMessage,
    };
    use windows::core::{BOOL, GUID, IUnknown, Interface, Ref, implement};

    use super::{Dropped, MenuVerb, Result, WinError};

    #[derive(Default)]
    struct State {
        items: Dropped,
        dropped: bool,
        /// The event `serve` waits on, as a raw handle value.
        wake: isize,
    }

    type Shared = Arc<Mutex<State>>;

    fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
        shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[implement(IClassFactory)]
    struct Factory {
        shared: Shared,
    }

    #[allow(non_snake_case)]
    impl IClassFactory_Impl for Factory_Impl {
        fn CreateInstance(
            &self,
            outer: Ref<IUnknown>,
            iid: *const GUID,
            object: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            if object.is_null() {
                return Err(windows::Win32::Foundation::E_POINTER.into());
            }
            // SAFETY: COM passes a valid out pointer.
            unsafe { *object = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let target: IDropTarget = Target {
                shared: self.shared.clone(),
            }
            .into();
            // SAFETY: `iid` and `object` come from COM's CreateInstance call.
            unsafe { target.query(iid, object) }.ok()
        }

        fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }

    #[implement(IDropTarget)]
    struct Target {
        shared: Shared,
    }

    fn answer_copy(effect: *mut DROPEFFECT) {
        if !effect.is_null() {
            // SAFETY: OLE passes a valid in/out DROPEFFECT pointer.
            unsafe { *effect = DROPEFFECT_COPY };
        }
    }

    #[allow(non_snake_case)]
    impl IDropTarget_Impl for Target_Impl {
        fn DragEnter(
            &self,
            data: Ref<IDataObject>,
            _keys: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            if let Some(data) = data.as_ref() {
                let items = read_items(data);
                if !items.is_empty() {
                    lock(&self.shared).items = items;
                }
            }
            answer_copy(effect);
            Ok(())
        }

        fn DragOver(
            &self,
            _keys: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            answer_copy(effect);
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn Drop(
            &self,
            data: Ref<IDataObject>,
            _keys: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            let items = data.as_ref().map(read_items).unwrap_or_default();
            let mut state = lock(&self.shared);
            if !items.is_empty() {
                state.items = items;
            }
            state.dropped = true;
            let wake = state.wake;
            drop(state);
            // Wakes the pump even when the call did not come as a message (an agile object
            // called directly from another apartment of this process).
            // SAFETY: the event lives until `serve` returns, after the class is revoked.
            unsafe {
                let _ = SetEvent(HANDLE(wake as *mut c_void));
            }
            answer_copy(effect);
            Ok(())
        }
    }

    /// A shell item's display name of kind `sigdn`.
    fn display_name(
        item: &windows::Win32::UI::Shell::IShellItem,
        sigdn: windows::Win32::UI::Shell::SIGDN,
    ) -> Option<String> {
        // SAFETY: a live shell item; the returned CoTaskMem string is freed here.
        let name = unsafe { item.GetDisplayName(sigdn) }.ok()?;
        let text = unsafe { name.to_string() };
        unsafe { CoTaskMemFree(Some(name.0 as *const c_void)) };
        text.ok().filter(|t| !t.is_empty())
    }

    /// The items a data object carries: through its shell items (covers search results and
    /// libraries), else its `CF_HDROP` list. An item without a file system path is listed in
    /// `unresolved`, never skipped silently.
    pub(super) fn read_items(data: &IDataObject) -> Dropped {
        let from_items = (|| -> windows::core::Result<Dropped> {
            // SAFETY: plain COM calls on a live data object.
            let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromDataObject(data) }?;
            let count = unsafe { items.GetCount() }?;
            let mut out = Dropped::default();
            for i in 0..count {
                let item = unsafe { items.GetItemAt(i) }?;
                match display_name(&item, SIGDN_FILESYSPATH) {
                    Some(path) => out.paths.push(PathBuf::from(path)),
                    None => out.unresolved.push(
                        display_name(&item, SIGDN_NORMALDISPLAY)
                            .unwrap_or_else(|| format!("item {}", i + 1)),
                    ),
                }
            }
            Ok(out)
        })();
        match from_items {
            Ok(items) if !items.is_empty() => items,
            _ => Dropped {
                paths: read_hdrop(data),
                unresolved: Vec::new(),
            },
        }
    }

    fn read_hdrop(data: &IDataObject) -> Vec<PathBuf> {
        use crate::virtual_files::{Medium, hglobal_format};

        // SAFETY: `hglobal_format` is a valid FORMATETC; the medium is owned by the guard.
        let Ok(medium) = unsafe { data.GetData(&hglobal_format(CF_HDROP.0)) }.map(Medium) else {
            return Vec::new();
        };
        if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
            return Vec::new();
        }
        // SAFETY: the medium says the union holds a global handle.
        let global = unsafe { medium.0.u.hGlobal };
        crate::clipboard::read_hdrop(HANDLE(global.0)).unwrap_or_default()
    }

    struct Com;

    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: balances the successful CoInitializeEx below.
            unsafe { CoUninitialize() };
        }
    }

    pub(super) fn serve(verb: MenuVerb, wait: Duration) -> Result<Dropped> {
        let clsid = GUID::try_from(verb.clsid().trim_matches(['{', '}']))
            .map_err(|_| WinError::Invalid(format!("bad class id {}", verb.clsid())))?;
        // SAFETY: once per thread, balanced by `Com`.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|e| crate::api_err("CoInitializeEx", e))?;
        let _com = Com;
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| crate::api_err("CreateEventW", e))?;
        struct Event(HANDLE);
        impl Drop for Event {
            fn drop(&mut self) {
                // SAFETY: created above, closed once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let event = Event(event);
        let shared: Shared = Arc::default();
        lock(&shared).wake = event.0.0 as isize;
        let factory: IClassFactory = Factory {
            shared: shared.clone(),
        }
        .into();
        let unknown: IUnknown = factory.cast().map_err(|e| crate::api_err("QueryInterface", e))?;
        // Single use: a second invocation while this one runs starts its own process.
        let cookie = unsafe {
            CoRegisterClassObject(&clsid, &unknown, CLSCTX_LOCAL_SERVER, REGCLS_SINGLEUSE)
        }
        .map_err(|e| crate::api_err("CoRegisterClassObject", e))?;
        let deadline = Instant::now() + wait;
        while !lock(&shared).dropped {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::warn!("Explorer started the {} menu command but dropped nothing", verb.name());
                break;
            }
            let ms = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            // SAFETY: a live event; also wakes on any message for this thread.
            unsafe { MsgWaitForMultipleObjects(Some(&[event.0]), false, ms, QS_ALLINPUT) };
            let mut msg = MSG::default();
            // SAFETY: standard message pump on this thread's queue.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        // SAFETY: the cookie came from the registration above.
        if let Err(e) = unsafe { CoRevokeClassObject(cookie) } {
            tracing::debug!("CoRevokeClassObject failed: {e}");
        }
        drop(unknown);
        drop(factory);
        drop(event);
        let items = std::mem::take(&mut lock(&shared).items);
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<std::ffi::OsString> {
        list.iter().map(Into::into).collect()
    }

    #[test]
    fn only_a_com_activation_of_a_known_verb_is_recognised() {
        assert_eq!(
            verb_of_activation(&args(&["--shell-verb", "add-zip", "-Embedding"])),
            Some(MenuVerb::AddZip)
        );
        assert_eq!(
            verb_of_activation(&args(&["--shell-verb", "extract-here", "/Embedding"])),
            Some(MenuVerb::ExtractHere)
        );
        for bad in [
            &["--shell-verb", "add-zip"][..],
            &["--shell-verb", "nope", "-Embedding"],
            &["--shell-verb", "add-zip", "-Embedding", "C:\\x"],
            &["C:\\x"],
            &[],
        ] {
            assert_eq!(verb_of_activation(&args(bad)), None, "{bad:?}");
        }
    }

    /// The class object registers and the wait ends on time when nothing is dropped.
    #[cfg(windows)]
    #[test]
    fn serving_without_a_drop_returns_no_paths_after_the_wait() {
        let started = std::time::Instant::now();
        let dropped = serve_drop(MenuVerb::Open, Duration::from_millis(300)).unwrap();
        assert!(dropped.is_empty());

        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The way Explorer runs a verb: create the class out of process and drop a data object
    /// holding several items on it. One drop carries every item. Nothing is registered in the
    /// registry: COM finds the class object this process registered.
    #[cfg(windows)]
    #[test]
    fn a_drop_on_the_class_delivers_every_selected_path_at_once() {
        use windows::Win32::System::Com::{
            CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize, IDataObject,
        };
        use windows::Win32::System::Ole::{DROPEFFECT_COPY, IDropTarget};
        use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
        use windows::Win32::UI::Shell::Common::ITEMIDLIST;
        use windows::Win32::UI::Shell::{
            BHID_DataObject, IShellItemArray, SHCreateShellItemArrayFromIDLists,
            SHParseDisplayName,
        };
        use windows::core::{GUID, HSTRING};

        let dir = tempfile::tempdir().unwrap();
        let files: Vec<PathBuf> = (0..3)
            .map(|i| {
                let path = dir.path().join(format!("item {i}.txt"));
                std::fs::write(&path, b"x").unwrap();
                path
            })
            .collect();
        let verb = MenuVerb::AddZip;
        let server = std::thread::spawn(move || serve_drop(verb, Duration::from_secs(20)));
        let clsid = GUID::try_from(verb.clsid().trim_matches(['{', '}'])).unwrap();
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().unwrap();
        let target: IDropTarget = {
            let started = std::time::Instant::now();
            loop {
                match unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) } {
                    Ok(target) => break target,
                    Err(e) if started.elapsed() < Duration::from_secs(10) => {
                        let _ = e;
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => panic!("the class never registered: {e}"),
                }
            }
        };
        let mut pidls = Vec::new();
        for file in &files {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            unsafe {
                SHParseDisplayName(&HSTRING::from(file.as_os_str()), None, &mut pidl, 0, None)
            }
            .unwrap();
            pidls.push(pidl as *const ITEMIDLIST);
        }
        let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromIDLists(&pidls) }.unwrap();
        let data: IDataObject = unsafe { items.BindToHandler(None, &BHID_DataObject) }.unwrap();
        let mut effect = DROPEFFECT_COPY;
        let point = windows::Win32::Foundation::POINTL::default();
        unsafe {
            target
                .DragEnter(&data, MODIFIERKEYS_FLAGS(0), point, &mut effect)
                .unwrap();
            target
                .Drop(&data, MODIFIERKEYS_FLAGS(0), point, &mut effect)
                .unwrap();
        }
        drop(target);
        drop(data);
        drop(items);
        for pidl in pidls {
            unsafe { CoTaskMemFree(Some(pidl as *const std::ffi::c_void)) };
        }
        unsafe { CoUninitialize() };
        let got = server.join().unwrap().unwrap();
        assert!(got.unresolved.is_empty());
        let name = |p: &PathBuf| p.file_name().unwrap().to_owned();
        assert_eq!(
            got.paths.iter().map(name).collect::<Vec<_>>(),
            files.iter().map(name).collect::<Vec<_>>()
        );
    }

    /// An item without a file system path ("This PC") is reported by name, not skipped.
    #[cfg(windows)]
    #[test]
    fn an_item_without_a_file_system_path_is_reported_not_skipped() {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IDataObject,
        };
        use windows::Win32::UI::Shell::Common::ITEMIDLIST;
        use windows::Win32::UI::Shell::{
            BHID_DataObject, IShellItemArray, SHCreateShellItemArrayFromIDLists,
            SHParseDisplayName,
        };
        use windows::core::HSTRING;

        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().unwrap();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe {
            SHParseDisplayName(
                &HSTRING::from("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
                None,
                &mut pidl,
                0,
                None,
            )
        }
        .unwrap();
        let dropped = {
            let items: IShellItemArray =
                unsafe { SHCreateShellItemArrayFromIDLists(&[pidl as *const ITEMIDLIST]) }.unwrap();
            let data: IDataObject = unsafe { items.BindToHandler(None, &BHID_DataObject) }.unwrap();
            imp::read_items(&data)
        };
        unsafe { CoTaskMemFree(Some(pidl as *const std::ffi::c_void)) };
        unsafe { CoUninitialize() };
        assert!(dropped.paths.is_empty(), "{dropped:?}");
        assert_eq!(dropped.unresolved.len(), 1, "{dropped:?}");
        assert!(!dropped.unresolved[0].is_empty());
    }
}
