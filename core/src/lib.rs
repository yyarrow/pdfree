//! pdfree-core: a permissively-licensed PDF text-editing engine.
//!
//! Built on lopdf for object-level parsing; this crate adds a content-stream
//! interpreter with position tracking, an editable text model (glyph → run
//! → line → block), and layout-preserving text replacement.

pub mod edit;
mod matrix;
pub mod metadata;
pub mod model;
pub mod replace;
pub mod salvage;
mod std14;
mod tounicode;
pub mod ttf;
pub mod type3gen;
pub mod walk;

pub use edit::replace_run_text;
pub use lopdf;
pub use metadata::{read_info, set_info, MetadataError};
pub use model::{build_page_model, MBlock};
pub use replace::{replace_text, ReplaceError, ReplaceReport};
pub use salvage::{load_with_salvage, load_with_salvage_bytes};
pub use ttf::TtfFont;
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

/// Extract text segments for a single page — after an edit only the touched
/// page needs re-walking, not the whole document.
pub fn extract_runs_page(doc: &Document, page_no: u32) -> Vec<Seg> {
    doc.get_pages()
        .get(&page_no)
        .and_then(|id| walk::walk_page(doc, *id, page_no).ok())
        .map(|(_, segs)| segs)
        .unwrap_or_default()
}

/// Build the editable text model for one page.
pub fn extract_model(doc: &Document, page_no: u32) -> Vec<MBlock> {
    build_page_model(&extract_runs_page(doc, page_no))
}
