//! Parsing of `FILEGROUPDESCRIPTORW` ("FileGroupDescriptorW"), the list of virtual files a
//! data object offers through `FileContents` (Explorer's zip folders, mail attachments).
//! Pure code: the bytes come from another application and are treated as untrusted.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One `FILEDESCRIPTORW` entry. Optional fields are `None` when the entry's flags say the
/// source did not fill them in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDescriptor {
    /// `cFileName` up to its first NUL, as the source wrote it: a relative path that may
    /// contain `\` separators. Not validated here.
    pub name: String,
    /// `FILE_ATTRIBUTE_DIRECTORY` was set (only known with `FD_ATTRIBUTES`).
    pub is_dir: bool,
    pub attributes: Option<u32>,
    pub size: Option<u64>,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    /// The raw `dwFlags`.
    pub flags: u32,
}

/// `FD_CLSID`: the `clsid` field is valid.
pub const FD_CLSID: u32 = 0x1;
/// `FD_SIZEPOINT`: the `sizel` and `pointl` fields are valid.
pub const FD_SIZEPOINT: u32 = 0x2;
/// `FD_ATTRIBUTES`: `dwFileAttributes` is valid.
pub const FD_ATTRIBUTES: u32 = 0x4;
/// `FD_CREATETIME`: `ftCreationTime` is valid.
pub const FD_CREATETIME: u32 = 0x8;
/// `FD_ACCESSTIME`: `ftLastAccessTime` is valid.
pub const FD_ACCESSTIME: u32 = 0x10;
/// `FD_WRITESTIME`: `ftLastWriteTime` is valid.
pub const FD_WRITESTIME: u32 = 0x20;
/// `FD_FILESIZE`: `nFileSizeHigh` and `nFileSizeLow` are valid.
pub const FD_FILESIZE: u32 = 0x40;
/// `FD_PROGRESSUI`: the source wants a progress UI while the file is copied.
pub const FD_PROGRESSUI: u32 = 0x4000;
/// `FD_LINKUI`: the source wants a link prompt.
pub const FD_LINKUI: u32 = 0x8000;
/// `FD_UNICODE`: the name is UTF-16 (always set for `FILEDESCRIPTORW`).
pub const FD_UNICODE: u32 = 0x8000_0000;

/// `FILE_ATTRIBUTE_DIRECTORY`.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Size of one `FILEDESCRIPTORW` entry in bytes.
const FILE_DESCRIPTOR_SIZE: usize = 592;
/// Size of the leading `cItems` count in `FILEGROUPDESCRIPTORW`.
const COUNT_SIZE: usize = 4;
/// Largest `cItems` accepted. A real drag never offers anywhere near this many files.
const MAX_ITEMS: u64 = 100_000;

// Byte offsets inside one `FILEDESCRIPTORW`.
const OFF_FLAGS: usize = 0;
#[allow(dead_code)] // Layout documentation: not read, the entry is skipped as a whole.
const OFF_CLSID: usize = 4;
#[allow(dead_code)] // Layout documentation: not read.
const OFF_SIZEL: usize = 20;
#[allow(dead_code)] // Layout documentation: not read.
const OFF_POINTL: usize = 28;
const OFF_ATTRIBUTES: usize = 36;
const OFF_CREATED: usize = 40;
#[allow(dead_code)] // Layout documentation: not read.
const OFF_ACCESSED: usize = 48;
const OFF_WRITTEN: usize = 56;
const OFF_SIZE_HIGH: usize = 64;
const OFF_SIZE_LOW: usize = 68;
const OFF_NAME: usize = 72;
/// `cFileName` holds `MAX_PATH` UTF-16 code units, including the terminating NUL.
const NAME_UNITS: usize = 260;

