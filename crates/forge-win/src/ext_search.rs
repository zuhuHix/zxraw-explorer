//! Filename search through external indexes, used by the search window as fast backends:
//!
//! * [`Backend::Everything`] — voidtools Everything, through its `es.exe` command-line client.
//! * [`Backend::WindowsSearch`] — the Windows Search index, queried with ADO from PowerShell.
//!
//! Queries never reach a shell. `es.exe` gets its arguments as a vector. PowerShell runs a
//! fixed script (base64 `-EncodedCommand`) that reads the SQL from an environment variable,
//! and the SQL itself is built by [`windows_search_sql`], which escapes the user's text as a
//! string literal and as a LIKE pattern.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Largest result count a single search may ask for.
pub const MAX_LIMIT: usize = 10_000;

#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
const TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(windows)]
const OUTPUT_CAP: u64 = 16 << 20;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// An external name-search service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Everything,
    WindowsSearch,
}

/// One matching path. `is_dir` tells folders from files so the caller can navigate to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Whether `b` can be used at all. Cheap after the first call (see [`availability`]).
pub fn available(b: Backend) -> bool {
    availability(b).is_ok()
}

/// Why `b` cannot be used, or `Ok` when it can. Probing Windows Search starts PowerShell,
/// so call this off the UI thread. The answer is cached for the life of the process.
pub fn availability(b: Backend) -> Result<(), String> {
    match b {
        Backend::Everything => everything_exe().map(|_| ()),
        Backend::WindowsSearch => windows_search_probe().clone(),
    }
}

/// Searches `b` for names matching `query` (Everything syntax for [`Backend::Everything`],
/// plain text matched against file names for [`Backend::WindowsSearch`]). With `scope`, only
/// paths under that absolute folder are returned. At most `limit` hits (clamped to
/// `1..=MAX_LIMIT`). Gives up after 30 seconds.
pub fn search(
    b: Backend,
    query: &str,
    scope: Option<&Path>,
    limit: usize,
) -> Result<Vec<Hit>, String> {
    let limit = limit.clamp(1, MAX_LIMIT);
    match b {
        Backend::Everything => {
            let exe = everything_exe()?;
            let args = es_args(query, scope, limit)?;
            let out = run(exe, &args, &[])?;
            if out.code != Some(0) {
                return Err(format!(
                    "Everything is not running (es.exe exit {})",
                    exit_text(out.code)
                ));
            }
            Ok(parse_everything_lines(&decode_output(&out.stdout))
                .into_iter()
                .take(limit)
                .map(|path| {
                    let is_dir = path.is_dir();
                    Hit { path, is_dir }
                })
                .collect())
        }
        Backend::WindowsSearch => {
            availability(Backend::WindowsSearch)?;
            let sql = windows_search_sql(query, scope, limit)?;
            let out = run_powershell(&sql)?;
            if out.code != Some(0) {
                return Err(format!("Windows Search failed: {}", first_line(&out.stderr)));
            }
            Ok(parse_windows_lines(&decode_output(&out.stdout))
                .into_iter()
                .take(limit)
                .collect())
        }
    }
}

// ---------------------------------------------------------------------------------------
// Everything (es.exe)
// ---------------------------------------------------------------------------------------

/// Candidate locations of `es.exe`, in the order they are tried: every `PATH` entry, then
/// the two Program Files folders.
pub fn es_candidates(path_var: &str, program_files: Option<&str>, program_files_x86: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::split_paths(path_var)
        .map(|dir| dir.join("es.exe"))
        .collect();
    for base in [program_files, program_files_x86].into_iter().flatten() {
        out.push(Path::new(base).join("Everything").join("es.exe"));
    }
    out
}

