//! Page-tree transforms: rotate, delete, reorder pages in place on a single
//! document. Operates on lopdf's own /Pages tree structure (ISO 32000-1
//! 7.7.3) — no borrowed rendering logic, clean-room against the spec only.

use crate::replace::reject_encrypted;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
pub enum PageOpsError {
    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("encrypted documents are not supported yet (saving would strip the encryption)")]
    EncryptedUnsupported,
    #[error("no pages selected")]
    EmptySelection,
    #[error("page {0} out of range (document has {1} pages)")]
    OutOfRange(u32, u32),
    #[error("rotation must be a multiple of 90 degrees, got {0}")]
    InvalidRotation(i64),
    #[error("refusing to delete all {0} page(s); a PDF needs at least one")]
    WouldDeleteAll(u32),
    #[error("order must be a permutation of 1..={0}, got {1} entries")]
    BadPermutationLen(u32, usize),
    #[error("order is not a permutation of 1..={0}: duplicate or out-of-range value {1}")]
    BadPermutationValue(u32, u32),
    #[error("malformed page tree: {0}")]
    Malformed(&'static str),
    #[error("invalid page spec {0:?}: {1}")]
    BadSpec(String, String),
}

/// Parse a 1-based page spec like "1-2,4" into a sorted, deduplicated list
/// of page numbers, validated against `num_pages`.
pub fn parse_page_spec(spec: &str, num_pages: u32) -> Result<Vec<u32>, PageOpsError> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo
                .trim()
                .parse()
                .map_err(|_| PageOpsError::BadSpec(spec.to_string(), format!("bad range start {lo:?}")))?;
            let hi: u32 = hi
                .trim()
                .parse()
                .map_err(|_| PageOpsError::BadSpec(spec.to_string(), format!("bad range end {hi:?}")))?;
            if lo == 0 || hi < lo {
                return Err(PageOpsError::BadSpec(spec.to_string(), format!("empty or invalid range {lo}-{hi}")));
            }
            // Bounds-check the endpoints BEFORE expanding the range: a spec like
            // "1-4294967295" on a small document must fail fast, never
            // materialize billions of ints into `out` (OOM). Since hi >= lo >= 1
            // here, hi <= num_pages guarantees every value in lo..=hi is valid.
            if hi > num_pages {
                return Err(PageOpsError::OutOfRange(hi, num_pages));
            }
            for p in lo..=hi {
                out.push(p);
            }
        } else {
            let p: u32 = part
                .parse()
                .map_err(|_| PageOpsError::BadSpec(spec.to_string(), format!("bad page number {part:?}")))?;
            out.push(p);
        }
    }
    if out.is_empty() {
        return Err(PageOpsError::EmptySelection);
    }
    for &p in &out {
        if p == 0 || p > num_pages {
            return Err(PageOpsError::OutOfRange(p, num_pages));
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Parse a 1-based permutation spec like "3,1,2"; must cover every page
/// 1..=num_pages exactly once.
pub fn parse_order_spec(spec: &str, num_pages: u32) -> Result<Vec<u32>, PageOpsError> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let p: u32 = part
            .parse()
            .map_err(|_| PageOpsError::BadSpec(spec.to_string(), format!("bad page number {part:?}")))?;
        out.push(p);
    }
    if out.len() != num_pages as usize {
        return Err(PageOpsError::BadPermutationLen(num_pages, out.len()));
    }
    let mut seen = vec![false; num_pages as usize + 1];
    for &p in &out {
        if p == 0 || p > num_pages || seen[p as usize] {
            return Err(PageOpsError::BadPermutationValue(num_pages, p));
        }
        seen[p as usize] = true;
    }
    Ok(out)
}

/// Normalize a rotation delta to {0, 90, 180, 270} mod 360.
fn normalize_rotation(deg: i64) -> i64 {
    ((deg % 360) + 360) % 360
}

/// Walk /Parent references from `start` looking for `key`, returning the
/// first value found (checking `start` itself first) — i.e. the effective
/// inherited value per 7.7.3.4.
fn find_inherited(doc: &Document, start: ObjectId, key: &[u8]) -> Option<Object> {
    let mut cur = Some(start);
    let mut steps = 0;
    while let Some(id) = cur {
        steps += 1;
        if steps > 256 {
            break; // cycle guard, mirrors lopdf's own page-tree depth limit
        }
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(v) = dict.get(key) {
            return Some(v.clone());
        }
        cur = dict.get(b"Parent").ok().and_then(|o| o.as_reference().ok());
    }
    None
}

/// Effective rotation of a page: its own /Rotate if set, else inherited,
/// else 0, normalized to {0, 90, 180, 270}.
pub fn effective_rotation(doc: &Document, page_id: ObjectId) -> i64 {
    let raw = match find_inherited(doc, page_id, b"Rotate") {
        // /Rotate may be stored as an indirect reference to an integer; resolve
        // it rather than letting as_i64() fail and default to 0.
        Some(Object::Reference(id)) => doc
            .get_object(id)
            .ok()
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(0),
        Some(o) => o.as_i64().unwrap_or(0),
        None => 0,
    };
    normalize_rotation(raw)
}

/// Rotate the selected pages by `degrees` (added to each page's current
/// effective rotation, then normalized). `/Rotate` is written on the page's
/// own dictionary, overriding any inherited value.
pub fn rotate_pages(doc: &mut Document, pages: &[u32], degrees: i64) -> Result<(), PageOpsError> {
    reject_encrypted_here(doc)?;
    if pages.is_empty() {
        return Err(PageOpsError::EmptySelection);
    }
    // The exported API must be safe on its own, independent of any caller-side
    // check: /Rotate must be a multiple of 90 (7.7.3.3). Reject anything else.
    if degrees % 90 != 0 {
        return Err(PageOpsError::InvalidRotation(degrees));
    }
    // Reduce the delta mod 360 up front (yields 0..=270). Adding it to a
    // normalized current rotation (also 0..=270) can never overflow i64, no
    // matter how large the caller's `degrees` was.
    let delta = normalize_rotation(degrees);
    let page_map = doc.get_pages();
    let num_pages = page_map.len() as u32;
    let mut ids = Vec::with_capacity(pages.len());
    for &p in pages {
        let id = *page_map.get(&p).ok_or(PageOpsError::OutOfRange(p, num_pages))?;
        ids.push(id);
    }
    for id in ids {
        let current = effective_rotation(doc, id);
        let new_rotation = normalize_rotation(current + delta);
        let dict = doc.get_object_mut(id).and_then(Object::as_dict_mut)?;
        dict.set("Rotate", new_rotation);
    }
    Ok(())
}

