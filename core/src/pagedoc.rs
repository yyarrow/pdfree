//! Page-level document operations: merge (concatenate pages from several
//! PDFs into one) and split/extract (keep only a page subset).
//!
//! Written purely against ISO 32000 (7.7.3 "Document Catalog"/"Page Tree")
//! and the lopdf (MIT) API; no GPL/AGPL PDF source was consulted.

use lopdf::{decode_text_string, Dictionary, Document, Object, ObjectId, StringFormat};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// A document-level table cannot be combined across inputs without losing
    /// what it describes. Refuse the merge instead of dropping it silently.
    #[error("cannot merge {0} across documents; the merge would lose it")]
    UnmergeableCatalog(&'static str),
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
    // carried onto the output catalog. See the carry-over policy note below.
    let mut extra_catalog: Vec<(Vec<u8>, Object)> = Vec::new();
    let single_input = inputs.len() == 1;
    let mut merger = CatalogMerger::default();

    for (input, path) in inputs.iter().enumerate() {
        let src = crate::load_with_salvage(path).map_err(|e| PageDocError::Load(path.clone(), e))?;
        check_not_encrypted(&src)?;

        // Fresh per-source id map: object numbers are only unique within one
        // source file, and this also lets pages from the same source that
        // share a Resources/font dict dedupe onto the same copied object.
        let mut id_map = HashMap::new();
        let pages = src.get_pages();

        // Every object this input contributes to `out` gets an id above this
        // mark (lopdf hands out ids by incrementing `max_id`). That is how a
        // destination renamed for collision (below) is rewritten in THIS
        // input's copied objects only, leaving earlier inputs untouched.
        let id_floor = out.max_id;

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
        let ctx = PruneCtx::new(&src, page_set);
        for (_, &page_id) in &pages {
            let new_id = id_map[&page_id];
            copy_page_into(&src, page_id, new_id, &mut out, &mut id_map, &ctx)?;
            if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
                dict.set("Parent", Object::Reference(pages_id));
            }
            kids.push(Object::Reference(new_id));
        }

        // Catalog carry-over policy (finding 1):
        //  - Single input: carry its document-level tables verbatim (pruned;
        //    nothing is excluded, so pruning is a no-op).
        //  - Multi-input: MERGE them (CatalogMerger). Concatenating /AcroForm
        //    /Fields and unioning /OCProperties /OCGs is well defined; named
        //    destinations are unioned with colliding names suffixed and every
        //    reference to a renamed one rewritten. A table that cannot be
        //    combined without losing what it describes (an XFA form, alternate
        //    optional-content configurations) fails the merge with
        //    UnmergeableCatalog rather than being dropped silently — the old
        //    behavior (carry the first input's tables, or none at all) lost
        //    inputs 2..N's interactive fields, OCG visibility and named dests
        //    without a word.
        if single_input {
            extra_catalog = collect_carried_catalog(&src, &mut id_map, &mut out, &ctx);
        } else {
            merger.add_source(&src, &mut out, &mut id_map, &ctx, InputMark { index: input, id_floor })?;
        }
    }

    if !single_input {
        extra_catalog = merger.finish(&mut out)?;
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
    let ctx = PruneCtx::new(doc, page_set);

    let mut filled: HashSet<ObjectId> = HashSet::new();
    for &p in &selected {
        let &page_id = pages.get(&p).ok_or(PageDocError::PageOutOfRange(p, pages.len()))?;
        let canonical = id_map[&page_id];
        let new_id = if filled.insert(page_id) {
            // First (canonical) copy of this page: fill its pre-registered id.
            copy_page_into(doc, page_id, canonical, &mut out, &mut id_map, &ctx)?;
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
            copy_duplicate_page(doc, page_id, canonical, dup, &mut out, &mut id_map, &ctx)?;
            dup
        };
        if let Ok(dict) = out.get_object_mut(new_id).and_then(Object::as_dict_mut) {
            dict.set("Parent", Object::Reference(pages_id));
        }
        kids.push(Object::Reference(new_id));
    }

    // Single input: carry over the whole document-level catalog data set,
    // pruned so entries targeting non-selected pages are dropped (finding 3).
    let extra_catalog = collect_carried_catalog(doc, &mut id_map, &mut out, &ctx);
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

/// Everything pruned copying needs to know about the SOURCE document: which
/// pages may be reproduced, and which page each annotation belongs to.
struct PruneCtx {
    /// Source page ids allowed in the output (see `PageSet`).
    pages: PageSet,
    /// annot id -> the page whose /Annots lists it, built from EVERY page of
    /// the source (not just the kept ones). A widget annotation's page cannot
    /// always be read off the widget: /P is only optional (ISO 32000 12.5.2),
    /// so without this map a /Kids widget with no /P could not be told apart
    /// from one on an excluded page (finding 4).
    annot_page: HashMap<ObjectId, ObjectId>,
}

impl PruneCtx {
    fn new(src: &Document, pages: PageSet) -> Self {
        let mut annot_page = HashMap::new();
        for &page_id in src.get_pages().values() {
            let Ok(dict) = src.get_dictionary(page_id) else { continue };
            for annot in dict_array(src, dict, b"Annots").unwrap_or(&[]) {
                if let Ok(id) = annot.as_reference() {
                    annot_page.insert(id, page_id);
                }
            }
        }
        Self { pages, annot_page }
    }

    fn keeps(&self, page_id: ObjectId) -> bool {
        self.pages.contains(&page_id)
    }

    /// The page an annotation sits on: its /P if that names a page, otherwise
    /// the page whose /Annots lists it. None if neither says (an orphan
    /// annotation on no page at all), in which case callers keep it: there is
    /// no evidence it belongs to an excluded page.
    fn page_of_annot(&self, src: &Document, annot_id: ObjectId) -> Option<ObjectId> {
        if let Ok(d) = src.get_dictionary(annot_id) {
            if let Ok(pid) = d.get(b"P").and_then(Object::as_reference) {
                if is_page_tree_node(src, pid) {
                    return Some(pid);
                }
            }
        }
        self.annot_page.get(&annot_id).copied()
    }
}

/// Resolve `d[key]` through `src` down to an array. Arrays that the spec allows
/// to be indirect (/Annots, a field's /Kids) are frequently written that way, and
/// a plain `as_array()` on the raw value silently fails on them (finding 2).
fn dict_array<'a>(src: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a [Object]> {
    let v = d.get(key).ok()?;
    let (_, obj) = src.dereference(v).ok()?;
    obj.as_array().ok().map(Vec::as_slice)
}

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
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Result<(), PageDocError> {
    let mut page_dict = resolve_inherited(src, page_id)?;
    page_dict.remove(b"Parent");
    page_dict.set("Type", Object::Name(b"Page".to_vec()));

    let new_dict = copy_page_dict(src, &page_dict, out, id_map, ctx);
    out.set_object(new_id, Object::Dictionary(new_dict));
    Ok(())
}

