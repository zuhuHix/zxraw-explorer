//! XMP sidecar writer: merges ratings, colour labels and keywords into the `.xmp` sidecar that
//! Lightroom, Capture One, darktable and Bridge read. Originals are never opened for writing.
//!
//! Merge strategy: the existing sidecar is never re-serialised. A quick-xml pass records the
//! byte span of every element and attribute involved, the requested changes become splices
//! into the original text, and the result is re-parsed and checked against the requested
//! values before anything touches the disk. Everything else (unknown namespaces, `crs:`
//! develop settings, comments, processing instructions, element order, whitespace) survives
//! byte-for-byte. A sidecar that cannot be parsed is reported and left alone. Each successful
//! replacement refreshes `.xmp.bak` with the exact bytes that were replaced; it is a one-version
//! recovery copy, not a multi-generation archive.
//!
//! Limitations: only `xmp:Rating`, `xmp:Label` and `dc:subject` are written. Picks are not
//! stored in XMP (there is no standard property for them) and stay in the app library.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;
use quick_xml::Reader;
use quick_xml::escape::{escape, unescape};
use quick_xml::events::Event;

use crate::error::{MediaError, Result};
use crate::image_meta::{XmpData, XmpSource, parse_xmp};

#[cfg(test)]
static EXTERNAL_EDIT_BEFORE_REPLACE: std::sync::Mutex<Option<(PathBuf, Vec<u8>)>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static CONCURRENT_UPDATE_PATH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static CONCURRENT_UPDATE_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static CONCURRENT_UPDATE_MAX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn simulate_external_edit(path: &Path) {
    let monitor = CONCURRENT_UPDATE_PATH
        .lock()
        .expect("concurrent-update test hook poisoned")
        .as_ref()
        .is_some_and(|target| target == path);
    if monitor {
        let active = CONCURRENT_UPDATE_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        CONCURRENT_UPDATE_MAX.fetch_max(active, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(40));
        CONCURRENT_UPDATE_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }

    let mut pending = EXTERNAL_EDIT_BEFORE_REPLACE
        .lock()
        .expect("external-edit test hook poisoned");
    if pending.as_ref().is_some_and(|(target, _)| target == path) {
        let (_, bytes) = pending.take().expect("test hook checked as present");
        std::fs::write(path, bytes).expect("simulate external sidecar writer");
    }
}

#[cfg(not(test))]
fn simulate_external_edit(_path: &Path) {}

const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// Sidecars are normally a few KB; anything this large is refused rather than rewritten.
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHANGE_RETRIES: usize = 3;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static WRITER_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

/// Packet used when no sidecar exists yet. Namespace declarations for `xmp:` and `dc:` are
/// added by the merge itself, so they are only present here if the packet needs them.
const NEW_PACKET: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"ZXraw Explorer\">\n\
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
<rdf:Description rdf:about=\"\"></rdf:Description>\n\
</rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>\n";

/// Changes to apply to a sidecar. `None` leaves a property alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidecarUpdate {
    /// `-1` = rejected (Lightroom convention), `0` = unrated, `1..=5` stars.
    pub rating: Option<i32>,
    /// `Some(None)` removes `xmp:Label`; `Some(Some("Red"))` sets it.
    pub label: Option<Option<String>>,
    pub add_keywords: Vec<String>,
    pub remove_keywords: Vec<String>,
}

