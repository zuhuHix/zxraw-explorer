//! Downloads over HTTPS through WinHTTP, so they use the system TLS stack, certificate
//! store and proxy settings and the app ships no networking code of its own.
//!
//! Only `https://` URLs are accepted, except plain `http://` to the loopback address (used
//! by tests that serve files from a local listener). Redirects are followed, but never
//! from HTTPS down to HTTP; [`Options::final_url`] lets a caller also refuse redirects to
//! hosts it does not trust.

use std::ops::ControlFlow;

use crate::{Result, WinError};

/// Total time allowed to resolve, connect and send, and the longest silence while reading.
#[cfg(windows)]
const TIMEOUT_MS: i32 = 30_000;

/// A parsed download URL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Url {
    secure: bool,
    host: String,
    port: u16,
    /// Path and query, starting with `/`.
    path: String,
}

fn parse_url(url: &str) -> Result<Url> {
    let invalid = |why: &str| WinError::Invalid(format!("{why}: {url}"));
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(invalid("only http(s) URLs can be downloaded"));
    };
    let (authority, path) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    if authority.is_empty() || authority.contains(['@', '[', ']', ' ', '\\']) {
        return Err(invalid("unsupported host"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| invalid("bad port"))?,
        ),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(invalid("missing host"));
    }
    if !secure && !matches!(host, "127.0.0.1" | "localhost") {
        return Err(invalid("plain HTTP is only allowed to this PC"));
    }
    let path = match path {
        "" => "/".to_string(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_string(),
    };
    Ok(Url {
        secure,
        host: host.to_string(),
        port,
        path,
    })
}

/// Extra request settings for [`download_with`].
#[derive(Default)]
pub struct Options<'a> {
    /// Request headers as `(name, value)`; the User-Agent is always
    /// `ZXraw-Explorer/<version>`.
    pub headers: &'a [(&'a str, &'a str)],
    /// Checked against the URL the response finally came from (after any redirects)
    /// before a single byte is handed to `on_data`; returning `false` fails the download.
    pub final_url: Option<&'a dyn Fn(&str) -> bool>,
}

/// Downloads `url`, handing each received chunk to `on_data` in order. Returns the number
/// of bytes received.
///
/// `on_data` returning [`ControlFlow::Break`] stops the transfer with
/// [`WinError::Aborted`]; callers use that for cancellation and size limits, and keep their
/// own reason. Any status other than 200 is an error. Blocks the calling thread.
pub fn download(url: &str, on_data: impl FnMut(&[u8]) -> ControlFlow<()>) -> Result<u64> {
    download_with(url, &Options::default(), on_data)
}

/// [`download`] with request headers and a check of the final (redirected) URL.
#[cfg(windows)]
pub fn download_with(
    url: &str,
    options: &Options<'_>,
    mut on_data: impl FnMut(&[u8]) -> ControlFlow<()>,
) -> Result<u64> {
    use windows::Win32::Networking::WinHttp::*;
    use windows::core::{PCWSTR, w};

    use crate::to_wide;

    let headers = header_block(options.headers)?;
    let url = parse_url(url)?;
    let agent = to_wide(concat!("ZXraw-Explorer/", env!("CARGO_PKG_VERSION")));
    let host = to_wide(&url.host);
    let path = to_wide(&url.path);

    unsafe {
        let session = Handle::new(
            "WinHttpOpen",
            WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
        )
        .or_else(|_| {
            // Automatic proxy needs Windows 8.1; older systems use the default proxy.
            Handle::new(
                "WinHttpOpen",
                WinHttpOpen(
                    PCWSTR(agent.as_ptr()),
                    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                ),
            )
        })?;
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)
            .map_err(|_| last_http_error("WinHttpSetTimeouts"))?;
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP.to_ne_bytes();
        WinHttpSetOption(Some(session.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&policy))
            .map_err(|_| last_http_error("WinHttpSetOption"))?;

        let connection = Handle::new(
            "WinHttpConnect",
            WinHttpConnect(session.0, PCWSTR(host.as_ptr()), url.port, 0),
        )?;
        let flags = if url.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(
            "WinHttpOpenRequest",
            WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            ),
        )?;
        let headers: Vec<u16> = headers.encode_utf16().collect();
        WinHttpSendRequest(
            request.0,
            (!headers.is_empty()).then_some(headers.as_slice()),
            None,
            0,
            0,
            0,
        )
        .map_err(|_| last_http_error("WinHttpSendRequest"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|_| last_http_error("WinHttpReceiveResponse"))?;

        if let Some(allowed) = options.final_url {
            let final_url = final_url(request.0)?;
            if !allowed(&final_url) {
                return Err(WinError::Invalid(format!(
                    "the download was redirected to an unexpected address: {final_url}"
                )));
            }
        }

        let mut status = 0u32;
        let mut len = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|_| last_http_error("WinHttpQueryHeaders"))?;
        if status != 200 {
            return Err(WinError::Invalid(format!(
                "the server answered HTTP {status} for {}",
                url.host
            )));
        }

        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
            )
            .map_err(|_| last_http_error("WinHttpReadData"))?;
            if read == 0 {
                return Ok(total);
            }
            total += u64::from(read);
            if on_data(&buf[..read as usize]).is_break() {
                return Err(WinError::Aborted { op: "download" });
            }
        }
    }
}