/// Delete the selected pages from the /Pages tree, fixing up /Count and
/// /Kids up the ancestor chain and pruning any Pages node left with no
/// children. Refuses to delete every page in the document.
pub fn delete_pages(doc: &mut Document, pages: &[u32]) -> Result<(), PageOpsError> {
    reject_encrypted_here(doc)?;
    if pages.is_empty() {
        return Err(PageOpsError::EmptySelection);
    }
    let page_map = doc.get_pages();
    let num_pages = page_map.len() as u32;
    let mut to_delete: Vec<u32> = pages.to_vec();
    to_delete.sort_unstable();
    to_delete.dedup();
    for &p in &to_delete {
        if p == 0 || p > num_pages {
            return Err(PageOpsError::OutOfRange(p, num_pages));
        }
    }
    if to_delete.len() as u32 >= num_pages {
        return Err(PageOpsError::WouldDeleteAll(num_pages));
    }
    // Resolve the full set of page ObjectIds up front (before mutating the
    // tree) so a batch delete can scrub every dangling reference in one pass.
    let ids: Vec<ObjectId> = to_delete.iter().map(|p| page_map[p]).collect();
    let deleted: HashSet<ObjectId> = ids.iter().copied().collect();
    for id in &ids {
        remove_page(doc, *id)?;
    }
    // The pages are gone from the /Pages tree, but their ObjectIds are still
    // referenced from outlines, named destinations, and link annotations on
    // surviving pages. Scrub those so nothing dangles at a nonexistent object.
    scrub_deleted_refs(doc, &deleted)?;
    Ok(())
}

fn remove_page(doc: &mut Document, page_id: ObjectId) -> Result<(), PageOpsError> {
    // The page contributes 1 to the /Count of EVERY ancestor Pages node, not
    // just its immediate parent, so a multi-level page tree needs the whole
    // chain decremented (7.7.3.3: /Count is the number of leaf pages in the
    // subtree). Collect the chain first (with a cycle guard).
    let ancestors = ancestor_chain(doc, page_id)?;
    let immediate_parent = *ancestors.first().ok_or(PageOpsError::Malformed("page missing /Parent"))?;

    for &anc in &ancestors {
        // /Count may be stored as an indirect reference; resolve it first so an
        // indirect value isn't silently mistaken for the default of 1 (which
        // would corrupt every ancestor's leaf-page count).
        let count = resolve_count(doc, anc);
        let dict = doc.get_object_mut(anc).and_then(Object::as_dict_mut)?;
        dict.set("Count", (count - 1).max(0));
    }

    remove_kid(doc, immediate_parent, page_id)?;
    doc.objects.remove(&page_id);

    // Prune ancestors that are now empty (walking up), but never the root.
    for i in 0..ancestors.len() {
        let node = ancestors[i];
        let empty = kids_is_empty(doc, node);
        if !empty {
            break; // once a non-empty node is reached, everything above stays
        }
        match ancestors.get(i + 1) {
            Some(&gp) => {
                remove_kid(doc, gp, node)?;
                doc.objects.remove(&node);
            }
            None => break, // node is the tree root — always keep it
        }
    }
    Ok(())
}

/// /Parent chain from `page_id`'s immediate parent up to the page-tree root
/// (inclusive), with a cycle guard.
fn ancestor_chain(doc: &Document, page_id: ObjectId) -> Result<Vec<ObjectId>, PageOpsError> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(page_id);
    let mut cur = doc
        .get_dictionary(page_id)?
        .get(b"Parent")
        .and_then(Object::as_reference)
        .map_err(|_| PageOpsError::Malformed("page missing /Parent"))?;
    while seen.insert(cur) {
        chain.push(cur);
        match doc.get_dictionary(cur).ok().and_then(|d| d.get(b"Parent").and_then(Object::as_reference).ok()) {
            Some(p) => cur = p,
            None => break,
        }
    }
    Ok(chain)
}

/// Remove the reference to `child_id` from `parent_id`'s /Kids array. The
/// /Kids value may itself be an indirect reference to an array object, in which
/// case we mutate the referenced array rather than failing.
fn remove_kid(doc: &mut Document, parent_id: ObjectId, child_id: ObjectId) -> Result<(), PageOpsError> {
    // Find where the array actually lives: inline in the parent dict, or in a
    // separate array object the parent's /Kids points at.
    let kids_ref = match doc
        .get_dictionary(parent_id)
        .map_err(|_| PageOpsError::Malformed("missing parent Pages dict"))?
        .get(b"Kids")
    {
        Ok(Object::Reference(id)) => Some(*id),
        _ => None,
    };
    let kids = match kids_ref {
        Some(id) => doc
            .get_object_mut(id)
            .and_then(Object::as_array_mut)
            .map_err(|_| PageOpsError::Malformed("Pages node /Kids is not an array"))?,
        None => doc
            .get_object_mut(parent_id)
            .and_then(Object::as_dict_mut)
            .map_err(|_| PageOpsError::Malformed("missing parent Pages dict"))?
            .get_mut(b"Kids")
            .and_then(Object::as_array_mut)
            .map_err(|_| PageOpsError::Malformed("Pages node missing /Kids"))?,
    };
    kids.retain(|o| o.as_reference().map(|r| r != child_id).unwrap_or(true));
    Ok(())
}

/// Resolve an object, following a single indirect reference if present.
/// Returns the original object on a dangling/failed lookup.
fn deref<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(_) => doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj),
        _ => obj,
    }
}

/// Resolve a Pages node's /Count as an i64, dereferencing an indirect value.
/// Falls back to 1 only when /Count is genuinely absent or unreadable.
fn resolve_count(doc: &Document, node: ObjectId) -> i64 {
    doc.get_dictionary(node)
        .ok()
        .and_then(|d| d.get(b"Count").ok())
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(1)
}

/// Is `node`'s /Kids array empty? Dereferences an indirect /Kids array so an
/// indirect-but-empty node is still recognized as prunable.
fn kids_is_empty(doc: &Document, node: ObjectId) -> bool {
    doc.get_dictionary(node)
        .ok()
        .and_then(|d| d.get(b"Kids").ok())
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
        .map(|k| k.is_empty())
        .unwrap_or(false)
}