impl SidecarUpdate {
    /// True when the update would change nothing; such an update never touches the disk.
    pub fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.label.is_none()
            && self.add_keywords.iter().all(|k| k.trim().is_empty())
            && self.remove_keywords.iter().all(|k| k.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarOutcome {
    /// No sidecar existed; a new one was written.
    Created(PathBuf),
    /// An existing sidecar was merged and rewritten; `.xmp.bak` holds the exact version
    /// replaced by this update and is refreshed for each successful replacement.
    Updated(PathBuf),
    /// The sidecar already contained the requested values; nothing was written.
    Unchanged(PathBuf),
}

impl SidecarOutcome {
    pub fn path(&self) -> &Path {
        match self {
            Self::Created(p) | Self::Updated(p) | Self::Unchanged(p) => p,
        }
    }
}

/// Writes `update` to the sidecar belonging to `image_path`.
///
/// Naming: an existing `IMG_0001.CR3.xmp` (darktable) is always used; otherwise
/// `IMG_0001.xmp` (Lightroom / Capture One) is used or created. Never both.
pub fn update_sidecar(image_path: &Path, update: &SidecarUpdate) -> Result<SidecarOutcome> {
    if forge_core::model::ext_of(image_path) == "xmp" {
        return Err(MediaError::Unsupported(format!(
            "{} is a sidecar, not an image",
            image_path.display()
        )));
    }
    if let Some(r) = update.rating
        && !(-1..=5).contains(&r)
    {
        return Err(MediaError::Unsupported(format!(
            "rating {r} is out of range"
        )));
    }
    let path = sidecar_path(image_path);
    if update.is_empty() {
        return Ok(SidecarOutcome::Unchanged(path));
    }
    let lock = writer_lock(&path);
    let _guard = lock.lock();

    let mut snapshot = read_snapshot(&path)?;
    for _ in 0..=MAX_CHANGE_RETRIES {
        let Some(original) = snapshot.as_deref() else {
            let merged =
                apply_update(NEW_PACKET, image_path, update).map_err(|e| with_path(&path, e))?;
            ensure_sidecar_size(merged.len(), &path)?;
            simulate_external_edit(&path);
            if let Some(latest) = read_snapshot(&path)? {
                snapshot = Some(latest);
                continue;
            }
            if create_new_atomic(&path, merged.as_bytes())? {
                return Ok(SidecarOutcome::Created(path));
            }
            snapshot = read_snapshot(&path)?;
            continue;
        };

        if original.len() as u64 > MAX_SIDECAR_BYTES {
            return Err(MediaError::Unsupported(format!(
                "{} is too large to rewrite safely",
                path.display()
            )));
        }
        let text = decode_for_write(original, &path)?;
        let merged = apply_update(&text, image_path, update).map_err(|e| with_path(&path, e))?;
        ensure_sidecar_size(merged.len(), &path)?;

        // This test-only seam simulates another editor saving after our read and merge.
        simulate_external_edit(&path);
        let latest = read_snapshot(&path)?;
        if latest.as_deref() != Some(original) {
            snapshot = latest;
            continue;
        }
        if merged == text {
            return Ok(SidecarOutcome::Unchanged(path));
        }

        // Refresh the recoverable copy with the exact bytes observed for this replacement.
        // Check again after backup I/O, which creates another opportunity for an external save.
        let bak = backup_path(&path);
        write_atomic(&bak, original)?;
        let latest = read_snapshot(&path)?;
        if latest.as_deref() != Some(original) {
            snapshot = latest;
            continue;
        }
        write_atomic(&path, merged.as_bytes())?;
        return Ok(SidecarOutcome::Updated(path));
    }

    Err(MediaError::Unsupported(format!(
        "{} changed repeatedly during the metadata edit; no update was written",
        path.display()
    )))
}

/// Reads the properties ZXraw edits from the RDF description belonging to this image. The
/// tolerant general parser remains the fallback for malformed or non-RDF sidecars.
pub(crate) fn parse_sidecar_xmp(xml: &str, image_path: &Path) -> XmpData {
    let fallback = parse_xmp(xml, XmpSource::Sidecar);
    let Ok(doc) = Doc::parse(xml) else {
        return fallback;
    };
    let Ok(description) = doc.description(image_path) else {
        return fallback;
    };
    let node = &doc.nodes[description];
    let mut data = parse_xmp(&xml[node.start..node.end], XmpSource::Sidecar);
    if let Ok(values) = simple_values(&doc, xml, description, XMP_NS, "Rating") {
        data.rating = values
            .first()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .map(|value| (value.round() as i32).clamp(-1, 5));
    }
    if let Ok(values) = simple_values(&doc, xml, description, XMP_NS, "Label") {
        data.label = values.into_iter().find(|value| !value.trim().is_empty());
    }
    if let Ok(keywords) = read_keywords(&doc, xml, description) {
        data.keywords = keywords;
    }
    data
}

/// The sidecar that `image` uses: an existing `name.ext.xmp` wins, otherwise `name.xmp`.
fn sidecar_path(image: &Path) -> PathBuf {
    let full = append_ext(image.as_os_str(), ".xmp");
    if full.is_file() {
        return full;
    }
    image.with_extension("xmp")
}

fn backup_path(sidecar: &Path) -> PathBuf {
    append_ext(sidecar.as_os_str(), ".bak")
}

fn writer_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = lock_key(path);
    let registry = WRITER_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry.lock();
    locks.retain(|_, weak| weak.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn lock_key(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    let parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let key = parent.join(absolute.file_name().unwrap_or_default());
    #[cfg(windows)]
    {
        PathBuf::from(key.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        key
    }
}

fn read_snapshot(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(MediaError::io(path, e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| MediaError::io(path, e))?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(MediaError::Unsupported(format!(
            "{} is too large to rewrite safely",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

fn ensure_sidecar_size(len: usize, path: &Path) -> Result<()> {
    if len as u64 > MAX_SIDECAR_BYTES {
        return Err(MediaError::Unsupported(format!(
            "merged XMP for {} is too large to write safely",
            path.display()
        )));
    }
    Ok(())
}

fn decode_for_write(bytes: &[u8], path: &Path) -> Result<String> {
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
    {
        return Err(MediaError::Unsupported(format!(
            "unsupported encoding: UTF-32 XMP at {}; left untouched",
            path.display()
        )));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) {
        return Err(MediaError::Unsupported(format!(
            "unsupported encoding: UTF-16 XMP at {}; left untouched",
            path.display()
        )));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        MediaError::Corrupt(format!("{}: not UTF-8 XMP; left untouched", path.display()))
    })
}

fn append_ext(base: &std::ffi::OsStr, ext: &str) -> PathBuf {
    let mut s = base.to_owned();
    s.push(ext);
    PathBuf::from(s)
}

fn with_path(path: &Path, e: MediaError) -> MediaError {
    match e {
        MediaError::Corrupt(m) => MediaError::Corrupt(format!("{}: {m}", path.display())),
        other => other,
    }
}

/// Writes through a unique, create-new temp file in the same folder, then renames over the
/// target. Collisions with stale files or another process are rejected by `create_new`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = write_temp(path, bytes)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(MediaError::io(path, e));
    }
    Ok(())
}

/// Atomically creates a target that is still absent. A competing creator wins without being
/// overwritten; the caller can then re-read and merge its sidecar.
fn create_new_atomic(path: &Path, bytes: &[u8]) -> Result<bool> {
    let tmp = write_temp(path, bytes)?;
    let moved = move_new_without_replace(&tmp, path);
    let _ = fs::remove_file(&tmp);
    match moved {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(MediaError::io(path, e)),
    }
}

#[cfg(windows)]
fn move_new_without_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let from: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(existing_name: *const u16, new_name: *const u16) -> i32;
    }

    // The temporary file is in the destination directory, so MoveFileW is an atomic,
    // same-volume move and fails with AlreadyExists instead of replacing a competing file.
    let moved = unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) };
    if moved != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn move_new_without_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)
}

fn write_temp(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let mut last_collision = None;
    for _ in 0..128 {
        let mut tmp_name = OsString::from(".");
        if let Some(name) = path.file_name() {
            tmp_name.push(name);
        }
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        tmp_name.push(format!(".{}.{}.tmp", std::process::id(), counter));
        let tmp = path.with_file_name(tmp_name);
        let open = OpenOptions::new().write(true).create_new(true).open(&tmp);
        let mut file = match open {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(e);
                continue;
            }
            Err(e) => return Err(MediaError::io(path, e)),
        };
        if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(MediaError::io(path, e));
        }
        drop(file);
        return Ok(tmp);
    }
    Err(MediaError::io(
        path,
        last_collision.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a unique temp file",
            )
        }),
    ))
}

fn corrupt(msg: impl Into<String>) -> MediaError {
    MediaError::Corrupt(msg.into())
}

fn unsupported(msg: impl Into<String>) -> MediaError {
    MediaError::Unsupported(msg.into())
}

/// Produces the merged text for `text` (the original sidecar, or the new packet).
fn apply_update(text: &str, image_path: &Path, update: &SidecarUpdate) -> Result<String> {
    let doc = Doc::parse(text)?;
    let description = doc.description(image_path)?;
    let current_keywords = read_keywords(&doc, text, description)?;
    let adds = normalize(&update.add_keywords);
    let removes = normalize(&update.remove_keywords);

    let mut edits = Vec::new();
    let mut namespaces = NamespacePlan::new(&doc);
    if let Some(rating) = update.rating {
        set_simple(
            &doc,
            text,
            description,
            PropertyName {
                namespace: XMP_NS,
                local: "Rating",
                preferred_prefix: "xmp",
            },
            &rating.to_string(),
            &mut namespaces,
            &mut edits,
        )?;
    }
    match &update.label {
        Some(Some(label)) => {
            set_simple(
                &doc,
                text,
                description,
                PropertyName {
                    namespace: XMP_NS,
                    local: "Label",
                    preferred_prefix: "xmp",
                },
                label,
                &mut namespaces,
                &mut edits,
            )?;
        }
        Some(None) => remove_simple(&doc, text, description, XMP_NS, "Label", &mut edits)?,
        None => {}
    }
    let (keywords, _) = keyword_edits(
        &doc,
        text,
        description,
        KeywordRequest {
            adds: &adds,
            removes: &removes,
        },
        &current_keywords,
        &mut namespaces,
        &mut edits,
    )?;
    namespaces.push_declarations(&mut edits);

    let out = splice(text, edits)?;
    verify(text, &out, image_path, update, &keywords)?;
    Ok(out)
}

/// Re-parses the merged packet, checks namespace-resolved values on the image description,
/// and ensures every other RDF description remains byte-identical.
fn verify(
    before: &str,
    out: &str,
    image_path: &Path,
    update: &SidecarUpdate,
    keywords: &[String],
) -> Result<()> {
    let before_doc = Doc::parse(before).map_err(|_| corrupt("original XMP is not well-formed"))?;
    let after_doc =
        Doc::parse(out).map_err(|_| corrupt("merged XMP is not well-formed; left untouched"))?;
    let before_desc = before_doc.description(image_path)?;
    let after_desc = after_doc.description(image_path)?;
    if description_snapshots(&before_doc, before, before_desc)
        != description_snapshots(&after_doc, out, after_desc)
    {
        return Err(corrupt(
            "merged XMP changed a description for another resource; left untouched",
        ));
    }

    if let Some(rating) = update.rating {
        let expected = rating.to_string();
        let values = simple_values(&after_doc, out, after_desc, XMP_NS, "Rating")?;
        if values.is_empty() || values.iter().any(|value| value.trim() != expected) {
            return Err(corrupt("merged XMP rating did not verify; left untouched"));
        }
    }
    if let Some(label) = &update.label {
        let values = simple_values(&after_doc, out, after_desc, XMP_NS, "Label")?;
        match label {
            Some(expected)
                if !values.is_empty() && values.iter().all(|value| value == expected) => {}
            None if values.is_empty() => {}
            _ => return Err(corrupt("merged XMP label did not verify; left untouched")),
        }
    }
    if read_keywords(&after_doc, out, after_desc)? != keywords {
        return Err(corrupt(
            "merged XMP keywords did not verify; left untouched",
        ));
    }
    Ok(())
}

