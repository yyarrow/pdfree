//! Fallback loader for PDFs whose cross-reference table is broken (bad
//! entries, `startxref` pointing past EOF, or the keyword missing entirely).
//!
//! lopdf has no recovery mode for this, so we implement the same trick qpdf
//! and pdfium use conceptually: ignore the xref/trailer entirely, scan the
//! raw bytes for every `N G obj` header, and synthesize a fresh conventional
//! xref table + trailer pointing at what we found. This is written purely
//! against the byte patterns described below and ISO 32000 (7.5.4 / 7.5.5);
//! no GPL/AGPL PDF source was consulted.
//!
//! Only lopdf's own grammar (src/parser/mod.rs `xref`/`trailer`) matters for
//! what we emit, since we feed the result straight back into
//! `Document::load_mem`.
//!
//! Known limitation: the rebuilt trailer carries /Root and /Info but not
//! /Encrypt (decryption would also need the original /ID, which is rarely
//! recoverable from a file this broken), so salvaged encrypted documents
//! yield undecrypted strings rather than failing outright.

use std::collections::BTreeMap;
use std::path::Path;

/// Try the normal loader first; on any failure, rebuild an xref/trailer from
/// scratch by scanning for object headers and retry from an in-memory copy.
pub fn load_with_salvage(path: &Path) -> lopdf::Result<lopdf::Document> {
    let original_err = match lopdf::Document::load(path) {
        Ok(doc) => return Ok(doc),
        Err(e) => e,
    };

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Err(original_err),
    };
    salvage_after_failure(&data, original_err)
}

/// In-memory variant for callers without a filesystem (the WASM build).
pub fn load_with_salvage_bytes(data: &[u8]) -> lopdf::Result<lopdf::Document> {
    let original_err = match lopdf::Document::load_mem(data) {
        Ok(doc) => return Ok(doc),
        Err(e) => e,
    };
    salvage_after_failure(data, original_err)
}

fn salvage_after_failure(data: &[u8], original_err: lopdf::Error) -> lopdf::Result<lopdf::Document> {
    // lopdf's own reader locates "%PDF-" anywhere in the buffer and treats
    // everything before it as not part of the document, rebasing all offsets
    // to that point. Do the same before computing our own offsets, or they'd
    // be off by the length of whatever junk precedes the header.
    let header_pos = find_first(data, b"%PDF-").unwrap_or(0);
    let data = &data[header_pos..];

    match rebuild(data) {
        Some(repaired) => lopdf::Document::load_mem(&repaired),
        None => Err(original_err),
    }
}

// PDF whitespace and delimiter sets (ISO 32000 7.2.2/7.2.3).
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00)
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

fn is_boundary(b: u8) -> bool {
    is_ws(b) || is_delim(b)
}

/// All (non-overlapping, left-to-right) positions of `needle` in `haystack`.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

