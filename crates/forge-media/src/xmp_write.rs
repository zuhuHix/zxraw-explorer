//! XMP sidecar writer: merges ratings, colour labels and keywords into the `.xmp` sidecar that
//! Lightroom, Capture One, darktable and Bridge read. Originals are never opened for writing.
//!
//! Merge strategy: the existing sidecar is never re-serialised. A quick-xml pass records the
//! byte span of every element and attribute involved, the requested changes become splices
//! into the original text, and the result is re-parsed and checked against the requested
//! values before anything touches the disk. Everything else (unknown namespaces, `crs:`
//! develop settings, comments, processing instructions, element order, whitespace) survives
//! byte-for-byte. A sidecar that cannot be parsed is reported and left alone.
//!
//! Limitations: only `xmp:Rating`, `xmp:Label` and `dc:subject` are written. Picks are not
//! stored in XMP (there is no standard property for them) and stay in the app library.

use std::ffi::OsString;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::escape::{escape, unescape};
use quick_xml::events::Event;

use crate::error::{MediaError, Result};
use crate::image_meta::{XmpData, XmpSource, parse_xmp};

const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
/// Sidecars are normally a few KB; anything this large is refused rather than rewritten.
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;

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
    /// An existing sidecar was merged and rewritten (a `.xmp.bak` copy was kept).
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
    if !path.exists() {
        let text = apply_update(NEW_PACKET, update).map_err(|e| with_path(&path, e))?;
        write_atomic(&path, text.as_bytes())?;
        return Ok(SidecarOutcome::Created(path));
    }

    let len = std::fs::metadata(&path)
        .map_err(|e| MediaError::io(&path, e))?
        .len();
    if len > MAX_SIDECAR_BYTES {
        return Err(MediaError::Unsupported(format!(
            "{} is too large to rewrite safely",
            path.display()
        )));
    }
    let bytes = std::fs::read(&path).map_err(|e| MediaError::io(&path, e))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        MediaError::Corrupt(format!("{}: not UTF-8 XMP; left untouched", path.display()))
    })?;
    let merged = apply_update(&text, update).map_err(|e| with_path(&path, e))?;
    if merged == text {
        return Ok(SidecarOutcome::Unchanged(path));
    }
    let bak = backup_path(&path);
    if !bak.exists() {
        std::fs::copy(&path, &bak).map_err(|e| MediaError::io(&bak, e))?;
    }
    write_atomic(&path, merged.as_bytes())?;
    Ok(SidecarOutcome::Updated(path))
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

/// Writes via a temp file in the same folder, then renames over the target.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp_name = OsString::from(".");
    if let Some(n) = path.file_name() {
        tmp_name.push(n);
    }
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(MediaError::io(path, e));
    }
    Ok(())
}

fn corrupt(msg: impl Into<String>) -> MediaError {
    MediaError::Corrupt(msg.into())
}

fn unsupported(msg: impl Into<String>) -> MediaError {
    MediaError::Unsupported(msg.into())
}

/// Produces the merged text for `text` (the original sidecar, or the new packet).
fn apply_update(text: &str, update: &SidecarUpdate) -> Result<String> {
    let before = parse_xmp(text, XmpSource::Sidecar);
    let doc = Doc::parse(text)?;
    let adds = normalize(&update.add_keywords);
    let removes = normalize(&update.remove_keywords);

    let mut edits = Vec::new();
    if let Some(r) = update.rating {
        set_simple(&doc, text, "xmp:Rating", &r.to_string(), &mut edits)?;
    }
    match &update.label {
        Some(Some(label)) => set_simple(&doc, text, "xmp:Label", label, &mut edits)?,
        Some(None) => remove_simple(&doc, text, "xmp:Label", &mut edits)?,
        None => {}
    }
    let (keywords, dc_written) =
        keyword_edits(&doc, text, &adds, &removes, &before.keywords, &mut edits)?;

    // Declare prefixes we write with, if the packet does not already declare them.
    let wrote_xmp = update.rating.is_some() || matches!(update.label, Some(Some(_)));
    if wrote_xmp || dc_written {
        let mut decl = String::new();
        if wrote_xmp && !text.contains("xmlns:xmp=") {
            decl.push_str(&format!(" xmlns:xmp=\"{XMP_NS}\""));
        }
        if dc_written && !text.contains("xmlns:dc=") {
            decl.push_str(&format!(" xmlns:dc=\"{DC_NS}\""));
        }
        if !decl.is_empty() {
            let desc = doc.description()?;
            edits.push(Edit::insert(doc.nodes[desc].name_end, decl));
        }
    }

    let out = splice(text, edits)?;
    verify(&out, &before, update, &keywords)?;
    Ok(out)
}