fn everything_exe() -> Result<&'static PathBuf, String> {
    static EXE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    EXE.get_or_init(|| {
        let path_var = std::env::var("PATH").unwrap_or_default();
        let pf = std::env::var("ProgramFiles").ok();
        let pf86 = std::env::var("ProgramFiles(x86)").ok();
        es_candidates(&path_var, pf.as_deref(), pf86.as_deref())
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| {
                "Everything is not installed (es.exe was not found on PATH or in Program Files)"
                    .to_string()
            })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Arguments for `es.exe`. The query is one argument, so the shell-free launch keeps it
/// intact. A query beginning with `-` would be read as an es switch, so it is refused.
pub fn es_args(query: &str, scope: Option<&Path>, limit: usize) -> Result<Vec<OsString>, String> {
    let query = validate_query(query)?;
    if query.starts_with('-') {
        return Err("a query cannot start with '-'".into());
    }
    let mut args: Vec<OsString> = vec![
        "-n".into(),
        limit.to_string().into(),
        "-full-path-and-name".into(),
    ];
    if let Some(dir) = scope {
        validate_scope(dir)?;
        args.push("-path".into());
        args.push(dir.as_os_str().to_owned());
    }
    args.push(query.into());
    Ok(args)
}

/// Splits es.exe output into paths. Accepts UTF-8 or UTF-16 text (already decoded by
/// [`decode_output`]), with CRLF or LF line endings, skipping blank lines.
pub fn parse_everything_lines(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

// ---------------------------------------------------------------------------------------
// Windows Search (ADO through PowerShell)
// ---------------------------------------------------------------------------------------

/// The PowerShell script. It reads the SQL from `ZX_WS_SQL` and prints one line per row:
/// `ItemType<TAB>ItemPathDisplay`. Nothing user-supplied is written into the script text.
const WS_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }
$conn = New-Object -ComObject ADODB.Connection
$conn.Open("Provider=Search.CollatorDSO;Extended Properties='Application=Windows'")
try {
    $rs = $conn.Execute($env:ZX_WS_SQL)
    while (-not $rs.EOF) {
        $type = [string]$rs.Fields.Item('System.ItemType').Value
        $path = [string]$rs.Fields.Item('System.ItemPathDisplay').Value
        [Console]::Out.WriteLine($type + [char]9 + $path)
        $rs.MoveNext() | Out-Null
    }
    $rs.Close()
}
finally {
    $conn.Close()
}
"#;

const PROBE_SQL: &str = "SELECT TOP 1 System.ItemPathDisplay FROM SystemIndex";

/// Builds the Windows Search SQL for a file-name search. `term` becomes a LIKE pattern
/// `%term%`, with `%`, `_` and `[` escaped; `scope` becomes `AND SCOPE='file:<path>'`.
/// Every text value is a single-quoted literal in which `'` is doubled, so no input can end
/// the literal.
pub fn windows_search_sql(term: &str, scope: Option<&Path>, limit: usize) -> Result<String, String> {
    let term = validate_query(term)?;
    let pattern = format!("%{}%", like_escape(term));
    let mut sql = format!(
        "SELECT TOP {} System.ItemPathDisplay, System.ItemType FROM SystemIndex WHERE System.FileName LIKE {}",
        limit.clamp(1, MAX_LIMIT),
        sql_string(&pattern)
    );
    if let Some(dir) = scope {
        validate_scope(dir)?;
        let scope_text = format!("file:{}", dir.display());
        sql.push_str(" AND SCOPE=");
        sql.push_str(&sql_string(&scope_text));
    }
    Ok(sql)
}

/// Escapes `text` as an SQL string literal: wrapped in single quotes, with each `'` doubled.
pub fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Escapes LIKE wildcards by putting each in brackets (`%` -> `[%]`, `_` -> `[_]`,
/// `[` -> `[[]`).
pub fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '[' => out.push_str("[[]"),
            '%' => out.push_str("[%]"),
            '_' => out.push_str("[_]"),
            c => out.push(c),
        }
    }
    out
}

/// Parses the Windows Search script output (`Type<TAB>path` per line) into hits. A line
/// without a tab is taken as a path of a file.
pub fn parse_windows_lines(text: &str) -> Vec<Hit> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| match line.split_once('\t') {
            Some((ty, path)) => Hit {
                path: PathBuf::from(path),
                is_dir: ty.trim().eq_ignore_ascii_case("directory"),
            },
            None => Hit {
                path: PathBuf::from(line),
                is_dir: false,
            },
        })
        .collect()
}

fn run_powershell(sql: &str) -> Result<Captured, String> {
    let script_bytes: Vec<u8> = WS_SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64_encode(&script_bytes);
    let exe = powershell_exe();
    let args: Vec<OsString> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        encoded.into(),
    ];
    run(&exe, &args, &[("ZX_WS_SQL", sql)])
}

