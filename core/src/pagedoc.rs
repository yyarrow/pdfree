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
    // Document-level catalog data (named dests, AcroForm, optional content, ...)
    // carried onto the output catalog. See the multi-input policy note below.
    let mut extra_catalog: Vec<(Vec<u8>, Object)> = Vec::new();
    let single_input = inputs.len() == 1;

    for path in inputs.iter() {
        let src = crate::load_with_salvage(path).map_err(|e| PageDocError::Load(path.clone(), e))?;
        check_not_encrypted(&src)?;

        // Fresh per-source id map: object numbers are only unique within one
        // source file, and this also lets pages from the same source that
        // share a Resources/font dict dedupe onto the same copied object.
        let mut id_map = HashMap::new();
        let pages = src.get_pages();

        // Pre-register every source page id -> a fresh output id BEFORE copying
        // anything, and record the set of copyable page ids. A page reached
        // transitively while another page is being deep-copied (e.g. a
        // Link/annotation /Dest pointing at it) then resolves to the SAME
        // object that lands in /Kids, and pruned copying can distinguish a
        // selected page (reuse its mapping) from a plain object (copy it). In a
        // full-document merge every page of the input is selected, so pruning
        // never drops anything here.
        let mut page_set: PageSet = HashSet::new();
        for (_, &page_id) in &pages {
            let new_id = out.new_object_id();
            id_map.insert(page_id, new_id);
            page_set.insert(page_id);
        }
        for (_, &page_id) in &pages {
            let new_id = id_map[&page_id];
            copy_page_into(&src, page_id, new_id, &mut out, &mut id_map, &page_set)?;
            if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
                dict.set("Parent", Object::Reference(pages_id));
            }
            kids.push(Object::Reference(new_id));
        }

        // Catalog carry-over policy (finding 1):
        //  - Single input: carry its document-level tables (pruned; nothing is
        //    excluded, so pruning is a no-op).
        //  - Multi-input merge: carry NOTHING. Cross-document merging of
        //    /AcroForm /Fields, /Names destination trees and /OCProperties
        //    /OCGs cannot be done without field-name / dest-name / OCG-id
        //    collisions, and carrying only the FIRST input's tables (the prior
        //    behavior) left inputs 2..N's widgets/OCGs referencing table
        //    entries that were never emitted. Dropping the tables keeps every
        //    widget/OCG reference resolvable — the field and OCG objects are
        //    still copied via the pages that reference them, so there are no
        //    dangling refs — at the cost of document-level form / optional-
        //    content / named-navigation registration. Known limitation.
        if single_input {
            extra_catalog = collect_carried_catalog(&src, &mut id_map, &mut out, &page_set);
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
    // before copying, and record the set of copyable page ids. Transitive
    // cross-page links resolve to the object that actually lands in /Kids, and
    // pruned copying drops references to pages OUTSIDE this set instead of
    // dragging an excluded page (and, via its /Parent, the original page tree)
    // into the output (finding 3).
    let mut page_set: PageSet = HashSet::new();
    for &p in &selected {
        let &page_id = pages.get(&p).ok_or(PageDocError::PageOutOfRange(p, pages.len()))?;
        id_map.entry(page_id).or_insert_with(|| out.new_object_id());
        page_set.insert(page_id);
    }

    let mut filled: HashSet<ObjectId> = HashSet::new();
    for &p in &selected {
        let &page_id = pages.get(&p).ok_or(PageDocError::PageOutOfRange(p, pages.len()))?;
        let canonical = id_map[&page_id];
        let new_id = if filled.insert(page_id) {
            // First (canonical) copy of this page: fill its pre-registered id.
            copy_page_into(doc, page_id, canonical, &mut out, &mut id_map, &page_set)?;
            canonical
        } else {
            // Same page selected more than once (duplicates are allowed): emit a
            // distinct page object so /Kids never lists one object twice. Its
            // /Annots are freshly copied with /P and self-destinations pointing
            // at the DUPLICATE, not the canonical copy (finding 2); immutable
            // substructure (contents, resources, /AP) is still shared via
            // id_map, and the canonical mapping is left untouched so cross-page
            // links from elsewhere keep resolving to the first copy.
            let dup = out.new_object_id();
            copy_duplicate_page(doc, page_id, canonical, dup, &mut out, &mut id_map, &page_set)?;
            dup
        };
        if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
            dict.set("Parent", Object::Reference(pages_id));
        }
        kids.push(Object::Reference(new_id));
    }

    // Single input: carry over the whole document-level catalog data set,
    // pruned so entries targeting non-selected pages are dropped (finding 3).
    let extra_catalog = collect_carried_catalog(doc, &mut id_map, &mut out, &page_set);
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

