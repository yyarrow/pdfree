//! Page-level document operations: merge (concatenate pages from several
//! PDFs into one) and split/extract (keep only a page subset).
//!
//! Written purely against ISO 32000 (7.7.3 "Document Catalog"/"Page Tree")
//! and the lopdf (MIT) API; no GPL/AGPL PDF source was consulted.

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PageDocError {
    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("failed to load {0}: {1}")]
    Load(PathBuf, lopdf::Error),
    #[error("encrypted documents are not supported yet (saving would strip the encryption)")]
    EncryptedUnsupported,
    #[error("no input files given")]
    NoInputs,
    #[error("page {0} out of range (document has {1} pages)")]
    PageOutOfRange(u32, usize),
    #[error("invalid page range spec: {0:?}")]
    BadRangeSpec(String),
    #[error("page selection is empty")]
    EmptySelection,
}

/// Refuse encrypted documents (same guard as text editing): lopdf's writer
/// drops /Encrypt on save, which would silently strip the owner's
/// permissions. See replace::reject_encrypted for the three-way check.
fn check_not_encrypted(doc: &Document) -> Result<(), PageDocError> {
    crate::replace::reject_encrypted(doc).map_err(|_| PageDocError::EncryptedUnsupported)
}

/// Concatenate the pages of `inputs`, in order, into a fresh document.
pub fn merge(inputs: &[PathBuf]) -> Result<Document, PageDocError> {
    if inputs.is_empty() {
        return Err(PageDocError::NoInputs);
    }

    let mut out = Document::with_version("1.7");
    let pages_id = out.new_object_id();
    let mut kids = Vec::new();

    for path in inputs {
        let src = crate::load_with_salvage(path).map_err(|e| PageDocError::Load(path.clone(), e))?;
        check_not_encrypted(&src)?;

        // Fresh per-source id map: object numbers are only unique within one
        // source file, and this also lets pages from the same source that
        // share a Resources/font dict dedupe onto the same copied object.
        let mut id_map = HashMap::new();
        for (_, page_id) in src.get_pages() {
            let new_id = copy_page(&src, page_id, &mut out, &mut id_map)?;
            if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
                dict.set("Parent", Object::Reference(pages_id));
            }
            kids.push(Object::Reference(new_id));
        }
    }

    finish_document(out, pages_id, kids)
}

/// Write a new document containing only the pages of `doc` selected by
/// `spec` (1-based, e.g. "1-3,5,8-10"), in the order given.
pub fn extract_pages(doc: &Document, spec: &str) -> Result<Document, PageDocError> {
    check_not_encrypted(doc)?;

    let pages = doc.get_pages();
    let selected = parse_page_spec(spec, pages.len())?;

    let mut out = Document::with_version(doc.version.clone());
    let pages_id = out.new_object_id();
    let mut kids = Vec::new();
    let mut id_map = HashMap::new();

    for p in selected {
        let &page_id = pages.get(&p).ok_or(PageDocError::PageOutOfRange(p, pages.len()))?;
        let new_id = copy_page(doc, page_id, &mut out, &mut id_map)?;
        if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
            dict.set("Parent", Object::Reference(pages_id));
        }
        kids.push(Object::Reference(new_id));
    }

    finish_document(out, pages_id, kids)
}

/// Parse "1-3,5,8-10" into an ordered, 1-based page number list. Ranges must
/// be ascending (a-b with a<=b); duplicates and reordering (e.g. "3,1") are
/// allowed since callers may legitimately want that.
fn parse_page_spec(spec: &str, page_count: usize) -> Result<Vec<u32>, PageDocError> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            let a: u32 = a.trim().parse().map_err(|_| PageDocError::BadRangeSpec(spec.to_string()))?;
            let b: u32 = b.trim().parse().map_err(|_| PageDocError::BadRangeSpec(spec.to_string()))?;
            if a == 0 || b == 0 || a > b {
                return Err(PageDocError::BadRangeSpec(spec.to_string()));
            }
            out.extend(a..=b);
        } else {
            let p: u32 = part.parse().map_err(|_| PageDocError::BadRangeSpec(spec.to_string()))?;
            if p == 0 {
                return Err(PageDocError::BadRangeSpec(spec.to_string()));
            }
            out.push(p);
        }
    }
    if out.is_empty() {
        return Err(PageDocError::EmptySelection);
    }
    for &p in &out {
        if p as usize > page_count {
            return Err(PageDocError::PageOutOfRange(p, page_count));
        }
    }
    Ok(out)
}