fn normalize(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect()
}

struct PropertyName<'a> {
    namespace: &'a str,
    local: &'a str,
    preferred_prefix: &'a str,
}

/// Sets a namespace-resolved simple property on the selected image description. Existing
/// attribute and element spellings are preserved, including aliases such as `xap:Rating`.
fn set_simple(
    doc: &Doc<'_>,
    text: &str,
    description: usize,
    property: PropertyName<'_>,
    value: &str,
    namespaces: &mut NamespacePlan<'_, '_>,
    edits: &mut Vec<Edit>,
) -> Result<()> {
    let mut found = false;
    for attr in doc.nodes[description]
        .attrs
        .iter()
        .filter(|attr| doc.attr_is(description, attr, property.namespace, property.local))
    {
        found = true;
        edits.push(Edit::replace(
            attr.value.clone(),
            escape(value).into_owned(),
        ));
    }
    for child in doc.children(description) {
        if doc.element_is(child, property.namespace, property.local) {
            found = true;
            let node = &doc.nodes[child];
            if node.empty {
                edits.push(Edit::replace(
                    node.start..node.end,
                    format!("<{}>{}</{}>", node.name, escape(value), node.name),
                ));
            } else {
                simple_content(text, node)?;
                edits.push(Edit::replace(
                    node.tag_end..node.close,
                    escape(value).into_owned(),
                ));
            }
        }
    }
    if !found {
        let prefix = namespaces.prefix_for(
            description,
            property.namespace,
            property.preferred_prefix,
            false,
        );
        let name = qualified_name(&prefix, property.local);
        edits.push(Edit::insert(
            doc.nodes[description].name_end,
            format!(" {name}=\"{}\"", escape(value)),
        ));
    }
    Ok(())
}

/// Removes a namespace-resolved simple property on the selected image description.
fn remove_simple(
    doc: &Doc<'_>,
    text: &str,
    description: usize,
    namespace: &str,
    local: &str,
    edits: &mut Vec<Edit>,
) -> Result<()> {
    for attr in doc.nodes[description]
        .attrs
        .iter()
        .filter(|attr| doc.attr_is(description, attr, namespace, local))
    {
        edits.push(Edit::replace(attr.span.clone(), ""));
    }
    for child in doc.children(description) {
        if doc.element_is(child, namespace, local) {
            let node = &doc.nodes[child];
            if !node.empty {
                simple_content(text, node)?;
            }
            edits.push(Edit::replace(node.start..node.end, ""));
        }
    }
    Ok(())
}

/// Simple (text-only) element content; anything structured is refused, never flattened.
fn simple_content<'a>(text: &'a str, node: &Node<'_>) -> Result<&'a str> {
    let body = &text[node.tag_end..node.close];
    if body.contains('<') {
        return Err(unsupported(format!(
            "{} has structured content; left untouched",
            node.name
        )));
    }
    Ok(body)
}

fn read_keywords(doc: &Doc<'_>, text: &str, description: usize) -> Result<Vec<String>> {
    let subject_attr = doc.nodes[description]
        .attrs
        .iter()
        .find(|attr| doc.attr_is(description, attr, DC_NS, "subject"));
    if let Some(attr) = subject_attr {
        let raw = unescape(&text[attr.value.clone()])
            .map_err(|e| corrupt(format!("bad entity in dc:subject: {e}")))?;
        return Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect());
    }
    let Some(subject) = doc
        .children(description)
        .into_iter()
        .find(|&child| doc.element_is(child, DC_NS, "subject"))
    else {
        return Ok(Vec::new());
    };
    let Some(bag) = doc.children(subject).into_iter().find(|&child| {
        doc.element_is(child, RDF_NS, "Bag") || doc.element_is(child, RDF_NS, "Seq")
    }) else {
        return Ok(Vec::new());
    };
    let mut keywords = Vec::new();
    for li in doc
        .children(bag)
        .into_iter()
        .filter(|&child| doc.element_is(child, RDF_NS, "li"))
    {
        let node = &doc.nodes[li];
        let value = if node.empty {
            String::new()
        } else {
            let body = simple_content(text, node)?;
            unescape(body.trim())
                .map_err(|e| corrupt(format!("bad entity in keyword: {e}")))?
                .into_owned()
        };
        if !value.is_empty() {
            keywords.push(value);
        }
    }
    keywords.dedup();
    Ok(keywords)
}

struct KeywordRequest<'a> {
    adds: &'a [String],
    removes: &'a [String],
}

/// Applies case-insensitive additions/removals to the selected description's subject bag.
fn keyword_edits(
    doc: &Doc<'_>,
    text: &str,
    description: usize,
    request: KeywordRequest<'_>,
    current: &[String],
    namespaces: &mut NamespacePlan<'_, '_>,
    edits: &mut Vec<Edit>,
) -> Result<(Vec<String>, bool)> {
    let removal_keys: Vec<String> = request
        .removes
        .iter()
        .map(|item| item.to_lowercase())
        .collect();
    let mut fin: Vec<String> = current
        .iter()
        .filter(|k| !removal_keys.contains(&k.to_lowercase()))
        .cloned()
        .collect();
    for k in request.adds {
        if !fin
            .iter()
            .any(|existing| existing.to_lowercase() == k.to_lowercase())
        {
            fin.push(k.clone());
        }
    }
    if fin == current {
        return Ok((fin, false));
    }
    if doc.nodes[description]
        .attrs
        .iter()
        .any(|attr| doc.attr_is(description, attr, DC_NS, "subject"))
    {
        return Err(unsupported(
            "dc:subject is stored as an attribute; left untouched",
        ));
    }

    let Some(subject) = doc
        .children(description)
        .into_iter()
        .find(|&child| doc.element_is(child, DC_NS, "subject"))
    else {
        let dc_prefix = namespaces.prefix_for(description, DC_NS, "dc", false);
        let rdf_prefix = namespaces.prefix_for(description, RDF_NS, "rdf", false);
        let subject_name = qualified_name(&dc_prefix, "subject");
        doc.append_child(
            text,
            description,
            &bag_xml(&subject_name, &rdf_prefix, &fin),
            edits,
        )?;
        return Ok((fin, true));
    };
    let s = &doc.nodes[subject];
    if doc.children(subject).is_empty() && text[s.tag_end..s.close].trim().is_empty() {
        let rdf_prefix = namespaces.prefix_for(description, RDF_NS, "rdf", false);
        edits.push(Edit::replace(
            s.start..s.end,
            bag_xml(s.name, &rdf_prefix, &fin),
        ));
        return Ok((fin, true));
    }

    let bag = doc
        .children(subject)
        .into_iter()
        .find(|&c| doc.element_is(c, RDF_NS, "Bag") || doc.element_is(c, RDF_NS, "Seq"))
        .ok_or_else(|| unsupported("dc:subject has no rdf:Bag; left untouched"))?;
    let lis: Vec<usize> = doc
        .children(bag)
        .into_iter()
        .filter(|&c| doc.element_is(c, RDF_NS, "li"))
        .collect();

    let mut kept: Vec<String> = Vec::new();
    for &li in &lis {
        let node = &doc.nodes[li];
        let value = if node.empty {
            String::new()
        } else {
            let body = simple_content(text, node)?;
            unescape(body.trim())
                .map_err(|e| corrupt(format!("bad entity in keyword: {e}")))?
                .into_owned()
        };
        if removal_keys.contains(&value.to_lowercase()) {
            edits.push(Edit::replace(leading_ws(text, node.start)..node.end, ""));
        } else {
            kept.push(value);
        }
    }
    let kept_keys: Vec<String> = kept.iter().map(|item| item.to_lowercase()).collect();
    let new_items: Vec<String> = fin
        .iter()
        .filter(|item| !kept_keys.contains(&item.to_lowercase()))
        .cloned()
        .collect();
    if new_items.is_empty() {
        return Ok((fin, false));
    }

    let bag_node = &doc.nodes[bag];
    let rdf_prefix = if bag_node.empty {
        namespaces.prefix_for(description, RDF_NS, "rdf", false)
    } else {
        namespaces.prefix_for(bag, RDF_NS, "rdf", false)
    };
    if bag_node.empty {
        edits.push(Edit::replace(
            bag_node.start..bag_node.end,
            format!(
                "<{0}>{1}</{0}>",
                bag_node.name,
                li_xml(&new_items, &rdf_prefix, "")
            ),
        ));
    } else if let Some(&last) = lis.last() {
        let last_node = &doc.nodes[last];
        let ws = &text[leading_ws(text, last_node.start)..last_node.start];
        edits.push(Edit::insert(
            last_node.end,
            li_xml(&new_items, &rdf_prefix, ws),
        ));
    } else {
        edits.push(Edit::insert(
            bag_node.close,
            li_xml(&new_items, &rdf_prefix, ""),
        ));
    }
    Ok((fin, true))
}