#[cfg(not(windows))]
pub fn download_with(
    url: &str,
    options: &Options<'_>,
    _on_data: impl FnMut(&[u8]) -> ControlFlow<()>,
) -> Result<u64> {
    header_block(options.headers)?;
    parse_url(url)?;
    Err(WinError::Unsupported)
}

/// `Name: value\r\n` lines for WinHTTP. Names and values must not break the header block.
fn header_block(headers: &[(&str, &str)]) -> Result<String> {
    let mut block = String::new();
    for (name, value) in headers {
        let bad_name = name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if bad_name || value.contains(['\r', '\n']) {
            return Err(WinError::Invalid(format!("bad request header {name:?}")));
        }
        block.push_str(&format!("{name}: {value}\r\n"));
    }
    Ok(block)
}

/// The URL a request's response came from, after redirects.
#[cfg(windows)]
fn final_url(request: *mut core::ffi::c_void) -> Result<String> {
    use windows::Win32::Networking::WinHttp::{WINHTTP_OPTION_URL, WinHttpQueryOption};

    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        WinHttpQueryOption(
            request,
            WINHTTP_OPTION_URL,
            Some(buf.as_mut_ptr().cast()),
            &mut len,
        )
    }
    .map_err(|_| last_http_error("WinHttpQueryOption"))?;
    let chars = (len as usize / 2).min(buf.len());
    let url = String::from_utf16_lossy(&buf[..chars]);
    Ok(url.trim_end_matches('\0').to_string())
}

/// An open WinHTTP handle, closed on drop.
#[cfg(windows)]
struct Handle(*mut core::ffi::c_void);

#[cfg(windows)]
impl Handle {
    fn new(op: &'static str, raw: *mut core::ffi::c_void) -> Result<Self> {
        if raw.is_null() {
            Err(last_http_error(op))
        } else {
            Ok(Self(raw))
        }
    }
}

#[cfg(windows)]
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
        }
    }
}

