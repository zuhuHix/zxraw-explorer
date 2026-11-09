//! "Open terminal here": picks a terminal, builds its argument string and starts it in a folder.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The terminal setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TerminalChoice {
    /// The first terminal found, suited to the folder (see [`pick`]).
    #[default]
    Auto,
    /// Windows Terminal.
    WindowsTerminal,
    /// PowerShell 7.
    Pwsh,
    /// Windows PowerShell 5.1.
    WindowsPowerShell,
    /// Command Prompt.
    Cmd,
}

impl TerminalChoice {
    /// Every choice, in settings-menu order.
    pub const ALL: [TerminalChoice; 5] = [
        TerminalChoice::Auto,
        TerminalChoice::WindowsTerminal,
        TerminalChoice::Pwsh,
        TerminalChoice::WindowsPowerShell,
        TerminalChoice::Cmd,
    ];

    /// The name shown in the settings menu.
    pub fn label(self) -> &'static str {
        match self {
            TerminalChoice::Auto => "Automatic",
            TerminalChoice::WindowsTerminal => "Windows Terminal",
            TerminalChoice::Pwsh => "PowerShell 7 (pwsh)",
            TerminalChoice::WindowsPowerShell => "Windows PowerShell",
            TerminalChoice::Cmd => "Command Prompt (cmd)",
        }
    }
}

/// A terminal program that can be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// Windows Terminal (`wt.exe`).
    WindowsTerminal,
    /// PowerShell 7 (`pwsh.exe`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    WindowsPowerShell,
    /// Command Prompt (`cmd.exe`).
    Cmd,
    /// A Linux terminal emulator (see [`Installed::emulator`]).
    Emulator,
}

impl Shell {
    /// The name used in messages.
    pub fn label(self) -> &'static str {
        match self {
            Shell::WindowsTerminal => "Windows Terminal",
            Shell::Pwsh => "PowerShell 7 (pwsh)",
            Shell::WindowsPowerShell => "Windows PowerShell",
            Shell::Cmd => "Command Prompt (cmd)",
            Shell::Emulator => "terminal",
        }
    }
}

/// Installed terminal executables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Installed {
    /// `wt.exe`, if found.
    pub wt: Option<PathBuf>,
    /// `pwsh.exe`, if found.
    pub pwsh: Option<PathBuf>,
    /// `powershell.exe`, if found.
    pub powershell: Option<PathBuf>,
    /// `cmd.exe`, if found.
    pub cmd: Option<PathBuf>,
    /// On Linux and other Unix systems: the terminal emulator to start (`$TERMINAL`, then the
    /// first of the common ones on `PATH`).
    pub emulator: Option<PathBuf>,
}

/// Terminal emulators tried in order when `$TERMINAL` is not set or not installed.
const EMULATORS: &[&str] = &[
    "x-terminal-emulator",
    "gnome-terminal",
    "konsole",
    "xfce4-terminal",
    "mate-terminal",
    "lxterminal",
    "tilix",
    "alacritty",
    "kitty",
    "wezterm",
    "xterm",
];

/// The emulator to start on Unix: `$TERMINAL` when it names an installed program (its first
/// word, so `"foo --bar"` finds `foo`), otherwise the first of [`EMULATORS`] on `PATH`.
fn detect_emulator(env: &dyn Fn(&str) -> Option<OsString>, dirs: &[PathBuf]) -> Option<PathBuf> {
    let configured = env("TERMINAL").and_then(|value| {
        value
            .to_string_lossy()
            .split_whitespace()
            .next()
            .map(str::to_owned)
    });
    if let Some(name) = configured.filter(|n| !n.is_empty()) {
        let found = if Path::new(&name).is_absolute() {
            Some(PathBuf::from(&name)).filter(|p| present(p))
        } else {
            find_on_path(dirs, &name)
        };
        if found.is_some() {
            return found;
        }
    }
    EMULATORS.iter().find_map(|name| find_on_path(dirs, name))
}

