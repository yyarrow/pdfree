//! pdfree-core: a permissively-licensed PDF text-editing engine.
//!
//! Built on lopdf for object-level parsing; this crate adds a content-stream
//! interpreter with position tracking, text extraction, and in-place text
//! replacement that preserves layout.

mod matrix;
pub mod replace;
mod std14;
pub mod walk;

pub use replace::{replace_text, ReplaceError, ReplaceReport};
pub use walk::Seg;

use lopdf::Document;

/// Extract all text segments from every page, with positions.
pub fn extract_runs(doc: &Document) -> lopdf::Result<Vec<Seg>> {
    let mut out = Vec::new();
    for (page_no, page_id) in doc.get_pages() {
        // Skip pages we can't parse rather than failing the whole document.
        if let Ok((_, mut segs)) = walk::walk_page(doc, page_id, page_no) {
            out.append(&mut segs);
        }
    }
    Ok(out)
}
