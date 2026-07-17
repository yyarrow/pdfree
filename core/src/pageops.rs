//! Page-tree transforms: rotate, delete, reorder pages in place on a single
//! document. Operates on lopdf's own /Pages tree structure (ISO 32000-1
//! 7.7.3) — no borrowed rendering logic, clean-room against the spec only.

use crate::replace::reject_encrypted;
use lopdf::{Dictionary, Document, Object, ObjectId};

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
    let raw = find_inherited(doc, page_id, b"Rotate")
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0);
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
    let page_map = doc.get_pages();
    let num_pages = page_map.len() as u32;
    let mut ids = Vec::with_capacity(pages.len());
    for &p in pages {
        let id = *page_map.get(&p).ok_or(PageOpsError::OutOfRange(p, num_pages))?;
        ids.push(id);
    }
    for id in ids {
        let current = effective_rotation(doc, id);
        let new_rotation = normalize_rotation(current + degrees);
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
    let ids: Vec<ObjectId> = to_delete.iter().map(|p| page_map[p]).collect();
    for id in ids {
        remove_page(doc, id)?;
    }
    Ok(())
}

fn remove_page(doc: &mut Document, page_id: ObjectId) -> Result<(), PageOpsError> {
    let parent_id = doc
        .get_dictionary(page_id)?
        .get(b"Parent")
        .and_then(Object::as_reference)
        .map_err(|_| PageOpsError::Malformed("page missing /Parent"))?;
    remove_kid_and_prune(doc, parent_id, page_id)?;
    doc.objects.remove(&page_id);
    Ok(())
}

/// Remove `child_id` from `parent_id`'s /Kids, decrement /Count, and if the
/// parent is left with no children, prune it from its own parent too
/// (recursively) — unless it's the page-tree root, which we always keep.
fn remove_kid_and_prune(doc: &mut Document, parent_id: ObjectId, child_id: ObjectId) -> Result<(), PageOpsError> {
    let (remaining, grandparent) = {
        let parent_dict = doc
            .get_object_mut(parent_id)
            .and_then(Object::as_dict_mut)
            .map_err(|_| PageOpsError::Malformed("missing parent Pages dict"))?;
        let kids = parent_dict
            .get_mut(b"Kids")
            .and_then(Object::as_array_mut)
            .map_err(|_| PageOpsError::Malformed("Pages node missing /Kids"))?;
        let before = kids.len();
        kids.retain(|o| o.as_reference().map(|r| r != child_id).unwrap_or(true));
        let remaining = kids.len();
        let count = parent_dict.get(b"Count").and_then(Object::as_i64).unwrap_or(before as i64);
        parent_dict.set("Count", (count - 1).max(0));
        let gp = parent_dict.get(b"Parent").and_then(Object::as_reference).ok();
        (remaining, gp)
    };
    if remaining == 0 {
        if let Some(gp) = grandparent {
            remove_kid_and_prune(doc, gp, parent_id)?;
            doc.objects.remove(&parent_id);
        }
        // No grandparent means `parent_id` is the tree root; leave it be
        // (unreachable in practice since we refuse deleting every page).
    }
    Ok(())
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
