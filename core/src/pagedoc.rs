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
    // Document-level catalog data (named dests, AcroForm, ...) to carry over.
    // Cross-document merging of these tables is a known limitation, so we only
    // keep the FIRST input's; for a single input this preserves everything.
    let mut extra_catalog: Vec<(Vec<u8>, Object)> = Vec::new();

    for (idx, path) in inputs.iter().enumerate() {
        let src = crate::load_with_salvage(path).map_err(|e| PageDocError::Load(path.clone(), e))?;
        check_not_encrypted(&src)?;

        // Fresh per-source id map: object numbers are only unique within one
        // source file, and this also lets pages from the same source that
        // share a Resources/font dict dedupe onto the same copied object.
        let mut id_map = HashMap::new();
        let pages = src.get_pages();

        // Pre-register every source page id -> a fresh output id BEFORE copying
        // anything. A page reached transitively while another page is being
        // deep-copied (e.g. a Link/annotation /Dest pointing at it) then
        // resolves to the SAME object that lands in /Kids, instead of an
        // orphaned early duplicate that later gets stranded off the page tree.
        for (_, &page_id) in &pages {
            let new_id = out.new_object_id();
            id_map.insert(page_id, new_id);
        }
        for (_, &page_id) in &pages {
            let new_id = id_map[&page_id];
            copy_page_into(&src, page_id, new_id, &mut out, &mut id_map)?;
            if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
                dict.set("Parent", Object::Reference(pages_id));
            }
            kids.push(Object::Reference(new_id));
        }

        if idx == 0 {
            extra_catalog = collect_carried_catalog(&src, &mut id_map, &mut out);
        }
    }

    finish_document(out, pages_id, kids, extra_catalog)
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

    // Pre-register a canonical output id for each DISTINCT selected page id
    // before copying, so transitive cross-page links resolve to the page
    // object that actually lands in /Kids rather than an orphan duplicate.
    for &p in &selected {
        let &page_id = pages.get(&p).ok_or(PageDocError::PageOutOfRange(p, pages.len()))?;
        id_map.entry(page_id).or_insert_with(|| out.new_object_id());
    }

    let mut filled: HashSet<ObjectId> = HashSet::new();
    for &p in &selected {
        let &page_id = pages.get(&p).ok_or(PageDocError::PageOutOfRange(p, pages.len()))?;
        let canonical = id_map[&page_id];
        let new_id = if filled.insert(page_id) {
            // First (canonical) copy of this page: fill its pre-registered id.
            copy_page_into(doc, page_id, canonical, &mut out, &mut id_map)?;
            canonical
        } else {
            // Same page selected more than once (duplicates are allowed): emit
            // a distinct page object so /Kids never lists one object twice. It
            // shares content/resource streams with the canonical copy via
            // id_map, and the canonical mapping is left untouched so cross-page
            // links keep resolving to the first copy.
            let dup = out.new_object_id();
            copy_page_into(doc, page_id, dup, &mut out, &mut id_map)?;
            dup
        };
        if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
            dict.set("Parent", Object::Reference(pages_id));
        }
        kids.push(Object::Reference(new_id));
    }

    // Single input: carry over the whole document-level catalog data set.
    let extra_catalog = collect_carried_catalog(doc, &mut id_map, &mut out);
    finish_document(out, pages_id, kids, extra_catalog)
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
            // Bounds-check BEFORE materializing the range. A spec like
            // "1-4294967295" on a small document must be rejected here, not
            // after `Vec::extend` has already tried to allocate billions of
            // entries (DoS/OOM). Since a <= b, checking the upper endpoint
            // covers every page number the range would produce.
            if b as usize > page_count {
                return Err(PageDocError::PageOutOfRange(b, page_count));
            }
            out.extend(a..=b);
        } else {
            let p: u32 = part.parse().map_err(|_| PageDocError::BadRangeSpec(spec.to_string()))?;
            if p == 0 {
                return Err(PageDocError::BadRangeSpec(spec.to_string()));
            }
            if p as usize > page_count {
                return Err(PageDocError::PageOutOfRange(p, page_count));
            }
            out.push(p);
        }
    }
    if out.is_empty() {
        return Err(PageDocError::EmptySelection);
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
/// into the caller-supplied, pre-allocated output id `new_id`. /Parent is
/// dropped here; the caller re-points it at the new document's Pages root.
///
/// `new_id` MUST already be registered in `id_map` as the mapping for
/// `page_id` (the caller pre-registers every page id up front). That keeps
/// self-references (an Annot's /P) and transitive cross-page links (another
/// page's /Dest pointing here) resolving to THIS object — the one placed in
/// /Kids — instead of spawning an orphaned duplicate. It also breaks
/// reference cycles: the id is mapped before we recurse into the content.
fn copy_page_into(
    src: &Document, page_id: ObjectId, new_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>,
) -> Result<(), PageDocError> {
    let mut page_dict = resolve_inherited(src, page_id)?;
    page_dict.remove(b"Parent");
    page_dict.set("Type", Object::Name(b"Page".to_vec()));

    let new_dict = deep_copy_dict(src, &page_dict, out, id_map);
    out.set_object(new_id, Object::Dictionary(new_dict));
    Ok(())
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

/// Document-level catalog keys carried over from a source catalog so features
/// that live above the page tree keep working after merge/split: named
/// destinations (/Names, /Dests), interactive form (/AcroForm), optional
/// content (/OCProperties), document language (/Lang), and viewer intent
/// (/ViewerPreferences, /PageLayout, /PageMode).
///
/// /StructTreeRoot is deliberately NOT carried: its /K structure holds parent
/// pointers into content and marked-content sequences we cannot guarantee
/// stay consistent across an arbitrary page subset, so we drop it rather than
/// emit a dangling/inconsistent tagging tree. Cross-document merging of these
/// tables (multi-input merge) is also unsupported — see `merge`, which only
/// carries the first input's entries. Both are known limitations.
const CARRIED_CATALOG_KEYS: &[&[u8]] = &[
    b"Names",
    b"Dests",
    b"AcroForm",
    b"OCProperties",
    b"Lang",
    b"ViewerPreferences",
    b"PageLayout",
    b"PageMode",
];

/// Deep-copy the carried document-level entries out of `src`'s catalog through
/// the SAME `id_map` used for the pages, so their references (e.g. a named
/// destination's page reference, an AcroForm field's /P and widget) resolve
/// onto the freshly copied page objects instead of dangling. Returns the
/// copied (key, value) pairs for `finish_document` to place on the new catalog.
///
/// Must be called AFTER the pages have been copied, so page ids and their
/// annotations/widgets are already present in `id_map` and get reused rather
/// than duplicated.
fn collect_carried_catalog(
    src: &Document, id_map: &mut HashMap<ObjectId, ObjectId>, out: &mut Document,
) -> Vec<(Vec<u8>, Object)> {
    let mut carried = Vec::new();
    let Ok(root) = src.trailer.get(b"Root").and_then(Object::as_reference) else {
        return carried;
    };
    let Ok(catalog) = src.get_dictionary(root) else {
        return carried;
    };
    for &key in CARRIED_CATALOG_KEYS {
        if let Ok(val) = catalog.get(key) {
            let copied = deep_copy(src, val, out, id_map);
            carried.push((key.to_vec(), copied));
        }
    }
    carried
}

/// Build the /Pages tree node and /Catalog for the accumulated `kids`, wire
/// up the trailer's /Root, and return the finished document. `extra_catalog`
/// carries document-level entries (named dests, AcroForm, ...) copied from the
/// source catalog. /Count and /Size are set here (writer.rs fills /Size from
/// max_id on save too, but we set it defensively in case that changes).
fn finish_document(
    mut out: Document, pages_id: ObjectId, kids: Vec<Object>, extra_catalog: Vec<(Vec<u8>, Object)>,
) -> Result<Document, PageDocError> {
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
    for (key, val) in extra_catalog {
        catalog.set(key, val);
    }
    let catalog_id = out.add_object(Object::Dictionary(catalog));

    out.trailer.set("Root", Object::Reference(catalog_id));
    out.trailer.set("Size", (out.max_id + 1) as i64);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    // ---- finding 1: range bounds checked before expansion -----------------

    #[test]
    fn huge_range_rejected_before_expansion() {
        // Would attempt a multi-gigabyte Vec if the range were materialized
        // before the bounds check. Must return quickly with PageOutOfRange.
        let err = parse_page_spec("1-4294967295", 3).unwrap_err();
        assert!(matches!(err, PageDocError::PageOutOfRange(4294967295, 3)), "got {err:?}");
    }

    #[test]
    fn single_page_out_of_range_rejected() {
        let err = parse_page_spec("9", 3).unwrap_err();
        assert!(matches!(err, PageDocError::PageOutOfRange(9, 3)), "got {err:?}");
    }

    #[test]
    fn valid_spec_keeps_order_and_duplicates() {
        assert_eq!(parse_page_spec("1-3,2", 3).unwrap(), vec![1, 2, 3, 2]);
    }

    // ---- finding 2: cross-page link maps to the canonical /Kids page -------

    /// Two-page doc where page 1 carries a Link annotation whose /Dest points
    /// (by indirect reference) at page 2. Copying page 1 therefore reaches
    /// page 2 transitively before page 2's own turn.
    fn build_cross_linked_doc() -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page1_id = doc.new_object_id();
        let page2_id = doc.new_object_id();

        let c1 = doc.add_object(Stream::new(dictionary! {}, b"BT /F1 12 Tf (one) Tj ET".to_vec()));
        let c2 = doc.add_object(Stream::new(dictionary! {}, b"BT /F1 12 Tf (two) Tj ET".to_vec()));

        let link_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 100.into(), 20.into()],
            "Dest" => vec![Object::Reference(page2_id), "Fit".into()],
        });

        doc.set_object(page1_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c1),
            "Annots" => vec![Object::Reference(link_id)],
        });
        doc.set_object(page2_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c2),
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page1_id), Object::Reference(page2_id)],
            "Count" => 2,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    fn kids_of(out: &Document) -> Vec<ObjectId> {
        let root = out.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        let catalog = out.get_dictionary(root).unwrap();
        let pages_ref = catalog.get(b"Pages").and_then(Object::as_reference).unwrap();
        let pages = out.get_dictionary(pages_ref).unwrap();
        pages
            .get(b"Kids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_reference().unwrap())
            .collect()
    }

    fn assert_link_resolves_to_kids(out: &Document) {
        let kids = kids_of(out);
        assert_eq!(kids.len(), 2);

        let page1 = out.get_dictionary(kids[0]).unwrap();
        let annots = page1.get(b"Annots").unwrap().as_array().unwrap();
        let annot = out.get_dictionary(annots[0].as_reference().unwrap()).unwrap();
        let dest = annot.get(b"Dest").unwrap().as_array().unwrap();
        let dest_page = dest[0].as_reference().unwrap();

        // The link must point at the second kid — the real page-2 object that
        // is in /Kids — not an orphaned duplicate copied out of turn.
        assert_eq!(dest_page, kids[1], "link /Dest must resolve to the page in /Kids");
        assert!(out.get_dictionary(dest_page).is_ok());
        // And that page must be wired back into the new page tree.
        let dp = out.get_dictionary(dest_page).unwrap();
        assert!(dp.get(b"Parent").and_then(Object::as_reference).is_ok());
    }

    #[test]
    fn extract_preserves_cross_page_link() {
        let doc = build_cross_linked_doc();
        let out = extract_pages(&doc, "1-2").unwrap();
        assert_link_resolves_to_kids(&out);
    }

    #[test]
    fn merge_single_preserves_cross_page_link() {
        // Write the constructed doc to a temp file and merge it (single input).
        let mut doc = build_cross_linked_doc();
        let path = std::env::temp_dir().join("pdfree_pagedoc_crosslink.pdf");
        doc.save(&path).unwrap();
        let out = merge(&[path.clone()]).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_link_resolves_to_kids(&out);
    }

    // ---- finding 3: document-level catalog data survives -------------------

    #[test]
    fn extract_carries_named_dests() {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page1_id = doc.new_object_id();
        let c1 = doc.add_object(Stream::new(dictionary! {}, b" ".to_vec()));
        doc.set_object(page1_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c1),
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page1_id)],
            "Count" => 1,
        });
        // A /Dests name tree whose destination points at page 1.
        let dests_id = doc.add_object(dictionary! {
            "Names" => vec![
                Object::String(b"target".to_vec(), lopdf::StringFormat::Literal),
                Object::Array(vec![Object::Reference(page1_id), "Fit".into()]),
            ],
        });
        let names_id = doc.add_object(dictionary! { "Dests" => Object::Reference(dests_id) });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names_id),
            "Lang" => Object::String(b"en-US".to_vec(), lopdf::StringFormat::Literal),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let out = extract_pages(&doc, "1").unwrap();

        let root = out.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        let catalog = out.get_dictionary(root).unwrap();
        assert!(catalog.get(b"Lang").is_ok(), "/Lang must survive");
        let names_ref = catalog.get(b"Names").and_then(Object::as_reference).unwrap();
        let names = out.get_dictionary(names_ref).unwrap();
        let dests_ref = names.get(b"Dests").and_then(Object::as_reference).unwrap();
        let dests = out.get_dictionary(dests_ref).unwrap();
        let arr = dests.get(b"Names").unwrap().as_array().unwrap();
        let dest_val = arr[1].as_array().unwrap();
        let dest_page = dest_val[0].as_reference().unwrap();

        // The named destination must resolve to the page that is in /Kids.
        let kids = {
            let pages_ref = catalog.get(b"Pages").and_then(Object::as_reference).unwrap();
            let pages = out.get_dictionary(pages_ref).unwrap();
            pages
                .get(b"Kids")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_reference().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(dest_page, kids[0], "named dest must point at the copied page in /Kids");
    }
}