/// Re-parses the merged text and checks the requested values made it in. Any mismatch means
/// the splice logic misjudged the document; the caller then leaves the file untouched.
fn verify(out: &str, before: &XmpData, update: &SidecarUpdate, keywords: &[String]) -> Result<()> {
    Doc::parse(out).map_err(|_| corrupt("merged XMP is not well-formed; left untouched"))?;
    let after = parse_xmp(out, XmpSource::Sidecar);
    let rating = update.rating.or(before.rating);
    let label = match &update.label {
        Some(l) => l.clone(),
        None => before.label.clone(),
    };
    if after.rating != rating || after.label != label || after.keywords != keywords {
        return Err(corrupt("merged XMP did not verify; left untouched"));
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

/// Sets a simple property in every existing spot (attribute or element form). If absent, it
/// is added as an attribute on the first `rdf:Description`.
fn set_simple(
    doc: &Doc<'_>,
    text: &str,
    name: &str,
    value: &str,
    edits: &mut Vec<Edit>,
) -> Result<()> {
    let mut found = false;
    for node in &doc.nodes {
        for a in node.attrs.iter().filter(|a| a.name == name) {
            found = true;
            edits.push(Edit::replace(a.value.clone(), escape(value).into_owned()));
        }
        if node.name == name {
            found = true;
            if node.empty {
                edits.push(Edit::replace(
                    node.start..node.end,
                    format!("<{name}>{}</{name}>", escape(value)),
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
        let desc = doc.description()?;
        edits.push(Edit::insert(
            doc.nodes[desc].name_end,
            format!(" {name}=\"{}\"", escape(value)),
        ));
    }
    Ok(())
}

/// Removes a simple property from every spot it occupies.
fn remove_simple(doc: &Doc<'_>, text: &str, name: &str, edits: &mut Vec<Edit>) -> Result<()> {
    for node in &doc.nodes {
        for a in node.attrs.iter().filter(|a| a.name == name) {
            edits.push(Edit::replace(a.span.clone(), ""));
        }
        if node.name == name {
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

/// Keyword changes on `dc:subject`. Returns the resulting keyword list and whether a `dc:`
/// element was written (so the caller knows to declare the prefix).
fn keyword_edits(
    doc: &Doc<'_>,
    text: &str,
    adds: &[String],
    removes: &[String],
    current: &[String],
    edits: &mut Vec<Edit>,
) -> Result<(Vec<String>, bool)> {
    let mut fin: Vec<String> = current
        .iter()
        .filter(|k| !removes.contains(k))
        .cloned()
        .collect();
    for k in adds {
        if !fin.contains(k) {
            fin.push(k.clone());
        }
    }
    if fin == current {
        return Ok((fin, false));
    }
    if doc
        .nodes
        .iter()
        .any(|n| n.attrs.iter().any(|a| a.name == "dc:subject"))
    {
        return Err(unsupported(
            "dc:subject is stored as an attribute; left untouched",
        ));
    }

    let Some(subject) = doc.find("dc:subject") else {
        let desc = doc.description()?;
        doc.append_child(text, desc, &bag_xml(&fin), edits)?;
        return Ok((fin, true));
    };
    let s = &doc.nodes[subject];
    if doc.children(subject).is_empty() && text[s.tag_end..s.close].trim().is_empty() {
        edits.push(Edit::replace(s.start..s.end, bag_xml(&fin)));
        return Ok((fin, true));
    }

    let bag = doc
        .children(subject)
        .into_iter()
        .find(|&c| matches!(doc.nodes[c].name, "rdf:Bag" | "rdf:Seq"))
        .ok_or_else(|| unsupported("dc:subject has no rdf:Bag; left untouched"))?;
    let lis: Vec<usize> = doc
        .children(bag)
        .into_iter()
        .filter(|&c| doc.nodes[c].name == "rdf:li")
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
        if removes.contains(&value) {
            edits.push(Edit::replace(leading_ws(text, node.start)..node.end, ""));
        } else {
            kept.push(value);
        }
    }
    let new_items: Vec<&String> = fin.iter().filter(|k| !kept.contains(*k)).collect();
    if new_items.is_empty() {
        return Ok((fin, false));
    }

    let bag_node = &doc.nodes[bag];
    if bag_node.empty {
        edits.push(Edit::replace(
            bag_node.start..bag_node.end,
            format!("<{0}>{1}</{0}>", bag_node.name, li_xml(&new_items, "")),
        ));
    } else if let Some(&last) = lis.last() {
        let last_node = &doc.nodes[last];
        let ws = &text[leading_ws(text, last_node.start)..last_node.start];
        edits.push(Edit::insert(last_node.end, li_xml(&new_items, ws)));
    } else {
        edits.push(Edit::insert(bag_node.close, li_xml(&new_items, "")));
    }
    Ok((fin, true))
}

fn bag_xml(items: &[String]) -> String {
    format!(
        "<dc:subject><rdf:Bag>{}</rdf:Bag></dc:subject>",
        li_xml(&items.iter().collect::<Vec<_>>(), "")
    )
}

fn li_xml(items: &[impl AsRef<str>], indent: &str) -> String {
    items
        .iter()
        .map(|k| format!("{indent}<rdf:li>{}</rdf:li>", escape(k.as_ref())))
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
        Ok(Self { nodes })
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name)
    }

    fn description(&self) -> Result<usize> {
        self.find("rdf:Description")
            .ok_or_else(|| corrupt("no rdf:Description element"))
    }

    fn children(&self, idx: usize) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&j| self.nodes[j].parent == Some(idx))
            .collect()
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
}