/// The target page ObjectId of a destination, if it names one via an explicit
/// `[pageref /XYZ ...]` array or a `<< /D [pageref ...] >>` wrapper (7.9.3),
/// following indirect references. Returns None for named/remote destinations.
fn dest_page_ref(doc: &Document, obj: &Object) -> Option<ObjectId> {
    match obj {
        Object::Reference(_) => {
            let (_, inner) = doc.dereference(obj).ok()?;
            // Guard against a reference resolving straight back to itself.
            if std::ptr::eq(inner, obj) {
                return None;
            }
            dest_page_ref(doc, inner)
        }
        Object::Array(a) => a.first().and_then(|o| o.as_reference().ok()),
        Object::Dictionary(d) => d.get(b"D").ok().and_then(|dd| dest_page_ref(doc, dd)),
        _ => None,
    }
}

/// Does destination `obj` point at a deleted page? Handles explicit array/dict
/// destinations (object-id match) and named destinations (Name or String key
/// that was pruned because it pointed at a deleted page).
fn dest_hits(
    doc: &Document,
    obj: &Object,
    deleted: &HashSet<ObjectId>,
    dead_names: &HashSet<Vec<u8>>,
) -> bool {
    if let Some(r) = dest_page_ref(doc, obj)
        && deleted.contains(&r)
    {
        return true;
    }
    match deref(doc, obj) {
        Object::Name(n) => dead_names.contains(n),
        Object::String(s, _) => dead_names.contains(s),
        _ => false,
    }
}

/// Does an /A action target a deleted page? Only a *local* GoTo action carries
/// a /D that names a destination in THIS document. Remote/embedded actions
/// (`/GoToR`, `/GoToE`, `/Launch`, `/URI`, ...) also use /D, but it points into
/// another file or is a page *number*, never a page object here — so a local
/// deleted page or pruned name can never be their referent. Gating on
/// `/S == /GoTo` prevents wrongly stripping e.g. `<< /S /GoToR /F (x.pdf)
/// /D (foo) >>` just because a local dest named `foo` was pruned (7.11.2,
/// 12.6.4.2/.3). `/S` is dereferenced before comparison, since it may itself
/// be an indirect reference to the name object. An action dict without a
/// readable /S is treated as non-local.
fn action_hits(
    doc: &Document,
    aobj: &Object,
    deleted: &HashSet<ObjectId>,
    dead_names: &HashSet<Vec<u8>>,
) -> bool {
    let Object::Dictionary(d) = deref(doc, aobj) else {
        return false;
    };
    match d.get(b"S").ok().map(|s| deref(doc, s)).and_then(|s| s.as_name().ok()) {
        Some(b"GoTo") => d
            .get(b"D")
            .map(|dd| dest_hits(doc, dd, deleted, dead_names))
            .unwrap_or(false),
        _ => false,
    }
}

/// After pages are removed from the /Pages tree, scrub every remaining
/// reference to a deleted page's ObjectId. A reference to a nonexistent object
/// is technically legal (resolves to null) but breaks navigation and trips
/// validators, so we neutralize it.
///
/// Two layers:
///  1. *Targeted* cleanup of the navigation sites the spec defines — named
///     destinations, outline items, the catalog /OpenAction and /AA, and /AA
///     plus /Dest//A on surviving pages and their annotations — so the output
///     is clean (dead keys removed, not just nulled) and by-name references to
///     pruned named destinations are caught too.
///  2. A *document-wide null-sweep* that replaces any lingering indirect
///     reference to a deleted page — anywhere, including form fields, the
///     structure tree, orphaned destination objects, or sites we don't model —
///     with null. This is the correctness backstop: after it runs, a
///     whole-document scan finds ZERO references to any removed page object.
fn scrub_deleted_refs(doc: &mut Document, deleted: &HashSet<ObjectId>) -> Result<(), PageOpsError> {
    if deleted.is_empty() {
        return Ok(());
    }
    // Prune the destination name tree / legacy /Dests dict first; this also
    // tells us which named destinations died, so references-by-name elsewhere
    // can be neutralized too.
    let dead_names = scrub_named_dests(doc, deleted);
    scrub_catalog(doc, deleted, &dead_names);
    scrub_outlines(doc, deleted, &dead_names);
    scrub_annots(doc, deleted, &dead_names);
    // Backstop: guarantee no surviving object still points at a removed page.
    null_out_deleted_refs(doc, deleted);
    Ok(())
}

/// Neutralize document-level navigation on the catalog: the /OpenAction
/// (7.7.2 — a destination or an action shown when the document opens) and the
/// /AA additional-actions dictionary, when they target a deleted page or a
/// pruned named destination.
fn scrub_catalog(doc: &mut Document, deleted: &HashSet<ObjectId>, dead_names: &HashSet<Vec<u8>>) {
    let Some(cat_id) = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|o| o.as_reference().ok())
    else {
        return;
    };
    // /OpenAction may be an explicit destination OR an action dictionary; strip
    // it if either interpretation targets a deleted page / dead name. (A remote
    // action like /GoToR is rejected by both predicates, so it is left intact.)
    let strip_oa = doc
        .get_dictionary(cat_id)
        .ok()
        .and_then(|d| d.get(b"OpenAction").ok())
        .map(|oa| dest_hits(doc, oa, deleted, dead_names) || action_hits(doc, oa, deleted, dead_names))
        .unwrap_or(false);
    if strip_oa
        && let Ok(d) = doc.get_object_mut(cat_id).and_then(Object::as_dict_mut)
    {
        d.remove(b"OpenAction");
    }
    // Document-level /AA (and, harmlessly, any /Dest//A the catalog shouldn't have).
    neutralize_item(doc, cat_id, deleted, dead_names);
}