/// The calling thread's last WinHTTP error. WinHTTP's codes (12001–12192) have no system
/// message text, so the common ones are spelled out here.
#[cfg(windows)]
fn last_http_error(op: &'static str) -> WinError {
    let code = unsafe { windows::Win32::Foundation::GetLastError().0 };
    let message = match code {
        12002 => "the connection timed out".to_string(),
        12007 => "the server name could not be resolved (are you online?)".to_string(),
        12029 => "could not connect to the server".to_string(),
        12030 | 12152 => "the connection was closed unexpectedly".to_string(),
        12175 | 12044 | 12045 | 12037 | 12038 | 12057 | 12169 | 12179 => {
            "the server's security certificate could not be verified".to_string()
        }
        12156 => "the server tried to redirect to an insecure address".to_string(),
        _ => std::io::Error::from_raw_os_error(code as i32).to_string(),
    };
    WinError::Api { op, code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_and_loopback_http_urls() {
        assert_eq!(
            parse_url("https://github.com/a/b.zip").unwrap(),
            Url {
                secure: true,
                host: "github.com".into(),
                port: 443,
                path: "/a/b.zip".into()
            }
        );
        let local = parse_url("http://127.0.0.1:8080?x=1").unwrap();
        assert_eq!((local.port, local.path.as_str()), (8080, "/?x=1"));
        assert_eq!(parse_url("https://example.com").unwrap().path, "/");
    }

    #[test]
    fn rejects_insecure_and_odd_urls() {
        for url in [
            "http://example.com/x.zip",
            "ftp://example.com/x",
            "file:///C:/x",
            "https://user@example.com/x",
            "https://:443/x",
            "https://example.com:0/x",
            "https://example.com:99999/x",
            "https:///x",
        ] {
            assert!(
                matches!(parse_url(url), Err(WinError::Invalid(_))),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn header_blocks_refuse_injected_lines() {
        assert_eq!(
            header_block(&[("Accept", "application/json"), ("X-Test", "1")]).unwrap(),
            "Accept: application/json\r\nX-Test: 1\r\n"
        );
        assert!(header_block(&[("Accept", "a\r\nHost: evil")]).is_err());
        assert!(header_block(&[("Bad Name", "x")]).is_err());
        assert!(header_block(&[("", "x")]).is_err());
    }

    /// Serves one request with `response` and returns what the client sent.
    #[cfg(windows)]
    fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let _ = stream.write_all(&response);
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        (format!("http://127.0.0.1:{port}/latest"), server)
    }

    #[cfg(windows)]
    #[test]
    fn sends_headers_and_checks_the_final_url() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec();
        let (url, server) = serve_once(ok.clone());
        let mut got = Vec::new();
        let seen = std::cell::Cell::new(String::new());
        let check = |u: &str| {
            seen.set(u.to_string());
            true
        };
        let options = Options {
            headers: &[("Accept", "application/vnd.github+json")],
            final_url: Some(&check),
        };
        download_with(&url, &options, |chunk| {
            got.extend_from_slice(chunk);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(got, b"{}");
        assert_eq!(seen.take(), url);
        let request = server.join().unwrap();
        assert!(
            request.contains("Accept: application/vnd.github+json\r\n"),
            "{request}"
        );
        assert!(request.contains("User-Agent: ZXraw-Explorer/"), "{request}");

        let (url, server) = serve_once(ok);
        let refuse = |_: &str| false;
        let options = Options {
            headers: &[],
            final_url: Some(&refuse),
        };
        let mut received = 0;
        let err = download_with(&url, &options, |chunk| {
            received += chunk.len();
            ControlFlow::Continue(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("unexpected address"), "{err}");
        assert_eq!(received, 0, "no data is handed over from a refused address");
        server.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn downloads_from_a_local_server_and_stops_when_asked() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let body: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = body.clone();
        let server = std::thread::spawn(move || {
            for (i, stream) in listener.incoming().take(3).enumerate() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let response = if i == 2 {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                } else {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        served.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(&served);
                    r
                };
                let _ = stream.write_all(&response);
            }
        });
        let url = format!("http://127.0.0.1:{port}/file.bin");

        let mut got = Vec::new();
        let n = download(&url, |chunk| {
            got.extend_from_slice(chunk);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(n, body.len() as u64);
        assert_eq!(got, body);

        let stopped = download(&url, |_| ControlFlow::Break(()));
        assert!(matches!(stopped, Err(WinError::Aborted { .. })));

        let missing = download(&url, |_| ControlFlow::Continue(())).unwrap_err();
        assert!(missing.to_string().contains("HTTP 404"), "{missing}");
        server.join().unwrap();
    }
}