/// FILETIME ticks (100 ns) between 1601-01-01 and the Unix epoch.
const UNIX_EPOCH_TICKS: u64 = 11_644_473_600 * 10_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Parses a whole `FILEGROUPDESCRIPTORW` block.
pub fn parse_file_group_descriptor_w(bytes: &[u8]) -> crate::Result<Vec<FileDescriptor>> {
    if bytes.len() < COUNT_SIZE {
        return Err(invalid("file group descriptor is shorter than its count"));
    }
    let count = u64::from(read_u32(bytes, 0));
    if count > MAX_ITEMS {
        return Err(invalid("file group descriptor lists too many items"));
    }
    let needed = count
        .checked_mul(FILE_DESCRIPTOR_SIZE as u64)
        .and_then(|body| body.checked_add(COUNT_SIZE as u64))
        .ok_or_else(|| invalid("file group descriptor size overflows"))?;
    if (bytes.len() as u64) < needed {
        return Err(invalid(
            "file group descriptor is shorter than its item count",
        ));
    }

    // The count was checked against MAX_ITEMS, so this conversion cannot truncate.
    let count = count as usize;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let start = COUNT_SIZE + index * FILE_DESCRIPTOR_SIZE;
        entries.push(parse_entry(&bytes[start..start + FILE_DESCRIPTOR_SIZE]));
    }
    Ok(entries)
}

/// Parses one 592-byte `FILEDESCRIPTORW`. The caller guarantees the slice length.
fn parse_entry(entry: &[u8]) -> FileDescriptor {
    let flags = read_u32(entry, OFF_FLAGS);

    let attributes = (flags & FD_ATTRIBUTES != 0).then(|| read_u32(entry, OFF_ATTRIBUTES));
    let is_dir = matches!(attributes, Some(attr) if attr & FILE_ATTRIBUTE_DIRECTORY != 0);

    let size = (flags & FD_FILESIZE != 0).then(|| {
        let high = u64::from(read_u32(entry, OFF_SIZE_HIGH));
        let low = u64::from(read_u32(entry, OFF_SIZE_LOW));
        (high << 32) | low
    });

    let created = (flags & FD_CREATETIME != 0)
        .then(|| filetime_at(entry, OFF_CREATED))
        .flatten();
    let modified = (flags & FD_WRITESTIME != 0)
        .then(|| filetime_at(entry, OFF_WRITTEN))
        .flatten();

    FileDescriptor {
        name: read_name(entry),
        is_dir,
        attributes,
        size,
        created,
        modified,
        flags,
    }
}

/// Decodes `cFileName`: UTF-16 up to the first NUL, or all of it when there is none.
fn read_name(entry: &[u8]) -> String {
    let units: Vec<u16> = (0..NAME_UNITS)
        .map(|index| read_u16(entry, OFF_NAME + index * 2))
        .collect();
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(NAME_UNITS);
    String::from_utf16_lossy(&units[..end])
}

/// Reads a FILETIME field. A zero FILETIME means "not set" and gives `None`.
fn filetime_at(entry: &[u8], at: usize) -> Option<SystemTime> {
    let low = u64::from(read_u32(entry, at));
    let high = u64::from(read_u32(entry, at + 4));
    let ticks = (high << 32) | low;
    if ticks == 0 {
        return None;
    }
    filetime_to_system_time(ticks)
}