/// Prune entries whose destination targets a deleted page from the catalog's
/// destination name tree (`/Names /Dests`) and the legacy `/Dests` dictionary.
/// Leaves the now-orphaned destination value objects in place (they may be
/// shared; the null-sweep severs any deleted-page reference inside them), and
/// returns the set of destination names that were pruned so references-by-name
/// elsewhere can be neutralized.
fn scrub_named_dests(doc: &mut Document, deleted: &HashSet<ObjectId>) -> HashSet<Vec<u8>> {
    let mut dead_names: HashSet<Vec<u8>> = HashSet::new();

    // Locate the containers to edit (read-only walk), then mutate.
    let mut leaf_ids: Vec<ObjectId> = Vec::new();
    let mut legacy_dests: Option<ObjectId> = None;
    if let Ok(catalog) = doc.catalog() {
        if let Ok(names) = catalog.get(b"Names")
            && let Object::Dictionary(nd) = deref(doc, names)
            && let Ok(dtree) = nd.get(b"Dests")
        {
            let mut visited = HashSet::new();
            collect_name_tree_leaves(doc, dtree, &mut leaf_ids, &mut visited);
        }
        // Legacy catalog /Dests dict, only when it is an indirect object we can
        // edit in place (inline dicts are left untouched — best effort).
        if let Ok(Object::Reference(id)) = catalog.get(b"Dests") {
            legacy_dests = Some(*id);
        }
    }

    // Name-tree leaves: drop each dead [key, value] pair and orphan its value.
    for leaf in leaf_ids {
        let arr: Vec<Object> = match doc
            .get_dictionary(leaf)
            .ok()
            .and_then(|d| d.get(b"Names").ok())
            .map(|n| deref(doc, n))
        {
            Some(Object::Array(a)) => a.clone(),
            _ => continue,
        };
        let names_ref = match doc.get_dictionary(leaf).ok().and_then(|d| d.get(b"Names").ok()) {
            Some(Object::Reference(id)) => Some(*id),
            _ => None,
        };
        let mut kept: Vec<Object> = Vec::with_capacity(arr.len());
        let mut i = 0;
        while i + 1 < arr.len() {
            let key = &arr[i];
            let val = &arr[i + 1];
            let hit = dest_page_ref(doc, val)
                .map(|r| deleted.contains(&r))
                .unwrap_or(false);
            if hit {
                if let Some(k) = key_bytes(key) {
                    dead_names.insert(k);
                }
                // Do not remove the value object: it may be shared (e.g. an
                // outline /Dest referencing the same dict). Dropping the entry
                // from the /Names array is enough; the null-sweep severs any
                // deleted-page reference still inside the orphaned value.
            } else {
                kept.push(key.clone());
                kept.push(val.clone());
            }
            i += 2;
        }
        let became_empty = kept.is_empty();
        match names_ref {
            Some(id) => {
                if let Ok(a) = doc.get_object_mut(id).and_then(Object::as_array_mut) {
                    *a = kept;
                }
            }
            None => {
                if let Ok(d) = doc.get_object_mut(leaf).and_then(Object::as_dict_mut) {
                    d.set("Names", kept);
                }
            }
        }
        // A now-empty leaf's /Limits are stale; drop them to avoid a bogus range.
        if became_empty
            && let Ok(d) = doc.get_object_mut(leaf).and_then(Object::as_dict_mut)
        {
            d.remove(b"Limits");
        }
    }

    // Legacy /Dests dict: remove dead keys and orphan their value objects.
    if let Some(id) = legacy_dests {
        let entries: Vec<(Vec<u8>, Object)> = match doc.get_object(id).and_then(Object::as_dict) {
            Ok(d) => d.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(_) => Vec::new(),
        };
        let mut dead_keys: Vec<Vec<u8>> = Vec::new();
        for (k, v) in &entries {
            if dest_page_ref(doc, v).map(|r| deleted.contains(&r)).unwrap_or(false) {
                dead_keys.push(k.clone());
                dead_names.insert(k.clone());
                // Value object left in place (may be shared); see the name-tree
                // branch above. The null-sweep severs any deleted-page reference.
            }
        }
        if let Ok(d) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
            for k in &dead_keys {
                d.remove(k);
            }
        }
    }

    dead_names
}

/// Recursively collect the ObjectIds of name-tree nodes that carry a /Names
/// array (i.e. leaf nodes), following /Kids. Cycle-guarded by node id.
fn collect_name_tree_leaves(
    doc: &Document,
    node_obj: &Object,
    out: &mut Vec<ObjectId>,
    visited: &mut HashSet<ObjectId>,
) {
    let node_id = node_obj.as_reference().ok();
    if let Some(id) = node_id
        && !visited.insert(id)
    {
        return;
    }
    let node = deref(doc, node_obj);
    let Object::Dictionary(d) = node else {
        return;
    };
    if d.has(b"Names")
        && let Some(id) = node_id
    {
        out.push(id);
    }
    if let Ok(kids) = d.get(b"Kids")
        && let Object::Array(a) = deref(doc, kids)
    {
        for kid in a {
            collect_name_tree_leaves(doc, kid, out, visited);
        }
    }
}

/// The bytes that identify a destination-tree key (a String), used to record
/// which named destinations were pruned.
fn key_bytes(obj: &Object) -> Option<Vec<u8>> {
    match obj {
        Object::String(s, _) => Some(s.clone()),
        Object::Name(n) => Some(n.clone()),
        _ => None,
    }
}

/// Walk the document Outlines tree and neutralize any outline item whose /Dest
/// or /A destination targets a deleted page.
fn scrub_outlines(doc: &mut Document, deleted: &HashSet<ObjectId>, dead_names: &HashSet<Vec<u8>>) {
    let root = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Outlines").ok())
        .and_then(|o| o.as_reference().ok());
    let Some(root) = root else {
        return;
    };
    let mut items: Vec<ObjectId> = Vec::new();
    let mut visited = HashSet::new();
    collect_outline_items(doc, root, &mut items, &mut visited);
    for id in items {
        neutralize_item(doc, id, deleted, dead_names);
    }
}

/// Collect every node reachable from the Outlines root via /First and /Next.
/// The root itself carries no destination, so including it is harmless.
///
/// Siblings form a /Next linked list that can be tens of thousands long; we
/// walk it *iteratively* so recursion depth is bounded by real tree depth
/// (nesting via /First), not sibling count — a per-sibling recursion would
/// blow the stack on a flat outline. Cycle-guarded by node id.
fn collect_outline_items(
    doc: &Document,
    start_id: ObjectId,
    out: &mut Vec<ObjectId>,
    visited: &mut HashSet<ObjectId>,
) {
    let mut cur = Some(start_id);
    while let Some(node_id) = cur {
        if !visited.insert(node_id) {
            break;
        }
        out.push(node_id);
        let dict = match doc.get_dictionary(node_id) {
            Ok(d) => d,
            Err(_) => break,
        };
        let first = dict.get(b"First").ok().and_then(|o| o.as_reference().ok());
        let next = dict.get(b"Next").ok().and_then(|o| o.as_reference().ok());
        // Recurse only into children (bounded by tree depth); loop across siblings.
        if let Some(f) = first {
            collect_outline_items(doc, f, out, visited);
        }
        cur = next;
    }
}

