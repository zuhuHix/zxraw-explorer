//! Reads Git objects (loose and packed) through the link-safe layer, with caps on every size.
//! Only what the badges need: commits and trees, to compare the index with `HEAD`.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use gix::ObjectId;
use gix::objs::Kind;

use super::safe_fs::{self, Dir, File, Fs};

/// Why an object could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OdbError {
    /// Not in this repository (shallow or partial clone, or damaged).
    Missing,
    /// A link among the object files.
    Link,
    /// Over one of the caps.
    TooBig,
    /// Damaged or unreadable data.
    Bad(String),
}

impl From<safe_fs::Error> for OdbError {
    fn from(e: safe_fs::Error) -> Self {
        match e {
            safe_fs::Error::NotFound => OdbError::Missing,
            safe_fs::Error::Link | safe_fs::Error::WrongKind | safe_fs::Error::BadName => {
                OdbError::Link
            }
            safe_fs::Error::TooBig => OdbError::TooBig,
            safe_fs::Error::Io(m) => OdbError::Bad(m),
        }
    }
}

/// Caps on object reading.
#[derive(Debug, Clone, Copy)]
pub struct OdbLimits {
    /// Largest single object, inflated (commits and trees are far smaller).
    pub object_bytes: u64,
    /// Bytes inflated over the whole scan.
    pub total_bytes: u64,
    /// Pack files in `objects/pack`.
    pub packs: usize,
    /// Size of all pack indexes together.
    pub idx_bytes: u64,
    /// Delta chain length.
    pub delta_depth: usize,
}

struct Pack {
    idx: Vec<u8>,
    count: usize,
    pack: File,
}

/// Object store of one repository.
pub struct Odb<'a> {
    fs: &'a Fs,
    objects: Dir,
    limits: OdbLimits,
    packs: Option<Vec<Pack>>,
    inflated: u64,
    /// Resolved packed objects by (pack, offset), as delta bases.
    cache: HashMap<(usize, u64), Arc<(Kind, Vec<u8>)>>,
    cache_bytes: usize,
}

const CACHE_BYTES: usize = 64 * 1024 * 1024;

impl<'a> Odb<'a> {
    pub fn new(fs: &'a Fs, objects: Dir, limits: OdbLimits) -> Self {
        Odb {
            fs,
            objects,
            limits,
            packs: None,
            inflated: 0,
            cache: HashMap::new(),
            cache_bytes: 0,
        }
    }

    /// The object `id`, of kind `want`.
    pub fn read(&mut self, id: &ObjectId, want: Kind) -> Result<Vec<u8>, OdbError> {
        let (kind, data) = match self.loose(id)? {
            Some(found) => found,
            None => self.packed(id, 0)?.ok_or(OdbError::Missing)?,
        };
        if kind != want {
            return Err(OdbError::Bad(format!("{id} is a {kind}, not a {want}")));
        }
        Ok(data)
    }