impl Installed {
    /// Looks on this PC (env vars LOCALAPPDATA, PATH, ProgramFiles, SystemRoot, ComSpec), or on
    /// Linux for a terminal emulator.
    pub fn detect() -> Self {
        let env = |k: &str| std::env::var_os(k);
        Self::detect_with(&env)
    }

    /// Testable: `env` returns an environment variable.
    pub fn detect_with(env: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let dirs = path_dirs(env);
        if !cfg!(windows) {
            return Self {
                emulator: detect_emulator(env, &dirs),
                ..Self::default()
            };
        }
        let local = env("LOCALAPPDATA").map(PathBuf::from);
        let program_files = env("ProgramFiles").map(PathBuf::from);
        let system_root = env("SystemRoot").map(PathBuf::from);
        let comspec = env("ComSpec").map(PathBuf::from);
        let existing = |p: Option<PathBuf>| p.filter(|p| present(p));

        // wt.exe is an app execution alias (a reparse point), so `exists()` can be false for it.
        let wt = existing(local.map(|l| l.join(r"Microsoft\WindowsApps\wt.exe")))
            .or_else(|| find_on_path(&dirs, "wt.exe"));
        let pwsh = find_on_path(&dirs, "pwsh.exe")
            .or_else(|| existing(program_files.map(|p| p.join(r"PowerShell\7\pwsh.exe"))));
        let powershell = existing(
            system_root
                .as_ref()
                .map(|s| s.join(r"System32\WindowsPowerShell\v1.0\powershell.exe")),
        )
        .or_else(|| find_on_path(&dirs, "powershell.exe"));
        let cmd = comspec
            .filter(|c| {
                c.file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe"))
                    && present(c)
            })
            .or_else(|| existing(system_root.map(|s| s.join(r"System32\cmd.exe"))));
        Self {
            wt,
            pwsh,
            powershell,
            cmd,
            emulator: None,
        }
    }

    /// The executable for `shell`, if installed.
    pub fn path(&self, shell: Shell) -> Option<&Path> {
        match shell {
            Shell::WindowsTerminal => self.wt.as_deref(),
            Shell::Pwsh => self.pwsh.as_deref(),
            Shell::WindowsPowerShell => self.powershell.as_deref(),
            Shell::Cmd => self.cmd.as_deref(),
            Shell::Emulator => self.emulator.as_deref(),
        }
    }
}

/// Whether `p` is an absolute path that exists. Uses `symlink_metadata` so that app execution
/// aliases count. A relative path would resolve against a working directory, which could be a
/// browsed folder, so it never counts.
fn present(p: &Path) -> bool {
    p.is_absolute() && std::fs::symlink_metadata(p).is_ok()
}

/// The non-empty entries of `PATH`, with surrounding quotes removed.
fn path_dirs(env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let Some(path) = env("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| {
            let text = d.to_string_lossy().into_owned();
            if text.contains('"') {
                PathBuf::from(text.replace('"', ""))
            } else {
                d
            }
        })
        // Relative entries (".", "bin") would resolve against a working directory.
        .filter(|d| d.is_absolute())
        .collect()
}

/// The first directory in `dirs` that holds `name`.
fn find_on_path(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| present(p))
}

/// A ready-to-start terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Which terminal this is.
    pub shell: Shell,
    /// The executable to start.
    pub exe: PathBuf,
    /// The raw parameter string passed to ShellExecuteW.
    pub params: String,
    /// The working directory for the new window.
    pub cwd: PathBuf,
    /// Whether to start it with the "runas" verb (UAC prompt).
    pub elevated: bool,
}

/// Whether `dir` is a UNC path (`\\server\share\...`), after [`plain_path`].
pub fn is_unc(dir: &Path) -> bool {
    plain_path(dir).to_string_lossy().starts_with(r"\\")
}

/// Strips a verbatim prefix: `\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` -> `\\srv\share`.
pub fn plain_path(dir: &Path) -> PathBuf {
    let s = dir.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        dir.to_path_buf()
    }
}