/// Copy a page dictionary leniently: a page must always be produced, so a key
/// whose value references an excluded page is dropped rather than failing the
/// page. /Annots is resolved through `src` (it is often an INDIRECT reference to
/// the array — finding 2) and pruned element-by-element, so one dead annotation
/// no longer takes the whole array down with it. The rebuilt array is emitted
/// DIRECTLY on the page: an /Annots array belongs to exactly one page, and a
/// direct array cannot be accidentally shared with another copy of it.
fn copy_page_dict(
    src: &Document, d: &Dictionary, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Dictionary {
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if k == b"Annots" {
            if let Some(items) = dict_array(src, d, b"Annots") {
                let kept: Vec<Object> = items
                    .iter()
                    .filter_map(|a| deep_copy_pruned(src, a, out, id_map, ctx))
                    .collect();
                nd.set(k.clone(), Object::Array(kept));
                continue;
            }
        }
        match deep_copy_pruned(src, v, out, id_map, ctx) {
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
///
/// /Annots is resolved through `src` before the fresh-copy path: when it is an
/// INDIRECT reference to the array, a raw `as_array()` fails and the generic
/// path would hand the duplicate the canonical page's already-copied array —
/// annotations and all — leaving this page's annots' /P on the first copy. The
/// duplicate always gets a fresh direct array of fresh annotation objects,
/// whatever shape the source used.
fn copy_duplicate_page(
    src: &Document, page_id: ObjectId, canonical_id: ObjectId, dup_id: ObjectId,
    out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Result<(), PageDocError> {
    let mut page_dict = resolve_inherited(src, page_id)?;
    page_dict.remove(b"Parent");
    page_dict.set("Type", Object::Name(b"Page".to_vec()));

    let mut nd = Dictionary::new();
    for (k, v) in page_dict.iter() {
        if k == b"Annots" {
            if let Some(items) = dict_array(src, &page_dict, b"Annots") {
                let kept = copy_dup_annots(src, items, canonical_id, dup_id, out, id_map, ctx);
                nd.set(k.clone(), Object::Array(kept));
                continue;
            }
        }
        match deep_copy_pruned(src, v, out, id_map, ctx) {
            Some(cv) => {
                nd.set(k.clone(), cv);
            }
            None => {}
        }
    }
    out.set_object(dup_id, Object::Dictionary(nd));
    Ok(())
}

/// Per-dup-page context for copying a duplicated page's annotation set into
/// fresh, self-contained objects.
#[derive(Clone, Copy)]
struct DupAnnots<'a> {
    /// Source annotation ids listed on THIS page. A reference from one annotation
    /// to another in this set (/IRT in-reply-to, /Popup, a popup's /Parent back
    /// to its markup annotation) is retargeted to the dup's OWN fresh copy rather
    /// than resolved — through the shared id_map — to the canonical page's copy.
    set: &'a HashSet<ObjectId>,
    /// The canonical page's output id and this duplicate's output id: a reference
    /// that resolves to the (shared) canonical page — an annot's /P, a
    /// self-referential /Dest — is redirected to the duplicate.
    canonical_id: ObjectId,
    dup_id: ObjectId,
}

/// Copy the /Annots of a page selected more than once into FRESH objects that do
/// not share their annotation graph with the canonical copy (finding: a
/// duplicated reply/popup used to point at the first copy's annotations). Each
/// annotation is copied into a fresh id kept in a per-dup `local` map; a nested
/// reference to another annotation of THIS page (in `set`) is copied through
/// `local` too, so the two page instances are fully independent. References that
/// leave the set — shared /AP streams, a widget's form-field /Parent — still
/// dedupe via `id_map` exactly as on the canonical copy.
fn copy_dup_annots(
    src: &Document, items: &[Object], canonical_id: ObjectId, dup_id: ObjectId,
    out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Vec<Object> {
    let set: HashSet<ObjectId> = items.iter().filter_map(|a| a.as_reference().ok()).collect();
    let dup = DupAnnots { set: &set, canonical_id, dup_id };
    let mut local: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut kept = Vec::new();
    for a in items {
        match a.as_reference() {
            Ok(id) => {
                if let Some(nid) = copy_dup_annot_id(src, id, dup, out, id_map, &mut local, ctx) {
                    kept.push(Object::Reference(nid));
                }
            }
            // A direct (inline) annotation dictionary: copy it standalone, still
            // retargeting page references and any intra-set links.
            Err(_) => {
                if let Some(mut obj) = deep_copy_dup(src, a, dup, out, id_map, &mut local, ctx) {
                    retarget_object(&mut obj, canonical_id, dup_id);
                    kept.push(obj);
                }
            }
        }
    }
    kept
}

/// Copy one annotation of a duplicated page into a FRESH object registered in
/// the per-dup `local` map (NOT `id_map`, so the canonical copy keeps its own).
/// Returns None if the annotation targets an excluded page.
fn copy_dup_annot_id(
    src: &Document, id: ObjectId, dup: DupAnnots, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, local: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<ObjectId> {
    if let Some(&nid) = local.get(&id) {
        return Some(nid);
    }
    let Ok(dict) = src.get_dictionary(id) else {
        return None;
    };
    let new_id = out.new_object_id();
    local.insert(id, new_id); // before recursing: breaks annot <-> popup cycles
    match deep_copy_dup_dict(src, dict, dup, out, id_map, local, ctx) {
        Some(mut nd) => {
            // Every reference that resolved to the annot's own (canonical) page —
            // its /P, a self-referential /Dest or /A /D — becomes the duplicate's.
            retarget_dict(&mut nd, dup.canonical_id, dup.dup_id);
            out.set_object(new_id, Object::Dictionary(nd));
            Some(new_id)
        }
        None => {
            local.remove(&id);
            None
        }
    }
}

/// Deep-copy a duplicated annotation's dictionary. Intra-set references resolve
/// to the dup's own copies; everything else shares via `id_map`. The markup
/// back-links /IRT and /Popup are dropped (key only) if their target cannot be
/// resolved within the set, rather than left pointing at the canonical page.
fn deep_copy_dup_dict(
    src: &Document, d: &Dictionary, dup: DupAnnots, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, local: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Dictionary> {
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        // /Parent: a popup's back-link to its markup annotation is intra-set and
        // follows the dup; a widget's form-field parent leaves the set (shared
        // via id_map); a page-tree parent is dropped.
        if k == b"Parent" {
            if let Ok(pid) = v.as_reference() {
                if dup.set.contains(&pid) {
                    if let Some(nid) = copy_dup_annot_id(src, pid, dup, out, id_map, local, ctx) {
                        nd.set(k.clone(), Object::Reference(nid));
                    }
                    continue;
                }
                if is_page_tree_node(src, pid) {
                    continue;
                }
                if let Some(nid) = copy_field_node(src, pid, out, id_map, ctx) {
                    nd.set(k.clone(), Object::Reference(nid));
                }
                continue;
            }
        }
        match deep_copy_dup(src, v, dup, out, id_map, local, ctx) {
            Some(cv) => {
                nd.set(k.clone(), cv);
            }
            // /IRT and /Popup are intra-set back-links: if the target could not be
            // reproduced in this dup, drop just the key. Any other unresolved key
            // targets an excluded page and takes the annotation with it.
            None => {
                if k != b"IRT" && k != b"Popup" {
                    return None;
                }
            }
        }
    }
    Some(nd)
}

/// Deep-copy one value of a duplicated annotation. A reference to another
/// annotation of the same page (in `dup.set`) is copied into the dup's own set;
/// any other reference shares the canonical copy via `id_map`.
fn deep_copy_dup(
    src: &Document, obj: &Object, dup: DupAnnots, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, local: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    match obj {
        Object::Reference(old_id) => {
            if dup.set.contains(old_id) {
                Some(Object::Reference(copy_dup_annot_id(src, *old_id, dup, out, id_map, local, ctx)?))
            } else {
                copy_ref_pruned(src, *old_id, out, id_map, ctx)
            }
        }
        Object::Array(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for it in items {
                arr.push(deep_copy_dup(src, it, dup, out, id_map, local, ctx)?);
            }
            Some(Object::Array(arr))
        }
        Object::Dictionary(d) => {
            Some(Object::Dictionary(deep_copy_dup_dict(src, d, dup, out, id_map, local, ctx)?))
        }
        Object::Stream(s) => {
            let dict = deep_copy_dup_dict(src, &s.dict, dup, out, id_map, local, ctx)?;
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
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    match obj {
        Object::Reference(old_id) => copy_ref_pruned(src, *old_id, out, id_map, ctx),
        Object::Array(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for it in items {
                arr.push(deep_copy_pruned(src, it, out, id_map, ctx)?);
            }
            Some(Object::Array(arr))
        }
        Object::Dictionary(d) => {
            Some(Object::Dictionary(deep_copy_dict_pruned(src, d, out, id_map, ctx)?))
        }
        Object::Stream(s) => {
            let dict = deep_copy_dict_pruned(src, &s.dict, out, id_map, ctx)?;
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
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Dictionary> {
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if k == b"Parent" {
            if let Ok(pid) = v.as_reference() {
                // Never chase a /Parent that points into the page tree: that is
                // the exact link that would drag excluded pages (and the whole
                // /Pages tree) into the output. Drop such a key.
                if is_page_tree_node(src, pid) {
                    continue;
                }
                // Any other /Parent is a form-field parent (a widget's field, a
                // field's ancestor field). Copy it PARTIALLY (finding 4): the
                // strict copy used to fail on the parent as soon as one of its
                // /Kids sat on an excluded page, which then deleted THIS object
                // — a widget belonging to a page we are keeping. On the (near
                // impossible) chance the parent still cannot be produced, drop
                // the /Parent key and keep the widget rather than the reverse.
                match copy_field_node(src, pid, out, id_map, ctx) {
                    Some(nid) => nd.set(k.clone(), Object::Reference(nid)),
                    None => {}
                }
                continue;
            }
        }
        nd.set(k.clone(), deep_copy_pruned(src, v, out, id_map, ctx)?);
    }
    Some(nd)
}

/// Copy a node of the /AcroForm field hierarchy, PARTIALLY (finding 4).
///
/// A field graph must not be all-or-nothing: a radio group's /Kids can hold
/// widgets on kept AND excluded pages, and rejecting the whole field (the old
/// strict copy) dropped the group from the form — and, reached through a kept
/// widget's /Parent, dropped that widget from its page too. Here only the
/// individual /Kids entries whose widget sits on an excluded page are pruned;
/// the field itself survives with the kept widgets still attached. Every other
/// key is copied leniently: a dead /P or /A action costs that key, not the field.
///
/// Returns None only when the node genuinely has nothing left — a terminal
/// field whose own page is excluded, or a node all of whose /Kids were pruned —
/// in which case the caller drops it from /Kids or /AcroForm /Fields.
fn copy_field_node(
    src: &Document, field_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<ObjectId> {
    // Already copied (or in progress, having been reached through a widget's
    // /Parent): reuse it. That mapping is also what breaks the widget <-> field
    // reference cycle.
    if let Some(&new_id) = id_map.get(&field_id) {
        return Some(new_id);
    }
    let Ok(dict) = src.get_dictionary(field_id) else {
        // Not a dictionary: not a field after all. Generic copy.
        return copy_object_id_pruned(src, field_id, out, id_map, ctx);
    };
    let kid_count = dict_array(src, dict, b"Kids").map_or(0, <[Object]>::len);
    if kid_count == 0 {
        // A terminal field with no /Kids IS its own widget annotation (ISO
        // 32000 12.7.3.1, merged field/widget): it lives on exactly one page,
        // so an excluded page takes it with it — nothing is lost that the page
        // itself did not already take.
        if let Some(p) = ctx.page_of_annot(src, field_id) {
            if !ctx.keeps(p) {
                return None;
            }
        }
    }

    let new_id = out.new_object_id();
    id_map.insert(field_id, new_id); // before recursing: breaks reference cycles
    let mut nd = Dictionary::new();
    let mut kids_kept = 0usize;
    for (k, v) in dict.iter() {
        match k.as_slice() {
            b"Kids" => {
                let Some(kids) = dict_array(src, dict, b"Kids") else { continue };
                let kept: Vec<Object> = kids
                    .iter()
                    .filter_map(|kid| copy_field_kid(src, kid, out, id_map, ctx))
                    .collect();
                kids_kept = kept.len();
                nd.set(k.clone(), Object::Array(kept));
            }
            b"Parent" => match v.as_reference() {
                Ok(pid) if !is_page_tree_node(src, pid) => {
                    if let Some(nid) = copy_field_node(src, pid, out, id_map, ctx) {
                        nd.set(k.clone(), Object::Reference(nid));
                    }
                }
                _ => {}
            },
            // Lenient: a key that targets an excluded page (a non-terminal
            // field carrying a stale /P, an /A action into a dropped page)
            // costs the key, never the field and never its kept widgets.
            _ => {
                if let Some(cv) = deep_copy_pruned(src, v, out, id_map, ctx) {
                    nd.set(k.clone(), cv);
                }
            }
        }
    }

    if kid_count > 0 && kids_kept == 0 {
        // Every widget of this field sat on an excluded page: the field has
        // nothing left to be attached to. Undo the tentative mapping and let
        // the caller drop it (from a parent's /Kids, or /AcroForm /Fields).
        // The reserved id is simply left unset — nothing references it.
        id_map.remove(&field_id);
        return None;
    }
    out.set_object(new_id, Object::Dictionary(nd));
    Some(new_id)
}

/// Copy one /Kids entry of a field: a leaf widget annotation (kept iff its page
/// is), or an intermediate field node (recursed into, so IT is pruned partially
/// rather than dropped whole). Returns None to prune the entry.
fn copy_field_kid(
    src: &Document, kid: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    let Ok(kid_id) = kid.as_reference() else {
        // Direct dictionary kid (not legal for widgets, which must be indirect
        // to be referenced from /Annots, but copy it rather than lose it).
        return deep_copy_pruned(src, kid, out, id_map, ctx);
    };
    let Ok(kd) = src.get_dictionary(kid_id) else {
        return copy_ref_pruned(src, kid_id, out, id_map, ctx);
    };
    // /Subtype marks an annotation: this kid is a widget (possibly merged with
    // its terminal field). Anything else is an intermediate field node.
    if kd.has(b"Subtype") {
        if let Some(p) = ctx.page_of_annot(src, kid_id) {
            if !ctx.keeps(p) {
                return None;
            }
        }
        return Some(Object::Reference(copy_object_id_pruned(src, kid_id, out, id_map, ctx)?));
    }
    Some(Object::Reference(copy_field_node(src, kid_id, out, id_map, ctx)?))
}

fn copy_ref_pruned(
    src: &Document, old_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    // A reference into the page tree is special. A selected page is reused via
    // its pre-registered mapping (never re-copied — that would chase /Parent).
    // Any other page-tree node (an excluded page, or an intermediate /Pages
    // node) is dropped so nothing follows /Parent/#Kids into the page tree.
    if is_page_tree_node(src, old_id) {
        if ctx.keeps(old_id) {
            return id_map.get(&old_id).map(|&n| Object::Reference(n));
        }
        return None;
    }
    Some(Object::Reference(copy_object_id_pruned(src, old_id, out, id_map, ctx)?))
}

fn copy_object_id_pruned(
    src: &Document, old_id: ObjectId, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<ObjectId> {
    if let Some(&new_id) = id_map.get(&old_id) {
        return Some(new_id);
    }
    let new_id = out.new_object_id();
    id_map.insert(old_id, new_id); // before recursing: breaks reference cycles
    match src.get_object(old_id) {
        Ok(obj) => match deep_copy_pruned(src, obj, out, id_map, ctx) {
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
/// dangling/inconsistent tagging tree. Known limitation. For a multi-input
/// merge these same tables are combined instead — see `CatalogMerger`.
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

/// Catalog keys that are a single document-wide setting rather than a table of
/// content: there is nothing to combine, so a multi-input merge takes the first
/// input's (`/Lang` is handled separately — see `CatalogMerger::add_source`).
const VIEWER_INTENT_KEYS: &[&[u8]] = &[b"ViewerPreferences", b"PageLayout", b"PageMode"];

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
    src: &Document, id_map: &mut HashMap<ObjectId, ObjectId>, out: &mut Document, ctx: &PruneCtx,
) -> Vec<(Vec<u8>, Object)> {
    let mut carried = Vec::new();
    let Some(catalog) = source_catalog(src) else {
        return carried;
    };
    for &key in CARRIED_CATALOG_KEYS {
        if let Ok(val) = catalog.get(key) {
            if let Some(copied) = copy_catalog_value(src, key, val, out, id_map, ctx) {
                carried.push((key.to_vec(), copied));
            }
        }
    }
    carried
}

/// The source document's catalog dictionary, if the trailer names a usable one.
fn source_catalog(src: &Document) -> Option<&Dictionary> {
    let root = src.trailer.get(b"Root").and_then(Object::as_reference).ok()?;
    src.get_dictionary(root).ok()
}

/// Copy one carried catalog entry, pruning page-tree references per table shape.
/// Returns None to drop the whole key (e.g. a table that itself resolves to an
/// excluded page, which should not happen for the keys we carry).
fn copy_catalog_value(
    src: &Document, key: &[u8], val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    match key {
        // Name-tree dictionary (/Dests, /AP, ...). Only /Dests holds page
        // destinations; the others simply never prune.
        b"Names" => copy_names_dict(src, val, out, id_map, ctx),
        // Catalog /Dests: a flat dictionary mapping name -> destination.
        b"Dests" => copy_dests_dict(src, val, out, id_map, ctx),
        b"AcroForm" => copy_acroform(src, val, out, id_map, ctx),
        // /OCProperties, /Lang, /ViewerPreferences, /PageLayout, /PageMode do
        // not reference pages; a generic pruned copy is a no-op wrt pruning.
        _ => deep_copy_pruned(src, val, out, id_map, ctx),
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
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    let Some((indirect, d)) = resolve_catalog_dict(src, val) else {
        return deep_copy_pruned(src, val, out, id_map, ctx);
    };
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        // A tree that kept nothing loses its key entirely rather than being
        // emitted as an empty root.
        if let Some(node) = copy_name_tree(src, v, out, id_map, ctx, true, 0) {
            nd.set(k.clone(), node.object);
        }
    }
    if nd.is_empty() {
        return None;
    }
    Some(place_catalog_dict(out, indirect, nd))
}

/// Depth bound for the recursive name-tree walks. A real tree is a handful of
/// levels deep (nodes hold many names each); a /Kids chain deeper than this is
/// malformed or hostile — a /Kids cycle would otherwise recurse until the stack
/// gives out. Bailing costs the entries below the bound in a file that is
/// already broken.
const NAME_TREE_MAX_DEPTH: u32 = 64;

/// A rebuilt name-tree node plus the extent of the names that survived under it.
struct NameTreeNode {
    object: Object,
    /// Least and greatest surviving name in this subtree, by byte order.
    first: Vec<u8>,
    last: Vec<u8>,
}

/// Copy a name-tree node, dropping /Names pairs whose destination targets an
/// excluded page and recursing into /Kids. Returns None when the subtree kept
/// nothing, so the caller PRUNES that child instead of leaving an empty node
/// behind (a reader that trusts an empty child's /Limits would look for names
/// in a node that no longer has any).
///
/// /Limits is REQUIRED on every non-root node (ISO 32000 7.9.6) — it is what a
/// reader uses to pick the child that could contain a name, so a rebuilt tree
/// without it (the previous behavior) can leave surviving destinations
/// unresolvable. It is RECOMPUTED here from what actually survived: [first last]
/// of the subtree, with names ordered by byte value as the spec requires. The
/// root carries no /Limits.
///
/// A node that is not a dictionary at all is malformed beyond what a name tree
/// can express (it holds no name we could account for): it is dropped too.
fn copy_name_tree(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx, is_root: bool, depth: u32,
) -> Option<NameTreeNode> {
    let (indirect, d) = resolve_catalog_dict(src, val)?;
    if depth > NAME_TREE_MAX_DEPTH {
        return None;
    }
    let mut nd = Dictionary::new();
    let mut first: Option<Vec<u8>> = None;
    let mut last: Option<Vec<u8>> = None;
    let mut extend = |name: &[u8]| {
        if first.as_deref().is_none_or(|f| name < f) {
            first = Some(name.to_vec());
        }
        if last.as_deref().is_none_or(|l| name > l) {
            last = Some(name.to_vec());
        }
    };

    if let Some(names) = dict_array(src, &d, b"Names") {
        // Flat [key1 value1 key2 value2 ...]; keep a pair only if its value
        // survives pruning (a destination on a kept page).
        let mut arr = Vec::new();
        let mut i = 0;
        while i + 1 < names.len() {
            if let Some(cv) = deep_copy_pruned(src, &names[i + 1], out, id_map, ctx) {
                if let Ok(name) = names[i].as_str() {
                    extend(name);
                }
                arr.push(names[i].clone());
                arr.push(cv);
            }
            i += 2;
        }
        if !arr.is_empty() {
            nd.set("Names", Object::Array(arr));
        }
    }
    if let Some(kids) = dict_array(src, &d, b"Kids") {
        let mut new_kids = Vec::new();
        for kid in kids {
            if let Some(node) = copy_name_tree(src, kid, out, id_map, ctx, false, depth + 1) {
                extend(&node.first);
                extend(&node.last);
                new_kids.push(node.object);
            }
        }
        if !new_kids.is_empty() {
            nd.set("Kids", Object::Array(new_kids));
        }
    }

    // Nothing under this node survived: prune it.
    let (first, last) = (first?, last?);
    if !is_root {
        nd.set("Limits", name_limits(&first, &last));
    }
    Some(NameTreeNode { object: place_catalog_dict(out, indirect, nd), first, last })
}

/// The /Limits array of a name-tree node: [least greatest], as strings (name
/// trees are keyed by strings — ISO 32000 7.9.6).
fn name_limits(first: &[u8], last: &[u8]) -> Object {
    Object::Array(vec![
        Object::String(first.to_vec(), StringFormat::Literal),
        Object::String(last.to_vec(), StringFormat::Literal),
    ])
}

/// Copy the catalog /Dests dictionary (name -> destination), dropping any named
/// destination whose target page is not in the selected set. None if nothing
/// survived, so the key is dropped rather than emitted empty.
fn copy_dests_dict(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    let Some((indirect, d)) = resolve_catalog_dict(src, val) else {
        return deep_copy_pruned(src, val, out, id_map, ctx);
    };
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if let Some(cv) = deep_copy_pruned(src, v, out, id_map, ctx) {
            nd.set(k.clone(), cv);
        }
        // else: destination points at an excluded page -> drop this name.
    }
    if nd.is_empty() {
        return None;
    }
    Some(place_catalog_dict(out, indirect, nd))
}

/// Copy /AcroForm. Each /Fields entry is copied PARTIALLY (`copy_field_node`):
/// a field spanning kept and excluded pages keeps its kept widgets and loses
/// only the /Kids that sat on excluded pages. A field left with no surviving
/// kid — or a terminal field whose own page went — is dropped from /Fields;
/// it has no widget left to be attached to, so nothing on the kept pages
/// references it.
fn copy_acroform(
    src: &Document, val: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    let (indirect, d) = resolve_catalog_dict(src, val)?;
    let mut nd = Dictionary::new();
    for (k, v) in d.iter() {
        if k == b"Fields" {
            if let Some(fields) = dict_array(src, &d, b"Fields") {
                let kept: Vec<Object> = fields
                    .iter()
                    .filter_map(|f| copy_form_field(src, f, out, id_map, ctx))
                    .collect();
                nd.set(k.clone(), Object::Array(kept));
                continue;
            }
        }
        match deep_copy_pruned(src, v, out, id_map, ctx) {
            Some(cv) => {
                nd.set(k.clone(), cv);
            }
            None => {}
        }
    }
    Some(place_catalog_dict(out, indirect, nd))
}

/// Copy one /AcroForm /Fields entry (a root field). None to drop it.
fn copy_form_field(
    src: &Document, field: &Object, out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Option<Object> {
    match field.as_reference() {
        Ok(id) => Some(Object::Reference(copy_field_node(src, id, out, id_map, ctx)?)),
        // A direct field dictionary cannot own indirect widgets; copy as-is.
        Err(_) => deep_copy_pruned(src, field, out, id_map, ctx),
    }
}

// ---- multi-input catalog merging (finding 1) --------------------------------

/// Accumulates the document-level tables of a MULTI-INPUT merge so the merged
/// document keeps what its inputs registered above the page tree: interactive
/// fields (/AcroForm /Fields), optional content and its visibility
/// (/OCProperties), and named destinations (/Names, /Dests).
///
/// Everything held here is already copied into the output document (through the
/// contributing input's id_map), i.e. output-space.
///
/// Where combining is well defined, we combine: /Fields concatenate, /OCGs
/// union, destination names union with collisions suffixed and every reference
/// to a renamed one rewritten. Where it is not, `add_source` fails the merge
/// with `UnmergeableCatalog` rather than dropping the table: losing a form or a
/// document's layers is not something the user should have to discover later.
#[derive(Default)]
struct CatalogMerger {
    form: Option<FormAccum>,
    oc: Option<OcAccum>,
    /// Merged /Names /Dests and catalog /Dests entries.
    tree_dests: NameEntries,
    flat_dests: NameEntries,
    /// Destination names already taken. The two dest tables share ONE
    /// namespace: a /Dest naming a destination may be looked up in either, so a
    /// name must mean the same thing whichever table ends up holding it.
    dest_names: HashSet<Vec<u8>>,
    /// The other /Names subtrees (/EmbeddedFiles, /JavaScript, ...), each with
    /// its own name namespace.
    name_trees: BTreeMap<Vec<u8>, NameSpace>,
    /// The distinct /Lang values seen, None for an input that declares none.
    langs: HashSet<Option<Vec<u8>>>,
    /// Viewer-intent keys, from the first input that has each.
    viewer: Dictionary,
    /// Fully-qualified /AcroForm field names taken so far. A name two inputs both
    /// use makes an ambiguous form namespace (the same fully qualified name is
    /// one field per ISO 32000 12.7.3.2, but the two came from independent
    /// documents) — the merge is refused rather than silently misrouted.
    field_names: HashSet<Vec<u8>>,
    /// Any input carried a JavaScript action (finding 5): if a destination is
    /// also renamed for a collision, JS that names dests by opaque string cannot
    /// be rewritten to match, so the merge is refused.
    has_javascript: bool,
    /// A destination was renamed to resolve a name collision (see `has_javascript`).
    dest_renamed: bool,
}

/// Name-tree entries flattened out of their tree: (name, value), unordered.
type NameEntries = Vec<(Vec<u8>, Object)>;

/// One /Names subtree's merged entries and the names it has taken.
#[derive(Default)]
struct NameSpace {
    entries: NameEntries,
    taken: HashSet<Vec<u8>>,
}

/// Which input of a merge is being folded in, and the output id watermark from
/// just before it contributed anything (see `rewrite_dest_names`).
#[derive(Clone, Copy)]
struct InputMark {
    /// 0-based position in the input list.
    index: usize,
    id_floor: u32,
}

#[derive(Default)]
struct FormAccum {
    fields: Vec<Object>,
    need_appearances: bool,
    sig_flags: i64,
    dr: Option<Dictionary>,
    /// /DA, /Q, /CO ...: from the first input that has each.
    extra: Dictionary,
    /// The first form input's default appearance (/DA) and quadding (/Q), kept to
    /// compare against later inputs (finding 3): a later input that disagrees has
    /// no common default, so the merge is refused rather than clobbering one.
    da: Option<Vec<u8>>,
    q: Option<i64>,
}

#[derive(Default)]
struct OcAccum {
    ocgs: Vec<Object>,
    seen: HashSet<ObjectId>,
    off: Vec<Object>,
    order: Vec<Object>,
    rb_groups: Vec<Object>,
    usage: Vec<Object>,
    locked: Vec<Object>,
    /// /Intent, /ListMode of the merged default config, first input wins.
    extra: Dictionary,
}

impl CatalogMerger {
    /// Fold one input's catalog into the accumulator. Must run AFTER that
    /// input's pages are copied, so the tables' references (a destination's
    /// page, a field's widgets) land on the copied pages. `id_floor` is
    /// `out.max_id` from before this input contributed anything.
    fn add_source(
        &mut self, src: &Document, out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>,
        ctx: &PruneCtx, mark: InputMark,
    ) -> Result<(), PageDocError> {
        let Some(catalog) = source_catalog(src) else {
            return Ok(());
        };
        if let Ok(val) = catalog.get(b"AcroForm") {
            self.add_acroform(src, val, out, id_map, ctx)?;
        }
        if let Ok(val) = catalog.get(b"OCProperties") {
            self.add_oc_properties(src, val, out, id_map, ctx)?;
        }
        // Finding 5: note JS before merging destinations, so a rename in any input
        // combined with JS in any input can be refused at `finish`.
        self.has_javascript |= source_has_javascript(src);
        self.add_destinations(src, catalog, out, id_map, ctx, mark)?;

        // /Lang labels the WHOLE document. Inputs that disagree (or one that
        // says nothing) have no common answer, so rather than mislabel another
        // input's pages with the first input's language we keep it only if all
        // inputs agree. Dropping it costs a default hint, not content: it is a
        // fallback for content that does not declare its own language.
        self.langs.insert(catalog.get(b"Lang").and_then(Object::as_str).ok().map(<[u8]>::to_vec));
        for &key in VIEWER_INTENT_KEYS {
            if self.viewer.has(key) {
                continue;
            }
            if let Ok(val) = catalog.get(key) {
                if let Some(copied) = deep_copy_pruned(src, val, out, id_map, ctx) {
                    self.viewer.set(key.to_vec(), copied);
                }
            }
        }
        Ok(())
    }

    /// Concatenate one input's /AcroForm /Fields onto the merged form.
    ///
    /// Two independent inputs that both use a fully-qualified field name (both
    /// `total`) cannot be merged: per ISO 32000 12.7.3.2 that name is ONE field,
    /// so concatenating both roots makes an ambiguous namespace whose value is
    /// undefined, and renaming a field would break the /JavaScript or FDF that
    /// names it. So a cross-input name collision refuses the merge (finding 2);
    /// non-colliding fields still concatenate and stay interactive.
    ///
    /// The calculation order (/CO) and default appearance (/DA//Q) that live on
    /// the form as a whole are likewise refused when later inputs disagree
    /// (finding 3): there is no single order or default that means what each
    /// input meant, and taking the first input's would silently clobber the rest.
    fn add_acroform(
        &mut self, src: &Document, val: &Object, out: &mut Document,
        id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
    ) -> Result<(), PageDocError> {
        let Some((_, d)) = resolve_catalog_dict(src, val) else {
            return Ok(());
        };
        // An XFA form is an XML packet describing THIS document's form as a
        // whole; two of them do not compose into one, and the packet — not the
        // /Fields — is what an XFA reader renders. Refuse rather than emit a
        // document whose form silently loses half its inputs.
        if d.has(b"XFA") {
            return Err(PageDocError::UnmergeableCatalog("an XFA form (/AcroForm /XFA)"));
        }

        // Finding 2: a fully-qualified field name shared with an EARLIER input is
        // unmergeable. Names within THIS input collide only with themselves (per
        // spec they are one field already), so check against the accumulated set
        // before folding this input's names in.
        let incoming = collect_field_names(src, &d);
        if incoming.iter().any(|n| self.field_names.contains(n)) {
            return Err(PageDocError::UnmergeableCatalog("AcroForm fields with duplicate names"));
        }
        self.field_names.extend(incoming);

        let mut fields = Vec::new();
        if let Some(list) = dict_array(src, &d, b"Fields") {
            for f in list {
                if let Some(copied) = copy_form_field(src, f, out, id_map, ctx) {
                    fields.push(copied);
                }
            }
        }
        let dr = d
            .get(b"DR")
            .ok()
            .and_then(|v| deep_copy_pruned(src, v, out, id_map, ctx))
            .and_then(|v| resolve_out_dict(out, &v));

        // Finding 3: /CO, /DA, /Q are document-wide form settings. The first form
        // input sets the baseline; a later input that carries /CO, or a /DA//Q
        // that disagrees, has no common answer -> refuse.
        let is_first_form = self.form.is_none();
        let src_da = d.get(b"DA").and_then(Object::as_str).ok().map(<[u8]>::to_vec);
        let src_q = d.get(b"Q").and_then(Object::as_i64).ok();
        if !is_first_form {
            let acc = self.form.as_ref().expect("form set once seen");
            // Round-5 finding 2: compare EFFECTIVE defaults symmetrically. An absent /Q
            // means the spec default 0 (ISO 32000 12.7.4.3), an absent /DA means
            // no form-wide default (empty). So "first input /Q 1, later omits /Q"
            // is a conflict: keeping /Q 1 would silently re-justify the later
            // input's fields that relied on the implicit default 0. Only two
            // forms whose effective /Q and /DA agree (both equal, or both the
            // default) merge; any disagreement — in either direction — refuses.
            let q_conflict = src_q.unwrap_or(0) != acc.q.unwrap_or(0);
            let da_conflict = src_da.clone().unwrap_or_default() != acc.da.clone().unwrap_or_default();
            if d.has(b"CO") || da_conflict || q_conflict {
                return Err(PageDocError::UnmergeableCatalog(
                    "AcroForm calculation order / default appearance cannot be merged",
                ));
            }
        }

        let acc = self.form.get_or_insert_with(FormAccum::default);
        acc.fields.extend(fields);
        if is_first_form {
            acc.da = src_da;
            acc.q = src_q;
        }
        for (k, v) in d.iter() {
            match k.as_slice() {
                // Handled above / below.
                b"Fields" | b"DR" | b"XFA" => {}
                // Any input needing appearances makes the merged form need them.
                b"NeedAppearances" => acc.need_appearances |= v.as_bool().unwrap_or(false),
                // Flags are a bit set: the union is the honest answer.
                b"SigFlags" => acc.sig_flags |= v.as_i64().unwrap_or(0),
                // /CO, /DA, /Q and everything else: first form input wins (later
                // inputs that disagree were already refused above).
                _ => {
                    if !acc.extra.has(k) {
                        if let Some(copied) = deep_copy_pruned(src, v, out, id_map, ctx) {
                            acc.extra.set(k.clone(), copied);
                        }
                    }
                }
            }
        }
        if let Some(dr) = dr {
            match acc.dr {
                None => acc.dr = Some(dr),
                Some(ref mut cur) => merge_resource_dict(out, cur, &dr),
            }
        }
        Ok(())
    }

    /// Union one input's optional-content groups and fold its default
    /// configuration into the merged one.
    fn add_oc_properties(
        &mut self, src: &Document, val: &Object, out: &mut Document,
        id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
    ) -> Result<(), PageDocError> {
        let Some((_, d)) = resolve_catalog_dict(src, val) else {
            return Ok(());
        };
        // An alternate configuration states every OCG's state for ONE document;
        // input A's configs say nothing about input B's groups, so no combined
        // /Configs preserves what either input meant by them.
        if dict_array(src, &d, b"Configs").is_some_and(|c| !c.is_empty()) {
            return Err(PageDocError::UnmergeableCatalog(
                "alternate optional-content configurations (/OCProperties /Configs)",
            ));
        }

        let mut ocgs = Vec::new();
        if let Some(list) = dict_array(src, &d, b"OCGs") {
            for g in list {
                if let Some(copied) = deep_copy_pruned(src, g, out, id_map, ctx) {
                    ocgs.push(copied);
                }
            }
        }
        let cfg = d
            .get(b"D")
            .ok()
            .and_then(|v| resolve_catalog_dict(src, v))
            .map_or_else(Dictionary::default, |(_, c)| c);
        let base_off = cfg.get(b"BaseState").and_then(Object::as_name).is_ok_and(|n| n == b"OFF");
        let on = copy_oc_list(src, &cfg, b"ON", out, id_map, ctx);
        let off = copy_oc_list(src, &cfg, b"OFF", out, id_map, ctx);
        let order = copy_oc_list(src, &cfg, b"Order", out, id_map, ctx);
        let rb_groups = copy_oc_list(src, &cfg, b"RBGroups", out, id_map, ctx);
        let usage = copy_oc_list(src, &cfg, b"AS", out, id_map, ctx);
        let locked = copy_oc_list(src, &cfg, b"Locked", out, id_map, ctx);

        let acc = self.oc.get_or_insert_with(OcAccum::default);
        for g in &ocgs {
            match g.as_reference() {
                // Ids are unique per output, so this only dedupes an input that
                // lists the same group twice.
                Ok(id) if !acc.seen.insert(id) => {}
                _ => acc.ocgs.push(g.clone()),
            }
        }
        // Visibility: the merged config states no /BaseState, i.e. the default
        // /ON, and lists every initially-hidden group in /OFF. An input whose
        // own config said /BaseState /OFF meant "hidden except /ON" — the same
        // set of hidden groups, written the other way round.
        if base_off {
            let on_ids: HashSet<ObjectId> = on.iter().filter_map(|o| o.as_reference().ok()).collect();
            for g in &ocgs {
                match g.as_reference() {
                    Ok(id) if on_ids.contains(&id) => {}
                    _ => acc.off.push(g.clone()),
                }
            }
        } else {
            acc.off.extend(off);
        }
        // /Order is what the layers panel lists; a group missing from it is not
        // shown there at all. So an input that states no order of its own
        // contributes its groups flat instead of disappearing from a panel
        // built out of another input's order.
        if order.is_empty() {
            acc.order.extend(ocgs);
        } else {
            acc.order.extend(order);
        }
        acc.rb_groups.extend(rb_groups);
        acc.usage.extend(usage);
        acc.locked.extend(locked);
        for &key in &[b"Intent".as_slice(), b"ListMode"] {
            if acc.extra.has(key) {
                continue;
            }
            if let Ok(v) = cfg.get(key) {
                if let Some(copied) = deep_copy_pruned(src, v, out, id_map, ctx) {
                    acc.extra.set(key.to_vec(), copied);
                }
            }
        }
        Ok(())
    }

    /// Merge one input's two destination tables. A name another input already
    /// took is suffixed, and every reference to the renamed destination in THIS
    /// input's copied objects is rewritten, so its links keep pointing where
    /// they did while the other input's keep their names.
    fn add_destinations(
        &mut self, src: &Document, catalog: &Dictionary, out: &mut Document,
        id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx, mark: InputMark,
    ) -> Result<(), PageDocError> {
        let mut tree_dests = NameEntries::new();
        let mut other_trees: Vec<(Vec<u8>, NameEntries)> = Vec::new();
        if let Some((_, names)) = catalog.get(b"Names").ok().and_then(|v| resolve_catalog_dict(src, v)) {
            for (k, v) in names.iter() {
                let mut flat = Vec::new();
                flatten_name_tree(src, v, out, id_map, ctx, 0, &mut flat);
                if k == b"Dests" {
                    tree_dests = flat;
                } else {
                    other_trees.push((k.clone(), flat));
                }
            }
        }
        let mut flat_dests = Vec::new();
        if let Some((_, dests)) = catalog.get(b"Dests").ok().and_then(|v| resolve_catalog_dict(src, v)) {
            for (k, v) in dests.iter() {
                if let Some(copied) = deep_copy_pruned(src, v, out, id_map, ctx) {
                    flat_dests.push((k.clone(), copied));
                }
            }
        }

        // One rename map for both tables: within an input a name means one
        // destination whichever table it was found in, so it must be renamed
        // the same way in both.
        let mut renames: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for (name, _) in tree_dests.iter().chain(flat_dests.iter()) {
            if !renames.contains_key(name) {
                let taken = alloc_name(&mut self.dest_names, name, mark.index);
                renames.insert(name.clone(), taken);
            }
        }
        for (name, value) in tree_dests {
            self.tree_dests.push((renames[&name].clone(), value));
        }
        for (name, value) in flat_dests {
            self.flat_dests.push((renames[&name].clone(), value));
        }
        // The other subtrees (/EmbeddedFiles, /JavaScript, ...) are keyed by
        // names that are labels, not link targets: nothing inside the document
        // resolves them by lookup, so a suffix on collision usually loses
        // nothing. /EmbeddedFiles needs one extra check (round-5 finding 3): a GoToE
        // action can select an embedded file by this exact name-tree key
        // (/T ... /N), and if THIS input carries such an action for a colliding
        // name, suffixing the file would leave the action resolving to the OTHER
        // input's attachment. So a collision refuses ONLY when a GoToE in this
        // input targets the collided name; otherwise both attachments are kept,
        // the later one suffixed. (A GoToE in an already-merged input names its
        // own, un-renamed file and is unaffected — the first to take a name keeps
        // it, only the later collider is renamed.)
        for (key, flat) in other_trees {
            if key.as_slice() == b"EmbeddedFiles" {
                if let Some(taken) = self.name_trees.get(key.as_slice()).map(|ns| &ns.taken) {
                    let collisions: Vec<&[u8]> = flat
                        .iter()
                        .map(|(name, _)| name.as_slice())
                        .filter(|name| taken.contains(*name))
                        .collect();
                    if !collisions.is_empty() {
                        let targeted = gotoe_target_names(src);
                        if collisions.iter().any(|name| targeted.contains(*name)) {
                            return Err(PageDocError::UnmergeableCatalog(
                                "colliding embedded-file names referenced by GoToE",
                            ));
                        }
                    }
                }
            }
            let ns = self.name_trees.entry(key).or_default();
            for (name, value) in flat {
                let taken = alloc_name(&mut ns.taken, &name, mark.index);
                ns.entries.push((taken, value));
            }
        }

        renames.retain(|old, new| old != new);
        if !renames.is_empty() {
            // Finding 5: record that a destination was renamed. If any input also
            // carries JavaScript (which names dests by opaque string we cannot
            // rewrite), `finish` refuses the merge.
            self.dest_renamed = true;
            rewrite_dest_names(out, mark.id_floor, &renames);
        }
        Ok(())
    }

    /// Emit the merged tables as catalog entries.
    fn finish(self, out: &mut Document) -> Result<Vec<(Vec<u8>, Object)>, PageDocError> {
        // Finding 5: a renamed destination plus JavaScript anywhere is
        // unmergeable — JS references named destinations by opaque string
        // (`this.gotoNamedDest("toc")`) that we cannot follow and rewrite, so a
        // rename would silently misroute it. Refuse rather than misbehave.
        if self.dest_renamed && self.has_javascript {
            return Err(PageDocError::UnmergeableCatalog(
                "named-destination collision with JavaScript present",
            ));
        }
        let mut carried = Vec::new();
        if let Some(form) = self.form {
            let mut d = form.extra;
            d.set("Fields", Object::Array(form.fields));
            if form.need_appearances {
                d.set("NeedAppearances", true);
            }
            if form.sig_flags != 0 {
                d.set("SigFlags", form.sig_flags);
            }
            if let Some(dr) = form.dr {
                d.set("DR", Object::Dictionary(dr));
            }
            let id = out.add_object(Object::Dictionary(d));
            carried.push((b"AcroForm".to_vec(), Object::Reference(id)));
        }
        if let Some(oc) = self.oc {
            let mut cfg = oc.extra;
            cfg.set("Name", Object::String(b"Default".to_vec(), StringFormat::Literal));
            for (key, list) in [
                (&b"OFF"[..], oc.off),
                (&b"Order"[..], oc.order),
                (&b"RBGroups"[..], oc.rb_groups),
                (&b"AS"[..], oc.usage),
                (&b"Locked"[..], oc.locked),
            ] {
                if !list.is_empty() {
                    cfg.set(key.to_vec(), Object::Array(list));
                }
            }
            let mut d = Dictionary::new();
            d.set("OCGs", Object::Array(oc.ocgs));
            d.set("D", Object::Dictionary(cfg));
            let id = out.add_object(Object::Dictionary(d));
            carried.push((b"OCProperties".to_vec(), Object::Reference(id)));
        }

        let mut names = Dictionary::new();
        if !self.tree_dests.is_empty() {
            names.set("Dests", build_name_tree_root(out, self.tree_dests));
        }
        for (key, ns) in self.name_trees {
            if !ns.entries.is_empty() {
                names.set(key, build_name_tree_root(out, ns.entries));
            }
        }
        if !names.is_empty() {
            let id = out.add_object(Object::Dictionary(names));
            carried.push((b"Names".to_vec(), Object::Reference(id)));
        }
        if !self.flat_dests.is_empty() {
            let mut d = Dictionary::new();
            for (name, value) in self.flat_dests {
                d.set(name, value);
            }
            let id = out.add_object(Object::Dictionary(d));
            carried.push((b"Dests".to_vec(), Object::Reference(id)));
        }

        if self.langs.len() == 1 {
            if let Some(Some(lang)) = self.langs.into_iter().next() {
                carried.push((b"Lang".to_vec(), Object::String(lang, StringFormat::Literal)));
            }
        }
        carried.extend(self.viewer);
        Ok(carried)
    }
}

/// Depth bound for the /AcroForm field-tree walk that collects fully-qualified
/// names — a /Kids cycle or a pathologically deep tree would otherwise recurse
/// until the stack gives out. A real field tree is a handful of levels deep.
const FIELD_TREE_MAX_DEPTH: u32 = 64;

/// Collect the fully-qualified names (ISO 32000 12.7.3.2: partial /T names of a
/// field and its ancestors joined by '.') of every named node in one input's
/// /AcroForm /Fields, so a merge can detect a name two inputs both use.
fn collect_field_names(src: &Document, acroform: &Dictionary) -> HashSet<Vec<u8>> {
    let mut names = HashSet::new();
    if let Some(fields) = dict_array(src, acroform, b"Fields") {
        for f in fields {
            collect_field_names_rec(src, f, &[], &mut names, 0);
        }
    }
    names
}

fn collect_field_names_rec(
    src: &Document, field: &Object, prefix: &[u8], names: &mut HashSet<Vec<u8>>, depth: u32,
) {
    if depth > FIELD_TREE_MAX_DEPTH {
        return;
    }
    let Ok(id) = field.as_reference() else {
        return;
    };
    let Ok(d) = src.get_dictionary(id) else {
        return;
    };
    // A pure widget annotation (a /Subtype with no /T of its own) is not a field
    // and contributes no name; a node with /T extends the qualified name.
    //
    // Round-5 finding 1: decode /T before folding it in. A partial field name is
    // a text string, so `(total)` and UTF-16BE `<FEFF0074006F00740061006C>` are the
    // SAME name; comparing raw bytes would miss that collision and concatenate
    // both roots into an ambiguous namespace. `decode_text_string` (the helper
    // metadata.rs uses) normalizes PDFDocEncoding / UTF-16BE / UTF-8 to one form.
    let fqn = d.get(b"T").ok().and_then(|t| decode_text_string(t).ok()).map(|t| {
        let mut n = prefix.to_vec();
        if !n.is_empty() {
            n.push(b'.');
        }
        n.extend_from_slice(t.as_bytes());
        n
    });
    if let Some(ref n) = fqn {
        names.insert(n.clone());
    }
    let child_prefix = fqn.as_deref().unwrap_or(prefix);
    if let Some(kids) = dict_array(src, d, b"Kids") {
        for kid in kids {
            // Only recurse into field kids, not the leaf widget annotations that
            // share the terminal field's page (they carry no /T of their own).
            let is_widget = kid
                .as_reference()
                .ok()
                .and_then(|kid_id| src.get_dictionary(kid_id).ok())
                .is_some_and(|kd| kd.has(b"Subtype") && !kd.has(b"T"));
            if !is_widget {
                collect_field_names_rec(src, kid, child_prefix, names, depth + 1);
            }
        }
    }
}

/// Whether one input carries any JavaScript action (finding 5): document-level
/// named JavaScript (/Names /JavaScript), or a /S /JavaScript action — or a
/// /S /Rendition action with a /JS entry (round-5 finding 4) — anywhere (/OpenAction,
/// an /AA additional-action, a field or annotation /A).
fn source_has_javascript(src: &Document) -> bool {
    if let Some(cat) = source_catalog(src) {
        if let Some((_, names)) = cat.get(b"Names").ok().and_then(|v| resolve_catalog_dict(src, v)) {
            if names.has(b"JavaScript") {
                return true;
            }
        }
    }
    src.objects.values().any(object_has_js_action)
}

fn object_has_js_action(obj: &Object) -> bool {
    match obj {
        Object::Dictionary(d) => dict_has_js_action(d),
        Object::Stream(s) => dict_has_js_action(&s.dict),
        Object::Array(a) => a.iter().any(object_has_js_action),
        _ => false,
    }
}

fn dict_has_js_action(d: &Dictionary) -> bool {
    if let Ok(s) = d.get(b"S").and_then(Object::as_name) {
        // A JavaScript action is JS outright. A rendition action (round-5 finding
        // 4) also carries JavaScript in its /JS entry (ISO 32000 12.6.4.13), so a
        // `<< /S /Rendition ... /JS (...) >>` names destinations by opaque
        // string just as a JS action does; treat it as JavaScript so a
        // destination rename with such an action present is correctly refused.
        if s == b"JavaScript" || (s == b"Rendition" && d.has(b"JS")) {
            return true;
        }
    }
    d.iter().any(|(_, v)| object_has_js_action(v))
}

/// Merge one input's /AcroForm /DR (the resources a field's /DA names) into the
/// accumulated one. Resource names are a per-document namespace, so a name two
/// inputs both define can keep only one binding: first seen wins, the same rule
/// the engine applies to every other resource-name collision. Widgets carry
/// their own /AP streams, so at worst a later input's field regenerates its
/// appearance with the first input's same-named font.
fn merge_resource_dict(out: &Document, dst: &mut Dictionary, add: &Dictionary) {
    for (k, v) in add.iter() {
        let Some(cur) = dst.get(k).ok().cloned() else {
            dst.set(k.clone(), v.clone());
            continue;
        };
        // Both inputs define this resource category (/Font, /XObject, ...):
        // union what is inside it, first input still winning per name.
        if let (Some(mut merged), Some(add_sub)) = (resolve_out_dict(out, &cur), resolve_out_dict(out, v)) {
            for (sk, sv) in add_sub.iter() {
                if !merged.has(sk) {
                    merged.set(sk.clone(), sv.clone());
                }
            }
            dst.set(k.clone(), Object::Dictionary(merged));
        }
    }
}

/// Resolve an OUTPUT-space value (our own copies, which may be indirect) to a
/// dictionary.
fn resolve_out_dict(out: &Document, val: &Object) -> Option<Dictionary> {
    match val {
        Object::Reference(id) => out.get_dictionary(*id).ok().cloned(),
        Object::Dictionary(d) => Some(d.clone()),
        _ => None,
    }
}

/// Copy an array-valued entry of an optional-content configuration.
fn copy_oc_list(
    src: &Document, cfg: &Dictionary, key: &[u8], out: &mut Document,
    id_map: &mut HashMap<ObjectId, ObjectId>, ctx: &PruneCtx,
) -> Vec<Object> {
    let Some(items) = dict_array(src, cfg, key) else {
        return Vec::new();
    };
    items.iter().filter_map(|o| deep_copy_pruned(src, o, out, id_map, ctx)).collect()
}

/// Flatten a name tree into its surviving (name, value) pairs, so the entries of
/// several inputs can be unioned into one tree.
fn flatten_name_tree(
    src: &Document, val: &Object, out: &mut Document, id_map: &mut HashMap<ObjectId, ObjectId>,
    ctx: &PruneCtx, depth: u32, into: &mut NameEntries,
) {
    let Some((_, d)) = resolve_catalog_dict(src, val) else {
        return;
    };
    if depth > NAME_TREE_MAX_DEPTH {
        return;
    }
    if let Some(names) = dict_array(src, &d, b"Names") {
        let mut i = 0;
        while i + 1 < names.len() {
            if let Some(copied) = deep_copy_pruned(src, &names[i + 1], out, id_map, ctx) {
                if let Ok(name) = names[i].as_str() {
                    into.push((name.to_vec(), copied));
                }
            }
            i += 2;
        }
    }
    if let Some(kids) = dict_array(src, &d, b"Kids") {
        for kid in kids {
            flatten_name_tree(src, kid, out, id_map, ctx, depth + 1, into);
        }
    }
}

/// Embedded-file name-tree keys that a /GoToE action in this input selects
/// (round-5 finding 3). A GoToE names an embedded file of the current document by
/// /N of its /T target dictionary (ISO 32000 12.6.4.4); a collision on such a
/// name would misroute the action after a rename, so those names are collected
/// to refuse the merge. Only the first-level /N is a key in THIS document's
/// /EmbeddedFiles tree (a nested /T descends into another file's own tree), so
/// only that is collected — matching deeper /N against this tree would refuse
/// safe merges. Actions may be indirect objects or inline in an annotation /A,
/// so every object is scanned and each dict's inline values are walked.
fn gotoe_target_names(src: &Document) -> HashSet<Vec<u8>> {
    let mut names = HashSet::new();
    for obj in src.objects.values() {
        scan_for_gotoe(src, obj, &mut names, 0);
    }
    names
}

fn scan_for_gotoe(src: &Document, obj: &Object, names: &mut HashSet<Vec<u8>>, depth: u32) {
    if depth > NAME_TREE_MAX_DEPTH {
        return;
    }
    let d = match obj {
        Object::Dictionary(d) => d,
        Object::Stream(s) => &s.dict,
        Object::Array(a) => {
            a.iter().for_each(|o| scan_for_gotoe(src, o, names, depth + 1));
            return;
        }
        _ => return,
    };
    if d.get(b"S").and_then(Object::as_name).is_ok_and(|s| s == b"GoToE") {
        if let Some((_, target)) = d.get(b"T").ok().and_then(|t| resolve_catalog_dict(src, t)) {
            if let Ok(n) = target.get(b"N").and_then(|v| src.dereference(v).map(|(_, o)| o)).and_then(Object::as_str) {
                names.insert(n.to_vec());
            }
        }
    }
    for (_, v) in d.iter() {
        scan_for_gotoe(src, v, names, depth + 1);
    }
}

/// Emit merged name-tree entries as a single leaf ROOT node: sorted by name (a
/// name tree is ordered by byte value — ISO 32000 7.9.6) and, being the root,
/// carrying no /Limits.
fn build_name_tree_root(out: &mut Document, mut entries: NameEntries) -> Object {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut arr = Vec::with_capacity(entries.len() * 2);
    for (name, value) in entries {
        arr.push(Object::String(name, StringFormat::Literal));
        arr.push(value);
    }
    let mut d = Dictionary::new();
    d.set("Names", Object::Array(arr));
    Object::Reference(out.add_object(Object::Dictionary(d)))
}

/// Take `name` in `taken`, suffixing it until it is free. `input` is 0-based, so
/// the second input's colliding "toc" becomes "toc_2". An underscore keeps the
/// result legal both as a name-tree string and as a catalog /Dests name.
fn alloc_name(taken: &mut HashSet<Vec<u8>>, name: &[u8], input: usize) -> Vec<u8> {
    if taken.insert(name.to_vec()) {
        return name.to_vec();
    }
    let mut attempt = 0u32;
    loop {
        let suffix = if attempt == 0 {
            format!("_{}", input + 1)
        } else {
            format!("_{}_{}", input + 1, attempt)
        };
        let mut candidate = name.to_vec();
        candidate.extend_from_slice(suffix.as_bytes());
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        attempt += 1;
    }
}

/// Rewrite references to destinations renamed by collision resolution, in the
/// objects one input contributed (everything with an id above `id_floor` —
/// lopdf allocates ids by incrementing `max_id`, so that is exactly this
/// input's copies).
///
/// A destination is named by a link annotation's /Dest or a /GoTo action's /D
/// (ISO 32000 12.3.2.3, 12.6.4.2), as a string or — the PDF 1.1 form — a name.
/// A remote destination (/GoToR, /GoToE) names an entry in ANOTHER file's
/// tables and must not be touched.
fn rewrite_dest_names(out: &mut Document, id_floor: u32, renames: &HashMap<Vec<u8>, Vec<u8>>) {
    for (id, obj) in out.objects.iter_mut() {
        if id.0 > id_floor {
            rewrite_dest_names_obj(obj, renames);
        }
    }
}

fn rewrite_dest_names_obj(obj: &mut Object, renames: &HashMap<Vec<u8>, Vec<u8>>) {
    match obj {
        Object::Array(items) => items.iter_mut().for_each(|o| rewrite_dest_names_obj(o, renames)),
        Object::Dictionary(d) => rewrite_dest_names_dict(d, renames),
        Object::Stream(s) => rewrite_dest_names_dict(&mut s.dict, renames),
        _ => {}
    }
}

fn rewrite_dest_names_dict(d: &mut Dictionary, renames: &HashMap<Vec<u8>, Vec<u8>>) {
    let local_goto = d.get(b"S").and_then(Object::as_name).is_ok_and(|s| s == b"GoTo");
    for (k, v) in d.iter_mut() {
        if k == b"Dest" || (local_goto && k == b"D") {
            rewrite_dest_name_value(v, renames);
        }
        rewrite_dest_names_obj(v, renames);
    }
}

/// Rewrite a destination reference that names a renamed destination. An array
/// value is an explicit destination (no name involved) and is left alone.
fn rewrite_dest_name_value(val: &mut Object, renames: &HashMap<Vec<u8>, Vec<u8>>) {
    let renamed = match val {
        Object::String(s, fmt) => renames.get(s.as_slice()).map(|n| Object::String(n.clone(), *fmt)),
        Object::Name(n) => renames.get(n.as_slice()).map(|n| Object::Name(n.clone())),
        _ => None,
    };
    if let Some(renamed) = renamed {
        *val = renamed;
    }
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

    fn acroform_of(out: &Document) -> &Dictionary {
        let acro = catalog_of(out).get(b"AcroForm").and_then(Object::as_reference).unwrap();
        out.get_dictionary(acro).unwrap()
    }

    fn field_names(out: &Document) -> Vec<Vec<u8>> {
        acroform_of(out)
            .get(b"Fields")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                let d = out.get_dictionary(f.as_reference().unwrap()).unwrap();
                d.get(b"T").unwrap().as_str().unwrap().to_vec()
            })
            .collect()
    }

    #[test]
    fn merge_multi_input_merges_acroform_fields() {
        let mut a = build_acroform_doc(b"(a)");
        let mut b = build_acroform_doc(b"(b)");
        let pa = save_temp(&mut a, "pdfree_pagedoc_form_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_form_b.pdf");

        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        assert_eq!(out.get_pages().len(), 2, "merge must sum pages");
        // BOTH inputs' fields are registered: a widget copied from input 2 that
        // is not in /AcroForm /Fields is not an interactive field any more, and
        // losing that silently is what this replaces.
        assert_eq!(field_names(&out), vec![b"(a)".to_vec(), b"(b)".to_vec()]);
        // A merged flag is the union: input B needing appearances must not be
        // forgotten because input A did not.
        assert_eq!(acroform_of(&out).get(b"NeedAppearances").unwrap().as_bool().unwrap(), true);

        // Every field's widget is still the one on its own page.
        let kids = kids_of(&out);
        let fields = acroform_of(&out).get(b"Fields").unwrap().as_array().unwrap().to_vec();
        for (field, page) in fields.iter().zip(kids.iter()) {
            let fd = out.get_dictionary(field.as_reference().unwrap()).unwrap();
            let widget_id = fd.get(b"Kids").unwrap().as_array().unwrap()[0].as_reference().unwrap();
            let widget = out.get_dictionary(widget_id).unwrap();
            assert_eq!(widget.get(b"P").unwrap().as_reference().unwrap(), *page);
            assert_eq!(widget.get(b"Parent").unwrap().as_reference().unwrap(), field.as_reference().unwrap());
            let annots = out.get_dictionary(*page).unwrap().get(b"Annots").unwrap().as_array().unwrap();
            assert_eq!(annots[0].as_reference().unwrap(), widget_id, "widget stays on its page");
        }
        assert_no_dangling(&out);
    }

    /// A one-page doc with an /AcroForm carrying an /XFA packet.
    fn build_xfa_doc() -> Document {
        let mut doc = build_acroform_doc(b"(xfa)");
        let acro = catalog_of(&doc).get(b"AcroForm").and_then(Object::as_reference).unwrap();
        let xfa = doc.add_object(Stream::new(dictionary! {}, b"<xdp/>".to_vec()));
        doc.get_dictionary_mut(acro).unwrap().set("XFA", Object::Reference(xfa));
        doc
    }

    #[test]
    fn merge_multi_input_rejects_xfa_form() {
        let mut a = build_acroform_doc(b"(plain)");
        let mut b = build_xfa_doc();
        let pa = save_temp(&mut a, "pdfree_pagedoc_xfa_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_xfa_b.pdf");

        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        // Honest refusal: an XFA form cannot be combined, so say so instead of
        // emitting a document whose form quietly lost an input.
        assert!(matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("XFA")), "got {err:?}");
    }

    // ---- finding 1: optional content survives a multi-input merge -----------

    /// A one-page doc with one optional-content group. `hidden` puts it in the
    /// input's own /D as initially invisible, via /BaseState /OFF (the form
    /// that needs normalizing when merged with a /BaseState /ON input).
    fn build_oc_doc(marker: &[u8], hidden: bool) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, marker.to_vec()));
        let ocg = doc.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => Object::String(marker.to_vec(), lopdf::StringFormat::Literal),
        });
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
            "Resources" => dictionary! {
                "Properties" => dictionary! { "MC0" => Object::Reference(ocg) },
            },
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let config = if hidden {
            // "everything off except /ON", with nothing turned on.
            dictionary! { "BaseState" => "OFF", "ON" => Object::Array(vec![]) }
        } else {
            dictionary! { "Order" => vec![Object::Reference(ocg)] }
        };
        let ocprops = doc.add_object(dictionary! {
            "OCGs" => vec![Object::Reference(ocg)],
            "D" => config,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "OCProperties" => Object::Reference(ocprops),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn merge_multi_input_unions_ocgs_and_keeps_visibility() {
        // Input A's group is visible; input B's is hidden by /BaseState /OFF.
        let mut a = build_oc_doc(b"(a)", false);
        let mut b = build_oc_doc(b"(b)", true);
        let pa = save_temp(&mut a, "pdfree_pagedoc_oc_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_oc_b.pdf");

        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        let ocp_ref = catalog_of(&out).get(b"OCProperties").and_then(Object::as_reference).unwrap();
        let ocp = out.get_dictionary(ocp_ref).unwrap();
        let ocgs = ocp.get(b"OCGs").unwrap().as_array().unwrap();
        // Both inputs' groups are registered: an OCG missing from /OCProperties
        // /OCGs is not optional content any more (its visibility is untouchable).
        assert_eq!(ocgs.len(), 2, "both inputs' OCGs must be registered");

        let cfg = ocp.get(b"D").unwrap().as_dict().unwrap();
        assert!(cfg.get(b"BaseState").is_err(), "merged config uses the default /ON base state");
        // B's group was hidden and stays hidden; A's stays visible.
        let off = cfg.get(b"OFF").unwrap().as_array().unwrap();
        assert_eq!(off.len(), 1, "only input B's group starts hidden");
        let hidden = out.get_dictionary(off[0].as_reference().unwrap()).unwrap();
        assert_eq!(hidden.get(b"Name").unwrap().as_str().unwrap(), b"(b)");
        // Both are listed in the layers panel, including B, whose own config
        // stated no /Order.
        assert_eq!(cfg.get(b"Order").unwrap().as_array().unwrap().len(), 2);
        assert_no_dangling(&out);
    }

    #[test]
    fn merge_multi_input_rejects_alternate_oc_configs() {
        let mut a = build_oc_doc(b"(a)", false);
        let mut b = build_oc_doc(b"(b)", false);
        // Give B an alternate configuration: it describes B's layers only, so
        // there is no merged form of it that still means what B meant.
        let ocp = catalog_of(&b).get(b"OCProperties").and_then(Object::as_reference).unwrap();
        let alt = b.add_object(dictionary! { "Name" => Object::String(b"alt".to_vec(), lopdf::StringFormat::Literal) });
        b.get_dictionary_mut(ocp).unwrap().set("Configs", vec![Object::Reference(alt)]);

        let pa = save_temp(&mut a, "pdfree_pagedoc_occfg_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_occfg_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        assert!(matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("Configs")), "got {err:?}");
    }

    // ---- finding 1: colliding named destinations are renamed, not dropped ---

    /// A one-page doc with a named destination "toc" -> its page, and a link
    /// annotation that reaches it BY NAME (/A /S /GoTo /D (toc)).
    fn build_named_link_doc(marker: &[u8]) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, marker.to_vec()));
        let link = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 100.into(), 20.into()],
            "A" => dictionary! {
                "S" => "GoTo",
                "D" => Object::String(b"toc".to_vec(), lopdf::StringFormat::Literal),
            },
        });
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
            "Annots" => vec![Object::Reference(link)],
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let dests = doc.add_object(dictionary! {
            "Names" => vec![
                Object::String(b"toc".to_vec(), lopdf::StringFormat::Literal),
                Object::Array(vec![Object::Reference(page_id), "Fit".into()]),
            ],
        });
        let names = doc.add_object(dictionary! { "Dests" => Object::Reference(dests) });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    /// The merged /Names /Dests as (name, target page) pairs.
    fn merged_dests(out: &Document) -> Vec<(Vec<u8>, ObjectId)> {
        let names_ref = catalog_of(out).get(b"Names").and_then(Object::as_reference).unwrap();
        let names = out.get_dictionary(names_ref).unwrap();
        let dests_ref = names.get(b"Dests").and_then(Object::as_reference).unwrap();
        let dests = out.get_dictionary(dests_ref).unwrap();
        let arr = dests.get(b"Names").unwrap().as_array().unwrap();
        assert!(dests.get(b"Limits").is_err(), "a name-tree ROOT must not carry /Limits");
        arr.chunks(2)
            .map(|p| {
                let page = p[1].as_array().unwrap()[0].as_reference().unwrap();
                (p[0].as_str().unwrap().to_vec(), page)
            })
            .collect()
    }

    /// The name a page's /A /S /GoTo link asks for.
    fn goto_name(out: &Document, page: ObjectId) -> Vec<u8> {
        let annots = out.get_dictionary(page).unwrap().get(b"Annots").unwrap().as_array().unwrap();
        let annot = out.get_dictionary(annots[0].as_reference().unwrap()).unwrap();
        let action = annot.get(b"A").unwrap().as_dict().unwrap();
        action.get(b"D").unwrap().as_str().unwrap().to_vec()
    }

    #[test]
    fn merge_multi_input_suffixes_colliding_dest_names() {
        // Both inputs name a destination "toc". Neither may be dropped, and
        // neither input's link may end up pointing at the other's page.
        let mut a = build_named_link_doc(b"(a)");
        let mut b = build_named_link_doc(b"(b)");
        let pa = save_temp(&mut a, "pdfree_pagedoc_dest_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_dest_b.pdf");

        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        let kids = kids_of(&out);
        let dests = merged_dests(&out);
        assert_eq!(dests.len(), 2, "both destinations must survive the collision");
        // Input 1 keeps the name; input 2's is suffixed. Each still resolves to
        // the page it was written for.
        assert_eq!(dests[0], (b"toc".to_vec(), kids[0]));
        assert_eq!(dests[1], (b"toc_2".to_vec(), kids[1]));
        // ... and each input's link follows its own destination: the renamed
        // one is rewritten only in the input that was renamed.
        assert_eq!(goto_name(&out, kids[0]), b"toc".to_vec());
        assert_eq!(goto_name(&out, kids[1]), b"toc_2".to_vec());
        assert_no_dangling(&out);
    }

    // ---- finding 2: an INDIRECT /Annots array duplicates correctly ----------

    /// Like `build_annotated_page_doc`, but /Annots is an INDIRECT reference to
    /// the array — the shape a raw `as_array()` fails on.
    fn build_indirect_annots_doc() -> Document {
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
        let annots_id = doc.add_object(Object::Array(vec![Object::Reference(annot_id)]));
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
            "Annots" => Object::Reference(annots_id),
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
    fn duplicated_page_with_indirect_annots_is_independent() {
        let doc = build_indirect_annots_doc();
        let out = extract_pages(&doc, "1,1").unwrap();

        let kids = kids_of(&out);
        assert_eq!(kids.len(), 2);
        assert_ne!(kids[0], kids[1], "the two page objects must be distinct");

        // Each page owns its annots array and its annotation objects: the
        // duplicate must not reuse the canonical page's already-copied array
        // (whose annots' /P point at the FIRST copy).
        let mut annot_ids = Vec::new();
        for &kid in &kids {
            let page = out.get_dictionary(kid).unwrap();
            let annots = page.get(b"Annots").unwrap().as_array().unwrap();
            assert_eq!(annots.len(), 1);
            let annot_id = annots[0].as_reference().unwrap();
            annot_ids.push(annot_id);
            let annot = out.get_dictionary(annot_id).unwrap();
            assert_eq!(annot.get(b"P").unwrap().as_reference().unwrap(), kid, "/P must parent to own page");
            let dest = annot.get(b"Dest").unwrap().as_array().unwrap()[0].as_reference().unwrap();
            assert_eq!(dest, kid, "self /Dest must follow the duplicate");
        }
        assert_ne!(annot_ids[0], annot_ids[1], "annotations must be independent objects");
        assert_no_dangling(&out);
    }

    // ---- finding 3: rebuilt name trees carry recomputed /Limits -------------

    /// Three-page doc with a TWO-LEVEL /Dests name tree: a root with /Kids ->
    /// two leaves that each carry /Limits and /Names. Leaf A holds "a" -> p1
    /// (kept) and "b" -> p3 (excluded); leaf B holds only names on p3.
    fn build_two_level_dest_tree_doc() -> Document {
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
        let lit = |s: &[u8]| Object::String(s.to_vec(), lopdf::StringFormat::Literal);
        let leaf_a = doc.add_object(dictionary! {
            "Limits" => vec![lit(b"a"), lit(b"b")],
            "Names" => vec![
                lit(b"a"), Object::Array(vec![Object::Reference(p1), "Fit".into()]),
                lit(b"b"), Object::Array(vec![Object::Reference(p3), "Fit".into()]),
            ],
        });
        let leaf_b = doc.add_object(dictionary! {
            "Limits" => vec![lit(b"c"), lit(b"d")],
            "Names" => vec![
                lit(b"c"), Object::Array(vec![Object::Reference(p3), "Fit".into()]),
                lit(b"d"), Object::Array(vec![Object::Reference(p3), "Fit".into()]),
            ],
        });
        let root = doc.add_object(dictionary! {
            "Kids" => vec![Object::Reference(leaf_a), Object::Reference(leaf_b)],
        });
        let names_id = doc.add_object(dictionary! { "Dests" => Object::Reference(root) });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn pruned_name_tree_recomputes_limits_and_prunes_empty_kids() {
        let doc = build_two_level_dest_tree_doc();
        let out = extract_pages(&doc, "1").unwrap();
        let kept_page = kids_of(&out)[0];

        let names_ref = catalog_of(&out).get(b"Names").and_then(Object::as_reference).unwrap();
        let names = out.get_dictionary(names_ref).unwrap();
        let root_ref = names.get(b"Dests").and_then(Object::as_reference).unwrap();
        let root = out.get_dictionary(root_ref).unwrap();
        // The ROOT of a name tree must not carry /Limits (ISO 32000 7.9.6).
        assert!(root.get(b"Limits").is_err(), "root must have no /Limits");

        // Leaf B kept nothing (all its dests were on the excluded page): it is
        // pruned, not left behind as an empty child.
        let kids = root.get(b"Kids").unwrap().as_array().unwrap();
        assert_eq!(kids.len(), 1, "the child that kept no names must be pruned");

        // Leaf A kept only "a", so its /Limits is recomputed to ["a" "a"] —
        // a stale ["a" "b"] would send a reader looking for "b" into a node
        // that no longer has it, and no /Limits at all is not legal here.
        let leaf = out.get_dictionary(kids[0].as_reference().unwrap()).unwrap();
        let limits = leaf.get(b"Limits").unwrap().as_array().unwrap();
        assert_eq!(limits[0].as_str().unwrap(), b"a");
        assert_eq!(limits[1].as_str().unwrap(), b"a");

        // ... and the surviving destination still resolves to the kept page.
        let entries = leaf.get(b"Names").unwrap().as_array().unwrap();
        assert_eq!(entries.len(), 2, "only the surviving name/dest pair remains");
        assert_eq!(entries[0].as_str().unwrap(), b"a");
        assert_eq!(entries[1].as_array().unwrap()[0].as_reference().unwrap(), kept_page);

        assert_eq!(count_page_objects(&out), 1, "excluded pages must not be re-embedded");
        assert_no_dangling(&out);
    }

    // ---- finding 4: a field spanning pages keeps its surviving widgets ------

    /// Two-page doc with a radio group whose /Kids holds two widgets, placed on
    /// the (1-based) pages given. `[1, 2]` is the shape that used to lose the
    /// whole field on a split; `[2, 2]` leaves nothing behind when page 1 is
    /// the one kept.
    fn build_radio_doc(widget_pages: [usize; 2]) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let pages = [doc.new_object_id(), doc.new_object_id()];
        let field_id = doc.new_object_id();

        let widgets: Vec<ObjectId> = widget_pages
            .iter()
            .map(|&p| {
                doc.add_object(dictionary! {
                    "Type" => "Annot",
                    "Subtype" => "Widget",
                    "Rect" => vec![0.into(), 0.into(), 20.into(), 20.into()],
                    "P" => Object::Reference(pages[p - 1]),
                    "Parent" => Object::Reference(field_id),
                    "AS" => "Off",
                })
            })
            .collect();
        doc.set_object(field_id, dictionary! {
            "FT" => "Btn",
            "Ff" => 32768,  // radio button group
            "T" => Object::String(b"choice".to_vec(), lopdf::StringFormat::Literal),
            "V" => "Off",
            "Kids" => widgets.iter().map(|&w| Object::Reference(w)).collect::<Vec<_>>(),
        });
        for (i, &page) in pages.iter().enumerate() {
            let c = doc.add_object(Stream::new(dictionary! {}, b"(x)".to_vec()));
            let annots: Vec<Object> = widgets
                .iter()
                .zip(widget_pages.iter())
                .filter(|(_, p)| **p == i + 1)
                .map(|(w, _)| Object::Reference(*w))
                .collect();
            doc.set_object(page, dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference(pages_id),
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Contents" => Object::Reference(c),
                "Annots" => annots,
            });
        }
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => pages.iter().map(|&p| Object::Reference(p)).collect::<Vec<_>>(),
            "Count" => 2,
        });
        let acroform_id = doc.add_object(dictionary! { "Fields" => vec![Object::Reference(field_id)] });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "AcroForm" => Object::Reference(acroform_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn split_keeps_field_with_widgets_on_several_pages() {
        let doc = build_radio_doc([1, 2]);
        let out = extract_pages(&doc, "1").unwrap();
        let page = kids_of(&out)[0];

        // The field survives: rejecting the whole graph because ONE kid sat on
        // page 2 used to drop the group from the form entirely.
        assert_eq!(field_names(&out), vec![b"choice".to_vec()]);
        let field_id = acroform_of(&out).get(b"Fields").unwrap().as_array().unwrap()[0]
            .as_reference()
            .unwrap();
        let field = out.get_dictionary(field_id).unwrap();

        // Only the page-1 widget remains in /Kids; the page-2 one is pruned.
        let kids = field.get(b"Kids").unwrap().as_array().unwrap();
        assert_eq!(kids.len(), 1, "only the kept page's widget stays in /Kids");
        let widget_id = kids[0].as_reference().unwrap();
        let widget = out.get_dictionary(widget_id).unwrap();
        assert_eq!(widget.get(b"P").unwrap().as_reference().unwrap(), page);
        // The widget is still ON the page, and parented to the copied field:
        // following /Parent must never delete a kept page's widget.
        let annots = out.get_dictionary(page).unwrap().get(b"Annots").unwrap().as_array().unwrap();
        assert_eq!(annots.len(), 1, "the widget must stay on its page");
        assert_eq!(annots[0].as_reference().unwrap(), widget_id);
        assert_eq!(widget.get(b"Parent").unwrap().as_reference().unwrap(), field_id);

        assert_eq!(count_page_objects(&out), 1, "page 2 must not be re-embedded");
        assert_no_dangling(&out);
    }

    #[test]
    fn split_drops_field_whose_widgets_all_went() {
        // Both widgets sit on page 2; keeping page 1 leaves the field with no
        // kid to be attached to, so it is dropped from /Fields rather than left
        // registered (and pointing at widgets that are not in the document).
        let doc = build_radio_doc([2, 2]);
        let out = extract_pages(&doc, "1").unwrap();

        let fields = acroform_of(&out).get(b"Fields").unwrap().as_array().unwrap();
        assert!(fields.is_empty(), "a field with no surviving widget must be dropped");
        assert_eq!(count_page_objects(&out), 1);
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

    // ---- F1: a duplicated page's reply/popup links retarget to the dup -------

    /// One-page doc whose page carries a markup Text annotation, its Popup, and a
    /// reply Text annotation, wired together by /Popup, /Parent and /IRT. On a
    /// duplicate selection these back-links must follow the duplicate, not the
    /// canonical copy.
    fn build_reply_popup_doc() -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let markup_id = doc.new_object_id();
        let popup_id = doc.new_object_id();
        let reply_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, b"(x)".to_vec()));
        doc.set_object(markup_id, dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => vec![0.into(), 0.into(), 20.into(), 20.into()],
            "P" => Object::Reference(page_id),
            "Popup" => Object::Reference(popup_id),
        });
        doc.set_object(popup_id, dictionary! {
            "Type" => "Annot",
            "Subtype" => "Popup",
            "Rect" => vec![20.into(), 20.into(), 120.into(), 120.into()],
            "P" => Object::Reference(page_id),
            "Parent" => Object::Reference(markup_id),
        });
        doc.set_object(reply_id, dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => vec![0.into(), 0.into(), 20.into(), 20.into()],
            "P" => Object::Reference(page_id),
            "IRT" => Object::Reference(markup_id),
            "RT" => "R",
        });
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
            "Annots" => vec![
                Object::Reference(markup_id),
                Object::Reference(popup_id),
                Object::Reference(reply_id),
            ],
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
    fn duplicated_page_retargets_reply_and_popup_within_the_dup() {
        let doc = build_reply_popup_doc();
        let out = extract_pages(&doc, "1,1").unwrap();

        let kids = kids_of(&out);
        assert_eq!(kids.len(), 2);
        assert_ne!(kids[0], kids[1]);

        let mut per_page: Vec<Vec<ObjectId>> = Vec::new();
        for &kid in &kids {
            let page = out.get_dictionary(kid).unwrap();
            let annots: Vec<ObjectId> = page
                .get(b"Annots")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_reference().unwrap())
                .collect();
            assert_eq!(annots.len(), 3);
            let (markup, popup, reply) = (annots[0], annots[1], annots[2]);

            // The markup annotation's /Popup and /P belong to THIS page instance.
            let md = out.get_dictionary(markup).unwrap();
            assert_eq!(md.get(b"P").unwrap().as_reference().unwrap(), kid);
            assert_eq!(md.get(b"Popup").unwrap().as_reference().unwrap(), popup, "/Popup follows the dup");
            // The popup's /Parent points back at THIS page's markup annotation.
            let pd = out.get_dictionary(popup).unwrap();
            assert_eq!(pd.get(b"P").unwrap().as_reference().unwrap(), kid);
            assert_eq!(pd.get(b"Parent").unwrap().as_reference().unwrap(), markup, "popup /Parent follows the dup");
            // The reply's /IRT points at THIS page's markup annotation.
            let rd = out.get_dictionary(reply).unwrap();
            assert_eq!(rd.get(b"P").unwrap().as_reference().unwrap(), kid);
            assert_eq!(rd.get(b"IRT").unwrap().as_reference().unwrap(), markup, "reply /IRT follows the dup");

            per_page.push(annots);
        }
        // The two duplicates share no annotation object at all.
        for a in &per_page[0] {
            assert!(!per_page[1].contains(a), "duplicates must not share annotation objects");
        }
        assert_no_dangling(&out);
    }

    // ---- F2: two inputs with a shared field name are unmergeable -------------

    #[test]
    fn merge_multi_input_rejects_duplicate_field_names() {
        // Both inputs have a field named "(shared)": one ambiguous namespace.
        let mut a = build_acroform_doc(b"(shared)");
        let mut b = build_acroform_doc(b"(shared)");
        let pa = save_temp(&mut a, "pdfree_pagedoc_dupfield_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_dupfield_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("duplicate names")),
            "got {err:?}"
        );
    }

    #[test]
    fn merge_multi_input_rejects_duplicate_field_names_across_encodings() {
        // Finding 1: the SAME field name written two ways — the literal "(total)"
        // and the UTF-16BE <FEFF0074006F00740061006C> — is one fully-qualified
        // name. Comparing DECODED names catches the collision that a raw byte
        // compare would miss (and would otherwise concatenate into an ambiguous
        // namespace).
        let total_utf16: Vec<u8> = {
            let mut v = vec![0xFE, 0xFF];
            for ch in "total".chars() {
                v.extend_from_slice(&(ch as u16).to_be_bytes());
            }
            v
        };
        let mut a = build_acroform_doc(b"total");
        let mut b = build_acroform_doc(&total_utf16);
        let pa = save_temp(&mut a, "pdfree_pagedoc_enc_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_enc_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("duplicate names")),
            "got {err:?}"
        );
    }

    // ---- F2/F3 (round 5): a later input's /CO or disagreeing /Q//DA makes
    //      the form unmergeable ----------------------------------------------

    #[test]
    fn merge_multi_input_rejects_later_calculation_order() {
        let mut a = build_acroform_doc(b"(a)");
        let mut b = build_acroform_doc(b"(b)");
        // Give input B a /CO (calculation order): merging two orders is undefined.
        let acro = catalog_of(&b).get(b"AcroForm").and_then(Object::as_reference).unwrap();
        let field = {
            let acro_d = b.get_dictionary(acro).unwrap();
            acro_d.get(b"Fields").unwrap().as_array().unwrap()[0].as_reference().unwrap()
        };
        b.get_dictionary_mut(acro).unwrap().set("CO", vec![Object::Reference(field)]);

        let pa = save_temp(&mut a, "pdfree_pagedoc_co_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_co_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("calculation order")),
            "got {err:?}"
        );
    }

    #[test]
    fn merge_multi_input_rejects_later_omitted_quadding() {
        // Finding 2: the first form sets /Q 1; the later form omits /Q, whose
        // EFFECTIVE value is the spec default 0. Keeping /Q 1 would silently
        // re-justify the later input's fields, so the disagreement is refused
        // even though the later /Q is only implicit.
        let mut a = build_acroform_doc(b"(a)");
        let mut b = build_acroform_doc(b"(b)");
        let acro_a = catalog_of(&a).get(b"AcroForm").and_then(Object::as_reference).unwrap();
        a.get_dictionary_mut(acro_a).unwrap().set("Q", 1);
        let pa = save_temp(&mut a, "pdfree_pagedoc_q_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_q_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("default appearance")),
            "got {err:?}"
        );
    }

    #[test]
    fn merge_multi_input_merges_matching_quadding() {
        // Finding 2: two forms whose effective /Q agree (both /Q 1) merge, and the
        // shared value is kept. Guards against over-refusing safe cases.
        let mut a = build_acroform_doc(b"(a)");
        let mut b = build_acroform_doc(b"(b)");
        for doc in [&mut a, &mut b] {
            let acro = catalog_of(doc).get(b"AcroForm").and_then(Object::as_reference).unwrap();
            doc.get_dictionary_mut(acro).unwrap().set("Q", 1);
        }
        let pa = save_temp(&mut a, "pdfree_pagedoc_qm_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_qm_b.pdf");
        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert_eq!(acroform_of(&out).get(b"Q").and_then(Object::as_i64).unwrap(), 1);
        assert_eq!(field_names(&out).len(), 2, "both distinct fields merge");
    }

    // ---- F3 (round 5): colliding /EmbeddedFiles names merge unless a GoToE
    //      selects the collided name -----------------------------------------

    /// One-page doc whose catalog /Names /EmbeddedFiles names one embedded file.
    fn build_embedded_file_doc(fname: &[u8]) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, b"(x)".to_vec()));
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let ef_stream = doc.add_object(Stream::new(dictionary! { "Type" => "EmbeddedFile" }, b"data".to_vec()));
        let filespec = doc.add_object(dictionary! {
            "Type" => "Filespec",
            "F" => Object::String(fname.to_vec(), lopdf::StringFormat::Literal),
            "EF" => dictionary! { "F" => Object::Reference(ef_stream) },
        });
        let ef_tree = doc.add_object(dictionary! {
            "Names" => vec![
                Object::String(fname.to_vec(), lopdf::StringFormat::Literal),
                Object::Reference(filespec),
            ],
        });
        let names = doc.add_object(dictionary! { "EmbeddedFiles" => Object::Reference(ef_tree) });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    /// As `build_embedded_file_doc`, plus an /OpenAction GoToE that selects an
    /// embedded file by its name-tree key (`/T << /R /C /N (target) >>`).
    fn build_embedded_file_doc_with_gotoe(fname: &[u8], target: &[u8]) -> Document {
        let mut doc = build_embedded_file_doc(fname);
        let gotoe = doc.add_object(dictionary! {
            "S" => "GoToE",
            "D" => Object::Array(vec![0.into(), "Fit".into()]),
            "T" => dictionary! {
                "R" => "C",
                "N" => Object::String(target.to_vec(), lopdf::StringFormat::Literal),
            },
        });
        let root = doc.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        doc.get_dictionary_mut(root).unwrap().set("OpenAction", Object::Reference(gotoe));
        doc
    }

    /// The embedded-file name-tree keys in the merged output.
    fn embedded_file_names(out: &Document) -> Vec<Vec<u8>> {
        let names_ref = catalog_of(out).get(b"Names").and_then(Object::as_reference).unwrap();
        let names = out.get_dictionary(names_ref).unwrap();
        let ef_ref = names.get(b"EmbeddedFiles").and_then(Object::as_reference).unwrap();
        let ef = out.get_dictionary(ef_ref).unwrap();
        ef.get(b"Names")
            .unwrap()
            .as_array()
            .unwrap()
            .chunks(2)
            .filter_map(|kv| kv[0].as_str().ok().map(<[u8]>::to_vec))
            .collect()
    }

    #[test]
    fn merge_multi_input_merges_colliding_embedded_files_without_gotoe() {
        // Finding 3: two ordinary PDFs both attaching "data.bin" with NO GoToE
        // are safely mergeable — the later name is suffixed and BOTH attachments
        // are kept, rather than over-refusing the whole merge.
        let mut a = build_embedded_file_doc(b"data.bin");
        let mut b = build_embedded_file_doc(b"data.bin");
        let pa = save_temp(&mut a, "pdfree_pagedoc_ef_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_ef_b.pdf");
        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        let names = embedded_file_names(&out);
        assert_eq!(names.len(), 2, "both attachments survive");
        assert!(names.contains(&b"data.bin".to_vec()), "the first keeps its name");
        assert!(names.iter().any(|n| n != b"data.bin"), "the later collider is suffixed");
        assert_no_dangling(&out);
    }

    #[test]
    fn merge_multi_input_rejects_colliding_embedded_files_referenced_by_gotoe() {
        // Finding 3: the later input attaches "data.bin" (a collision) AND carries
        // a GoToE selecting it by that exact name — a rename would leave the
        // action resolving to the other input's attachment, so the merge refuses.
        let mut a = build_embedded_file_doc(b"data.bin");
        let mut b = build_embedded_file_doc_with_gotoe(b"data.bin", b"data.bin");
        let pa = save_temp(&mut a, "pdfree_pagedoc_efg_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_efg_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("embedded-file")),
            "got {err:?}"
        );
    }

    #[test]
    fn merge_multi_input_merges_distinct_embedded_files() {
        let mut a = build_embedded_file_doc(b"a.bin");
        let mut b = build_embedded_file_doc(b"b.bin");
        let pa = save_temp(&mut a, "pdfree_pagedoc_ef2_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_ef2_b.pdf");
        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        let names_ref = catalog_of(&out).get(b"Names").and_then(Object::as_reference).unwrap();
        let names = out.get_dictionary(names_ref).unwrap();
        let ef_ref = names.get(b"EmbeddedFiles").and_then(Object::as_reference).unwrap();
        let ef = out.get_dictionary(ef_ref).unwrap();
        let arr = ef.get(b"Names").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 4, "both named embedded files survive (2 name/value pairs)");
        assert_no_dangling(&out);
    }

    // ---- F5: a dest rename plus JavaScript is unmergeable --------------------

    /// One-page doc with a named destination and a document /OpenAction that runs
    /// JavaScript naming a destination by opaque string.
    fn build_named_dest_js_doc(dest: &[u8]) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, b"(x)".to_vec()));
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let dests = doc.add_object(dictionary! {
            "Names" => vec![
                Object::String(dest.to_vec(), lopdf::StringFormat::Literal),
                Object::Array(vec![Object::Reference(page_id), "Fit".into()]),
            ],
        });
        let names = doc.add_object(dictionary! { "Dests" => Object::Reference(dests) });
        let open_action = doc.add_object(dictionary! {
            "S" => "JavaScript",
            "JS" => Object::String(b"this.gotoNamedDest('toc');".to_vec(), lopdf::StringFormat::Literal),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names),
            "OpenAction" => Object::Reference(open_action),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn merge_multi_input_rejects_dest_collision_with_javascript() {
        // Both name a destination "toc" (a rename is forced) AND both carry JS
        // that could reference it by string: unmergeable.
        let mut a = build_named_dest_js_doc(b"toc");
        let mut b = build_named_dest_js_doc(b"toc");
        let pa = save_temp(&mut a, "pdfree_pagedoc_js_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_js_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("JavaScript")),
            "got {err:?}"
        );
    }

    #[test]
    fn merge_multi_input_merges_distinct_dests_with_javascript() {
        // JS present in both, but the dest names differ: no rename, so the merge
        // is safe and proceeds.
        let mut a = build_named_dest_js_doc(b"home");
        let mut b = build_named_dest_js_doc(b"toc");
        let pa = save_temp(&mut a, "pdfree_pagedoc_js2_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_js2_b.pdf");
        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        assert_eq!(out.get_pages().len(), 2, "distinct dests + JS still merge");
        let dests = merged_dests(&out);
        assert_eq!(dests.len(), 2, "both destinations survive unrenamed");
        let names: Vec<Vec<u8>> = dests.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&b"home".to_vec()) && names.contains(&b"toc".to_vec()));
        assert_no_dangling(&out);
    }

    // ---- F4 (round 5): a rendition action's /JS counts as JavaScript ---------

    /// One-page doc with a named destination whose ONLY action is a rendition
    /// action carrying its JavaScript in /JS — no /S /JavaScript action exists,
    /// so only the Rendition detection can flag this input (finding 4).
    fn build_named_dest_rendition_doc(dest: &[u8]) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let c = doc.add_object(Stream::new(dictionary! {}, b"(x)".to_vec()));
        doc.set_object(page_id, dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(c),
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let dests = doc.add_object(dictionary! {
            "Names" => vec![
                Object::String(dest.to_vec(), lopdf::StringFormat::Literal),
                Object::Array(vec![Object::Reference(page_id), "Fit".into()]),
            ],
        });
        let names = doc.add_object(dictionary! { "Dests" => Object::Reference(dests) });
        let open_action = doc.add_object(dictionary! {
            "S" => "Rendition",
            "JS" => Object::String(b"this.gotoNamedDest('toc');".to_vec(), lopdf::StringFormat::Literal),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
            "Names" => Object::Reference(names),
            "OpenAction" => Object::Reference(open_action),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn merge_multi_input_rejects_dest_collision_with_rendition_js() {
        // Finding 4: a rendition action carries JavaScript in /JS. Both inputs
        // name "toc" (forcing a rename); the Rendition-borne JS could reference
        // the renamed dest by opaque string, so the merge is refused.
        let mut a = build_named_dest_rendition_doc(b"toc");
        let mut b = build_named_dest_rendition_doc(b"toc");
        let pa = save_temp(&mut a, "pdfree_pagedoc_rend_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_rend_b.pdf");
        let err = merge(&[pa.clone(), pb.clone()]).unwrap_err();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert!(
            matches!(err, PageDocError::UnmergeableCatalog(what) if what.contains("JavaScript")),
            "got {err:?}"
        );
    }

    #[test]
    fn merge_multi_input_rendition_without_js_is_not_javascript() {
        // Finding 4 boundary: a rendition action with NO /JS carries no
        // JavaScript, so a dest rename alongside it is safe and the merge
        // proceeds — the /JS entry, not the /S /Rendition, is what flags it.
        let mut a = build_named_dest_rendition_doc(b"toc");
        let mut b = build_named_dest_rendition_doc(b"toc");
        for doc in [&mut a, &mut b] {
            let root = doc.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
            let open = doc
                .get_dictionary(root)
                .unwrap()
                .get(b"OpenAction")
                .and_then(Object::as_reference)
                .unwrap();
            doc.get_dictionary_mut(open).unwrap().remove(b"JS");
        }
        let pa = save_temp(&mut a, "pdfree_pagedoc_rend2_a.pdf");
        let pb = save_temp(&mut b, "pdfree_pagedoc_rend2_b.pdf");
        let out = merge(&[pa.clone(), pb.clone()]).unwrap();
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
        assert_eq!(out.get_pages().len(), 2, "dest collision without JS still merges");
        assert_no_dangling(&out);
    }
}