    fn loose(&mut self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, OdbError> {
        let hex = id.to_hex().to_string();
        let dir = match self.fs.dir(&self.objects, &hex[..2]) {
            Ok(dir) => dir,
            Err(safe_fs::Error::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let file = match self.fs.file(&dir, &hex[2..]) {
            Ok(file) => file,
            Err(safe_fs::Error::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Compressed data is never much larger than what it inflates to.
        let compressed = file.read_capped(self.limits.object_bytes + 1024)?;
        let mut data = Vec::new();
        flate2::read::ZlibDecoder::new(&compressed[..])
            .take(self.limits.object_bytes + 64)
            .read_to_end(&mut data)
            .map_err(|e| OdbError::Bad(e.to_string()))?;
        let nul = data
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| OdbError::Bad("loose object header".into()))?;
        let header = std::str::from_utf8(&data[..nul]).map_err(|e| OdbError::Bad(e.to_string()))?;
        let (kind, size) = header
            .split_once(' ')
            .ok_or_else(|| OdbError::Bad("loose object header".into()))?;
        let kind = Kind::from_bytes(kind.as_bytes()).map_err(|e| OdbError::Bad(e.to_string()))?;
        let size: u64 = size.parse().map_err(|_| OdbError::Bad("size".into()))?;
        if size > self.limits.object_bytes {
            return Err(OdbError::TooBig);
        }
        let body = data.split_off(nul + 1);
        if body.len() as u64 != size {
            return Err(OdbError::Bad("loose object size".into()));
        }
        self.count(size)?;
        Ok(Some((kind, body)))
    }

    fn count(&mut self, bytes: u64) -> Result<(), OdbError> {
        self.inflated += bytes;
        if self.inflated > self.limits.total_bytes {
            return Err(OdbError::TooBig);
        }
        Ok(())
    }

    /// Opens every `pack-*.idx` with its `.pack`, once.
    fn load_packs(&mut self) -> Result<(), OdbError> {
        if self.packs.is_some() {
            return Ok(());
        }
        let dir = match self.fs.dir(&self.objects, "pack") {
            Ok(dir) => dir,
            Err(safe_fs::Error::NotFound) => {
                self.packs = Some(Vec::new());
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let mut names = Vec::new();
        let mut entries = 0usize;
        let mut too_many = false;
        let max_entries = self.limits.packs.saturating_mul(4).max(16);
        dir.list(|entry| {
            entries += 1;
            if entries > max_entries {
                too_many = true;
                return false;
            }
            if let Some(stem) = entry.name.strip_suffix(".idx") {
                if stem.starts_with("pack-") {
                    names.push(stem.to_string());
                    if names.len() > self.limits.packs {
                        too_many = true;
                        return false;
                    }
                }
            }
            true
        })?;
        if too_many {
            return Err(OdbError::TooBig);
        }
        let mut packs = Vec::new();
        let mut idx_total = 0u64;
        for stem in names {
            let idx_file = self.fs.file(&dir, &format!("{stem}.idx"))?;
            idx_total += idx_file.info.size;
            if idx_total > self.limits.idx_bytes {
                return Err(OdbError::TooBig);
            }
            let idx = idx_file.read_capped(self.limits.idx_bytes)?;
            let count = check_idx(&idx)?;
            let pack = match self.fs.file(&dir, &format!("{stem}.pack")) {
                Ok(pack) => pack,
                Err(safe_fs::Error::NotFound) => continue,
                Err(e) => return Err(e.into()),
            };
            packs.push(Pack { idx, count, pack });
        }
        self.packs = Some(packs);
        Ok(())
    }

    /// Looks `id` up in the packs, then in loose objects again for a delta base.
    fn packed(&mut self, id: &ObjectId, depth: usize) -> Result<Option<(Kind, Vec<u8>)>, OdbError> {
        self.load_packs()?;
        let found = self
            .packs
            .as_ref()
            .unwrap()
            .iter()
            .enumerate()
            .find_map(|(n, p)| idx_lookup(&p.idx, p.count, id.as_slice()).map(|o| (n, o)));
        match found {
            Some((pack, offset)) => {
                let object = self.at(pack, offset, depth)?;
                Ok(Some((object.0, object.1.clone())))
            }
            None => Ok(None),
        }
    }

    /// The object at `offset` in pack `pack`, with deltas applied.
    fn at(
        &mut self,
        pack: usize,
        offset: u64,
        depth: usize,
    ) -> Result<Arc<(Kind, Vec<u8>)>, OdbError> {
        if let Some(hit) = self.cache.get(&(pack, offset)) {
            return Ok(Arc::clone(hit));
        }
        // Follow the chain down to a full object, then apply the deltas back up.
        let mut chain: Vec<Vec<u8>> = Vec::new();
        let mut at = offset;
        let mut base: Arc<(Kind, Vec<u8>)>;
        loop {
            if depth + chain.len() > self.limits.delta_depth {
                return Err(OdbError::TooBig);
            }
            if let Some(hit) = self.cache.get(&(pack, at)) {
                base = Arc::clone(hit);
                break;
            }
            let (header, size, data_at) = self.entry_header(pack, at)?;
            if size > self.limits.object_bytes {
                return Err(OdbError::TooBig);
            }
            match header {
                Header::Full(kind) => {
                    let data = self.inflate(pack, data_at, size)?;
                    base = Arc::new((kind, data));
                    self.remember(pack, at, &base);
                    break;
                }
                Header::OfsDelta(back) => {
                    chain.push(self.inflate(pack, data_at, size)?);
                    at = at
                        .checked_sub(back)
                        .ok_or_else(|| OdbError::Bad("delta base".into()))?;
                }
                Header::RefDelta(base_id) => {
                    chain.push(self.inflate(pack, data_at, size)?);
                    let next = depth + chain.len();
                    base = match self.loose(&base_id)? {
                        Some(found) => Arc::new(found),
                        None => {
                            let found = self.packed(&base_id, next)?.ok_or(OdbError::Missing)?;
                            Arc::new(found)
                        }
                    };
                    break;
                }
            }
        }
        while let Some(delta) = chain.pop() {
            let out = apply_delta(&base.1, &delta, self.limits.object_bytes)?;
            self.count(out.len() as u64)?;
            base = Arc::new((base.0, out));
        }
        self.remember(pack, offset, &base);
        Ok(base)
    }

    fn remember(&mut self, pack: usize, offset: u64, object: &Arc<(Kind, Vec<u8>)>) {
        if self.cache_bytes + object.1.len() > CACHE_BYTES {
            self.cache.clear();
            self.cache_bytes = 0;
        }
        self.cache_bytes += object.1.len();
        self.cache.insert((pack, offset), Arc::clone(object));
    }

    /// Kind, inflated size and data offset of the entry at `offset`.
    fn entry_header(&self, pack: usize, offset: u64) -> Result<(Header, u64, u64), OdbError> {
        let file = &self.packs.as_ref().unwrap()[pack].pack;
        let mut buf = [0u8; 64];
        let n = file
            .read_at(&mut buf, offset)
            .map_err(|e| OdbError::Bad(e.to_string()))?;
        let buf = &buf[..n];
        let mut pos = 0;
        let mut byte = *buf
            .first()
            .ok_or_else(|| OdbError::Bad("pack entry".into()))?;
        let kind = (byte >> 4) & 7;
        let mut size = u64::from(byte & 15);
        let mut shift = 4;
        while byte & 0x80 != 0 {
            pos += 1;
            byte = *buf
                .get(pos)
                .ok_or_else(|| OdbError::Bad("pack entry".into()))?;
            if shift > 57 {
                return Err(OdbError::Bad("pack entry size".into()));
            }
            size |= u64::from(byte & 0x7f) << shift;
            shift += 7;
        }
        pos += 1;
        let header = match kind {
            1 => Header::Full(Kind::Commit),
            2 => Header::Full(Kind::Tree),
            3 => Header::Full(Kind::Blob),
            4 => Header::Full(Kind::Tag),
            6 => {
                let mut byte = *buf
                    .get(pos)
                    .ok_or_else(|| OdbError::Bad("ofs delta".into()))?;
                pos += 1;
                let mut back = u64::from(byte & 0x7f);
                while byte & 0x80 != 0 {
                    byte = *buf
                        .get(pos)
                        .ok_or_else(|| OdbError::Bad("ofs delta".into()))?;
                    pos += 1;
                    back = back
                        .checked_add(1)
                        .and_then(|b| b.checked_mul(128))
                        .ok_or_else(|| OdbError::Bad("ofs delta".into()))?
                        | u64::from(byte & 0x7f);
                }
                if back == 0 {
                    return Err(OdbError::Bad("ofs delta".into()));
                }
                Header::OfsDelta(back)
            }
            7 => {
                let raw = buf
                    .get(pos..pos + 20)
                    .ok_or_else(|| OdbError::Bad("ref delta".into()))?;
                pos += 20;
                Header::RefDelta(ObjectId::from_bytes_or_panic(raw))
            }
            _ => return Err(OdbError::Bad("pack entry kind".into())),
        };
        Ok((header, size, offset + pos as u64))
    }

    /// Inflates the `size`-byte zlib stream at `offset` of pack `pack`.
    fn inflate(&mut self, pack: usize, offset: u64, size: u64) -> Result<Vec<u8>, OdbError> {
        self.count(size)?;
        let file = &self.packs.as_ref().unwrap()[pack].pack;
        let reader = std::io::BufReader::with_capacity(16 * 1024, At { file, pos: offset });
        let mut out = Vec::with_capacity(size.min(1 << 20) as usize);
        flate2::bufread::ZlibDecoder::new(reader)
            .take(size + 1)
            .read_to_end(&mut out)
            .map_err(|e| OdbError::Bad(e.to_string()))?;
        if out.len() as u64 != size {
            return Err(OdbError::Bad("pack entry size".into()));
        }
        Ok(out)
    }
}

enum Header {
    Full(Kind),
    OfsDelta(u64),
    RefDelta(ObjectId),
}

/// Sequential reads from a file opened by the safe layer.
struct At<'f> {
    file: &'f File,
    pos: u64,
}

impl Read for At<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Checks a version 2 pack index and returns its object count.
fn check_idx(idx: &[u8]) -> Result<usize, OdbError> {
    let bad = || OdbError::Bad("pack index".into());
    if idx.len() < 8 + 1024 || idx[..8] != [0xff, b't', b'O', b'c', 0, 0, 0, 2] {
        return Err(bad());
    }
    let count = u32::from_be_bytes(idx[8 + 1020..8 + 1024].try_into().unwrap()) as usize;
    // Names, CRCs and small offsets, then the trailer; large offsets may follow the offsets.
    let need = 8 + 1024 + count.checked_mul(28).ok_or_else(bad)? + 40;
    if idx.len() < need {
        return Err(bad());
    }
    Ok(count)
}

/// Offset of `id` in a checked pack index.
fn idx_lookup(idx: &[u8], count: usize, id: &[u8]) -> Option<u64> {
    let fan =
        |i: usize| u32::from_be_bytes(idx[8 + i * 4..12 + i * 4].try_into().unwrap()) as usize;
    let first = id[0] as usize;
    let lo = if first == 0 { 0 } else { fan(first - 1) };
    let hi = fan(first).min(count);
    let names = 8 + 1024;
    let name = |i: usize| &idx[names + i * 20..names + i * 20 + 20];
    let (mut lo, mut hi) = (lo.min(hi), hi);
    while lo < hi {
        let mid = (lo + hi) / 2;
        match name(mid).cmp(id) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let offsets = names + count * 24;
                let small = u32::from_be_bytes(
                    idx[offsets + mid * 4..offsets + mid * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                if small & 0x8000_0000 == 0 {
                    return Some(u64::from(small));
                }
                let large = offsets + count * 4 + (small & 0x7fff_ffff) as usize * 8;
                let raw = idx.get(large..large + 8)?;
                return Some(u64::from_be_bytes(raw.try_into().unwrap()));
            }
        }
    }
    None
}

/// Applies a Git delta to `base`, refusing results over `cap` bytes.
fn apply_delta(base: &[u8], delta: &[u8], cap: u64) -> Result<Vec<u8>, OdbError> {
    let bad = || OdbError::Bad("delta".into());
    let mut pos = 0usize;
    let varint = |pos: &mut usize| -> Result<u64, OdbError> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = *delta.get(*pos).ok_or_else(bad)?;
            *pos += 1;
            if shift > 63 {
                return Err(bad());
            }
            value |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    };
    let src = varint(&mut pos)?;
    let dst = varint(&mut pos)?;
    if src != base.len() as u64 {
        return Err(bad());
    }
    if dst > cap {
        return Err(OdbError::TooBig);
    }
    let mut out = Vec::with_capacity(dst as usize);
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    offset |= usize::from(*delta.get(pos).ok_or_else(bad)?) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    size |= usize::from(*delta.get(pos).ok_or_else(bad)?) << (8 * i);
                    pos += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset.checked_add(size).ok_or_else(bad)?;
            out.extend_from_slice(base.get(offset..end).ok_or_else(bad)?);
        } else if op != 0 {
            let end = pos + op as usize;
            out.extend_from_slice(delta.get(pos..end).ok_or_else(bad)?);
            pos = end;
        } else {
            return Err(bad());
        }
        if out.len() as u64 > dst {
            return Err(bad());
        }
    }
    if out.len() as u64 != dst {
        return Err(bad());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_apply_and_reject_bad_input() {
        let base = b"hello world";
        // src 11, dst 11: copy "hello " (offset 0, size 6), insert "there"
        let delta = [11, 11, 0x90, 6, 5, b't', b'h', b'e', b'r', b'e'];
        assert_eq!(apply_delta(base, &delta, 100).unwrap(), b"hello there");
        assert_eq!(apply_delta(base, &delta, 5).unwrap_err(), OdbError::TooBig);
        // Copy past the end of the base.
        let delta = [11, 20, 0x91, 5, 20];
        assert!(apply_delta(base, &delta, 100).is_err());
        assert!(apply_delta(b"x", &[11, 1, 1, b'a'], 100).is_err());
    }
}
