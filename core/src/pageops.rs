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

/// Does an /A action target a deleted page? Only GoTo-style actions carry a
/// local /D destination; remote/URI actions never reference a page object.
fn action_hits(
    doc: &Document,
    aobj: &Object,
    deleted: &HashSet<ObjectId>,
    dead_names: &HashSet<Vec<u8>>,
) -> bool {
    match deref(doc, aobj) {
        Object::Dictionary(d) => d
            .get(b"D")
            .map(|dd| dest_hits(doc, dd, deleted, dead_names))
            .unwrap_or(false),
        _ => false,
    }
}

/// After pages are removed from the /Pages tree, scrub every remaining
/// reference to a deleted page's ObjectId: outline destinations, named
/// destinations, and link-annotation actions on surviving pages. A reference
/// to a nonexistent object is technically legal (resolves to null) but breaks
/// navigation and trips validators, so we neutralize it.
fn scrub_deleted_refs(doc: &mut Document, deleted: &HashSet<ObjectId>) -> Result<(), PageOpsError> {
    if deleted.is_empty() {
        return Ok(());
    }
    // Prune the destination name tree / legacy /Dests dict first; this also
    // tells us which named destinations died, so references-by-name elsewhere
    // can be neutralized too.
    let dead_names = scrub_named_dests(doc, deleted);
    scrub_outlines(doc, deleted, &dead_names);
    scrub_annots(doc, deleted, &dead_names);
    Ok(())
}

/// Prune entries whose destination targets a deleted page from the catalog's
/// destination name tree (`/Names /Dests`) and the legacy `/Dests` dictionary.
/// Removes the now-orphaned destination value objects too, and returns the set
/// of destination names that were pruned.
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
        let mut orphans: Vec<ObjectId> = Vec::new();
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
                if let Object::Reference(id) = val {
                    orphans.push(*id);
                }
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
        for o in orphans {
            doc.objects.remove(&o);
        }
    }

    // Legacy /Dests dict: remove dead keys and orphan their value objects.
    if let Some(id) = legacy_dests {
        let entries: Vec<(Vec<u8>, Object)> = match doc.get_object(id).and_then(Object::as_dict) {
            Ok(d) => d.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(_) => Vec::new(),
        };
        let mut orphans: Vec<ObjectId> = Vec::new();
        let mut dead_keys: Vec<Vec<u8>> = Vec::new();
        for (k, v) in &entries {
            if dest_page_ref(doc, v).map(|r| deleted.contains(&r)).unwrap_or(false) {
                dead_keys.push(k.clone());
                dead_names.insert(k.clone());
                if let Object::Reference(rid) = v {
                    orphans.push(*rid);
                }
            }
        }
        if let Ok(d) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
            for k in &dead_keys {
                d.remove(k);
            }
        }
        for o in orphans {
            doc.objects.remove(&o);
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

/// Collect every node reachable from the Outlines root via /First and /Next
/// (recursing into children). The root itself carries no destination, so
/// including it is harmless. Cycle-guarded by node id.
fn collect_outline_items(
    doc: &Document,
    node_id: ObjectId,
    out: &mut Vec<ObjectId>,
    visited: &mut HashSet<ObjectId>,
) {
    if !visited.insert(node_id) {
        return;
    }
    out.push(node_id);
    let dict = match doc.get_dictionary(node_id) {
        Ok(d) => d,
        Err(_) => return,
    };
    let first = dict.get(b"First").ok().and_then(|o| o.as_reference().ok());
    let next = dict.get(b"Next").ok().and_then(|o| o.as_reference().ok());
    if let Some(f) = first {
        collect_outline_items(doc, f, out, visited);
    }
    if let Some(n) = next {
        collect_outline_items(doc, n, out, visited);
    }
}

/// Neutralize link annotations on surviving pages whose /Dest or /A targets a
/// deleted page.
fn scrub_annots(doc: &mut Document, deleted: &HashSet<ObjectId>, dead_names: &HashSet<Vec<u8>>) {
    // get_pages() now returns only surviving pages.
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let mut annot_ids: Vec<ObjectId> = Vec::new();
    for pid in page_ids {
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
    for id in annot_ids {
        neutralize_item(doc, id, deleted, dead_names);
    }
}

/// Strip a /Dest and/or /A whose destination targets a deleted page from the
/// dict `id` (an outline item or annotation). Also removes an orphaned
/// destination/action object that was referenced solely from the stripped key.
fn neutralize_item(
    doc: &mut Document,
    id: ObjectId,
    deleted: &HashSet<ObjectId>,
    dead_names: &HashSet<Vec<u8>>,
) {
    let (strip_dest, strip_a, orphans) = {
        let dict = match doc.get_dictionary(id) {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut orphans: Vec<ObjectId> = Vec::new();
        let mut strip_dest = false;
        let mut strip_a = false;
        if let Ok(dest) = dict.get(b"Dest")
            && dest_hits(doc, dest, deleted, dead_names)
        {
            strip_dest = true;
            if let Object::Reference(rid) = dest {
                orphans.push(*rid);
            }
        }
        if let Ok(a) = dict.get(b"A")
            && action_hits(doc, a, deleted, dead_names)
        {
            strip_a = true;
            if let Object::Reference(rid) = a {
                orphans.push(*rid);
            }
        }
        (strip_dest, strip_a, orphans)
    };
    if strip_dest || strip_a {
        if let Ok(dict) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
            if strip_dest {
                dict.remove(b"Dest");
            }
            if strip_a {
                dict.remove(b"A");
            }
        }
        for o in orphans {
            doc.objects.remove(&o);
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
}