/// Converts FILETIME ticks (100 ns since 1601-01-01 UTC) to a `SystemTime`. Times before
/// 1970 become earlier than `UNIX_EPOCH`. Returns `None` if the result is out of range.
fn filetime_to_system_time(ticks: u64) -> Option<SystemTime> {
    if ticks >= UNIX_EPOCH_TICKS {
        let since_epoch = ticks - UNIX_EPOCH_TICKS;
        let secs = since_epoch / TICKS_PER_SECOND;
        let nanos = (since_epoch % TICKS_PER_SECOND) * 100;
        UNIX_EPOCH.checked_add(Duration::new(secs, nanos as u32))
    } else {
        let before_epoch = UNIX_EPOCH_TICKS - ticks;
        let secs = before_epoch / TICKS_PER_SECOND;
        let nanos = (before_epoch % TICKS_PER_SECOND) * 100;
        UNIX_EPOCH.checked_sub(Duration::new(secs, nanos as u32))
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(raw)
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    let mut raw = [0u8; 2];
    raw.copy_from_slice(&bytes[at..at + 2]);
    u16::from_le_bytes(raw)
}

fn invalid(message: &str) -> crate::WinError {
    crate::WinError::Invalid(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout check: the fields in `FILEDESCRIPTORW` add up to 592 bytes.
    #[test]
    fn layout_adds_up_to_entry_size() {
        let fields = 4 // dwFlags
            + 16 // clsid
            + 8 // sizel
            + 8 // pointl
            + 4 // dwFileAttributes
            + 8 // ftCreationTime
            + 8 // ftLastAccessTime
            + 8 // ftLastWriteTime
            + 4 // nFileSizeHigh
            + 4 // nFileSizeLow
            + NAME_UNITS * 2; // cFileName
        assert_eq!(fields, FILE_DESCRIPTOR_SIZE);
        assert_eq!(OFF_NAME + NAME_UNITS * 2, FILE_DESCRIPTOR_SIZE);
        assert_eq!(OFF_SIZE_LOW + 4, OFF_NAME);
    }

    /// FILETIME ticks for a Unix timestamp in seconds.
    fn ft(unix_secs: i64) -> u64 {
        ((unix_secs + 11_644_473_600) as u64) * TICKS_PER_SECOND
    }

    /// Encodes one `FILEDESCRIPTORW` with the given fields and UTF-16 name.
    fn entry(
        flags: u32,
        attributes: u32,
        size: u64,
        created: u64,
        written: u64,
        name: &[u16],
    ) -> [u8; FILE_DESCRIPTOR_SIZE] {
        let mut out = [0u8; FILE_DESCRIPTOR_SIZE];
        out[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        out[OFF_ATTRIBUTES..OFF_ATTRIBUTES + 4].copy_from_slice(&attributes.to_le_bytes());
        out[OFF_CREATED..OFF_CREATED + 8].copy_from_slice(&created.to_le_bytes());
        out[OFF_WRITTEN..OFF_WRITTEN + 8].copy_from_slice(&written.to_le_bytes());
        out[OFF_SIZE_HIGH..OFF_SIZE_HIGH + 4].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        out[OFF_SIZE_LOW..OFF_SIZE_LOW + 4].copy_from_slice(&(size as u32).to_le_bytes());
        for (index, unit) in name.iter().take(NAME_UNITS).enumerate() {
            let at = OFF_NAME + index * 2;
            out[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out
    }

    /// Encodes a block: the count followed by the entries.
    fn block(count: u32, entries: &[[u8; FILE_DESCRIPTOR_SIZE]]) -> Vec<u8> {
        let mut out = count.to_le_bytes().to_vec();
        for item in entries {
            out.extend_from_slice(item);
        }
        out
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn at_unix(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn parses_folder_and_large_file() {
        let all = FD_ATTRIBUTES | FD_FILESIZE | FD_CREATETIME | FD_WRITESTIME | FD_UNICODE;
        let folder = entry(
            FD_ATTRIBUTES | FD_UNICODE,
            FILE_ATTRIBUTE_DIRECTORY,
            0,
            0,
            0,
            &utf16("sub"),
        );
        let file = entry(
            all,
            0x20,
            (1u64 << 32) + 0x10,
            ft(1_577_836_800),
            ft(1_622_505_600),
            &utf16("sub\\b.txt"),
        );
        let parsed = parse_file_group_descriptor_w(&block(2, &[folder, file])).unwrap();
        assert_eq!(parsed.len(), 2);

        assert_eq!(parsed[0].name, "sub");
        assert!(parsed[0].is_dir);
        assert_eq!(parsed[0].attributes, Some(FILE_ATTRIBUTE_DIRECTORY));
        assert_eq!(parsed[0].size, None);
        assert_eq!(parsed[0].created, None);
        assert_eq!(parsed[0].modified, None);
        assert_eq!(parsed[0].flags, FD_ATTRIBUTES | FD_UNICODE);

        assert_eq!(parsed[1].name, "sub\\b.txt");
        assert!(!parsed[1].is_dir);
        assert_eq!(parsed[1].attributes, Some(0x20));
        assert_eq!(parsed[1].size, Some((1u64 << 32) + 0x10));
        assert_eq!(parsed[1].created, Some(at_unix(1_577_836_800)));
        assert_eq!(parsed[1].modified, Some(at_unix(1_622_505_600)));
        assert_eq!(parsed[1].flags, all);
    }

    #[test]
    fn missing_flags_give_none_fields() {
        // Every field is filled in, but no flags say so: the parser must ignore them.
        let item = entry(
            0,
            FILE_ATTRIBUTE_DIRECTORY,
            99,
            ft(1_577_836_800),
            ft(1_577_836_800),
            &utf16("x"),
        );
        let parsed = parse_file_group_descriptor_w(&block(1, &[item])).unwrap();
        assert_eq!(parsed[0].name, "x");
        assert!(!parsed[0].is_dir);
        assert_eq!(parsed[0].attributes, None);
        assert_eq!(parsed[0].size, None);
        assert_eq!(parsed[0].created, None);
        assert_eq!(parsed[0].modified, None);
        assert_eq!(parsed[0].flags, 0);
    }

    #[test]
    fn name_without_nul_uses_all_260_units() {
        let name = vec![u16::from(b'a'); NAME_UNITS];
        let item = entry(0, 0, 0, 0, 0, &name);
        let parsed = parse_file_group_descriptor_w(&block(1, &[item])).unwrap();
        assert_eq!(parsed[0].name.len(), NAME_UNITS);
        assert!(parsed[0].name.chars().all(|c| c == 'a'));
    }

    #[test]
    fn zero_filetime_is_none() {
        let item = entry(FD_CREATETIME | FD_WRITESTIME, 0, 0, 0, 0, &utf16("z"));
        let parsed = parse_file_group_descriptor_w(&block(1, &[item])).unwrap();
        assert_eq!(parsed[0].created, None);
        assert_eq!(parsed[0].modified, None);
    }

    #[test]
    fn pre_1970_filetime_is_before_unix_epoch() {
        let one_day_before = ft(-86_400);
        let item = entry(FD_WRITESTIME, 0, 0, 0, one_day_before, &utf16("old"));
        let parsed = parse_file_group_descriptor_w(&block(1, &[item])).unwrap();
        assert_eq!(
            parsed[0].modified,
            Some(UNIX_EPOCH - Duration::from_secs(86_400))
        );
    }

    #[test]
    fn filetime_of_one_tick_is_representable() {
        assert!(filetime_to_system_time(1).is_some());
    }

    #[test]
    fn truncated_block_is_an_error() {
        let item = entry(0, 0, 0, 0, 0, &utf16("a"));
        let bytes = block(2, &[item]);
        assert!(matches!(
            parse_file_group_descriptor_w(&bytes),
            Err(crate::WinError::Invalid(_))
        ));
    }

    #[test]
    fn huge_count_is_rejected_without_allocating() {
        let bytes = u32::MAX.to_le_bytes();
        assert!(matches!(
            parse_file_group_descriptor_w(&bytes),
            Err(crate::WinError::Invalid(_))
        ));
    }

    #[test]
    fn count_over_limit_is_rejected_even_with_enough_bytes() {
        let bytes = (MAX_ITEMS as u32 + 1).to_le_bytes();
        assert!(matches!(
            parse_file_group_descriptor_w(&bytes),
            Err(crate::WinError::Invalid(_))
        ));
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(matches!(
            parse_file_group_descriptor_w(&[]),
            Err(crate::WinError::Invalid(_))
        ));
        assert!(matches!(
            parse_file_group_descriptor_w(&[0, 0, 0]),
            Err(crate::WinError::Invalid(_))
        ));
    }

    #[test]
    fn zero_count_is_empty_and_trailing_bytes_are_fine() {
        let mut bytes = block(0, &[]);
        bytes.extend_from_slice(&[0xAB; 37]);
        let parsed = parse_file_group_descriptor_w(&bytes).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn lone_surrogate_does_not_panic() {
        let item = entry(0, 0, 0, 0, 0, &[0xD800, u16::from(b'a'), 0]);
        let parsed = parse_file_group_descriptor_w(&block(1, &[item])).unwrap();
        assert_eq!(parsed[0].name, "\u{FFFD}a");
    }
}