/// Which terminal to use for `dir`. Err is a user-facing message.
///
/// Off Windows only the emulator is available, so every choice other than Automatic falls back
/// to it (a settings file copied from Windows still works).
pub fn pick(choice: TerminalChoice, installed: &Installed, dir: &Path) -> Result<Shell, String> {
    if !cfg!(windows) {
        let _ = (choice, dir);
        return if installed.emulator.is_some() {
            Ok(Shell::Emulator)
        } else {
            Err("No terminal emulator was found. Set the TERMINAL environment variable to one, or install gnome-terminal, konsole, xfce4-terminal or xterm.".to_string())
        };
    }
    let dir = plain_path(dir);
    let explicit = match choice {
        TerminalChoice::Auto => None,
        TerminalChoice::WindowsTerminal => Some(Shell::WindowsTerminal),
        TerminalChoice::Pwsh => Some(Shell::Pwsh),
        TerminalChoice::WindowsPowerShell => Some(Shell::WindowsPowerShell),
        TerminalChoice::Cmd => Some(Shell::Cmd),
    };
    if let Some(shell) = explicit {
        return if installed.path(shell).is_some() {
            Ok(shell)
        } else {
            Err(format!("{} is not installed", shell.label()))
        };
    }
    // cmd cannot use a UNC working directory, so UNC folders prefer PowerShell.
    let order = if is_unc(&dir) {
        [
            Shell::Pwsh,
            Shell::WindowsPowerShell,
            Shell::WindowsTerminal,
            Shell::Cmd,
        ]
    } else {
        [
            Shell::WindowsTerminal,
            Shell::Pwsh,
            Shell::WindowsPowerShell,
            Shell::Cmd,
        ]
    };
    order
        .into_iter()
        .find(|s| installed.path(*s).is_some())
        .ok_or_else(|| "No terminal was found (Windows Terminal, PowerShell or cmd)".to_string())
}

/// Builds the launch. Err is a user-facing message.
pub fn build(
    shell: Shell,
    exe: &Path,
    dir: &Path,
    elevated: bool,
    system_root: &Path,
) -> Result<Launch, String> {
    let dir = plain_path(dir);
    let d = dir.to_string_lossy().into_owned();
    let unc = is_unc(&dir);
    let (params, cwd) = match shell {
        Shell::WindowsTerminal => (join_args(&["-d".to_string(), escape_wt(&d)]), dir.clone()),
        // The emulator is started by `open` with its own arguments (see `emulator_args`).
        Shell::Emulator => (String::new(), dir.clone()),
        Shell::Pwsh | Shell::WindowsPowerShell => {
            let script = format!("Set-Location -LiteralPath {}", ps_literal(&d));
            let args = [
                "-NoExit".to_string(),
                "-EncodedCommand".to_string(),
                powershell_encoded(&script),
            ];
            (join_args(&args), dir.clone())
        }
        Shell::Cmd => {
            // cmd does not use MSVCRT parsing, so its parameter string is written by hand.
            if d.contains('%') && (unc || elevated) {
                return Err(
                    "Command Prompt can't safely open a folder whose name contains %; choose PowerShell in Settings → Terminal"
                        .to_string(),
                );
            }
            if unc {
                // pushd maps a temporary drive letter; the start folder avoids cmd's UNC warning.
                (format!("/K pushd \"{d}\""), system_root.to_path_buf())
            } else if elevated {
                // runas ignores the working directory, so cd explicitly.
                (format!("/K cd /d \"{d}\""), dir.clone())
            } else {
                (String::new(), dir.clone())
            }
        }
    };
    Ok(Launch {
        shell,
        exe: exe.to_path_buf(),
        params,
        cwd,
        elevated,
    })
}

/// MSVCRT/CommandLineToArgvW quoting of one argument (quote only when it is empty or contains space, tab, newline or `"`; backslashes before a `"` or before the closing quote are doubled; `"` becomes `\"`).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '"')) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Backslashes before the closing quote must be doubled.
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// [`quote_arg`] each argument and join them with single spaces.
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Windows Terminal splits its command line into sub-commands at every `;` (even inside quotes); `\;` is a literal `;`.
pub fn escape_wt(arg: &str) -> String {
    arg.replace(';', "\\;")
}

