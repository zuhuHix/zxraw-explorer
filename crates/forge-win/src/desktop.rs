//! A private, invisible desktop for automated UI runs.
//!
//! Windows created on another desktop are never shown on the user's screen and cannot
//! receive the user's mouse or keyboard input, so tests can run while the PC is in use.
//! On Linux the same role is played by a private X server per run ([`PrivateDisplay`]).

use crate::Result;

/// Moves the calling thread onto a new, private desktop named `name`, so every window the
/// thread creates afterwards lives there. Must be called before the thread creates any
/// window (Windows refuses the switch otherwise). The desktop closes when the process ends.
#[cfg(windows)]
pub fn move_thread_to_private_desktop(name: &str) -> Result<()> {
    use windows::Win32::System::StationsAndDesktops::{
        CreateDesktopW, DESKTOP_CONTROL_FLAGS, SetThreadDesktop,
    };
    use windows::core::PCWSTR;

    // GENERIC_ALL: the thread needs to create, draw and enumerate windows there.
    const GENERIC_ALL: u32 = 0x1000_0000;
    let wide = crate::to_wide(name);
    unsafe {
        let desktop = CreateDesktopW(
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            GENERIC_ALL,
            None,
        )
        .map_err(|e| crate::api_err("CreateDesktopW", e))?;
        // The handle stays open for the life of the process: the desktop must outlive
        // every window on it.
        SetThreadDesktop(desktop).map_err(|e| crate::api_err("SetThreadDesktop", e))
    }
}

/// Off Windows there is no per-thread desktop. A run is private when whoever started it put
/// it on its own X server ([`PrivateDisplay`]) and said so with `ZXRAW_PRIVATE_DISPLAY`
/// (equal to `DISPLAY`); then this succeeds without doing anything.
#[cfg(not(windows))]
pub fn move_thread_to_private_desktop(_name: &str) -> Result<()> {
    match (
        std::env::var_os(PRIVATE_DISPLAY_ENV),
        std::env::var_os("DISPLAY"),
    ) {
        (Some(private), Some(display)) if private == display => Ok(()),
        _ => Err(crate::WinError::Unsupported),
    }
}

/// Set by the starter of a run to the `DISPLAY` of the private X server it gave the run.
pub const PRIVATE_DISPLAY_ENV: &str = "ZXRAW_PRIVATE_DISPLAY";

/// A private, invisible X server (`Xvfb`) for one automated UI run on Linux.
///
/// Windows on it are never shown on the user's screen and get none of the user's input. It
/// also gives the run its own clipboard. The server is stopped (only this one: it is our
/// child process, signalled by its PID) when the value is dropped.
#[cfg(target_os = "linux")]
pub struct PrivateDisplay {
    child: std::process::Child,
    display: String,
}

#[cfg(target_os = "linux")]
impl PrivateDisplay {
    /// Starts `Xvfb` on the first free display number (`-displayfd`) with one
    /// `width`x`height` 24-bit screen and no TCP listener, and waits until it accepts
    /// clients (it prints its display number only then).
    pub fn start(width: u32, height: u32) -> Result<Self> {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let io_err = |op: &'static str, source: std::io::Error| crate::WinError::Io {
            op,
            path: "Xvfb".into(),
            source,
        };
        // `-displayfd 1`: the server writes the display number it picked to its stdout, a
        // pipe we read. Its log goes to stderr, which is dropped.
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-nolisten",
                "tcp",
                "-noreset",
                "-screen",
                "0",
            ])
            .arg(format!("{width}x{height}x24"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| io_err("start Xvfb", e))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut number = String::new();
        let mut byte = [0u8; 1];
        // Reads until the newline after the number; EOF means the server gave up.
        loop {
            match stdout.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) => number.push(byte[0] as char),
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io_err("read the Xvfb display number", e));
                }
            }
        }
        let number = number.trim();
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            let _ = child.kill();
            let status = child.wait().ok();
            return Err(crate::WinError::Invalid(format!(
                "Xvfb did not report a display number (exit {status:?}, output {number:?})"
            )));
        }
        Ok(Self {
            child,
            display: format!(":{number}"),
        })
    }

    /// The `DISPLAY` value for clients of this server, e.g. `:3`.
    pub fn display(&self) -> &str {
        &self.display
    }
}

#[cfg(target_os = "linux")]
impl Drop for PrivateDisplay {
    /// Asks the server to quit (SIGTERM, so it removes its socket and lock file) and waits
    /// up to 3 s for it, then kills it.
    fn drop(&mut self) {
        let asked = std::process::Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if asked {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_thread_can_move_to_a_private_desktop() {
        let name = format!("forge-win-test-{}", std::process::id());
        std::thread::spawn(move || move_thread_to_private_desktop(&name))
            .join()
            .unwrap()
            .unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn a_private_display_starts_and_stops() {
        if std::process::Command::new("Xvfb")
            .arg("-help")
            .output()
            .is_err()
        {
            eprintln!("Xvfb is not installed; skipped");
            return;
        }
        let display = PrivateDisplay::start(640, 480).unwrap();
        let number = display.display().trim_start_matches(':').to_string();
        let socket = std::path::PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        assert!(socket.exists(), "no socket for {}", display.display());
        drop(display);
        // The server removes its socket on the way out.
        let gone = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            !socket.exists()
        });
        assert!(gone, "Xvfb :{number} still has its socket after drop");
    }
}