/// Neutralize navigation on surviving pages: each page's own /AA (additional
/// actions, e.g. /O open, /C close — 12.6.3) and every link annotation whose
/// /Dest, /A, or /AA targets a deleted page.
fn scrub_annots(doc: &mut Document, deleted: &HashSet<ObjectId>, dead_names: &HashSet<Vec<u8>>) {
    // get_pages() now returns only surviving pages.
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let mut annot_ids: Vec<ObjectId> = Vec::new();
    for &pid in &page_ids {
        let annots = doc
            .get_dictionary(pid)
            .ok()
            .and_then(|d| d.get(b"Annots").ok())
            .map(|o| deref(doc, o));
        if let Some(Object::Array(a)) = annots {
            for o in a {
                if let Ok(id) = o.as_reference() {
                    annot_ids.push(id);
                }
            }
        }
    }
    // Page-level /AA on the surviving pages themselves.
    for pid in page_ids {
        neutralize_item(doc, pid, deleted, dead_names);
    }
    for id in annot_ids {
        neutralize_item(doc, id, deleted, dead_names);
    }
}

/// Replace every indirect reference to a deleted page object with null, across
/// the entire object store *and* the trailer dictionary. Page objects were
/// removed from `doc.objects`, so any surviving `Reference` to one dangles
/// (resolves to null) and trips validators. We cannot always know the
/// semantic container — a destination array, a structure element's /Pg, a
/// form widget's /P, an action we don't model, or even a private/extension
/// trailer key — so we neutralize the *reference itself* to null, a value
/// that is legal in every context and severs the link to the removed page.
/// This is the last-resort backstop: the targeted scrubbers above give clean
/// output for the sites they model, and this guarantees nothing at all —
/// including the trailer — still points at a removed page. Recurses through
/// arrays/dicts/stream dicts; indirect references are not followed (each
/// stored object, and each trailer value, is rewritten exactly once).
///
/// The trailer's standard entries (/Root, /Info, /Size, /Prev, /Encrypt, ...)
/// are never references to page objects, so `deleted.contains(id)` never
/// matches them; only a trailer value that actually references a deleted
/// page — e.g. a private/extension key — is affected.
fn null_out_deleted_refs(doc: &mut Document, deleted: &HashSet<ObjectId>) {
    for obj in doc.objects.values_mut() {
        null_refs_in_object(obj, deleted);
    }
    for (_k, v) in doc.trailer.iter_mut() {
        null_refs_in_object(v, deleted);
    }
}

fn null_refs_in_object(obj: &mut Object, deleted: &HashSet<ObjectId>) {
    match obj {
        Object::Reference(id) if deleted.contains(id) => {
            *obj = Object::Null;
        }
        Object::Array(a) => {
            for o in a.iter_mut() {
                null_refs_in_object(o, deleted);
            }
        }
        Object::Dictionary(d) => {
            for (_k, v) in d.iter_mut() {
                null_refs_in_object(v, deleted);
            }
        }
        Object::Stream(s) => {
            for (_k, v) in s.dict.iter_mut() {
                null_refs_in_object(v, deleted);
            }
        }
        _ => {}
    }
}

/// Strip navigation entries whose destination targets a deleted page or a
/// pruned named destination from the dict `id` (an outline item, annotation,
/// page, or catalog): the /Dest, the /A action, and any /AA (additional-
/// actions) sub-action that hits. Empty /AA dicts are dropped.
///
/// It only ever removes *keys from this dict*. It does NOT remove the
/// referenced destination/action objects, because a single indirect
/// dest/action object may be legally shared by several sites (7.11.4);
/// eagerly deleting it while processing one sharer would dangle every other
/// sharer's reference. Orphaned dest/action objects are left in place
/// (harmless — a later gc pass could collect the truly-unreferenced ones),
/// and any page reference still living inside them is severed by the
/// document-wide null-sweep in `scrub_deleted_refs`.
fn neutralize_item(
    doc: &mut Document,
    id: ObjectId,
    deleted: &HashSet<ObjectId>,
    dead_names: &HashSet<Vec<u8>>,
) {
    let (strip_dest, strip_a, dead_aa_events, drop_aa) = {
        let dict = match doc.get_dictionary(id) {
            Ok(d) => d,
            Err(_) => return,
        };
        let strip_dest = dict
            .get(b"Dest")
            .map(|dest| dest_hits(doc, dest, deleted, dead_names))
            .unwrap_or(false);
        let strip_a = dict
            .get(b"A")
            .map(|a| action_hits(doc, a, deleted, dead_names))
            .unwrap_or(false);
        // /AA is a dictionary of event-name -> action; drop each event whose
        // action targets a deleted page, and the whole /AA if none survive.
        let mut dead_aa_events: Vec<Vec<u8>> = Vec::new();
        let mut drop_aa = false;
        if let Ok(aa) = dict.get(b"AA")
            && let Object::Dictionary(aad) = deref(doc, aa)
        {
            let mut live = 0usize;
            for (ev, act) in aad.iter() {
                if action_hits(doc, act, deleted, dead_names) {
                    dead_aa_events.push(ev.clone());
                } else {
                    live += 1;
                }
            }
            drop_aa = live == 0 && !dead_aa_events.is_empty();
        }
        (strip_dest, strip_a, dead_aa_events, drop_aa)
    };
    if !strip_dest && !strip_a && dead_aa_events.is_empty() {
        return;
    }
    // If /AA is an indirect dictionary object, edit that object; otherwise it is
    // inline in this dict. Resolve its id (if any) before taking the mut borrow.
    let aa_ref = if drop_aa || dead_aa_events.is_empty() {
        None
    } else {
        match doc.get_dictionary(id).ok().and_then(|d| d.get(b"AA").ok()) {
            Some(Object::Reference(rid)) => Some(*rid),
            _ => None,
        }
    };
    if let Ok(dict) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
        if strip_dest {
            dict.remove(b"Dest");
        }
        if strip_a {
            dict.remove(b"A");
        }
        if drop_aa {
            dict.remove(b"AA");
        } else if aa_ref.is_none()
            && let Ok(aad) = dict.get_mut(b"AA").and_then(Object::as_dict_mut)
        {
            for ev in &dead_aa_events {
                aad.remove(ev);
            }
        }
    }
    if let Some(rid) = aa_ref
        && let Ok(aad) = doc.get_object_mut(rid).and_then(Object::as_dict_mut)
    {
        for ev in &dead_aa_events {
            aad.remove(ev);
        }
    }
}

