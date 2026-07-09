//! WASM bindings for pdfree-core. Everything runs in the browser; PDF bytes
//! never leave the user's machine.
//!
//! The document lives inside a `DocSession`: parse once, edit many times,
//! serialize only when the caller actually needs bytes (render refresh or
//! download). This is what makes edits feel instant — the old free-function
//! API reparsed and reserialized the whole file on every call.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct DocSession {
    doc: pdfree_core::lopdf::Document,
    fallback: Option<pdfree_core::TtfFont>,
}

#[wasm_bindgen]
impl DocSession {
    /// Parse a PDF (with xref salvage) and keep it resident.
    #[wasm_bindgen(constructor)]
    pub fn new(data: &[u8]) -> Result<DocSession, JsError> {
        let doc = pdfree_core::load_with_salvage_bytes(data).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(DocSession { doc, fallback: None })
    }

    pub fn page_count(&self) -> u32 {
        self.doc.get_pages().len() as u32
    }

    /// Provide TTF bytes used to synthesize glyphs the document lacks.
    /// Fetch lazily and set once per session.
    pub fn set_fallback_font(&mut self, bytes: Vec<u8>) -> Result<(), JsError> {
        self.fallback = Some(pdfree_core::TtfFont::parse(bytes).ok_or_else(|| JsError::new("bad fallback font"))?);
        Ok(())
    }

    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    /// All pages: {"pages": N, "runs": [...]}.
    pub fn extract_all(&self) -> Result<String, JsError> {
        let runs = pdfree_core::extract_runs(&self.doc).map_err(|e| JsError::new(&e.to_string()))?;
        serde_json::to_string(&serde_json::json!({
            "pages": self.doc.get_pages().len(),
            "runs": runs,
        }))
        .map_err(|e| JsError::new(&e.to_string()))
    }

    /// One page's runs: {"runs": [...]}.
    pub fn extract_page(&self, page: u32) -> Result<String, JsError> {
        let runs = pdfree_core::extract_runs_page(&self.doc, page);
        serde_json::to_string(&serde_json::json!({ "runs": runs })).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Replace in place; returns the report JSON. The session keeps the
    /// mutated document — call `save()` for fresh bytes.
    pub fn replace(&mut self, page: u32, find: &str, with_text: &str) -> Result<String, JsError> {
        let report = pdfree_core::replace_text(&mut self.doc, page, find, with_text, self.fallback.as_ref())
            .map_err(|e| JsError::new(&e.to_string()))?;
        serde_json::to_string(&report).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Serialize the current state.
    pub fn save(&mut self) -> Result<Vec<u8>, JsError> {
        let mut out = Vec::new();
        self.doc.save_to(&mut out).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(out)
    }
}