/// PowerShell -EncodedCommand payload: base64 of the UTF-16LE script.
pub fn powershell_encoded(script: &str) -> String {
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64_encode(&bytes)
}

/// Standard base64 with padding.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A PowerShell single-quoted string literal: wraps in ' and doubles every ' (also the typographic quotes ‘ ’ ‚ ‛, which PowerShell also treats as single quotes — double each of those too).
pub fn ps_literal(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();
static RECORD_ONLY: AtomicBool = AtomicBool::new(false);
static RECORDED: Mutex<Option<Launch>> = Mutex::new(None);

/// Detects (once, cached in a OnceLock), picks, builds and starts the terminal. In automated test runs
/// (`set_record_only(true)`) nothing is started: the launch is stored for `recorded()` instead.
/// Runs on a background thread.
pub fn open(choice: TerminalChoice, dir: &Path, elevated: bool) -> Result<Launch, String> {
    let dir = plain_path(dir);
    if !dir.is_dir() {
        return Err(format!("{} is not a folder on disk", dir.display()));
    }
    if elevated && !cfg!(windows) {
        return Err("Opening a terminal as administrator is only available on Windows".into());
    }
    let installed = INSTALLED.get_or_init(Installed::detect);
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let shell = pick(choice, installed, &dir)?;
    let exe = installed
        .path(shell)
        .ok_or_else(|| format!("{} is not installed", shell.label()))?
        .to_path_buf();
    let launch = build(shell, &exe, &dir, elevated, &system_root)?;
    if RECORD_ONLY.load(Ordering::SeqCst) {
        *RECORDED.lock() = Some(launch.clone());
        return Ok(launch);
    }
    if cfg!(windows) {
        forge_win::process::launch_console(
            &launch.exe,
            &launch.params,
            Some(&launch.cwd),
            launch.elevated,
        )
        .map_err(|e| e.to_string())?;
    } else {
        spawn_emulator(&launch.exe, &launch.cwd)
            .map_err(|e| format!("could not start {}: {e}", launch.exe.display()))?;
    }
    Ok(launch)
}

/// The arguments that make `emulator` start in `dir`. Most emulators also take the working
/// directory from the process, which [`spawn_emulator`] sets; the ones with their own flag get
/// it too, since some ignore the inherited directory. Unknown emulators get no arguments.
#[cfg_attr(windows, allow(dead_code))]
pub fn emulator_args(emulator: &str, dir: &Path) -> Vec<OsString> {
    let dir = dir.as_os_str();
    let flag = |flag: &str, value: &OsStr| -> Vec<OsString> {
        vec![OsString::from(flag), value.to_os_string()]
    };
    match emulator {
        "gnome-terminal" | "mate-terminal" | "xfce4-terminal" | "lxterminal" | "tilix" => {
            let mut arg = OsString::from("--working-directory=");
            arg.push(dir);
            vec![arg]
        }
        "konsole" => flag("--workdir", dir),
        "alacritty" => flag("--working-directory", dir),
        "kitty" => flag("--directory", dir),
        "wezterm" => vec!["start".into(), "--cwd".into(), dir.to_os_string()],
        _ => Vec::new(),
    }
}

/// The program a file name belongs to: `gnome-terminal.wrapper` is `gnome-terminal`.
#[cfg(not(windows))]
fn emulator_program(file_name: &str) -> &str {
    file_name.split('.').next().unwrap_or(file_name)
}

/// Starts the terminal emulator at `exe` in `dir`, detached. The emulator is named by the
/// file name of `exe`, so a symlink such as `x-terminal-emulator` gets its own arguments only
/// when the name it was resolved to is known.
#[cfg(not(windows))]
fn spawn_emulator(exe: &Path, dir: &Path) -> std::io::Result<()> {
    // The name the user sees (`x-terminal-emulator`) may resolve to a wrapper
    // (`gnome-terminal.wrapper`), so the arguments are chosen by either name.
    let resolved = std::fs::canonicalize(exe).ok();
    let args = [Some(exe), resolved.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|p| p.file_name())
        .map(|n| emulator_args(emulator_program(&n.to_string_lossy()), dir))
        .find(|args| !args.is_empty())
        .unwrap_or_default();
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn spawn_emulator(_exe: &Path, _dir: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "terminal emulators are not used on Windows",
    ))
}