fn bag_xml(subject_name: &str, rdf_prefix: &str, items: &[String]) -> String {
    format!(
        "<{subject_name}><{bag}>{items}</{bag}></{subject_name}>",
        bag = qualified_name(rdf_prefix, "Bag"),
        items = li_xml(items, rdf_prefix, "")
    )
}

fn li_xml(items: &[String], rdf_prefix: &str, indent: &str) -> String {
    let li_name = qualified_name(rdf_prefix, "li");
    items
        .iter()
        .map(|k| format!("{indent}<{li_name}>{}</{li_name}>", escape(k)))
        .collect()
}

/// Start of the whitespace run immediately before `pos`.
fn leading_ws(text: &str, pos: usize) -> usize {
    let b = text.as_bytes();
    let mut s = pos;
    while s > 0 && is_ws(b[s - 1]) {
        s -= 1;
    }
    s
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// A byte-range replacement against the original text.
struct Edit {
    at: Range<usize>,
    with: String,
}

impl Edit {
    fn replace(at: Range<usize>, with: impl Into<String>) -> Self {
        Self {
            at,
            with: with.into(),
        }
    }

    fn insert(pos: usize, with: impl Into<String>) -> Self {
        Self {
            at: pos..pos,
            with: with.into(),
        }
    }
}

fn splice(text: &str, mut edits: Vec<Edit>) -> Result<String> {
    edits.sort_by_key(|e| (e.at.start, e.at.end));
    let mut out = String::with_capacity(text.len() + 256);
    let mut pos = 0;
    for e in edits {
        if e.at.start < pos || e.at.end < e.at.start {
            return Err(corrupt("internal: overlapping XMP edits; left untouched"));
        }
        out.push_str(&text[pos..e.at.start]);
        out.push_str(&e.with);
        pos = e.at.end;
    }
    out.push_str(&text[pos..]);
    Ok(out)
}

/// Element and attribute positions of a well-formed XML document. Built from quick-xml's
/// event stream, so every range is an exact byte offset into the original text.
struct Doc<'a> {
    text: &'a str,
    nodes: Vec<Node<'a>>,
}

struct Node<'a> {
    name: &'a str,
    /// Offset of `<`.
    start: usize,
    /// Offset just after the name (where new attributes are inserted).
    name_end: usize,
    /// Offset just after the start tag's `>` (or `/>`).
    tag_end: usize,
    empty: bool,
    /// Offset of `</` for the matching end tag; equals `tag_end` for empty elements.
    close: usize,
    /// Offset just after the end tag; equals `tag_end` for empty elements.
    end: usize,
    parent: Option<usize>,
    attrs: Vec<Attr<'a>>,
}

struct Attr<'a> {
    name: &'a str,
    /// Value bytes between the quotes.
    value: Range<usize>,
    /// Leading whitespace through the closing quote; used to remove the attribute.
    span: Range<usize>,
}

impl<'a> Doc<'a> {
    fn parse(text: &'a str) -> Result<Self> {
        // quick-xml skips a leading BOM without counting it in its offsets, so the reader
        // runs on the text after it and every offset is shifted back to the full text.
        let bom = if text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let mut reader = Reader::from_str(&text[bom..]);
        reader.config_mut().check_end_names = true;
        let mut nodes: Vec<Node<'a>> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut prev = bom;
        loop {
            let event = reader
                .read_event()
                .map_err(|e| corrupt(format!("XML syntax: {e}")))?;
            let pos = bom + reader.buffer_position() as usize;
            match &event {
                Event::Start(_) | Event::Empty(_) => {
                    let empty = matches!(&event, Event::Empty(_));
                    let parent = stack.last().copied();
                    let node = parse_tag(text, prev, pos, empty, parent)?;
                    let idx = nodes.len();
                    nodes.push(node);
                    if !empty {
                        stack.push(idx);
                    }
                }
                Event::End(_) => {
                    let idx = stack.pop().ok_or_else(|| corrupt("unbalanced end tag"))?;
                    nodes[idx].close = prev;
                    nodes[idx].end = pos;
                }
                Event::Eof => break,
                _ => {}
            }
            prev = pos;
        }
        if !stack.is_empty() {
            return Err(corrupt("unclosed XML element"));
        }
        Ok(Self { text, nodes })
    }