fn find_first(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scan the whole file for `<num> <generation> obj` headers, returning
/// (object_number, generation, byte_offset_of_the_num_token).
///
/// Requires whitespace directly before "obj" and a delimiter/whitespace/EOF
/// right after it, so this can't match inside "endobj". Object number and
/// generation are validated as small ASCII integers to reject accidental
/// matches inside binary stream data from blowing up the rebuilt xref table.
fn find_obj_headers(data: &[u8]) -> Vec<(u32, u32, usize)> {
    const MAX_OBJ_NUM: u32 = 9_999_999;
    const MAX_GEN: u32 = 65_535;

    let mut out = Vec::new();
    let n = data.len();
    let mut i = 0;
    while i + 3 <= n {
        if &data[i..i + 3] == b"obj" {
            let before_ok = i > 0 && is_ws(data[i - 1]);
            let after_ok = i + 3 == n || is_boundary(data[i + 3]);
            if before_ok && after_ok {
                if let Some((num, generation, start)) = parse_header_backwards(data, i) {
                    if num <= MAX_OBJ_NUM && generation <= MAX_GEN {
                        out.push((num, generation, start));
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// Given the position of the "obj" keyword, walk backwards over
/// "<ws>* <num> <ws>+ <generation> <ws>+" and return (num, generation, offset_of_num).
fn parse_header_backwards(data: &[u8], obj_pos: usize) -> Option<(u32, u32, usize)> {
    let mut j = obj_pos;
    while j > 0 && is_ws(data[j - 1]) {
        j -= 1;
    }
    let gen_end = j;
    let mut k = gen_end;
    while k > 0 && data[k - 1].is_ascii_digit() {
        k -= 1;
    }
    if k == gen_end {
        return None;
    }
    let gen_start = k;

    let mut m = gen_start;
    while m > 0 && is_ws(data[m - 1]) {
        m -= 1;
    }
    if m == gen_start {
        return None; // num and generation must be separated by whitespace
    }
    let num_end = m;
    let mut p = num_end;
    while p > 0 && data[p - 1].is_ascii_digit() {
        p -= 1;
    }
    if p == num_end {
        return None;
    }
    let num_start = p;

    if num_start != 0 && !is_boundary(data[num_start - 1]) {
        return None; // e.g. would otherwise match the tail of a larger number
    }

    let num: u32 = std::str::from_utf8(&data[num_start..num_end]).ok()?.parse().ok()?;
    let generation: u32 = std::str::from_utf8(&data[gen_start..gen_end]).ok()?.parse().ok()?;
    Some((num, generation, num_start))
}

/// Find `/Key <num> <generation> R` anywhere in `window`, returning the first match.
fn find_indirect_ref(window: &[u8], key: &[u8]) -> Option<(u32, u32)> {
    for pos in find_all(window, key) {
        let after_key = pos + key.len();
        if after_key < window.len() && !is_boundary(window[after_key]) {
            continue; // e.g. "/Info" must not be a prefix of "/InfoDict"
        }
        let mut idx = after_key;
        while idx < window.len() && is_ws(window[idx]) {
            idx += 1;
        }
        let num_start = idx;
        while idx < window.len() && window[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == num_start {
            continue;
        }
        let num_end = idx;
        while idx < window.len() && is_ws(window[idx]) {
            idx += 1;
        }
        if idx == num_end {
            continue;
        }
        let gen_start = idx;
        while idx < window.len() && window[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == gen_start {
            continue;
        }
        let gen_end = idx;
        while idx < window.len() && is_ws(window[idx]) {
            idx += 1;
        }
        if idx >= window.len() || window[idx] != b'R' {
            continue;
        }
        let after_r = idx + 1;
        if after_r < window.len() && !is_boundary(window[after_r]) {
            continue;
        }
        let num: u32 = std::str::from_utf8(&window[num_start..num_end]).ok()?.parse().ok()?;
        let generation: u32 = std::str::from_utf8(&window[gen_start..gen_end]).ok()?.parse().ok()?;
        return Some((num, generation));
    }
    None
}

/// Take the last `trailer` keyword in the file whose following dictionary
/// text contains a `/Root n g R`; also pick up `/Info` from the same window
/// if present. Deliberately does not require `<< >>` delimiters, since some
/// corpus files have a `trailer` keyword whose dict markers were stripped.
fn find_root_and_info(data: &[u8]) -> Option<((u32, u32), Option<(u32, u32)>)> {
    const WINDOW: usize = 4096;

    for pos in find_all(data, b"trailer").into_iter().rev() {
        let before_ok = pos == 0 || is_boundary(data[pos - 1]);
        let after = pos + b"trailer".len();
        let after_ok = after >= data.len() || is_boundary(data[after]);
        if !before_ok || !after_ok {
            continue;
        }
        let window_end = (after + WINDOW).min(data.len());
        let window = &data[after..window_end];
        if let Some(root) = find_indirect_ref(window, b"/Root") {
            let info = find_indirect_ref(window, b"/Info");
            return Some((root, info));
        }
    }
    None
}

/// Last-resort: find an object whose body has `/Type` immediately (modulo
/// whitespace) followed by `/Catalog`. Among candidates, pick the one at the
/// highest byte offset, matching the "later definition wins" rule used for
/// the object table itself.
fn find_catalog_object(data: &[u8], objects: &BTreeMap<u32, (u32, usize)>) -> Option<(u32, u32)> {
    const BODY_CAP: usize = 20_000;

    let mut best: Option<(u32, u32, usize)> = None;
    for (&num, &(generation, offset)) in objects {
        let scan_end = (offset + BODY_CAP).min(data.len());
        let body_end = find_first(&data[offset..scan_end], b"endobj")
            .map(|p| offset + p)
            .unwrap_or(scan_end);
        let body = &data[offset..body_end];
        if has_type_catalog(body) && best.is_none_or(|(_, _, o)| offset > o) {
            best = Some((num, generation, offset));
        }
    }
    best.map(|(num, generation, _)| (num, generation))
}

fn has_type_catalog(body: &[u8]) -> bool {
    for pos in find_all(body, b"/Type") {
        let after = pos + b"/Type".len();
        if after < body.len() && !is_boundary(body[after]) {
            continue;
        }
        let mut idx = after;
        while idx < body.len() && is_ws(body[idx]) {
            idx += 1;
        }
        if body[idx..].starts_with(b"/Catalog") {
            let end = idx + b"/Catalog".len();
            if end >= body.len() || is_boundary(body[end]) {
                return true;
            }
        }
    }
    false
}

/// Cap on the highest object number we'll build a table for, so a spurious
/// match (e.g. a huge number inside binary stream data mistaken for an object
/// header, or a corrupt `/Root` reference) can't blow up the free-entry
/// padding in `build_repaired`.
const MAX_TABLE_SIZE: u32 = 5_000_000;

fn rebuild(data: &[u8]) -> Option<Vec<u8>> {
    let mut objects: BTreeMap<u32, (u32, usize)> = BTreeMap::new();
    for (num, generation, offset) in find_obj_headers(data) {
        // Later occurrence wins: incremental updates append newer definitions.
        objects.insert(num, (generation, offset));
    }
    objects.remove(&0); // object 0 is reserved for the free-list head

    if objects.is_empty() {
        return None;
    }

    let (root, info) = match find_root_and_info(data) {
        Some((root, info)) => (root, info),
        None => (find_catalog_object(data, &objects)?, None),
    };

    let max_num = objects
        .keys()
        .next_back()
        .copied()
        .unwrap_or(0)
        .max(root.0)
        .max(info.map_or(0, |(n, _)| n));
    if max_num > MAX_TABLE_SIZE {
        return None;
    }

    Some(build_repaired(data, &objects, max_num, root, info))
}

/// Append a fresh `xref` + `trailer` + `startxref` to `data` covering object
/// numbers `0..=max_num`. Numbers missing from `objects` (gaps in the
/// numbering) are emitted as free entries, so this is always a single
/// contiguous subsection starting at 0.
fn build_repaired(
    data: &[u8], objects: &BTreeMap<u32, (u32, usize)>, max_num: u32, root: (u32, u32), info: Option<(u32, u32)>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + (max_num as usize + 1) * 20 + 128);
    out.extend_from_slice(data);
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }

    let xref_offset = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_num + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f\r\n"); // object 0: free-list head
    for num in 1..=max_num {
        match objects.get(&num) {
            Some(&(generation, offset)) => {
                out.extend_from_slice(format!("{offset:010} {generation:05} n\r\n").as_bytes());
            }
            None => out.extend_from_slice(b"0000000000 65535 f\r\n"),
        }
    }

    out.extend_from_slice(format!("trailer\n<< /Size {} /Root {} {} R", max_num + 1, root.0, root.1).as_bytes());
    if let Some((inum, igen)) = info {
        out.extend_from_slice(format!(" /Info {inum} {igen} R").as_bytes());
    }
    out.extend_from_slice(b" >>\n");
    out.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF").as_bytes());
    out
}