/// Inheritable page attributes (7.7.3.4 Table 30) that must be baked onto
/// each page dict before we detach it from its original ancestor chain.
const INHERITABLE_KEYS: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// Reorder pages to the given 1-based permutation of 1..=N. Flattens the
/// /Pages tree to a single level under the root Pages node (resolving
/// inherited Resources/MediaBox/CropBox/Rotate onto each page first, so
/// nothing is lost when pages leave their original subtree).
pub fn reorder_pages(doc: &mut Document, order: &[u32]) -> Result<(), PageOpsError> {
    reject_encrypted_here(doc)?;
    let page_map = doc.get_pages();
    let num_pages = page_map.len() as u32;
    if order.len() as u32 != num_pages {
        return Err(PageOpsError::BadPermutationLen(num_pages, order.len()));
    }
    let mut seen = vec![false; num_pages as usize + 1];
    for &p in order {
        if p == 0 || p > num_pages || seen[p as usize] {
            return Err(PageOpsError::BadPermutationValue(num_pages, p));
        }
        seen[p as usize] = true;
    }

    let old_ids: Vec<ObjectId> = (1..=num_pages).map(|i| page_map[&i]).collect();

    // Bake inherited attributes onto every page before we reparent it.
    for &id in &old_ids {
        let mut resolved: Vec<(&[u8], Object)> = Vec::new();
        for key in INHERITABLE_KEYS {
            if let Some(v) = find_inherited(doc, id, key) {
                resolved.push((key, v));
            }
        }
        let dict = doc.get_object_mut(id).and_then(Object::as_dict_mut)?;
        for (key, v) in resolved {
            dict.set(key, v);
        }
    }

    let pages_id = doc
        .catalog()?
        .get(b"Pages")
        .and_then(Object::as_reference)
        .map_err(|_| PageOpsError::Malformed("catalog missing /Pages"))?;

    let new_ids: Vec<ObjectId> = order.iter().map(|&i| old_ids[(i - 1) as usize]).collect();

    for &id in &new_ids {
        let dict = doc.get_object_mut(id).and_then(Object::as_dict_mut)?;
        dict.set("Parent", Object::Reference(pages_id));
    }

    let kids: Vec<Object> = new_ids.into_iter().map(Object::Reference).collect();
    let pages_dict = doc.get_object_mut(pages_id).and_then(Object::as_dict_mut)?;
    pages_dict.set("Kids", kids);
    pages_dict.set("Count", num_pages as i64);
    let _: &Dictionary = pages_dict; // keep type explicit for readability

    Ok(())
}

