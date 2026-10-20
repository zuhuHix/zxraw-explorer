//! Times sending a big test tree to the Recycle Bin: the plain shell (no progress sink)
//! against [`forge_win::recycle_items`]. Builds its own tree under %TEMP%, and removes what
//! it put in the Recycle Bin afterwards.
//!
//! `cargo run -p forge-win --example recycle_bench --release -- [files] [runs]`

#[cfg(windows)]
fn main() {
    use std::time::Instant;

    let args: Vec<String> = std::env::args().collect();
    let files: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(50_000);
    let runs: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1);
    let root = std::env::temp_dir().join("zx-recycle-bench");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    for run in 0..runs {
        let only = args.get(3).cloned();
        for mode in ["shell-no-sink", "explorer-like", "zx", "no-allowundo", "nuke-check-off", "shfileop"] {
            if only.as_deref().is_some_and(|o| o != mode) {
                continue;
            }
            let tree = root.join(format!("tree-{mode}-{run}"));
            build(&tree, files);
            let t = Instant::now();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let watch = {
                let (tree, stop) = (tree.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        let left = std::fs::read_dir(&tree).map(|d| d.count()).unwrap_or(0);
                        let mut io = windows::Win32::System::Threading::IO_COUNTERS::default();
                        unsafe {
                            let _ = windows::Win32::System::Threading::GetProcessIoCounters(
                                windows::Win32::System::Threading::GetCurrentProcess(),
                                &mut io,
                            );
                        }
                        eprintln!(
                            "  {:6.2}s top-level entries left: {left}  io r{} w{} o{}",
                            t.elapsed().as_secs_f64(),
                            io.ReadOperationCount, io.WriteOperationCount, io.OtherOperationCount
                        );
                    }
                })
            };
            let info = match mode {
                "shell-no-sink" => plain(&tree, true),
                "explorer-like" => plain(&tree, false),
                "shfileop" => shfileop(&tree),
                "no-allowundo" => plain_flags(&tree, 2),
                "nuke-check-off" => plain_flags(&tree, 3),
                _ => zx(&tree),
            };
            let secs = t.elapsed().as_secs_f64();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = watch.join();
            println!(
                "{mode:14} {files} files: {secs:7.2}s  gone={}  {info}",
                !tree.exists()
            );
            purge_bin();
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
fn build(tree: &std::path::Path, files: usize) {
    let per_dir = 500;
    for i in 0..files {
        let dir = tree.join(format!("d{:04}", i / per_dir)).join("inner");
        if i % per_dir == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(dir.join(format!("f{i:06}.txt")), b"zxraw bench").unwrap();
    }
    for i in 0..3 {
        let f = std::fs::File::create(tree.join(format!("big{i}.bin"))).unwrap();
        f.set_len(256 << 20).unwrap();
    }
}

/// The shell alone, with ZXraw's flags (`silent`) or Explorer's (progress window shown).
#[cfg(windows)]
fn plain(tree: &std::path::Path, silent: bool) -> String {
    plain_flags(tree, if silent { 1 } else { 0 })
}

#[cfg(windows)]
fn plain_flags(tree: &std::path::Path, variant: u32) -> String {
    let silent = variant != 0;
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CLSCTX_ALL, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOFX_RECYCLEONDELETE,
        FOFX_SHOWELEVATIONPROMPT, FileOperation, IFileOperation, IShellItem,
        SHCreateItemFromParsingName,
    };
    use windows::core::PCWSTR;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).unwrap();
        let mut flags = FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOFX_RECYCLEONDELETE;
        if silent {
            flags |= FOF_SILENT | FOF_NOERRORUI | FOFX_SHOWELEVATIONPROMPT;
        }
        if variant == 2 {
            flags &= !FOF_ALLOWUNDO;
        }
        if variant == 3 {
            flags |= windows::Win32::UI::Shell::FOF_NO_UI;
        }
        op.SetOperationFlags(flags).unwrap();
        let wide: Vec<u16> = tree.as_os_str().encode_wide_z();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).unwrap();
        op.DeleteItem(&item, None).unwrap();
        let r = op.PerformOperations();
        drop(item);
        drop(op);
        CoUninitialize();
        format!("{r:?}")
    }
}

#[cfg(windows)]
fn shfileop(tree: &std::path::Path) -> String {
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        SHFILEOPSTRUCTW, SHFileOperationW,
    };
    let mut wide: Vec<u16> = tree.as_os_str().encode_wide_z();
    wide.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(wide.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    format!("{r}")
}

#[cfg(windows)]
fn zx(tree: &std::path::Path) -> String {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    #[derive(Default)]
    struct Seen {
        calls: u64,
        first_total: Option<f64>,
        last: (u64, u64, usize, usize),
    }
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s2 = seen.clone();
    let start = Instant::now();
    let report = forge_win::recycle_items(
        &[tree.to_path_buf()],
        Box::new(move |p| {
            let mut s = s2.lock().unwrap();
            s.calls += 1;
            if s.calls % 20000 == 1 || p.work_total > 0 {
                eprintln!(
                    "  {:6.2}s call {} work {}/{} items {}/{} current {:?}",
                    start.elapsed().as_secs_f64(), s.calls, p.work_done, p.work_total,
                    p.items_done, p.items_total, p.current.as_ref().map(|c| c.display().to_string())
                );
            }
            if p.work_total > 0 && s.first_total.is_none() {
                s.first_total = Some(start.elapsed().as_secs_f64());
            }
            s.last = (p.work_done, p.work_total, p.items_done, p.items_total);
            true
        }),
    );
    let s = seen.lock().unwrap();
    format!(
        "observer calls={} first work_total at {:?}s last(work {}/{}, items {}/{}) done={} failed={:?}",
        s.calls, s.first_total, s.last.0, s.last.1, s.last.2, s.last.3,
        report.done.len(), report.failed
    )
}

/// Removes from the Recycle Bin everything this bench put there.
#[cfg(windows)]
fn purge_bin() {
    let drive = std::env::temp_dir()
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap();
    let bin = std::path::PathBuf::from(format!("{drive}\\$Recycle.Bin"));
    let Ok(sids) = std::fs::read_dir(&bin) else { return };
    for sid in sids.flatten() {
        let Ok(entries) = std::fs::read_dir(sid.path()) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix("$I") else { continue };
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let text = String::from_utf16_lossy(
                &bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<_>>(),
            );
            if text.contains("zx-recycle-bench") {
                let r = sid.path().join(format!("$R{rest}"));
                let _ = std::fs::remove_dir_all(&r);
                let _ = std::fs::remove_file(&r);
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(windows)]
trait WideZ {
    fn encode_wide_z(&self) -> Vec<u16>;
}
#[cfg(windows)]
impl WideZ for std::ffi::OsStr {
    fn encode_wide_z(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(not(windows))]
fn main() {}