/// Resolve a page's own dict plus whatever /MediaBox, /Resources, /Rotate,
/// /CropBox it would otherwise inherit by walking up /Parent (ISO 32000
/// 7.7.3.4 "Inheritance of Page Attributes"): nearest ancestor wins, so we
/// only fill in attributes the page itself doesn't already carry.
fn resolve_inherited(doc: &Document, page_id: ObjectId) -> Result<Dictionary, PageDocError> {
    let mut page_dict = doc.get_dictionary(page_id)?.clone();
    let mut mediabox = page_dict.get(b"MediaBox").ok().cloned();
    let mut resources = page_dict.get(b"Resources").ok().cloned();
    let mut rotate = page_dict.get(b"Rotate").ok().cloned();
    let mut cropbox = page_dict.get(b"CropBox").ok().cloned();

    let mut current = page_id;
    let mut seen = HashSet::new();
    seen.insert(page_id);
    while mediabox.is_none() || resources.is_none() || rotate.is_none() || cropbox.is_none() {
        let dict = doc.get_dictionary(current)?;
        let parent_id = match dict.get(b"Parent").and_then(Object::as_reference) {
            Ok(pid) if seen.insert(pid) => pid,
            _ => break,
        };
        let parent = doc.get_dictionary(parent_id)?;
        if mediabox.is_none() {
            mediabox = parent.get(b"MediaBox").ok().cloned();
        }
        if resources.is_none() {
            resources = parent.get(b"Resources").ok().cloned();
        }
        if rotate.is_none() {
            rotate = parent.get(b"Rotate").ok().cloned();
        }
        if cropbox.is_none() {
            cropbox = parent.get(b"CropBox").ok().cloned();
        }
        current = parent_id;
    }

    if let Some(v) = mediabox {
        page_dict.set("MediaBox", v);
    }
    if let Some(v) = resources {
        page_dict.set("Resources", v);
    }
    if let Some(v) = rotate {
        page_dict.set("Rotate", v);
    }
    if let Some(v) = cropbox {
        page_dict.set("CropBox", v);
    }
    Ok(page_dict)
}

/// Deep-copy one page (with inherited attributes materialized) from `src`
/// into `out`, returning its new object id. /Parent is dropped here; the
/// caller sets it to point at the new document's Pages root.
fn copy_page(
    src: &Document, page_id: ObjectId, out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>,
) -> Result<ObjectId, PageDocError> {
    let mut page_dict = resolve_inherited(src, page_id)?;
    page_dict.remove(b"Parent");
    page_dict.set("Type", Object::Name(b"Page".to_vec()));

    // Register the new id before recursing: an Annot's /P (or similar
    // self-reference back to the page) must resolve to the copy, not
    // trigger a second copy or infinite recursion.
    let new_id = out.new_object_id();
    id_map.insert(page_id, new_id);
    let new_dict = deep_copy_dict(src, &page_dict, out, id_map);
    out.set_object(new_id, Object::Dictionary(new_dict));
    Ok(new_id)
}

/// Deep-copy an arbitrary object graph reachable from `obj`, translating
/// indirect references through `id_map` and allocating fresh ids in `out`
/// for anything not yet copied. Cycle-safe: `id_map` is populated before
/// recursing into a referenced object's own content.
fn deep_copy(src: &Document, obj: &Object, out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>) -> Object {
    match obj {
        Object::Reference(old_id) => Object::Reference(copy_object_id(src, *old_id, out, id_map)),
        Object::Array(items) => Object::Array(items.iter().map(|o| deep_copy(src, o, out, id_map)).collect()),
        Object::Dictionary(d) => Object::Dictionary(deep_copy_dict(src, d, out, id_map)),
        Object::Stream(s) => {
            let dict = deep_copy_dict(src, &s.dict, out, id_map);
            Object::Stream(lopdf::Stream {
                dict,
                content: s.content.clone(),
                allows_compression: s.allows_compression,
                start_position: None,
            })
        }
        other => other.clone(),
    }
}

fn deep_copy_dict(
    src: &Document, d: &Dictionary, out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>,
) -> Dictionary {
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        nd.set(k.clone(), deep_copy(src, v, out, id_map));
    }
    nd
}

fn copy_object_id(
    src: &Document, old_id: ObjectId, out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>,
) -> ObjectId {
    if let Some(&new_id) = id_map.get(&old_id) {
        return new_id;
    }
    let new_id = out.new_object_id();
    id_map.insert(old_id, new_id); // before recursing: breaks reference cycles
    match src.get_object(old_id) {
        Ok(obj) => {
            let new_obj = deep_copy(src, obj, out, id_map);
            out.set_object(new_id, new_obj);
        }
        // Dangling reference in the source (missing/unresolvable object):
        // keep the graph structurally valid rather than failing the whole copy.
        Err(_) => out.set_object(new_id, Object::Null),
    }
    new_id
}

/// Build the /Pages tree node and /Catalog for the accumulated `kids`, wire
/// up the trailer's /Root, and return the finished document. /Count and
/// /Size are set here (writer.rs fills /Size from max_id on save too, but we
/// set it defensively in case that changes).
fn finish_document(mut out: Document, pages_id: ObjectId, kids: Vec<Object>) -> Result<Document, PageDocError> {
    if kids.is_empty() {
        return Err(PageDocError::EmptySelection);
    }

    let mut pages_dict = Dictionary::new();
    pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
    pages_dict.set("Count", kids.len() as i64);
    pages_dict.set("Kids", Object::Array(kids));
    out.set_object(pages_id, Object::Dictionary(pages_dict));

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    let catalog_id = out.add_object(Object::Dictionary(catalog));

    out.trailer.set("Root", Object::Reference(catalog_id));
    out.trailer.set("Size", (out.max_id + 1) as i64);

    Ok(out)
}
