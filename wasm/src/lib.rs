//! WASM bindings for pdfree-core. Everything runs in the browser; PDF bytes
//! never leave the user's machine.

use wasm_bindgen::prelude::*;

/// Extract text segments with positions. Returns the same JSON shape as the
/// CLI: {"pages": N, "runs": [{page, text, font, font_size, bbox, ...}]}.
#[wasm_bindgen]
pub fn extract(data: &[u8]) -> Result<String, JsError> {
    let doc = pdfree_core::load_with_salvage_bytes(data).map_err(|e| JsError::new(&e.to_string()))?;
    let runs = pdfree_core::extract_runs(&doc).map_err(|e| JsError::new(&e.to_string()))?;
    let pages = doc.get_pages().len();
    serde_json::to_string(&serde_json::json!({ "pages": pages, "runs": runs }))
        .map_err(|e| JsError::new(&e.to_string()))
}

#[wasm_bindgen]
pub struct ReplaceResult {
    pdf: Vec<u8>,
    report: String,
}

#[wasm_bindgen]
impl ReplaceResult {
    #[wasm_bindgen(getter)]
    pub fn pdf(&self) -> Vec<u8> {
        self.pdf.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn report(&self) -> String {
        self.report.clone()
    }
}

/// Replace the first occurrence of `find` on 1-based page `page`.
#[wasm_bindgen]
pub fn replace(data: &[u8], page: u32, find: &str, with_text: &str) -> Result<ReplaceResult, JsError> {
    let mut doc = pdfree_core::load_with_salvage_bytes(data).map_err(|e| JsError::new(&e.to_string()))?;
    let report = pdfree_core::replace_text(&mut doc, page, find, with_text)
        .map_err(|e| JsError::new(&e.to_string()))?;
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(ReplaceResult {
        pdf: out,
        report: serde_json::to_string(&report).unwrap_or_default(),
    })
}