    fn description(&self, image_path: &Path) -> Result<usize> {
        let descriptions: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(idx, _)| self.element_is(idx, RDF_NS, "Description").then_some(idx))
            .collect();
        if let Some(matching) = descriptions.iter().copied().find(|&idx| {
            self.about(idx)
                .is_some_and(|about| about_matches_image(&about, image_path))
        }) {
            return Ok(matching);
        }
        if let Some(empty) = descriptions
            .iter()
            .copied()
            .find(|&idx| self.about(idx).as_deref() == Some(""))
        {
            return Ok(empty);
        }
        if let Some(unscoped) = descriptions
            .iter()
            .copied()
            .find(|&idx| self.about(idx).is_none() && self.description_has_xmp_properties(idx))
        {
            return Ok(unscoped);
        }
        if let Some(unscoped) = descriptions
            .iter()
            .copied()
            .find(|&idx| self.about(idx).is_none())
        {
            return Ok(unscoped);
        }
        Err(corrupt("no rdf:Description for this image; left untouched"))
    }

    fn children(&self, idx: usize) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&j| self.nodes[j].parent == Some(idx))
            .collect()
    }

    fn namespace_value(&self, element: usize, prefix: &str) -> Option<String> {
        if prefix == "xml" {
            return Some("http://www.w3.org/XML/1998/namespace".to_owned());
        }
        let mut current = Some(element);
        while let Some(idx) = current {
            for attr in &self.nodes[idx].attrs {
                if declaration_prefix(attr.name) == Some(prefix) {
                    return unescape(&self.text[attr.value.clone()])
                        .ok()
                        .map(|v| v.into_owned());
                }
            }
            current = self.nodes[idx].parent;
        }
        None
    }

    fn element_is(&self, idx: usize, namespace: &str, local: &str) -> bool {
        let (_, name) = split_qname(self.nodes[idx].name);
        name == local && self.element_in_namespace(idx, namespace)
    }

    fn element_in_namespace(&self, idx: usize, namespace: &str) -> bool {
        let (prefix, _) = split_qname(self.nodes[idx].name);
        self.namespace_value(idx, prefix)
            .is_some_and(|found| found == namespace)
    }

    fn attr_is(&self, element: usize, attr: &Attr<'_>, namespace: &str, local: &str) -> bool {
        let (prefix, name) = split_qname(attr.name);
        name == local && self.attr_in_namespace(element, attr, namespace) && !prefix.is_empty()
    }

    fn attr_in_namespace(&self, element: usize, attr: &Attr<'_>, namespace: &str) -> bool {
        let (prefix, _) = split_qname(attr.name);
        !prefix.is_empty()
            && self
                .namespace_value(element, prefix)
                .is_some_and(|found| found == namespace)
    }

    fn about(&self, description: usize) -> Option<String> {
        self.nodes[description]
            .attrs
            .iter()
            .find(|attr| self.attr_is(description, attr, RDF_NS, "about"))
            .and_then(|attr| unescape(&self.text[attr.value.clone()]).ok())
            .map(|value| value.into_owned())
    }

    fn description_has_xmp_properties(&self, description: usize) -> bool {
        self.nodes[description]
            .attrs
            .iter()
            .any(|attr| self.attr_in_namespace(description, attr, XMP_NS))
            || self
                .children(description)
                .into_iter()
                .any(|child| self.element_in_namespace(child, XMP_NS))
    }

    /// Adds `child` as the last child of `parent`, keeping the parent's end tag.
    fn append_child(
        &self,
        text: &str,
        parent: usize,
        child: &str,
        edits: &mut Vec<Edit>,
    ) -> Result<()> {
        let p = &self.nodes[parent];
        if p.empty {
            if text.get(p.tag_end - 2..p.tag_end) != Some("/>") {
                return Err(corrupt("unexpected empty-element form; left untouched"));
            }
            edits.push(Edit::replace(
                p.tag_end - 2..p.tag_end,
                format!(">{child}</{}>", p.name),
            ));
        } else {
            edits.push(Edit::insert(p.close, child));
        }
        Ok(())
    }
}

fn simple_values(
    doc: &Doc<'_>,
    text: &str,
    description: usize,
    namespace: &str,
    local: &str,
) -> Result<Vec<String>> {
    let mut values = Vec::new();
    for attr in doc.nodes[description]
        .attrs
        .iter()
        .filter(|attr| doc.attr_is(description, attr, namespace, local))
    {
        values.push(
            unescape(&text[attr.value.clone()])
                .map_err(|e| corrupt(format!("bad entity in property: {e}")))?
                .into_owned(),
        );
    }
    for child in doc.children(description) {
        if doc.element_is(child, namespace, local) {
            let node = &doc.nodes[child];
            if node.empty {
                values.push(String::new());
            } else {
                let body = simple_content(text, node)?;
                values.push(
                    unescape(body)
                        .map_err(|e| corrupt(format!("bad entity in property: {e}")))?
                        .into_owned(),
                );
            }
        }
    }
    Ok(values)
}

fn description_snapshots(doc: &Doc<'_>, text: &str, selected: usize) -> Vec<String> {
    doc.nodes
        .iter()
        .enumerate()
        .filter(|(idx, _)| *idx != selected && doc.element_is(*idx, RDF_NS, "Description"))
        .map(|(_, node)| text[node.start..node.end].to_owned())
        .collect()
}

fn split_qname(name: &str) -> (&str, &str) {
    name.split_once(':').unwrap_or(("", name))
}

fn declaration_prefix(name: &str) -> Option<&str> {
    if name == "xmlns" {
        Some("")
    } else {
        name.strip_prefix("xmlns:")
    }
}

fn qualified_name(prefix: &str, local: &str) -> String {
    if prefix.is_empty() {
        local.to_owned()
    } else {
        format!("{prefix}:{local}")
    }
}

struct NamespacePlan<'d, 'a> {
    doc: &'d Doc<'a>,
    additions: Vec<(usize, String, String)>,
}

impl<'d, 'a> NamespacePlan<'d, 'a> {
    fn new(doc: &'d Doc<'a>) -> Self {
        Self {
            doc,
            additions: Vec::new(),
        }
    }

    fn binding(&self, element: usize, prefix: &str) -> Option<String> {
        let mut current = Some(element);
        while let Some(idx) = current {
            if let Some((_, _, namespace)) = self
                .additions
                .iter()
                .rev()
                .find(|(node, found, _)| *node == idx && found == prefix)
            {
                return Some(namespace.clone());
            }
            for attr in &self.doc.nodes[idx].attrs {
                if declaration_prefix(attr.name) == Some(prefix) {
                    return unescape(&self.doc.text[attr.value.clone()])
                        .ok()
                        .map(|v| v.into_owned());
                }
            }
            current = self.doc.nodes[idx].parent;
        }
        if prefix == "xml" {
            Some("http://www.w3.org/XML/1998/namespace".to_owned())
        } else {
            None
        }
    }

    fn scoped_prefixes(&self, element: usize) -> Vec<String> {
        let mut prefixes = Vec::new();
        let mut current = Some(element);
        while let Some(idx) = current {
            for (_, prefix, _) in self
                .additions
                .iter()
                .rev()
                .filter(|(node, _, _)| *node == idx)
            {
                if !prefixes.contains(prefix) {
                    prefixes.push(prefix.clone());
                }
            }
            for attr in &self.doc.nodes[idx].attrs {
                if let Some(prefix) = declaration_prefix(attr.name)
                    && !prefixes.iter().any(|found| found == prefix)
                {
                    prefixes.push(prefix.to_owned());
                }
            }
            current = self.doc.nodes[idx].parent;
        }
        prefixes
    }

    fn prefix_for(
        &mut self,
        element: usize,
        namespace: &str,
        preferred: &str,
        allow_default: bool,
    ) -> String {
        if allow_default && self.binding(element, "").as_deref() == Some(namespace) {
            return String::new();
        }
        if self.binding(element, preferred).as_deref() == Some(namespace) {
            return preferred.to_owned();
        }
        if let Some(prefix) = self.scoped_prefixes(element).into_iter().find(|prefix| {
            !prefix.is_empty() && self.binding(element, prefix).as_deref() == Some(namespace)
        }) {
            return prefix;
        }

        let mut suffix = 0u32;
        loop {
            let prefix = if suffix == 0 {
                preferred.to_owned()
            } else {
                format!("{preferred}{suffix}")
            };
            match self.binding(element, &prefix) {
                Some(found) if found == namespace => return prefix,
                Some(_) => suffix += 1,
                None => {
                    self.additions
                        .push((element, prefix.clone(), namespace.to_owned()));
                    return prefix;
                }
            }
        }
    }

    fn push_declarations(&self, edits: &mut Vec<Edit>) {
        let mut declarations: Vec<(usize, String)> = Vec::new();
        for (node, prefix, namespace) in &self.additions {
            let declaration = format!(" xmlns:{prefix}=\"{}\"", escape(namespace));
            if let Some((_, text)) = declarations.iter_mut().find(|(idx, _)| idx == node) {
                text.push_str(&declaration);
            } else {
                declarations.push((*node, declaration));
            }
        }
        for (node, declaration) in declarations {
            edits.push(Edit::insert(self.doc.nodes[node].name_end, declaration));
        }
    }
}

