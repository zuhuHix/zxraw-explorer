//! Crash report writing and one-time discovery for the next app startup.

use std::{
    backtrace::Backtrace,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::Utc;

const MAX_REPORTS: usize = 20;
const STARTUP_MARKER: &str = ".last-startup-nanos";

/// Find a crash report written after the previous app startup, marking this startup so the
/// same report is offered only once.
pub fn pending_crash_report(paths: &crate::settings::Paths) -> Option<PathBuf> {
    pending_crash_report_in(&paths.data)
}

fn pending_crash_report_in(data_dir: &Path) -> Option<PathBuf> {
    let crash_dir = data_dir.join("crashes");
    if fs::create_dir_all(&crash_dir).is_err() {
        return None;
    }

    let marker = crash_dir.join(STARTUP_MARKER);
    let previous_start = fs::read_to_string(&marker)
        .ok()
        .and_then(|value| value.trim().parse::<u128>().ok());

    let newest = fs::read_dir(&crash_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with("crash-") || !name.ends_with(".txt") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            let nanos = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos();
            Some((nanos, entry.path()))
        })
        .max_by_key(|(nanos, path)| (*nanos, path.clone()));

    let pending = newest
        .as_ref()
        .filter(|(nanos, _)| previous_start.is_none_or(|previous| *nanos > previous))
        .map(|(_, path)| path.clone());

    let now = system_time_nanos(SystemTime::now());
    // This marker contains only one integer, so replacing it in place is sufficient and
    // works on Windows where rename does not replace an existing destination.
    let _ = fs::write(marker, now.to_string());
    pending
}

pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

pub(crate) fn write_crash_report(
    data_dir: &Path,
    message: &str,
    location: Option<&str>,
    backtrace: &Backtrace,
) {
    let crash_dir = data_dir.join("crashes");
    if fs::create_dir_all(&crash_dir).is_err() {
        return;
    }

    let now = Utc::now();
    let timestamp = now.format("%Y%m%dT%H%M%S%.9fZ");
    let report_path = crash_dir.join(format!("crash-{timestamp}.txt"));
    let report = format!(
        "ZXraw Explorer crash report\nTimestamp: {}\nVersion: {}\nOS: {}\nPanic: {}\nLocation: {}\n\nBacktrace:\n{}\n",
        now.to_rfc3339(),
        env!("CARGO_PKG_VERSION"),
        os_version(),
        message,
        location.unwrap_or("unavailable"),
        backtrace,
    );

    let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report_path)
    else {
        return;
    };
    if file.write_all(report.as_bytes()).is_ok() {
        prune_reports(&crash_dir);
    }
}

fn prune_reports(crash_dir: &Path) {
    let mut reports: Vec<_> = fs::read_dir(crash_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with("crash-") || !name.ends_with(".txt") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, name.to_owned(), entry.path()))
        })
        .collect();
    reports.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let remove_count = reports.len().saturating_sub(MAX_REPORTS);
    for (_, _, path) in reports.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

fn system_time_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(windows)]
fn os_version() -> String {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(version: *mut OsVersionInfoW) -> i32;
    }

    let mut version = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    let status = unsafe { RtlGetVersion(&mut version) };
    if status == 0 {
        format!(
            "Windows {}.{}.{}",
            version.major, version.minor, version.build
        )
    } else {
        "Windows version unavailable".to_owned()
    }
}

#[cfg(not(windows))]
fn os_version() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash_files(data_dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(data_dir.join("crashes"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("crash-") && name.ends_with(".txt"))
            })
            .collect()
    }

    #[test]
    fn deliberate_harness_panic_writes_a_report_and_retains_only_twenty() {
        let dir = tempfile::tempdir().unwrap();
        let crash_dir = dir.path().join("crashes");
        fs::create_dir_all(&crash_dir).unwrap();
        for index in 0..21 {
            fs::write(
                crash_dir.join(format!("crash-19990101T000000{index:09}Z.txt")),
                "old test report",
            )
            .unwrap();
        }

        let caught = std::panic::catch_unwind(|| panic!("deliberate test-harness panic"))
            .expect_err("the test deliberately panics inside catch_unwind");
        let message = panic_message(caught.as_ref());
        write_crash_report(
            dir.path(),
            &message,
            Some("crash.rs test harness"),
            &Backtrace::force_capture(),
        );

        let reports = crash_files(dir.path());
        assert_eq!(reports.len(), MAX_REPORTS);
        let newest = reports
            .iter()
            .max_by(|left, right| left.file_name().cmp(&right.file_name()))
            .unwrap();
        let contents = fs::read_to_string(newest).unwrap();
        assert!(contents.contains("ZXraw Explorer crash report"));
        assert!(contents.contains("Version: "));
        assert!(contents.contains("OS: "));
        assert!(contents.contains("Panic: deliberate test-harness panic"));
        assert!(contents.contains("Location: crash.rs test harness"));
        assert!(contents.contains("Backtrace:"));
    }

    #[test]
    fn pending_report_is_returned_once_per_startup() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir
            .path()
            .join("crashes/crash-20260101T000000000000000Z.txt");
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        fs::write(&report, "test report").unwrap();

        assert_eq!(pending_crash_report_in(dir.path()), Some(report.clone()));
        assert_eq!(pending_crash_report_in(dir.path()), None);
    }
}