/// Wrapper so all three entry points read consistently — mirrors
/// replace::reject_encrypted's contract (see its doc comment).
fn reject_encrypted_here(doc: &Document) -> Result<(), PageOpsError> {
    reject_encrypted(doc).map_err(|_| PageOpsError::EncryptedUnsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    /// Build a minimal, valid single-page document.
    fn one_page_doc() -> Document {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc
    }

    #[test]
    fn rotate_pages_rejects_non_multiple_of_90() {
        let mut doc = one_page_doc();
        let err = rotate_pages(&mut doc, &[1], 45).unwrap_err();
        assert!(matches!(err, PageOpsError::InvalidRotation(45)));
    }

    #[test]
    fn rotate_pages_accepts_multiple_of_90() {
        let mut doc = one_page_doc();
        rotate_pages(&mut doc, &[1], 90).expect("90 is valid");
        let page_id = doc.get_pages()[&1];
        assert_eq!(effective_rotation(&doc, page_id), 90);
    }

    #[test]
    fn rotate_pages_huge_degrees_does_not_overflow() {
        let mut doc = one_page_doc();
        // A multiple of 90 near i64::MAX must normalize without panicking.
        let big = (i64::MAX / 90) * 90;
        rotate_pages(&mut doc, &[1], big).expect("multiple of 90 is valid");
        let page_id = doc.get_pages()[&1];
        let r = effective_rotation(&doc, page_id);
        assert!(matches!(r, 0 | 90 | 180 | 270));
    }

    #[test]
    fn parse_page_spec_rejects_overlarge_range_without_expanding() {
        // Must fail fast on the endpoint check, never build a 4-billion Vec.
        let err = parse_page_spec("1-4294967295", 1).unwrap_err();
        assert!(matches!(err, PageOpsError::OutOfRange(4294967295, 1)));
    }

    #[test]
    fn parse_page_spec_valid_range() {
        assert_eq!(parse_page_spec("1-3", 5).unwrap(), vec![1, 2, 3]);
    }

    // ---- delete-pages / dangling-reference tests -------------------------

    /// Build a doc with `n` pages under a single flat /Pages node. Returns the
    /// doc plus the catalog id, pages-node id, and page ids in order.
    fn flat_doc(n: usize) -> (Document, ObjectId, ObjectId, Vec<ObjectId>) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut page_ids = Vec::new();
        for _ in 0..n {
            let pid = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            });
            page_ids.push(pid);
        }
        let kids: Vec<Object> = page_ids.iter().map(|&id| id.into()).collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => n as i64,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        (doc, catalog_id, pages_id, page_ids)
    }

    fn set_key(doc: &mut Document, id: ObjectId, key: &str, val: Object) {
        doc.get_object_mut(id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .set(key, val);
    }

    /// Every indirect reference held anywhere in the object store + trailer.
    fn all_references(doc: &Document) -> Vec<ObjectId> {
        fn walk(o: &Object, refs: &mut Vec<ObjectId>) {
            match o {
                Object::Reference(id) => refs.push(*id),
                Object::Array(a) => a.iter().for_each(|x| walk(x, refs)),
                Object::Dictionary(d) => d.iter().for_each(|(_, v)| walk(v, refs)),
                Object::Stream(s) => s.dict.iter().for_each(|(_, v)| walk(v, refs)),
                _ => {}
            }
        }
        let mut refs = Vec::new();
        for o in doc.objects.values() {
            walk(o, &mut refs);
        }
        for (_, v) in doc.trailer.iter() {
            walk(v, &mut refs);
        }
        refs
    }

    fn refs_to(doc: &Document, id: ObjectId) -> usize {
        all_references(doc).iter().filter(|r| **r == id).count()
    }

    /// Assert every reference in the document resolves to a stored object.
    fn assert_no_dangling(doc: &Document) {
        for r in all_references(doc) {
            assert!(
                doc.objects.contains_key(&r),
                "dangling reference to {r:?} (object not in store)"
            );
        }
    }

    fn goto_dest(page: ObjectId) -> Object {
        Object::Array(vec![Object::Reference(page), Object::Name(b"Fit".to_vec())])
    }

    fn goto_action(page: ObjectId) -> Dictionary {
        dictionary! { "S" => "GoTo", "D" => goto_dest(page) }
    }

    /// Finding 1: catalog /OpenAction, catalog /AA and page /AA that target a
    /// deleted page are neutralized, and a whole-doc scan shows zero refs.
    #[test]
    fn delete_neutralizes_open_action_and_additional_actions() {
        let (mut doc, cat, _pages, pids) = flat_doc(3);
        let (p1, p2) = (pids[0], pids[1]);
        // Catalog /OpenAction -> explicit destination on the doomed page.
        set_key(&mut doc, cat, "OpenAction", goto_dest(p2));
        // Catalog document-level /AA with one event pointing at the doomed page.
        let cat_aa = Object::Dictionary(dictionary! { "WC" => Object::Dictionary(goto_action(p2)) });
        set_key(&mut doc, cat, "AA", cat_aa);
        // Page /AA on a surviving page pointing at the doomed page.
        let pg_aa = Object::Dictionary(dictionary! { "O" => Object::Dictionary(goto_action(p2)) });
        set_key(&mut doc, p1, "AA", pg_aa);

        delete_pages(&mut doc, &[2]).unwrap();

        assert!(!doc.objects.contains_key(&p2), "page object removed");
        let cat_dict = doc.get_dictionary(cat).unwrap();
        assert!(cat_dict.get(b"OpenAction").is_err(), "/OpenAction stripped");
        assert!(cat_dict.get(b"AA").is_err(), "empty catalog /AA dropped");
        assert!(
            doc.get_dictionary(p1).unwrap().get(b"AA").is_err(),
            "empty page /AA dropped"
        );
        assert_eq!(refs_to(&doc, p2), 0, "zero references to the removed page");
        assert_no_dangling(&doc);
    }

    /// Finding 2: a 50k-long /Next sibling chain must be walked iteratively —
    /// a per-sibling recursion would overflow the (2 MiB) test-thread stack.
    #[test]
    fn delete_with_huge_outline_next_chain_does_not_overflow() {
        let (mut doc, cat, _pages, pids) = flat_doc(2);
        let p1 = pids[0];
        let root_id = doc.new_object_id();
        let n = 50_000usize;
        let item_ids: Vec<ObjectId> = (0..n).map(|_| doc.new_object_id()).collect();
        for i in 0..n {
            let mut d = dictionary! { "Parent" => root_id, "Dest" => goto_dest(p1) };
            if i + 1 < n {
                d.set("Next", Object::Reference(item_ids[i + 1]));
            }
            if i > 0 {
                d.set("Prev", Object::Reference(item_ids[i - 1]));
            }
            doc.objects.insert(item_ids[i], Object::Dictionary(d));
        }
        doc.objects.insert(
            root_id,
            Object::Dictionary(dictionary! {
                "Type" => "Outlines",
                "First" => item_ids[0],
                "Last" => item_ids[n - 1],
                "Count" => n as i64,
            }),
        );
        set_key(&mut doc, cat, "Outlines", Object::Reference(root_id));

        // Deleting page 2 forces a full outline walk; must simply complete.
        delete_pages(&mut doc, &[2]).unwrap();
        assert_eq!(doc.get_pages().len(), 1);
        assert_no_dangling(&doc);
    }

    /// Finding 3: one indirect action object shared by two annotations must not
    /// be deleted out from under the second sharer.
    #[test]
    fn delete_keeps_shared_action_object_and_leaves_no_dangling() {
        let (mut doc, _cat, _pages, pids) = flat_doc(2);
        let (p1, p2) = (pids[0], pids[1]);
        let action_id = doc.add_object(goto_action(p2));
        let annot1 = doc.add_object(dictionary! {
            "Subtype" => "Link", "A" => Object::Reference(action_id),
        });
        let annot2 = doc.add_object(dictionary! {
            "Subtype" => "Link", "A" => Object::Reference(action_id),
        });
        set_key(
            &mut doc,
            p1,
            "Annots",
            Object::Array(vec![Object::Reference(annot1), Object::Reference(annot2)]),
        );

        delete_pages(&mut doc, &[2]).unwrap();

        // The shared object survives (only orphaned), and BOTH annotations had
        // their now-dead /A stripped — neither dangles at a removed object.
        assert!(
            doc.objects.contains_key(&action_id),
            "shared action object must not be removed while a sharer might still reference it"
        );
        assert!(doc.get_dictionary(annot1).unwrap().get(b"A").is_err());
        assert!(doc.get_dictionary(annot2).unwrap().get(b"A").is_err());
        assert_eq!(refs_to(&doc, p2), 0, "no reference to the removed page anywhere");
        assert_no_dangling(&doc);
    }

    /// Finding 4: a remote /GoToR action naming a dest that collides with a
    /// pruned *local* name must NOT be stripped (its /D lives in another file).
    #[test]
    fn delete_does_not_strip_remote_gotor_action() {
        let (mut doc, cat, _pages, pids) = flat_doc(2);
        let (p1, p2) = (pids[0], pids[1]);
        // Local named destination "foo" -> doomed page, in the /Names dest tree.
        let leaf_id = doc.add_object(dictionary! {
            "Names" => Object::Array(vec![Object::string_literal("foo"), goto_dest(p2)]),
            "Limits" => Object::Array(vec![Object::string_literal("foo"), Object::string_literal("foo")]),
        });
        let names_id = doc.add_object(dictionary! { "Dests" => Object::Reference(leaf_id) });
        set_key(&mut doc, cat, "Names", Object::Reference(names_id));

        // Remote GoToR whose /D (foo) targets *another* document.
        let gotor = doc.add_object(dictionary! {
            "S" => "GoToR",
            "F" => Object::string_literal("other.pdf"),
            "D" => Object::string_literal("foo"),
        });
        let remote_annot = doc.add_object(dictionary! {
            "Subtype" => "Link", "A" => Object::Reference(gotor),
        });
        // Control: a LOCAL GoTo naming the same (now dead) local dest -> stripped.
        let goto_local = doc.add_object(dictionary! {
            "S" => "GoTo", "D" => Object::string_literal("foo"),
        });
        let local_annot = doc.add_object(dictionary! {
            "Subtype" => "Link", "A" => Object::Reference(goto_local),
        });
        set_key(
            &mut doc,
            p1,
            "Annots",
            Object::Array(vec![Object::Reference(remote_annot), Object::Reference(local_annot)]),
        );

        delete_pages(&mut doc, &[2]).unwrap();

        assert!(
            doc.get_dictionary(remote_annot).unwrap().get(b"A").is_ok(),
            "remote /GoToR action must be preserved"
        );
        assert!(
            doc.get_dictionary(local_annot).unwrap().get(b"A").is_err(),
            "local /GoTo naming a pruned dest must be stripped"
        );
        assert_no_dangling(&doc);
    }

    /// A bookmarked page: deleting it strips the outline /Dest and leaves no
    /// dangling reference (mirrors the TAMReview corpus regression).
    #[test]
    fn delete_bookmarked_page_scrubs_outline() {
        let (mut doc, cat, _pages, pids) = flat_doc(3);
        let p2 = pids[1];
        let item = doc.add_object(dictionary! { "Title" => Object::string_literal("Ch2"), "Dest" => goto_dest(p2) });
        let root = doc.add_object(dictionary! {
            "Type" => "Outlines", "First" => item, "Last" => item, "Count" => 1,
        });
        set_key(&mut doc, item, "Parent", Object::Reference(root));
        set_key(&mut doc, cat, "Outlines", Object::Reference(root));

        delete_pages(&mut doc, &[2]).unwrap();

        assert!(doc.get_dictionary(item).unwrap().get(b"Dest").is_err(), "outline /Dest stripped");
        assert_eq!(refs_to(&doc, p2), 0);
        assert_no_dangling(&doc);
    }

    /// Multi-level page tree: deleting a leaf decrements /Count on the whole
    /// ancestor chain (3->2 at root, 2->1 at the intermediate node).
    #[test]
    fn delete_updates_count_up_multi_level_tree() {
        let mut doc = Document::with_version("1.5");
        let root_id = doc.new_object_id();
        let inter_id = doc.new_object_id();
        let p1 = doc.add_object(dictionary! { "Type" => "Page", "Parent" => inter_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] });
        let p2 = doc.add_object(dictionary! { "Type" => "Page", "Parent" => inter_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] });
        let p3 = doc.add_object(dictionary! { "Type" => "Page", "Parent" => root_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] });
        doc.objects.insert(inter_id, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Parent" => root_id,
            "Kids" => vec![p1.into(), p2.into()], "Count" => 2,
        }));
        doc.objects.insert(root_id, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![inter_id.into(), p3.into()], "Count" => 3,
        }));
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root_id });
        doc.trailer.set("Root", cat);

        delete_pages(&mut doc, &[1]).unwrap();

        assert_eq!(resolve_count(&doc, root_id), 2, "root /Count 3->2");
        assert_eq!(resolve_count(&doc, inter_id), 1, "intermediate /Count 2->1");
        assert_eq!(doc.get_pages().len(), 2);
        assert!(!doc.objects.contains_key(&p1));
        assert_no_dangling(&doc);
    }

    /// Round-3 finding 1: a custom/private trailer key that references a
    /// deleted page must be nulled by the document-wide sweep too, not just
    /// entries under `doc.objects`. Mirrors the whole-document (incl. trailer)
    /// scan the other tests already do via `all_references`/`refs_to`.
    #[test]
    fn delete_nulls_custom_trailer_ref_to_deleted_page() {
        let (mut doc, _cat, _pages, pids) = flat_doc(2);
        let p2 = pids[1];
        // A private/extension trailer key pointing directly at the doomed page.
        doc.trailer.set("PdfreePageRef", Object::Reference(p2));

        delete_pages(&mut doc, &[2]).unwrap();

        assert!(
            matches!(doc.trailer.get(b"PdfreePageRef"), Ok(Object::Null)),
            "trailer ref to the deleted page must be nulled, not left dangling"
        );
        assert_eq!(
            refs_to(&doc, p2),
            0,
            "whole-document scan (including trailer) must find zero refs to the removed page"
        );
        assert_no_dangling(&doc);
    }

    /// Round-3 finding 2: `/S` on an action dict may itself be an indirect
    /// reference to the `/GoTo` name object, rather than an inline name. Such
    /// an action must still be recognized as a *local* GoTo and scrubbed when
    /// its /D targets a deleted page/name — and a /GoToR action must still
    /// survive regardless of how /S is encoded.
    #[test]
    fn delete_scrubs_action_with_indirect_s_name() {
        let (mut doc, _cat, _pages, pids) = flat_doc(2);
        let (p1, p2) = (pids[0], pids[1]);

        // /S stored as an indirect reference to the name object `/GoTo`.
        let s_goto_id = doc.add_object(Object::Name(b"GoTo".to_vec()));
        let action = doc.add_object(dictionary! {
            "S" => Object::Reference(s_goto_id),
            "D" => goto_dest(p2),
        });
        let annot = doc.add_object(dictionary! {
            "Subtype" => "Link", "A" => Object::Reference(action),
        });

        // Control: /S is also indirect but names /GoToR — must NOT be stripped.
        let s_gotor_id = doc.add_object(Object::Name(b"GoToR".to_vec()));
        let remote_action = doc.add_object(dictionary! {
            "S" => Object::Reference(s_gotor_id),
            "F" => Object::string_literal("x.pdf"),
            "D" => Object::string_literal("foo"),
        });
        let remote_annot = doc.add_object(dictionary! {
            "Subtype" => "Link", "A" => Object::Reference(remote_action),
        });

        set_key(
            &mut doc,
            p1,
            "Annots",
            Object::Array(vec![Object::Reference(annot), Object::Reference(remote_annot)]),
        );

        delete_pages(&mut doc, &[2]).unwrap();

        assert!(
            doc.get_dictionary(annot).unwrap().get(b"A").is_err(),
            "local GoTo action with indirect /S must be scrubbed"
        );
        assert!(
            doc.get_dictionary(remote_annot).unwrap().get(b"A").is_ok(),
            "GoToR action with indirect /S must survive"
        );
        assert_eq!(refs_to(&doc, p2), 0);
        assert_no_dangling(&doc);
    }
}