fn about_matches_image(about: &str, image_path: &Path) -> bool {
    let image = if image_path.is_absolute() {
        image_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(image_path))
            .unwrap_or_else(|_| image_path.to_path_buf())
    };
    let image = image.canonicalize().unwrap_or(image);
    normalize_identifier(about) == normalize_identifier(&image.to_string_lossy())
}

fn normalize_identifier(value: &str) -> String {
    let decoded = percent_decode(value).replace('\\', "/");
    let path = if let Some(file) = decoded.strip_prefix("file://") {
        if file.starts_with('/') && file.as_bytes().get(2) == Some(&b':') {
            &file[1..]
        } else {
            file
        }
    } else {
        decoded.as_str()
    };
    #[cfg(windows)]
    {
        path.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

fn percent_decode(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[idx + 1]), hex(bytes[idx + 2]))
        {
            decoded.push((high << 4) | low);
            idx += 3;
        } else {
            decoded.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn parse_tag(
    text: &str,
    start: usize,
    tag_end: usize,
    empty: bool,
    parent: Option<usize>,
) -> Result<Node<'_>> {
    let b = text.as_bytes();
    if b.get(start) != Some(&b'<') || tag_end > b.len() || b[tag_end - 1] != b'>' {
        return Err(corrupt("internal: bad tag span"));
    }
    let mut i = start + 1;
    while i < tag_end && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' {
        i += 1;
    }
    let name = &text[start + 1..i];
    let name_end = i;
    let mut attrs = Vec::new();
    loop {
        let ws_start = i;
        while i < tag_end && is_ws(b[i]) {
            i += 1;
        }
        if i >= tag_end || b[i] == b'/' || b[i] == b'>' {
            break;
        }
        let n0 = i;
        while i < tag_end && b[i] != b'=' && !is_ws(b[i]) {
            i += 1;
        }
        let aname = &text[n0..i];
        while i < tag_end && is_ws(b[i]) {
            i += 1;
        }
        if b.get(i) != Some(&b'=') {
            return Err(corrupt("malformed attribute"));
        }
        i += 1;
        while i < tag_end && is_ws(b[i]) {
            i += 1;
        }
        let q = match b.get(i) {
            Some(&q) if q == b'"' || q == b'\'' => q,
            _ => return Err(corrupt("unquoted attribute value")),
        };
        let v0 = i + 1;
        let v1 = v0
            + text[v0..tag_end]
                .find(q as char)
                .ok_or_else(|| corrupt("unterminated attribute value"))?;
        i = v1 + 1;
        attrs.push(Attr {
            name: aname,
            value: v0..v1,
            span: ws_start..i,
        });
    }
    Ok(Node {
        name,
        start,
        name_end,
        tag_end,
        empty,
        close: tag_end,
        end: tag_end,
        parent,
        attrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_meta::read_xmp_sidecar;

    fn update() -> SidecarUpdate {
        SidecarUpdate::default()
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// A Lightroom-style sidecar with develop settings, a Photoshop property, an element-form
    /// label, a keyword bag and a tone curve sequence.
    const LR_ELEMENTS: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 5.6-c140">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    photoshop:DateCreated="2024-05-01T10:00:00"
    crs:Exposure2012="+0.35"
    crs:HasSettings="True">
   <xmp:Rating>3</xmp:Rating>
   <xmp:Label>Red</xmp:Label>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>portrait</rdf:li>
     <rdf:li>studio &amp; lights</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <crs:ToneCurve>
    <rdf:Seq>
     <rdf:li>0, 0</rdf:li>
     <rdf:li>255, 255</rdf:li>
    </rdf:Seq>
   </crs:ToneCurve>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>
"#;

    /// Same content, attribute-form rating and label.
    const LR_ATTRS: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmp:Rating="2" xmp:Label="Red"
    crs:Exposure2012="+0.35">
   <dc:subject>
    <rdf:Bag>
     <rdf:li>portrait</rdf:li>
    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

    fn write_file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn create_then_read_back_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_0001.CR3");
        std::fs::write(&img, b"not really a raw").unwrap();
        let upd = SidecarUpdate {
            rating: Some(4),
            label: Some(Some("Blue".into())),
            add_keywords: vec![s("wedding"), s("a & <b> \"c\"")],
            ..update()
        };
        let out = update_sidecar(&img, &upd).unwrap();
        assert_eq!(
            out,
            SidecarOutcome::Created(dir.path().join("IMG_0001.xmp"))
        );
        let read = read_xmp_sidecar(&img).unwrap().unwrap();
        assert_eq!(read.rating, Some(4));
        assert_eq!(read.label.as_deref(), Some("Blue"));
        assert_eq!(read.keywords, vec![s("wedding"), s("a & <b> \"c\"")]);
        // The original image is never touched.
        assert_eq!(std::fs::read(&img).unwrap(), b"not really a raw");
    }

    #[test]
    fn reject_and_unrated_ratings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.jpg");
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(-1),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().rating, Some(-1));
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(0),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().rating, Some(0));
    }

    #[test]
    fn out_of_range_rating_is_refused_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.jpg");
        assert!(
            update_sidecar(
                &img,
                &SidecarUpdate {
                    rating: Some(6),
                    ..update()
                }
            )
            .is_err()
        );
        assert!(!dir.path().join("a.xmp").exists());
    }

    #[test]
    fn empty_update_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.jpg");
        let out = update_sidecar(&img, &update()).unwrap();
        assert!(matches!(out, SidecarOutcome::Unchanged(_)));
        assert!(!dir.path().join("a.xmp").exists());
    }

    #[test]
    fn element_form_rating_is_replaced_in_place_and_everything_else_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_1.CR3");
        let side = write_file(dir.path(), "IMG_1.xmp", LR_ELEMENTS);
        let out = update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(-1),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(out, SidecarOutcome::Updated(side.clone()));
        let expected =
            LR_ELEMENTS.replace("<xmp:Rating>3</xmp:Rating>", "<xmp:Rating>-1</xmp:Rating>");
        assert_eq!(std::fs::read_to_string(&side).unwrap(), expected);
        assert!(dir.path().join("IMG_1.xmp.bak").exists());
    }

    #[test]
    fn attribute_form_rating_and_label_are_replaced_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_2.NEF");
        let side = write_file(dir.path(), "IMG_2.xmp", LR_ATTRS);
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(5),
                label: Some(Some("Green".into())),
                ..update()
            },
        )
        .unwrap();
        let expected = LR_ATTRS
            .replace("xmp:Rating=\"2\"", "xmp:Rating=\"5\"")
            .replace("xmp:Label=\"Red\"", "xmp:Label=\"Green\"");
        assert_eq!(std::fs::read_to_string(&side).unwrap(), expected);
    }

    #[test]
    fn missing_rating_is_inserted_as_attribute_on_description() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("x.tif");
        let side = write_file(
            dir.path(),
            "x.xmp",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" crs:Exposure=\"1\" xmlns:crs=\"c\"/></rdf:RDF></x:xmpmeta>",
        );
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(2),
                ..update()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&side).unwrap();
        assert!(text.contains("xmp:Rating=\"2\""));
        assert!(text.contains("crs:Exposure=\"1\""));
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().rating, Some(2));
    }

    #[test]
    fn keywords_add_and_remove_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_3.CR3");
        let side = write_file(dir.path(), "IMG_3.xmp", LR_ELEMENTS);
        update_sidecar(
            &img,
            &SidecarUpdate {
                add_keywords: vec![s("portrait"), s("outdoor")],
                remove_keywords: vec![s("studio & lights")],
                ..update()
            },
        )
        .unwrap();
        let read = read_xmp_sidecar(&img).unwrap().unwrap();
        assert_eq!(read.keywords, vec![s("portrait"), s("outdoor")]);
        let expected = LR_ELEMENTS.replace(
            "     <rdf:li>studio &amp; lights</rdf:li>\n",
            "     <rdf:li>outdoor</rdf:li>\n",
        );
        assert_eq!(std::fs::read_to_string(&side).unwrap(), expected);
    }

    #[test]
    fn adding_existing_keyword_is_a_byte_identical_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_4.CR3");
        let side = write_file(dir.path(), "IMG_4.xmp", LR_ELEMENTS);
        let out = update_sidecar(
            &img,
            &SidecarUpdate {
                add_keywords: vec![s("portrait")],
                ..update()
            },
        )
        .unwrap();
        assert!(matches!(out, SidecarOutcome::Unchanged(_)));
        assert_eq!(std::fs::read_to_string(&side).unwrap(), LR_ELEMENTS);
        assert!(!dir.path().join("IMG_4.xmp.bak").exists());
    }

    #[test]
    fn label_set_and_clear_in_both_forms() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_5.CR3");
        let side = write_file(dir.path(), "IMG_5.xmp", LR_ATTRS);
        update_sidecar(
            &img,
            &SidecarUpdate {
                label: Some(None),
                ..update()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&side).unwrap();
        assert!(!text.contains("xmp:Label"));
        assert!(text.contains("crs:Exposure2012=\"+0.35\""));
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().label, None);

        update_sidecar(
            &img,
            &SidecarUpdate {
                label: Some(Some("Purple".into())),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(
            read_xmp_sidecar(&img).unwrap().unwrap().label.as_deref(),
            Some("Purple")
        );

        let img2 = dir.path().join("IMG_6.CR3");
        let side2 = write_file(dir.path(), "IMG_6.xmp", LR_ELEMENTS);
        update_sidecar(
            &img2,
            &SidecarUpdate {
                label: Some(None),
                ..update()
            },
        )
        .unwrap();
        let text2 = std::fs::read_to_string(&side2).unwrap();
        assert!(!text2.contains("xmp:Label"));
        assert!(text2.contains("<xmp:Rating>3</xmp:Rating>"));
    }

    #[test]
    fn darktable_full_name_sidecar_is_preferred_and_never_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_7.CR3");
        let dt = write_file(dir.path(), "IMG_7.CR3.xmp", LR_ATTRS);
        let out = update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(1),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(out.path(), dt.as_path());
        assert!(!dir.path().join("IMG_7.xmp").exists());
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().rating, Some(1));
    }

    #[test]
    fn unparseable_sidecar_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_8.CR3");
        let broken = "<x:xmpmeta><rdf:RDF><rdf:Description xmp:Rating=\"3\"";
        let side = write_file(dir.path(), "IMG_8.xmp", broken);
        assert!(
            update_sidecar(
                &img,
                &SidecarUpdate {
                    rating: Some(5),
                    ..update()
                }
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&side).unwrap(), broken);
        assert!(!dir.path().join("IMG_8.xmp.bak").exists());
    }

    #[test]
    fn sidecar_without_description_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("IMG_9.CR3");
        let body = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"></x:xmpmeta>";
        let side = write_file(dir.path(), "IMG_9.xmp", body);
        assert!(
            update_sidecar(
                &img,
                &SidecarUpdate {
                    rating: Some(5),
                    ..update()
                }
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&side).unwrap(), body);
    }

    #[test]
    fn empty_description_element_is_expanded_for_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("e.jpg");
        let body = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\"/></rdf:RDF></x:xmpmeta>";
        write_file(dir.path(), "e.xmp", body);
        update_sidecar(
            &img,
            &SidecarUpdate {
                add_keywords: vec![s("k")],
                ..update()
            },
        )
        .unwrap();
        assert_eq!(
            read_xmp_sidecar(&img).unwrap().unwrap().keywords,
            vec![s("k")]
        );
    }

    #[test]
    fn unicode_paths_work() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("写真 é_01.CR3");
        std::fs::write(&img, b"x").unwrap();
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(3),
                label: Some(Some("Red".into())),
                ..update()
            },
        )
        .unwrap();
        assert!(dir.path().join("写真 é_01.xmp").is_file());
        let read = read_xmp_sidecar(&img).unwrap().unwrap();
        assert_eq!(read.rating, Some(3));
        assert_eq!(read.label.as_deref(), Some("Red"));
    }

    #[test]
    fn bom_and_unknown_content_survive_a_merge() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("b.CR3");
        let body = format!("\u{feff}{LR_ELEMENTS}");
        let side = write_file(dir.path(), "b.xmp", &body);
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(1),
                ..update()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&side).unwrap();
        assert!(text.starts_with('\u{feff}'));
        assert!(text.contains("<crs:ToneCurve>"));
        assert!(text.contains("photoshop:DateCreated=\"2024-05-01T10:00:00\""));
    }

    #[test]
    fn xmp_sidecar_path_itself_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            update_sidecar(
                &dir.path().join("a.xmp"),
                &SidecarUpdate {
                    rating: Some(1),
                    ..update()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn splice_rejects_overlapping_edits() {
        let r = splice(
            "abcdef",
            vec![Edit::replace(1..4, "x"), Edit::replace(2..3, "y")],
        );
        assert!(r.is_err());
    }

    #[test]
    fn external_edit_during_merge_is_remerged_and_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("race.CR3");
        let side = write_file(dir.path(), "race.xmp", LR_ATTRS);
        let external = LR_ATTRS.replace("xmp:Label=\"Red\"", "xmp:Label=\"Gold\"");
        *EXTERNAL_EDIT_BEFORE_REPLACE.lock().unwrap() =
            Some((side.clone(), external.as_bytes().to_vec()));

        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(5),
                ..update()
            },
        )
        .unwrap();

        let result = std::fs::read_to_string(&side).unwrap();
        assert!(result.contains("xmp:Rating=\"5\""));
        assert!(result.contains("xmp:Label=\"Gold\""));
        assert_eq!(
            std::fs::read(backup_path(&side)).unwrap(),
            external.as_bytes()
        );
    }

    #[test]
    fn backup_is_refreshed_to_the_version_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("backup.CR3");
        let side = write_file(dir.path(), "backup.xmp", LR_ATTRS);
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(4),
                ..update()
            },
        )
        .unwrap();

        let external = LR_ATTRS.replace("xmp:Rating=\"2\"", "xmp:Rating=\"3\"");
        std::fs::write(&side, &external).unwrap();
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(5),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read(backup_path(&side)).unwrap(),
            external.as_bytes()
        );
    }

    #[test]
    fn same_process_updates_to_one_sidecar_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("serialized.CR3");
        write_file(dir.path(), "serialized.xmp", LR_ATTRS);
        let side = dir.path().join("serialized.xmp");
        *CONCURRENT_UPDATE_PATH.lock().unwrap() = Some(side.clone());
        CONCURRENT_UPDATE_ACTIVE.store(0, Ordering::SeqCst);
        CONCURRENT_UPDATE_MAX.store(0, Ordering::SeqCst);

        let start = Arc::new(std::sync::Barrier::new(3));
        let rating_start = Arc::clone(&start);
        let rating_img = img.clone();
        let rating = std::thread::spawn(move || {
            rating_start.wait();
            update_sidecar(
                &rating_img,
                &SidecarUpdate {
                    rating: Some(5),
                    ..update()
                },
            )
        });
        let label_start = Arc::clone(&start);
        let label_img = img.clone();
        let label = std::thread::spawn(move || {
            label_start.wait();
            update_sidecar(
                &label_img,
                &SidecarUpdate {
                    label: Some(Some("Blue".into())),
                    ..update()
                },
            )
        });
        start.wait();
        rating.join().unwrap().unwrap();
        label.join().unwrap().unwrap();

        *CONCURRENT_UPDATE_PATH.lock().unwrap() = None;
        assert_eq!(CONCURRENT_UPDATE_MAX.load(Ordering::SeqCst), 1);
        let read = read_xmp_sidecar(&img).unwrap().unwrap();
        assert_eq!(read.rating, Some(5));
        assert_eq!(read.label.as_deref(), Some("Blue"));
    }

    #[test]
    fn namespace_alias_and_image_description_are_updated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("namespace.CR3");
        const FOREIGN: &str = r#"<r:Description r:about="urn:other" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="1" keep:Value="untouched" xmlns:keep="urn:keep"/>"#;
        const TARGET: &str = r#"<r:Description r:about="" xmlns:alt="http://ns.adobe.com/xap/1.0/" alt:Rating="2"/>"#;
        let body = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><r:RDF xmlns:r="http://www.w3.org/1999/02/22-rdf-syntax-ns#">{FOREIGN}{TARGET}</r:RDF></x:xmpmeta>"#
        );
        let side = write_file(dir.path(), "namespace.xmp", &body);
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(5),
                ..update()
            },
        )
        .unwrap();

        let result = std::fs::read_to_string(&side).unwrap();
        assert!(
            result.contains(FOREIGN),
            "foreign description changed: {result}"
        );
        assert!(result.contains(&TARGET.replace("alt:Rating=\"2\"", "alt:Rating=\"5\"")));
        assert!(!result.contains("xmp:Rating=\"5\""));
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().rating, Some(5));
    }

    #[test]
    fn namespace_binding_on_a_sibling_description_is_not_inherited() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("scope.CR3");
        const FOREIGN: &str = r#"<rdf:Description rdf:about="urn:other" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="1"/>"#;
        const TARGET: &str = r#"<rdf:Description rdf:about=""/>"#;
        let body = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">{FOREIGN}{TARGET}</rdf:RDF></x:xmpmeta>"#
        );
        let side = write_file(dir.path(), "scope.xmp", &body);
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(4),
                ..update()
            },
        )
        .unwrap();

        let result = std::fs::read_to_string(&side).unwrap();
        assert!(result.contains(FOREIGN));
        let target = result
            .split(FOREIGN)
            .nth(1)
            .expect("target description follows the other resource");
        assert!(target.contains("rdf:about=\"\""));
        assert!(target.contains("xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\""));
        assert!(target.contains("xmp:Rating=\"4\""));
    }

    #[test]
    fn explicit_image_about_is_preferred_to_an_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("matching.CR3");
        let image_about = img.to_string_lossy().replace('\\', "/");
        let empty = r#"<rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="1"/>"#;
        let matching = format!(
            r#"<rdf:Description rdf:about="{image_about}" xmlns:p="http://ns.adobe.com/xap/1.0/" p:Rating="2"/>"#
        );
        let body = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">{empty}{matching}</rdf:RDF></x:xmpmeta>"#
        );
        let side = write_file(dir.path(), "matching.xmp", &body);
        update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(4),
                ..update()
            },
        )
        .unwrap();

        let result = std::fs::read_to_string(&side).unwrap();
        assert!(result.contains(empty));
        assert!(result.contains(&matching.replace("p:Rating=\"2\"", "p:Rating=\"4\"")));
        assert_eq!(read_xmp_sidecar(&img).unwrap().unwrap().rating, Some(4));
    }

    #[test]
    fn keyword_add_and_remove_use_unicode_lowercase_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("case.CR3");
        let body = LR_ELEMENTS.replace("portrait", "Wedding");
        let side = write_file(dir.path(), "case.xmp", &body);

        let out = update_sidecar(
            &img,
            &SidecarUpdate {
                add_keywords: vec![s("wedding")],
                ..update()
            },
        )
        .unwrap();
        assert!(matches!(out, SidecarOutcome::Unchanged(_)));
        assert_eq!(std::fs::read_to_string(&side).unwrap(), body);

        update_sidecar(
            &img,
            &SidecarUpdate {
                remove_keywords: vec![s("wedding")],
                ..update()
            },
        )
        .unwrap();
        assert_eq!(
            read_xmp_sidecar(&img).unwrap().unwrap().keywords,
            vec![s("studio & lights")]
        );
    }

    #[test]
    fn custom_dc_and_rdf_prefixes_are_resolved_by_namespace_uri() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("custom-keywords.CR3");
        let body = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><q:RDF xmlns:q="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><q:Description q:about="" xmlns:terms="http://purl.org/dc/elements/1.1/"><terms:subject><q:Bag><q:li>Wedding</q:li></q:Bag></terms:subject></q:Description></q:RDF></x:xmpmeta>"#;
        let side = write_file(dir.path(), "custom-keywords.xmp", body);
        update_sidecar(
            &img,
            &SidecarUpdate {
                add_keywords: vec![s("wedding"), s("client")],
                remove_keywords: vec![s("WEDDING")],
                ..update()
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(side).unwrap();
        assert!(text.contains("<terms:subject>"));
        assert!(text.contains("<q:Bag>"));
        assert_eq!(
            read_xmp_sidecar(&img).unwrap().unwrap().keywords,
            vec![s("wedding"), s("client")]
        );
    }

    #[test]
    fn utf16_le_and_be_sidecars_are_decoded_by_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let le_img = dir.path().join("utf16-le.CR3");
        let be_img = dir.path().join("utf16-be.CR3");
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"/></rdf:RDF></x:xmpmeta>"#;
        let le: Vec<u8> = [
            vec![0xFF, 0xFE],
            xml.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        ]
        .concat();
        let be: Vec<u8> = [
            vec![0xFE, 0xFF],
            xml.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        ]
        .concat();
        std::fs::write(dir.path().join("utf16-le.xmp"), le).unwrap();
        std::fs::write(dir.path().join("utf16-be.xmp"), be).unwrap();

        assert_eq!(read_xmp_sidecar(&le_img).unwrap().unwrap().rating, Some(4));
        assert_eq!(read_xmp_sidecar(&be_img).unwrap().unwrap().rating, Some(4));
    }

    #[test]
    fn utf16_sidecars_are_refused_by_the_writer_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("readonly.CR3");
        let side = dir.path().join("readonly.xmp");
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"/></rdf:RDF></x:xmpmeta>"#;
        let bytes: Vec<u8> = [
            vec![0xFF, 0xFE],
            xml.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        ]
        .concat();
        std::fs::write(&side, &bytes).unwrap();

        let error = update_sidecar(
            &img,
            &SidecarUpdate {
                rating: Some(5),
                ..update()
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .to_lowercase()
                .contains("unsupported encoding")
        );
        assert_eq!(std::fs::read(side).unwrap(), bytes);
    }

    #[test]
    fn temp_files_are_unique_and_created_without_truncating_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let side = dir.path().join("unique.xmp");
        let first = write_temp(&side, b"first").unwrap();
        let second = write_temp(&side, b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }
}