fn powershell_exe() -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    PathBuf::from(root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

fn windows_search_probe() -> &'static Result<(), String> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    PROBE.get_or_init(|| {
        let sql = PROBE_SQL.to_string();
        match run_powershell(&sql) {
            Ok(out) if out.code == Some(0) => Ok(()),
            Ok(out) => Err(format!(
                "Windows Search is not available: {}",
                first_line(&out.stderr)
            )),
            Err(e) => Err(format!("Windows Search is not available: {e}")),
        }
    })
}

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

fn validate_query(query: &str) -> Result<&str, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("the search is empty".into());
    }
    if q.chars().any(char::is_control) {
        return Err("the search contains control characters".into());
    }
    Ok(q)
}

fn validate_scope(dir: &Path) -> Result<(), String> {
    let text = dir.to_string_lossy();
    if !dir.is_absolute() {
        return Err(format!("the search folder must be absolute: {text}"));
    }
    if text.chars().any(char::is_control) {
        return Err("the search folder contains control characters".into());
    }
    Ok(())
}

/// Decodes command output: UTF-16 LE (with BOM, or detected from the NUL bytes), UTF-8 with
/// or without BOM, lossy on bad bytes.
pub fn decode_output(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16_le(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    // ASCII-heavy UTF-16 has a NUL in every odd byte; UTF-8 text has almost none.
    let odd_nuls = bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    if bytes.len() >= 4 && odd_nuls * 4 >= bytes.len() {
        return utf16_le(bytes);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn utf16_le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn first_line(bytes: &[u8]) -> String {
    let text = decode_output(bytes);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no details");
    line.chars().take(300).collect()
}

fn exit_text(code: Option<i32>) -> String {
    code.map_or_else(|| "killed".to_string(), |c| c.to_string())
}

#[derive(Debug)]
struct Captured {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `exe` without a console window, with the given extra environment, and captures its
/// output (capped). It is killed after [`TIMEOUT`].
#[cfg(windows)]
fn run(exe: &Path, args: &[OsString], env: &[(&str, &str)]) -> Result<Captured, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    fn read_capped(r: Option<impl Read>) -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(r) = r {
            let _ = r.take(OUTPUT_CAP).read_to_end(&mut buf);
        }
        buf
    }

    let mut child = Command::new(exe)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || read_capped(stdout));
    let err_thread = std::thread::spawn(move || read_capped(stderr));

    let deadline = Instant::now() + TIMEOUT;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} timed out", exe.display()));
            }
            Err(e) => return Err(format!("waiting for {} failed: {e}", exe.display())),
        }
    };
    Ok(Captured {
        code,
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

#[cfg(not(windows))]
fn run(_exe: &Path, _args: &[OsString], _env: &[(&str, &str)]) -> Result<Captured, String> {
    Err("external search backends are only available on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn es_args_put_query_last_and_scope_before_it() {
        let args = es_args("ext:jpg", Some(Path::new(r"C:\Photos")), 50).unwrap();
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            ["-n", "50", "-full-path-and-name", "-path", r"C:\Photos", "ext:jpg"]
        );
    }

    #[test]
    fn es_args_without_scope_have_no_path_switch() {
        let args = es_args("readme", None, 5).unwrap();
        assert!(!args.iter().any(|a| a == "-path"));
        assert_eq!(args.last().unwrap(), "readme");
    }

    #[test]
    fn es_args_refuse_empty_switch_like_and_control_text() {
        assert!(es_args("   ", None, 5).is_err());
        assert!(es_args("-folder", None, 5).is_err());
        assert!(es_args("a\nb", None, 5).is_err());
        assert!(es_args("ok", Some(Path::new("relative")), 5).is_err());
    }

    #[test]
    fn sql_string_doubles_quotes() {
        assert_eq!(sql_string("a'b"), "'a''b'");
        assert_eq!(sql_string("a'; DROP"), "'a''; DROP'");
    }

    #[test]
    fn like_escape_brackets_wildcards() {
        assert_eq!(like_escape("100%_done"), "100[%][_]done");
        assert_eq!(like_escape("[x]"), "[[]x]");
    }

    #[test]
    fn windows_sql_keeps_injection_inside_one_literal() {
        let sql = windows_search_sql("a'; DROP", None, 10).unwrap();
        assert_eq!(
            sql,
            "SELECT TOP 10 System.ItemPathDisplay, System.ItemType FROM SystemIndex WHERE System.FileName LIKE '%a''; DROP%'"
        );
        // Undoing the doubled quotes leaves exactly the two delimiters of the literal.
        assert_eq!(sql.replace("''", "").matches('\'').count(), 2);
    }

    #[test]
    fn windows_sql_escapes_like_wildcards() {
        let sql = windows_search_sql("100%_done", None, 3).unwrap();
        assert!(sql.ends_with("LIKE '%100[%][_]done%'"), "{sql}");
    }

    #[test]
    fn windows_sql_scope_cannot_break_out() {
        let scope = Path::new(r"C:\Users\O'Brien\x' OR '1'='1");
        let sql = windows_search_sql("doc", Some(scope), 20).unwrap();
        assert!(
            sql.ends_with(r"AND SCOPE='file:C:\Users\O''Brien\x'' OR ''1''=''1'"),
            "{sql}"
        );
        assert_eq!(sql.replace("''", "").matches('\'').count(), 4);
    }

    #[test]
    fn windows_sql_rejects_relative_scope_and_newlines() {
        assert!(windows_search_sql("doc", Some(Path::new("Photos")), 5).is_err());
        assert!(windows_search_sql("do\nc", None, 5).is_err());
    }

    #[test]
    fn decode_handles_utf16_bom_utf8_and_plain_text() {
        let utf16: Vec<u8> = "C:\\a\r\nD:\\é\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_output(&utf16), "C:\\a\r\nD:\\é\r\n");
        let mut bom_utf16 = vec![0xFF, 0xFE];
        bom_utf16.extend("C:\\a\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_output(&bom_utf16), "C:\\a\r\n");
        let mut bom8 = vec![0xEF, 0xBB, 0xBF];
        bom8.extend("C:\\b\n".as_bytes());
        assert_eq!(decode_output(&bom8), "C:\\b\n");
        assert_eq!(decode_output("C:\\c\r\n".as_bytes()), "C:\\c\r\n");
    }

    #[test]
    fn everything_lines_skip_blanks_and_crlf() {
        let paths = parse_everything_lines("C:\\a.txt\r\n\r\nD:\\dir\\\r\n");
        assert_eq!(paths, vec![PathBuf::from("C:\\a.txt"), PathBuf::from("D:\\dir\\")]);
    }

    #[test]
    fn windows_lines_split_type_and_path() {
        let hits = parse_windows_lines("Directory\tC:\\Photos\r\nFile\tC:\\Photos\\a.jpg\r\n\r\nbare");
        assert_eq!(
            hits,
            vec![
                Hit { path: PathBuf::from("C:\\Photos"), is_dir: true },
                Hit { path: PathBuf::from("C:\\Photos\\a.jpg"), is_dir: false },
                Hit { path: PathBuf::from("bare"), is_dir: false },
            ]
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Hello, world"), "SGVsbG8sIHdvcmxk");
    }

    #[test]
    fn es_candidates_try_path_then_program_files() {
        let c = es_candidates(r"C:\bin;D:\tools", Some(r"C:\PF"), Some(r"C:\PF86"));
        assert_eq!(c[0], Path::new(r"C:\bin").join("es.exe"));
        assert_eq!(c[1], Path::new(r"D:\tools").join("es.exe"));
        assert_eq!(c[2], Path::new(r"C:\PF").join("Everything").join("es.exe"));
        assert_eq!(c[3], Path::new(r"C:\PF86").join("Everything").join("es.exe"));
    }

    #[test]
    #[ignore = "runs es.exe; needs Everything installed and running"]
    fn everything_search_runs_against_live_index() {
        match search(Backend::Everything, "*.txt", None, 5) {
            Ok(hits) => assert!(hits.len() <= 5),
            Err(e) => eprintln!("Everything not usable here: {e}"),
        }
    }

    #[test]
    #[ignore = "runs PowerShell and Windows Search; needs the WSearch service"]
    fn windows_search_runs_against_live_index() {
        match search(Backend::WindowsSearch, "readme", None, 5) {
            Ok(hits) => assert!(hits.len() <= 5),
            Err(e) => eprintln!("Windows Search not usable here: {e}"),
        }
    }
}