#[cfg_attr(not(any(debug_assertions, feature = "automation")), allow(dead_code))]
/// Turns record-only mode on or off (automated test runs start nothing).
pub fn set_record_only(on: bool) {
    RECORD_ONLY.store(on, Ordering::SeqCst);
}

#[cfg_attr(not(any(debug_assertions, feature = "automation")), allow(dead_code))]
/// The last launch recorded in record-only mode.
pub fn recorded() -> Option<Launch> {
    RECORDED.lock().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::collections::HashMap;

    /// CommandLineToArgvW rules for the arguments produced by this module.
    fn parse_msvcrt(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_arg = false;
        let mut in_quotes = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if !in_quotes && (c == ' ' || c == '\t') {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
                i += 1;
                continue;
            }
            in_arg = true;
            if c == '\\' {
                let mut n = 0;
                while i < chars.len() && chars[i] == '\\' {
                    n += 1;
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    for _ in 0..n / 2 {
                        cur.push('\\');
                    }
                    if n % 2 == 1 {
                        cur.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    i += 1;
                } else {
                    for _ in 0..n {
                        cur.push('\\');
                    }
                }
                continue;
            }
            if c == '"' {
                in_quotes = !in_quotes;
            } else {
                cur.push(c);
            }
            i += 1;
        }
        if in_arg {
            args.push(cur);
        }
        args
    }

    /// Standard base64 decoder (ignores padding).
    fn b64_decode(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf: u32 = 0;
        let mut bits = 0;
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => continue,
            };
            buf = (buf << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
        out
    }

    /// Recovers the PowerShell script from an -EncodedCommand payload.
    fn decode_script(b64: &str) -> String {
        let units: Vec<u16> = b64_decode(b64)
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).expect("valid UTF-16")
    }

    const SHELLS: [Shell; 4] = [
        Shell::WindowsTerminal,
        Shell::Pwsh,
        Shell::WindowsPowerShell,
        Shell::Cmd,
    ];

    const AWKWARD_DIRS: [&str; 10] = [
        r"C:\Photos & Raw",
        r"C:\100% done",
        r"C:\a^b",
        r"C:\it's here",
        r"C:\semi;colon",
        r"C:\",
        r"C:\trailing space\",
        r"C:\ünïcödé ✓",
        r"\\server\share\Shoot 1",
        r"\\?\C:\long",
    ];

    #[test]
    fn quote_arg_round_trips() {
        for a in [
            "",
            "plain",
            "a b",
            r"C:\a b\",
            r"C:\",
            "with\"quote",
            "back\\\\\\\"slash",
            "tab\there",
            r"C:\x\\",
        ] {
            assert_eq!(
                parse_msvcrt(&quote_arg(a)),
                vec![a.to_string()],
                "arg {a:?}"
            );
        }
    }

    #[test]
    fn join_args_round_trips() {
        let args = vec!["-d".to_string(), r"C:\a b\".to_string(), String::new()];
        assert_eq!(parse_msvcrt(&join_args(&args)), args);
    }

    #[test]
    fn build_round_trips_every_awkward_folder() {
        let sysroot = Path::new(r"C:\Windows");
        let exe = Path::new(r"C:\tools\dummy.exe");
        for dir_str in AWKWARD_DIRS {
            let dir = Path::new(dir_str);
            let plain = plain_path(dir);
            let plain_s = plain.to_string_lossy().into_owned();
            let unc = is_unc(dir);
            for shell in SHELLS {
                for elevated in [false, true] {
                    let ctx = format!("{shell:?} elevated={elevated} dir={dir_str}");
                    let res = build(shell, exe, dir, elevated, sysroot);
                    if let Ok(l) = &res {
                        assert_eq!(l.shell, shell, "{ctx}");
                        assert_eq!(l.exe, exe, "{ctx}");
                        assert_eq!(l.elevated, elevated, "{ctx}");
                    }
                    match shell {
                        Shell::WindowsTerminal => {
                            let l = res.unwrap_or_else(|e| panic!("{ctx}: {e}"));
                            let args = parse_msvcrt(&l.params);
                            assert_eq!(args, vec!["-d".to_string(), escape_wt(&plain_s)], "{ctx}");
                            assert_eq!(args[1].replace("\\;", ";"), plain_s, "{ctx}");
                            assert_eq!(l.cwd, plain, "{ctx}");
                        }
                        Shell::Pwsh | Shell::WindowsPowerShell => {
                            let l = res.unwrap_or_else(|e| panic!("{ctx}: {e}"));
                            let args = parse_msvcrt(&l.params);
                            assert_eq!(args.len(), 3, "{ctx}");
                            assert_eq!(args[0], "-NoExit", "{ctx}");
                            assert_eq!(args[1], "-EncodedCommand", "{ctx}");
                            assert_eq!(
                                decode_script(&args[2]),
                                format!(
                                    "Set-Location -LiteralPath '{}'",
                                    plain_s.replace('\'', "''")
                                ),
                                "{ctx}"
                            );
                            assert_eq!(l.cwd, plain, "{ctx}");
                        }
                        Shell::Emulator => {
                            assert!(res.is_ok(), "{ctx}");
                        }
                        Shell::Cmd => {
                            if plain_s.contains('%') && (unc || elevated) {
                                let err = res.expect_err(&ctx);
                                assert!(err.contains('%'), "{ctx}: {err}");
                                continue;
                            }
                            let l = res.unwrap_or_else(|e| panic!("{ctx}: {e}"));
                            let (params, cwd) = if unc {
                                (format!("/K pushd \"{plain_s}\""), sysroot.to_path_buf())
                            } else if elevated {
                                (format!("/K cd /d \"{plain_s}\""), plain.clone())
                            } else {
                                (String::new(), plain.clone())
                            };
                            assert_eq!(l.params, params, "{ctx}");
                            assert_eq!(l.cwd, cwd, "{ctx}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cmd_refuses_percent_when_elevated_or_unc() {
        let sysroot = Path::new(r"C:\Windows");
        let exe = Path::new(r"C:\Windows\System32\cmd.exe");
        let pct = Path::new(r"C:\100% done");
        assert!(build(Shell::Cmd, exe, pct, true, sysroot).is_err());
        assert!(
            build(
                Shell::Cmd,
                exe,
                Path::new(r"\\srv\s\100% done"),
                false,
                sysroot
            )
            .is_err()
        );
        let l = build(Shell::Cmd, exe, pct, false, sysroot).unwrap();
        assert_eq!(l.params, "");
        assert_eq!(l.cwd, pct);
    }

    #[test]
    fn escape_wt_escapes_semicolons() {
        assert_eq!(escape_wt("a;b;c"), r"a\;b\;c");
    }

    #[test]
    fn powershell_encoded_matches_known_value() {
        assert_eq!(powershell_encoded("ab"), "YQBiAA==");
        assert_eq!(
            decode_script(&powershell_encoded("Set-Location ü ✓")),
            "Set-Location ü ✓"
        );
    }

    #[test]
    fn ps_literal_doubles_quotes() {
        assert_eq!(ps_literal("it's"), "'it''s'");
        assert_eq!(ps_literal("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(ps_literal("a\u{201A}b"), "'a\u{201A}\u{201A}b'");
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn installed_all() -> Installed {
        Installed {
            wt: Some(PathBuf::from("wt.exe")),
            pwsh: Some(PathBuf::from("pwsh.exe")),
            powershell: Some(PathBuf::from("powershell.exe")),
            cmd: Some(PathBuf::from("cmd.exe")),
            emulator: None,
        }
    }

    #[test]
    #[cfg(windows)]
    fn pick_prefers_windows_terminal_for_local_folders() {
        let dir = Path::new(r"C:\Users\zuhu");
        assert_eq!(
            pick(TerminalChoice::Auto, &installed_all(), dir),
            Ok(Shell::WindowsTerminal)
        );
    }

    #[test]
    #[cfg(windows)]
    fn pick_prefers_pwsh_for_unc_folders() {
        let dir = Path::new(r"\\server\share\Shoot 1");
        assert_eq!(
            pick(TerminalChoice::Auto, &installed_all(), dir),
            Ok(Shell::Pwsh)
        );
        // Verbatim UNC prefixes are plain UNC too.
        let verbatim = Path::new(r"\\?\UNC\server\share");
        assert_eq!(
            pick(TerminalChoice::Auto, &installed_all(), verbatim),
            Ok(Shell::Pwsh)
        );
    }

    #[test]
    #[cfg(windows)]
    fn pick_explicit_missing_is_an_error() {
        let installed = Installed {
            wt: None,
            ..installed_all()
        };
        assert_eq!(
            pick(
                TerminalChoice::WindowsTerminal,
                &installed,
                Path::new(r"C:\x")
            ),
            Err("Windows Terminal is not installed".to_string())
        );
        assert_eq!(
            pick(TerminalChoice::Pwsh, &installed, Path::new(r"C:\x")),
            Ok(Shell::Pwsh)
        );
    }

    #[test]
    #[cfg(windows)]
    fn pick_nothing_installed_is_an_error() {
        let err = pick(
            TerminalChoice::Auto,
            &Installed::default(),
            Path::new(r"C:\x"),
        )
        .unwrap_err();
        assert!(err.starts_with("No terminal was found"), "{err}");
    }

    #[test]
    #[cfg(windows)]
    fn detect_finds_fake_terminals() {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("local");
        let sys = tmp.path().join("sys");
        let bin = tmp.path().join("bin");
        let wt = local.join(r"Microsoft\WindowsApps\wt.exe");
        let pwsh = bin.join("pwsh.exe");
        let ps = sys.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let cmd = sys.join(r"System32\cmd.exe");
        for p in [&wt, &pwsh, &ps, &cmd] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let path = format!(
            "\"{}\";{}",
            bin.display(),
            tmp.path().join("missing").display()
        );
        let mut vars: HashMap<&str, OsString> = HashMap::new();
        vars.insert("LOCALAPPDATA", local.clone().into_os_string());
        vars.insert("SystemRoot", sys.clone().into_os_string());
        vars.insert("PATH", OsString::from(path));
        let env = |k: &str| vars.get(k).cloned();
        let found = Installed::detect_with(&env);
        assert_eq!(found.wt, Some(wt));
        assert_eq!(found.pwsh, Some(pwsh));
        assert_eq!(found.powershell, Some(ps));
        assert_eq!(found.cmd, Some(cmd));
    }

    #[test]
    #[cfg(windows)]
    fn relative_path_entries_are_ignored() {
        // "." or "bin" would find a program in whatever folder is current, e.g. a browsed one.
        let env = |k: &str| (k == "PATH").then(|| OsString::from(r".;bin;;C:\Tools;.\x"));
        assert_eq!(path_dirs(&env), vec![PathBuf::from(r"C:\Tools")]);
        assert!(
            !present(Path::new("Cargo.toml")),
            "relative paths never count as found"
        );
    }

    #[test]
    fn plain_path_strips_verbatim_prefix() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\srv\share")),
            PathBuf::from(r"\\srv\share")
        );
        assert_eq!(plain_path(Path::new(r"C:\x")), PathBuf::from(r"C:\x"));
        assert_eq!(
            plain_path(Path::new(r"\\srv\share")),
            PathBuf::from(r"\\srv\share")
        );
    }

    #[test]
    #[cfg(unix)]
    fn emulator_is_picked_from_terminal_or_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let konsole = bin.join("konsole");
        std::fs::write(&konsole, b"x").unwrap();
        let path = OsString::from(bin.as_os_str());
        let env = |k: &str| match k {
            "PATH" => Some(path.clone()),
            _ => None,
        };
        // Without $TERMINAL the first common emulator on PATH is used.
        let found = Installed::detect_with(&env);
        assert_eq!(found.emulator, Some(konsole.clone()));
        assert_eq!(found.wt, None);
        // $TERMINAL wins, and its first word is the program (arguments are ignored).
        let env = |k: &str| match k {
            "PATH" => Some(path.clone()),
            "TERMINAL" => Some(OsString::from("konsole --new-tab")),
            _ => None,
        };
        assert_eq!(Installed::detect_with(&env).emulator, Some(konsole));
        // A $TERMINAL that is not installed falls back to the list.
        let env = |k: &str| match k {
            "PATH" => Some(path.clone()),
            "TERMINAL" => Some(OsString::from("zxraw-no-such-terminal")),
            _ => None,
        };
        assert!(Installed::detect_with(&env).emulator.is_some());
        // Nothing installed: nothing found, and the error names the fix.
        let none = |_: &str| None;
        let installed = Installed::detect_with(&none);
        let err = pick(TerminalChoice::Auto, &installed, tmp.path()).unwrap_err();
        assert!(err.contains("TERMINAL"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn unix_always_uses_the_emulator_whatever_the_choice() {
        let installed = Installed {
            emulator: Some(PathBuf::from("/usr/bin/xterm")),
            ..Installed::default()
        };
        for choice in TerminalChoice::ALL {
            assert_eq!(
                pick(choice, &installed, Path::new("/home/zuhu")),
                Ok(Shell::Emulator),
                "{choice:?}"
            );
        }
        let launch = build(
            Shell::Emulator,
            Path::new("/usr/bin/xterm"),
            Path::new("/home/zuhu"),
            false,
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(launch.cwd, PathBuf::from("/home/zuhu"));
        assert_eq!(launch.params, "");
    }

    #[test]
    #[cfg(unix)]
    fn emulator_arguments_name_the_folder_for_known_emulators() {
        let dir = Path::new("/home/zuhu/My Pics");
        assert_eq!(
            emulator_args("gnome-terminal", dir),
            vec![OsString::from("--working-directory=/home/zuhu/My Pics")]
        );
        assert_eq!(
            emulator_args("konsole", dir),
            vec![
                OsString::from("--workdir"),
                OsString::from("/home/zuhu/My Pics")
            ]
        );
        assert_eq!(
            emulator_args("wezterm", dir),
            vec![
                OsString::from("start"),
                OsString::from("--cwd"),
                OsString::from("/home/zuhu/My Pics"),
            ]
        );
        // xterm and unknown emulators only get the working directory of the process.
        assert!(emulator_args("xterm", dir).is_empty());
        assert!(emulator_args("some-other-term", dir).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn wrapper_names_select_the_emulator_arguments() {
        assert_eq!(emulator_program("gnome-terminal.wrapper"), "gnome-terminal");
        assert_eq!(emulator_program("konsole"), "konsole");
        assert_eq!(
            emulator_args(emulator_program("gnome-terminal.wrapper"), Path::new("/x")),
            vec![OsString::from("--working-directory=/x")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn elevated_terminal_is_refused_on_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let err = open(TerminalChoice::Auto, tmp.path(), true).unwrap_err();
        assert!(err.contains("only available on Windows"), "{err}");
    }

    #[test]
    fn record_only_stores_the_launch() {
        // Not running `open` here: it would start a real terminal if record-only were off.
        set_record_only(true);
        let tmp = tempfile::tempdir().unwrap();
        let l = open(TerminalChoice::Cmd, tmp.path(), false);
        set_record_only(false);
        // Cmd may not be installed on every test machine; only check the recorded value when it was.
        if let Ok(l) = l {
            assert_eq!(recorded(), Some(l));
        }
    }
}
