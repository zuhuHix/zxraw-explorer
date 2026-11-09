//! Mark-of-the-Web propagation: archive files carry a `:Zone.Identifier` stream
//! (ZoneId 3 = Internet, 4 = Restricted), and extracted files inherit it like Explorer does.
//! Everything goes through the NTFS alternate data stream `<path>:Zone.Identifier`.

use std::io;
use std::path::Path;
#[cfg(windows)]
use std::fs::{self, File};
#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::path::PathBuf;

/// Zones at or above this id (Internet and Restricted) are marked.
#[cfg(windows)]
const MARK_FROM_ZONE: u32 = 3;
/// Upper bound on how much of a zone stream is read.
#[cfg(windows)]
const MAX_STREAM_BYTES: u64 = 64 * 1024;

/// Zone information read from an archive's `:Zone.Identifier` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Zone {
    pub zone_id: u32,
    /// `HostUrl=` line of the archive's stream, if any (the download URL).
    pub host_url: Option<String>,
}

/// `<path>:Zone.Identifier`, built on the OS string so `\\?\` paths keep working.
#[cfg(windows)]
fn stream_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(":Zone.Identifier");
    PathBuf::from(s)
}

/// Lenient parse of a zone stream. Returns `None` if no usable `ZoneId` is present.
#[cfg(windows)]
fn parse_zone(bytes: &[u8]) -> Option<Zone> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut zone_id = None;
    let mut host_url = None;
    // `lines()` accepts both \r\n and \n.
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "zoneid" => zone_id = value.parse::<u32>().ok(),
            "hosturl" if !value.is_empty() => host_url = Some(value.to_string()),
            _ => {}
        }
    }
    Some(Zone {
        zone_id: zone_id?,
        host_url,
    })
}

/// Reads `<path>:Zone.Identifier`. Returns `None` if the stream is missing/unreadable/unparsable,
/// or if ZoneId < 3.
#[cfg(windows)]
pub(crate) fn read_zone(path: &Path) -> Option<Zone> {
    let mut buf = Vec::new();
    File::open(stream_path(path))
        .ok()?
        .take(MAX_STREAM_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    parse_zone(&buf).filter(|zone| zone.zone_id >= MARK_FROM_ZONE)
}

#[cfg(not(windows))]
pub(crate) fn read_zone(_path: &Path) -> Option<Zone> {
    None
}

/// Writes `<file>:Zone.Identifier`, replacing any existing stream.
#[cfg(windows)]
pub(crate) fn write_zone(file: &Path, zone: &Zone, archive: &Path) -> io::Result<()> {
    let mut content = format!(
        "[ZoneTransfer]\r\nZoneId={}\r\nReferrerUrl={}\r\n",
        zone.zone_id,
        archive.display()
    );
    if let Some(url) = &zone.host_url {
        content.push_str(&format!("HostUrl={url}\r\n"));
    }
    fs::write(stream_path(file), content)
}

#[cfg(not(windows))]
pub(crate) fn write_zone(_file: &Path, _zone: &Zone, _archive: &Path) -> io::Result<()> {
    Ok(())
}

/// Copies the raw `:Zone.Identifier` stream of `from` onto `to` (any zone).
/// A source without a stream is a no-op.
#[cfg(windows)]
pub(crate) fn copy_zone_stream(from: &Path, to: &Path) -> io::Result<()> {
    match fs::read(stream_path(from)) {
        Ok(bytes) => fs::write(stream_path(to), bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(windows))]
pub(crate) fn copy_zone_stream(_from: &Path, _to: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::*;

    #[cfg(windows)]
    fn zone(id: u32) -> Zone {
        Zone {
            zone_id: id,
            host_url: Some("https://example.com/download/archive.zip".to_string()),
        }
    }

    #[cfg(windows)]
    fn new_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"data").unwrap();
        path
    }

    #[cfg(windows)]
    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = new_file(dir.path(), "a.zip");
        let original = zone(3);
        write_zone(&file, &original, &file).unwrap();
        assert_eq!(read_zone(&file), Some(original));
    }

    #[cfg(windows)]
    #[test]
    fn zone_below_three_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = new_file(dir.path(), "a.zip");
        write_zone(&file, &zone(2), &file).unwrap();
        assert_eq!(read_zone(&file), None);
    }

    #[cfg(windows)]
    #[test]
    fn missing_stream_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = new_file(dir.path(), "a.zip");
        assert_eq!(read_zone(&file), None);
    }

    #[cfg(windows)]
    #[test]
    fn bom_lowercase_keys_and_lf_endings_parse() {
        let dir = tempfile::tempdir().unwrap();
        let file = new_file(dir.path(), "a.zip");
        let raw = "\u{feff}[ZoneTransfer]\nzoneid=4\nhosturl=https://h/x.zip\n";
        fs::write(stream_path(&file), raw).unwrap();
        assert_eq!(
            read_zone(&file),
            Some(Zone {
                zone_id: 4,
                host_url: Some("https://h/x.zip".to_string()),
            })
        );
    }

    #[cfg(windows)]
    #[test]
    fn written_content_has_referrer_and_host() {
        let dir = tempfile::tempdir().unwrap();
        let file = new_file(dir.path(), "a.zip");
        let archive = dir.path().join("download.zip");
        write_zone(&file, &zone(3), &archive).unwrap();
        let raw = fs::read_to_string(stream_path(&file)).unwrap();
        assert!(raw.contains("ZoneId=3\r\n"));
        assert!(raw.contains(&format!("ReferrerUrl={}", archive.display())));
        assert!(raw.contains("HostUrl=https://example.com/download/archive.zip\r\n"));
    }

    #[cfg(windows)]
    #[test]
    fn copy_zone_stream_copies_raw_stream() {
        let dir = tempfile::tempdir().unwrap();
        let from = new_file(dir.path(), "from.zip");
        let to = new_file(dir.path(), "to.zip");
        write_zone(&from, &zone(3), &from).unwrap();
        copy_zone_stream(&from, &to).unwrap();
        assert_eq!(read_zone(&to), Some(zone(3)));
        assert_eq!(
            fs::read(stream_path(&from)).unwrap(),
            fs::read(stream_path(&to)).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn copy_from_source_without_stream_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let from = new_file(dir.path(), "from.zip");
        let to = new_file(dir.path(), "to.zip");
        copy_zone_stream(&from, &to).unwrap();
        assert!(fs::metadata(stream_path(&to)).is_err());
    }
}
