//! In-place text replacement: find a shown string on a page, re-encode the
//! replacement through the same font encoding, and rewrite the content stream.

use crate::walk::{load_fonts, walk_page, Seg};
use lopdf::{Document, Object};

#[derive(Debug, serde::Serialize)]
pub struct ReplaceReport {
    pub page: u32,
    pub old_text: String,
    pub new_text: String,
    /// Region affected by the edit, in PDF user space (y up), already
    /// widened to cover whichever of old/new is wider.
    pub bbox: [f32; 4],
}

#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("page {0} not found")]
    PageNotFound(u32),
    #[error("text not found on page (searched {0} segments)")]
    TextNotFound(usize),
    #[error("font encoding cannot represent the replacement text")]
    Unencodable,
    #[error("unsupported show operator {0}")]
    UnsupportedOperator(String),
}

/// Replace the first occurrence of `find` on page `page_no` (1-based) with
/// `with`. The match must fall within a single shown string.
pub fn replace_text(
    doc: &mut Document,
    page_no: u32,
    find: &str,
    with: &str,
) -> Result<ReplaceReport, ReplaceError> {
    let pages = doc.get_pages();
    let page_id = *pages.get(&page_no).ok_or(ReplaceError::PageNotFound(page_no))?;

    let (mut content, segs) = walk_page(doc, page_id, page_no)?;

    let seg: &Seg = segs
        .iter()
        .find(|s| s.text.contains(find))
        .ok_or(ReplaceError::TextNotFound(segs.len()))?;

    let new_text = seg.text.replacen(find, with, 1);

    // Re-encode through the same font's encoding and verify the roundtrip:
    // if decoding the new bytes doesn't give back the new text, the font's
    // encoding (or its embedded subset) can't express the replacement.
    let (new_bytes, width_ratio) = {
        let fonts = load_fonts(doc, page_id);
        let font = fonts.get(seg.font.as_bytes()).ok_or(ReplaceError::Unencodable)?;
        let enc = font
            .dict
            .get_font_encoding(doc)
            .map_err(|_| ReplaceError::Unencodable)?;
        let bytes = Document::encode_text(&enc, &new_text);
        let roundtrip = Document::decode_text(&enc, &bytes).map_err(|_| ReplaceError::Unencodable)?;
        if roundtrip != new_text || bytes.is_empty() {
            return Err(ReplaceError::Unencodable);
        }
        let old_adv = font.advance(&seg.bytes, seg.text.chars().count());
        let new_adv = font.advance(&bytes, new_text.chars().count());
        (bytes, (old_adv, new_adv))
    };
    let (old_adv, new_adv) = width_ratio;
    let width_ratio = if old_adv > 0.0 { new_adv / old_adv } else { 1.0 };

    // Widen the reported bbox horizontally if the replacement renders wider
    // than the original (glyph advance widths, not byte counts).
    let mut bbox = seg.bbox;
    if width_ratio > 1.0 {
        bbox[2] = bbox[0] + (bbox[2] - bbox[0]) * width_ratio;
    }

    // Width delta in 1000ths of em. A positive TJ number shifts subsequent
    // text left by n/1000*Tfs, so inserting `delta` after the replaced string
    // keeps everything that follows in exactly its original position.
    let delta = new_adv - old_adv;
    let needs_comp = delta.abs() > 0.01;

    let op = &mut content.operations[seg.op_idx];
    let old_text = seg.text.clone();
    match op.operator.as_str() {
        // Tj repositions nothing afterwards by itself, but a following show op
        // without an intervening Td/Tm continues from the current pen, so we
        // rewrite Tj into a compensated TJ to be safe.
        "Tj" => {
            let mut arr = vec![Object::String(new_bytes, lopdf::StringFormat::Literal)];
            if needs_comp {
                arr.push(Object::Real(delta));
            }
            op.operator = "TJ".into();
            op.operands = vec![Object::Array(arr)];
        }
        // ' and " have line-advance side effects we can't fold into TJ; the
        // pen drift after them is accepted for now (rare in practice).
        "'" | "\"" => {
            let idx = if op.operator == "\"" { 2 } else { 0 };
            let target = op
                .operands
                .get_mut(idx)
                .ok_or_else(|| ReplaceError::UnsupportedOperator(op.operator.clone()))?;
            if let Object::String(bytes, _) = target {
                *bytes = new_bytes;
            }
        }
        "TJ" => {
            let arr = op.operands[0].as_array_mut().map_err(ReplaceError::Pdf)?;
            let pos = arr
                .iter()
                .enumerate()
                .filter(|(_, o)| matches!(o, Object::String(..)))
                .map(|(i, _)| i)
                .nth(seg.str_idx)
                .ok_or_else(|| ReplaceError::UnsupportedOperator("TJ".into()))?;
            if let Object::String(bytes, _) = &mut arr[pos] {
                *bytes = new_bytes;
            }
            if needs_comp {
                // Fold into an existing kerning number if one follows.
                if let Some(next) = arr.get_mut(pos + 1) {
                    if let Ok(v) = next.as_float() {
                        *next = Object::Real(v + delta);
                    } else {
                        arr.insert(pos + 1, Object::Real(delta));
                    }
                } else {
                    arr.push(Object::Real(delta));
                }
            }
        }
        other => return Err(ReplaceError::UnsupportedOperator(other.to_string())),
    }

    let encoded = content.encode()?;
    doc.change_page_content(page_id, encoded)?;

    Ok(ReplaceReport {
        page: page_no,
        old_text,
        new_text,
        bbox,
    })
}