/// Set of SOURCE page ids that pruned copying is allowed to reproduce (the
/// selected / output pages). Every one is pre-registered in `id_map` before any
/// copy runs. A reference reaching a page-tree node NOT in this set is a
/// reference to an EXCLUDED page: pruned copying returns None so the caller
/// drops that entry, rather than copying the excluded page and — via its
/// /Parent — dragging the original page tree into the output (finding 3).
type PageSet = HashSet<ObjectId>;

/// True if `id` is a node of the source page tree (/Type /Page or /Pages).
/// Such nodes are handled specially by pruned copying: selected pages are
/// reused via `id_map`, everything else (excluded pages, intermediate /Pages
/// nodes) is dropped, so a /Parent link is never chased into the page tree.
fn is_page_tree_node(src: &Document, id: ObjectId) -> bool {
    src.get_dictionary(id)
        .ok()
        .and_then(|d| d.get(b"Type").ok())
        .and_then(|t| t.as_name().ok())
        .map(|n| n == b"Page" || n == b"Pages")
        .unwrap_or(false)
}

/// Deep-copy one page (with inherited attributes materialized) from `src` into
/// the caller-supplied, pre-allocated output id `new_id`. /Parent is dropped
/// here; the caller re-points it at the new document's Pages root.
///
/// `new_id` MUST already be registered in `id_map` as the mapping for `page_id`
/// (the caller pre-registers every selected page id up front). That keeps
/// self-references (an Annot's /P) and transitive cross-page links (another
/// selected page's /Dest pointing here) resolving to THIS object — the one
/// placed in /Kids. Copying is pruned against `pages`: any /Annots (or other)
/// reference to a page NOT selected is dropped instead of pulling that page in.
fn copy_page_into(
    src: &Document, page_id: ObjectId, new_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Result<(), PageDocError> {
    let mut page_dict = resolve_inherited(src, page_id)?;
    page_dict.remove(b"Parent");
    page_dict.set("Type", Object::Name(b"Page".to_vec()));

    let new_dict = copy_page_dict(src, &page_dict, out, id_map, pages);
    out.set_object(new_id, Object::Dictionary(new_dict));
    Ok(())
}

/// Copy a page dictionary leniently: a page must always be produced, so a key
/// whose value references an excluded page is dropped rather than failing the
/// page. /Annots is pruned element-by-element (dead annotations dropped).
fn copy_page_dict(
    src: &Document, d: &Dictionary, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Dictionary {
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if k == b"Annots" {
            if let Ok(items) = v.as_array() {
                let kept: Vec<Object> = items
                    .iter()
                    .filter_map(|a| deep_copy_pruned(src, a, out, id_map, pages))
                    .collect();
                nd.set(k.clone(), Object::Array(kept));
                continue;
            }
        }
        match deep_copy_pruned(src, v, out, id_map, pages) {
            Some(cv) => {
                nd.set(k.clone(), cv);
            }
            // Non-/Annots key that transitively targets an excluded page (rare:
            // e.g. /B article beads): drop the key to keep the page valid.
            None => {}
        }
    }
    nd
}

/// Emit a DISTINCT page object for a page selected more than once, with its own
/// freshly-copied /Annots whose /P and self-destinations point at the DUPLICATE
/// (finding 2). Non-/Annots keys share immutable substructure (contents,
/// resources) with the canonical copy via `id_map`; the annotation dictionaries
/// themselves are new objects so the two page instances are independent.
fn copy_duplicate_page(
    src: &Document, page_id: ObjectId, canonical_id: ObjectId, dup_id: ObjectId,
    out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Result<(), PageDocError> {
    let mut page_dict = resolve_inherited(src, page_id)?;
    page_dict.remove(b"Parent");
    page_dict.set("Type", Object::Name(b"Page".to_vec()));

    let mut nd = Dictionary::new();
    for (k, v) in page_dict.iter() {
        if k == b"Annots" {
            if let Ok(items) = v.as_array() {
                let kept: Vec<Object> = items
                    .iter()
                    .filter_map(|a| copy_dup_annot(src, a, canonical_id, dup_id, out, id_map, pages))
                    .collect();
                nd.set(k.clone(), Object::Array(kept));
                continue;
            }
        }
        match deep_copy_pruned(src, v, out, id_map, pages) {
            Some(cv) => {
                nd.set(k.clone(), cv);
            }
            None => {}
        }
    }
    out.set_object(dup_id, Object::Dictionary(nd));
    Ok(())
}

/// Copy one annotation of a duplicated page into a FRESH object (not shared
/// with the canonical copy) and re-target it at the duplicate. The annotation's
/// inner structure (/AP appearance streams, resources) is still shared via
/// `id_map`. Any reference that pruned copying resolved to the canonical page
/// (the annot's own /P, a self-referential link /Dest or /A /D) is rewritten to
/// the duplicate so the two instances are independent. Returns None if the
/// annotation targets an excluded page (dropped, like on the canonical copy).
fn copy_dup_annot(
    src: &Document, annot: &Object, canonical_id: ObjectId, dup_id: ObjectId,
    out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Object> {
    let annot_dict = match annot {
        Object::Reference(id) => src.get_dictionary(*id).ok()?,
        Object::Dictionary(d) => d,
        _ => return None,
    };
    // Copy the annotation's contents into a fresh dictionary WITHOUT registering
    // the source annotation id in id_map, so the canonical copy keeps its own
    // annotation object. Shared substructure (/AP etc.) still dedupes via id_map.
    let mut copied = deep_copy_dict_pruned(src, annot_dict, out, id_map, pages)?;
    // pruned copy mapped every reference to the annot's own (canonical) page to
    // `canonical_id`; redirect those to the duplicate. Covers /P and any
    // self-referential destination.
    retarget_dict(&mut copied, canonical_id, dup_id);
    Some(Object::Reference(out.add_object(Object::Dictionary(copied))))
}

/// Rewrite every indirect reference to `from` into a reference to `to`,
/// recursively, throughout a dictionary's values.
fn retarget_dict(d: &mut Dictionary, from: ObjectId, to: ObjectId) {
    for (_, v) in d.iter_mut() {
        retarget_object(v, from, to);
    }
}

fn retarget_object(obj: &mut Object, from: ObjectId, to: ObjectId) {
    match obj {
        Object::Reference(id) if *id == from => *id = to,
        Object::Array(items) => items.iter_mut().for_each(|o| retarget_object(o, from, to)),
        Object::Dictionary(d) => retarget_dict(d, from, to),
        Object::Stream(s) => retarget_dict(&mut s.dict, from, to),
        _ => {}
    }
}

/// Deep-copy an arbitrary object graph reachable from `obj`, translating
/// indirect references through `id_map` and allocating fresh ids in `out` for
/// anything not yet copied. Cycle-safe: `id_map` is populated before recursing.
///
/// Returns None iff `obj` (transitively) references a page-tree node that is
/// NOT in `pages` — an excluded page. This is how a dead named destination /
/// form field / cross-page link is signalled for dropping instead of copying
/// the excluded page (and chasing its /Parent into the page tree). A reference
/// to a SELECTED page resolves to its pre-registered id and is never re-copied.
fn deep_copy_pruned(
    src: &Document, obj: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Object> {
    match obj {
        Object::Reference(old_id) => copy_ref_pruned(src, *old_id, out, id_map, pages),
        Object::Array(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for it in items {
                arr.push(deep_copy_pruned(src, it, out, id_map, pages)?);
            }
            Some(Object::Array(arr))
        }
        Object::Dictionary(d) => {
            Some(Object::Dictionary(deep_copy_dict_pruned(src, d, out, id_map, pages)?))
        }
        Object::Stream(s) => {
            let dict = deep_copy_dict_pruned(src, &s.dict, out, id_map, pages)?;
            Some(Object::Stream(lopdf::Stream {
                dict,
                content: s.content.clone(),
                allows_compression: s.allows_compression,
                start_position: None,
            }))
        }
        other => Some(other.clone()),
    }
}

fn deep_copy_dict_pruned(
    src: &Document, d: &Dictionary, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Dictionary> {
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        // Never chase a /Parent that points into the page tree: that is the
        // exact link that would drag excluded pages (and the whole /Pages tree)
        // into the output. Drop such a key. (A field's /Parent points at a
        // parent field, not a page node, and is preserved.)
        if k == b"Parent" {
            if let Ok(pid) = v.as_reference() {
                if is_page_tree_node(src, pid) {
                    continue;
                }
            }
        }
        nd.set(k.clone(), deep_copy_pruned(src, v, out, id_map, pages)?);
    }
    Some(nd)
}

fn copy_ref_pruned(
    src: &Document, old_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Object> {
    // A reference into the page tree is special. A selected page is reused via
    // its pre-registered mapping (never re-copied — that would chase /Parent).
    // Any other page-tree node (an excluded page, or an intermediate /Pages
    // node) is dropped so nothing follows /Parent/#Kids into the page tree.
    if is_page_tree_node(src, old_id) {
        if pages.contains(&old_id) {
            return id_map.get(&old_id).map(|&n| Object::Reference(n));
        }
        return None;
    }
    Some(Object::Reference(copy_object_id_pruned(src, old_id, out, id_map, pages)?))
}

fn copy_object_id_pruned(
    src: &Document, old_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<ObjectId> {
    if let Some(&new_id) = id_map.get(&old_id) {
        return Some(new_id);
    }
    let new_id = out.new_object_id();
    id_map.insert(old_id, new_id); // before recursing: breaks reference cycles
    match src.get_object(old_id) {
        Ok(obj) => match deep_copy_pruned(src, obj, out, id_map, pages) {
            Some(new_obj) => {
                out.set_object(new_id, new_obj);
                Some(new_id)
            }
            None => {
                // References an excluded page. Undo the tentative mapping so a
                // later prunable context re-evaluates it, and signal drop. The
                // reserved `new_id` is simply left unset — nothing references it.
                id_map.remove(&old_id);
                None
            }
        },
        // Dangling reference in the source (missing/unresolvable object): keep
        // the graph structurally valid rather than failing the whole copy.
        Err(_) => {
            out.set_object(new_id, Object::Null);
            Some(new_id)
        }
    }
}

/// Document-level catalog keys carried over from a source catalog so features
/// that live above the page tree keep working after merge/split: named
/// destinations (/Names, /Dests), interactive form (/AcroForm), optional
/// content (/OCProperties), document language (/Lang), and viewer intent
/// (/ViewerPreferences, /PageLayout, /PageMode).
///
/// /StructTreeRoot is deliberately NOT carried: its /K structure holds parent
/// pointers into content and marked-content sequences we cannot guarantee stay
/// consistent across an arbitrary page subset, so we drop it rather than emit a
/// dangling/inconsistent tagging tree. Cross-document merging of these tables
/// (multi-input merge) is also unsupported — see `merge`, which carries NONE of
/// them for multi-input. Both are known limitations.
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
/// the SAME `id_map` used for the pages, so their references (a named
/// destination's page reference, an AcroForm field's /P and widget) resolve
/// onto the freshly copied page objects. Entries whose target page is NOT in
/// `pages` (the selected/output set) are PRUNED — the dead named destination is
/// removed, the excluded-page form field is skipped — instead of copying the
/// excluded page (finding 3). Returns the surviving (key, value) pairs for
/// `finish_document` to place on the new catalog.
///
/// Must be called AFTER the pages have been copied, so selected page ids and
/// their annotations/widgets are already in `id_map` and get reused.
fn collect_carried_catalog(
    src: &Document, id_map: &mut HashMap<ObjectId, ObjectId>, out: &mut Document, pages: &PageSet,
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
            if let Some(copied) = copy_catalog_value(src, key, val, out, id_map, pages) {
                carried.push((key.to_vec(), copied));
            }
        }
    }
    carried
}

/// Copy one carried catalog entry, pruning page-tree references per table shape.
/// Returns None to drop the whole key (e.g. a table that itself resolves to an
/// excluded page, which should not happen for the keys we carry).
fn copy_catalog_value(
    src: &Document, key: &[u8], val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Object> {
    match key {
        // Name-tree dictionary (/Dests, /AP, ...). Only /Dests holds page
        // destinations; the others simply never prune.
        b"Names" => copy_names_dict(src, val, out, id_map, pages),
        // Catalog /Dests: a flat dictionary mapping name -> destination.
        b"Dests" => Some(copy_dests_dict(src, val, out, id_map, pages)),
        b"AcroForm" => copy_acroform(src, val, out, id_map, pages),
        // /OCProperties, /Lang, /ViewerPreferences, /PageLayout, /PageMode do
        // not reference pages; a generic pruned copy is a no-op wrt pruning.
        _ => deep_copy_pruned(src, val, out, id_map, pages),
    }
}

/// Resolve `val` to a dictionary, returning whether it was indirect (so the
/// rebuilt copy is re-emitted as an indirect object) plus a clone to iterate
/// (cloning frees the borrow on `src` for the copy recursion).
fn resolve_catalog_dict(src: &Document, val: &Object) -> Option<(bool, Dictionary)> {
    match val {
        Object::Reference(id) => src.get_dictionary(*id).ok().map(|d| (true, d.clone())),
        Object::Dictionary(d) => Some((false, d.clone())),
        _ => None,
    }
}

/// Place a rebuilt dictionary back, indirectly if the source was indirect.
fn place_catalog_dict(out: &mut Document, indirect: bool, dict: Dictionary) -> Object {
    if indirect {
        Object::Reference(out.add_object(Object::Dictionary(dict)))
    } else {
        Object::Dictionary(dict)
    }
}

/// Copy the catalog /Names dictionary (name -> name-tree root), pruning each
/// sub-tree. If /Names is not a dictionary, fall back to a generic pruned copy.
fn copy_names_dict(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Object> {
    let Some((indirect, d)) = resolve_catalog_dict(src, val) else {
        return deep_copy_pruned(src, val, out, id_map, pages);
    };
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        nd.set(k.clone(), copy_name_tree(src, v, out, id_map, pages));
    }
    Some(place_catalog_dict(out, indirect, nd))
}

/// Copy a name-tree node, dropping /Names key/value pairs whose destination
/// targets an excluded page and recursing into /Kids. /Limits is intentionally
/// NOT carried: after pruning, a stale [min,max] would be wrong, and omitting
/// it only forces readers into a linear scan (spec-tolerant). Empty leaf nodes
/// are harmless and left in place.
fn copy_name_tree(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Object {
    let Some((indirect, d)) = resolve_catalog_dict(src, val) else {
        // Not a dict (unexpected): best-effort generic copy, dropping if it
        // targets an excluded page.
        return deep_copy_pruned(src, val, out, id_map, pages).unwrap_or(Object::Null);
    };
    let mut nd = Dictionary::new();
    if let Ok(names) = d.get(b"Names").and_then(Object::as_array) {
        // Flat [key1 dest1 key2 dest2 ...]; keep a pair only if its destination
        // survives pruning.
        let mut arr = Vec::new();
        let mut i = 0;
        while i + 1 < names.len() {
            if let Some(cd) = deep_copy_pruned(src, &names[i + 1], out, id_map, pages) {
                arr.push(names[i].clone());
                arr.push(cd);
            }
            i += 2;
        }
        nd.set("Names", Object::Array(arr));
    }
    if let Ok(kids) = d.get(b"Kids").and_then(Object::as_array) {
        let new_kids: Vec<Object> = kids
            .iter()
            .map(|kid| copy_name_tree(src, kid, out, id_map, pages))
            .collect();
        nd.set("Kids", Object::Array(new_kids));
    }
    place_catalog_dict(out, indirect, nd)
}

/// Copy the catalog /Dests dictionary (name -> destination), dropping any named
/// destination whose target page is not in the selected set.
fn copy_dests_dict(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Object {
    let Some((indirect, d)) = resolve_catalog_dict(src, val) else {
        return deep_copy_pruned(src, val, out, id_map, pages).unwrap_or(Object::Null);
    };
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if let Some(cv) = deep_copy_pruned(src, v, out, id_map, pages) {
            nd.set(k.clone(), cv);
        }
        // else: destination points at an excluded page -> drop this name.
    }
    place_catalog_dict(out, indirect, nd)
}

/// Copy /AcroForm, dropping /Fields entries whose field (or a widget under it)
/// targets an excluded page. A field is copied strictly: if any part of it —
/// its /P, or a /Kids widget's /P — resolves to an excluded page, the whole
/// field is skipped rather than emitted with a dangling page reference.
fn copy_acroform(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, pages: &PageSet,
) -> Option<Object> {
    let (indirect, d) = resolve_catalog_dict(src, val)?;
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if k == b"Fields" {
            if let Ok(fields) = v.as_array() {
                let kept: Vec<Object> = fields
                    .iter()
                    .filter_map(|f| deep_copy_pruned(src, f, out, id_map, pages))
                    .collect();
                nd.set(k.clone(), Object::Array(kept));
                continue;
            }
        }
        match deep_copy_pruned(src, v, out, id_map, pages) {
            Some(cv) => {
                nd.set(k.clone(), cv);
            }
            None => {}
        }
    }
    Some(place_catalog_dict(out, indirect, nd))
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

    // ---- shared test helpers ----------------------------------------------

    fn collect_refs(obj: &Object, into: &mut Vec<ObjectId>) {
        match obj {
            Object::Reference(id) => into.push(*id),
            Object::Array(a) => a.iter().for_each(|o| collect_refs(o, into)),
            Object::Dictionary(d) => d.iter().for_each(|(_, v)| collect_refs(v, into)),
            Object::Stream(s) => s.dict.iter().for_each(|(_, v)| collect_refs(v, into)),
            _ => {}
        }
    }

    /// Whole-document scan: every indirect reference (from any object and from
    /// the trailer) must resolve to an object present in the output.
    fn assert_no_dangling(out: &Document) {
        for obj in out.objects.values() {
            let mut refs = Vec::new();
            collect_refs(obj, &mut refs);
            for r in refs {
                assert!(out.objects.contains_key(&r), "dangling reference to {r:?}");
            }
        }
        let root = out.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        assert!(out.objects.contains_key(&root), "trailer /Root dangles");
    }

    fn count_page_objects(out: &Document) -> usize {
        out.objects
            .values()
            .filter(|o| {
                o.as_dict()
                    .ok()
                    .and_then(|d| d.get(b"Type").ok())
                    .and_then(|t| t.as_name().ok())
                    .map(|n| n == b"Page")
                    .unwrap_or(false)
            })
            .count()
    }

    fn catalog_of(out: &Document) -> &Dictionary {
        let root = out.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        out.get_dictionary(root).unwrap()
    }

    // ---- finding 3 (DEEP): excluded pages are never re-embedded ------------

    /// Three-page doc with a /Dests name tree holding two entries: "home" ->
    /// page 1 (kept on split) and "toc" -> page 3 (excluded on split).
    fn build_named_dest_doc() -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let p1 = doc.new_object_id();
        let p2 = doc.new_object_id();
        let p3 = doc.new_object_id();
        for (id, body) in [(p1, &b"(one)"[..]), (p2, &b"(two)"[..]), (p3, &b"(three)"[..])] {
            let c = doc.add_object(Stream::new(dictionary! {}, body.to_vec()));
            doc.set_object(id, dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference(pages_id),
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Contents" => Object::Reference(c),
            });
        }
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(p1), Object::Reference(p2), Object::Reference(p3)],
            "Count" => 3,
        });
        // Name-tree root: [home, dest->p1, toc, dest->p3].
        let dests_id = doc.add_object(dictionary! {
            "Names" => vec![
                Object::String(b"home".to_vec(), lopdf::StringFormat::Literal),
                Object::Array(vec![Object::Reference(p1), "Fit".into()]),
                Object::String(b"toc".to_vec(), lopdf::StringFormat::Literal),
                Object::Array(vec![Object::Reference(p3), "Fit".into()]),
            ],
        });
        let names_id = doc.add_object(dictionary! { "Dests" => Object::Reference(dests_id) });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn split_drops_dest_to_excluded_page_without_reembedding() {
        let doc = build_named_dest_doc();
        let out = extract_pages(&doc, "1").unwrap();

        // Exactly one page object survives: page 3 (and page 2) are NOT pulled
        // back in through the "toc" destination or any /Parent chase.
        assert_eq!(count_page_objects(&out), 1, "excluded pages must not be re-embedded");
        assert_eq!(kids_of(&out).len(), 1);

        // The dead "toc" destination is dropped; the live "home" one survives.
        let catalog = catalog_of(&out);
        let names_ref = catalog.get(b"Names").and_then(Object::as_reference).unwrap();
        let names = out.get_dictionary(names_ref).unwrap();
        let dests_ref = names.get(b"Dests").and_then(Object::as_reference).unwrap();
        let dests = out.get_dictionary(dests_ref).unwrap();
        let arr = dests.get(b"Names").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2, "only the surviving name/dest pair should remain");
        assert_eq!(arr[0].as_str().unwrap(), b"home");
        let dest_page = arr[1].as_array().unwrap()[0].as_reference().unwrap();
        assert_eq!(dest_page, kids_of(&out)[0], "surviving dest must point at the kept page");

        assert_no_dangling(&out);
    }

    // ---- finding 2: duplicated page gets independent annotations -----------

    /// One-page doc whose page carries a Link annotation with /P -> its page
    /// and a self-referential /Dest -> [page Fit].
    fn build_annotated_page_doc() -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, b"(x)".to_vec()));
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 100.into(), 20.into()],
            "P" => Object::Reference(page_id),
            "Dest" => vec![Object::Reference(page_id), "Fit".into()],
        });
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
            "Annots" => vec![Object::Reference(annot_id)],
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn duplicated_page_has_independent_annotations() {
        let doc = build_annotated_page_doc();
        let out = extract_pages(&doc, "1,1").unwrap();

        let kids = kids_of(&out);
        assert_eq!(kids.len(), 2, "duplicate selection must emit two page objects");
        assert_ne!(kids[0], kids[1], "the two page objects must be distinct");

        // Each page's annotation is a DISTINCT object whose /P and self-/Dest
        // point at its OWN page, not the sibling copy.
        let mut annot_ids = Vec::new();
        for &kid in &kids {
            let page = out.get_dictionary(kid).unwrap();
            let annots = page.get(b"Annots").unwrap().as_array().unwrap();
            assert_eq!(annots.len(), 1);
            let annot_id = annots[0].as_reference().unwrap();
            annot_ids.push(annot_id);
            let annot = out.get_dictionary(annot_id).unwrap();
            assert_eq!(annot.get(b"P").unwrap().as_reference().unwrap(), kid, "/P must parent to own page");
            let dest_page = annot.get(b"Dest").unwrap().as_array().unwrap()[0].as_reference().unwrap();
            assert_eq!(dest_page, kid, "self /Dest must follow the duplicate");
        }
        assert_ne!(annot_ids[0], annot_ids[1], "annotations must be independent objects");
        assert_no_dangling(&out);
    }

    // ---- finding 1: multi-input merge drops unmergeable catalog tables -----

    /// One-page doc carrying an /AcroForm with a single text field and its
    /// widget annotation (widget /P -> page, /Parent -> field; field /Kids ->
    /// widget, /P -> page).
    fn build_acroform_doc(marker: &[u8]) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let field_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, marker.to_vec()));
        let widget_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "Rect" => vec![0.into(), 0.into(), 100.into(), 20.into()],
            "P" => Object::Reference(page_id),
            "Parent" => Object::Reference(field_id),
        });
        doc.set_object(field_id, dictionary! {
            "FT" => "Tx",
            "T" => Object::String(marker.to_vec(), lopdf::StringFormat::Literal),
            "Kids" => vec![Object::Reference(widget_id)],
            "P" => Object::Reference(page_id),
        });
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
            "Annots" => vec![Object::Reference(widget_id)],
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let acroform_id = doc.add_object(dictionary! {
            "Fields" => vec![Object::Reference(field_id)],
            "NeedAppearances" => true,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "AcroForm" => Object::Reference(acroform_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    fn save_temp(doc: &mut Document, name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        doc.save(&path).unwrap();
        path
    }

    #[test]
    fn merge_multi_input_drops_acroform_without_dangling() {
        let mut a = build_acroform_doc(b"(a)");
        let mut b = build_acroform_doc(b"(b)");
        let pa = save_temp(&mut a, "pdfree_pagedoc_form_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_form_b.pdf");

        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        assert_eq!(out.get_pages().len(), 2, "merge must sum pages");
        // Unmergeable across inputs: /AcroForm is dropped entirely so no
        // input's widgets reference a form table that only holds another input.
        assert!(catalog_of(&out).get(b"AcroForm").is_err(), "multi-input merge must drop /AcroForm");
        // The widgets and their parent fields are still fully present (reached
        // via the pages), so nothing dangles.
        assert_no_dangling(&out);
    }

    #[test]
    fn merge_single_input_keeps_acroform() {
        let mut b = build_acroform_doc(b"(only)");
        let pb = save_temp(&mut b, "pdfree_pagedoc_form_single.pdf");
        let out = merge(&[pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pb);

        let acro_ref = catalog_of(&out).get(b"AcroForm").and_then(Object::as_reference);
        let acro = out.get_dictionary(acro_ref.unwrap()).unwrap();
        let fields = acro.get(b"Fields").unwrap().as_array().unwrap();
        assert_eq!(fields.len(), 1, "single-input merge keeps the form field");
        assert_no_dangling(&out);
    }
}
