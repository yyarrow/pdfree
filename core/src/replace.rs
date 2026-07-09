//! In-place text replacement: find a shown string on a page, re-encode the
//! replacement through the same font encoding, and rewrite the content stream.

use crate::ttf::TtfFont;
use crate::type3gen::build_type3_font;
use crate::walk::{load_fonts, walk_page, Seg};
use lopdf::content::Operation;
use lopdf::{Document, Object, ObjectId};

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
    #[error("font subset cannot represent the replacement text (missing glyph)")]
    MissingGlyph,
    #[error("unsupported show operator {0}")]
    UnsupportedOperator(String),
}

/// Replace the first occurrence of `find` on page `page_no` (1-based) with
/// `with`. The match must fall within a single shown string. When the
/// original font can't render the replacement and `fallback` is provided,
/// whole-segment edits are rendered in a synthesized Type3 fallback font.
pub fn replace_text(
    doc: &mut Document,
    page_no: u32,
    find: &str,
    with: &str,
    fallback: Option<&TtfFont>,
) -> Result<ReplaceReport, ReplaceError> {
    let pages = doc.get_pages();
    let page_id = *pages.get(&page_no).ok_or(ReplaceError::PageNotFound(page_no))?;

    let (mut content, segs) = walk_page(doc, page_id, page_no)?;

    let seg: &Seg = segs
        .iter()
        .find(|s| s.text.contains(find))
        .ok_or(ReplaceError::TextNotFound(segs.len()))?;

    let new_text = seg.text.replacen(find, with, 1);

    // Re-encode through the same font (ToUnicode-first, roundtrip-verified).
    let encoded = {
        let fonts = load_fonts(doc, page_id);
        let font = fonts.get(seg.font.as_bytes()).ok_or(ReplaceError::Unencodable)?;
        let result = font.encode(doc, &new_text).ok_or(ReplaceError::Unencodable).and_then(|bytes| {
            // Encoding proves the code points exist in the encoding, not that
            // the (often subsetted) font has glyphs for them. Require
            // verifiable metrics/procedures for every byte we introduce;
            // bytes already present in the original string obviously render.
            if !font.cid {
                for &b in &bytes {
                    if !seg.bytes.contains(&b) && !font.glyph_available(b) {
                        return Err(ReplaceError::MissingGlyph);
                    }
                }
            }
            Ok(bytes)
        });
        let old_adv = font.advance(&seg.bytes, seg.text.chars().count());
        result.map(|bytes| {
            let new_adv = font.advance(&bytes, new_text.chars().count());
            (bytes, old_adv, new_adv)
        })
        .map_err(|e| (e, old_adv))
    };

    let (new_bytes, old_adv, new_adv) = match encoded {
        Ok(v) => v,
        Err((e @ (ReplaceError::Unencodable | ReplaceError::MissingGlyph), old_adv)) => {
            // The document's font can't express the replacement — fall back
            // to a synthesized Type3 font when the whole segment is edited.
            let Some(ttf) = fallback else { return Err(e) };
            if find != seg.text {
                return Err(e);
            }
            let seg = seg.clone();
            return replace_with_fallback(doc, page_id, page_no, content, seg, with, old_adv, ttf);
        }
        Err((e, _)) => return Err(e),
    };
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

/// Whole-segment replacement rendered in a synthesized Type3 fallback font:
/// the original show op becomes Tf(fallback) TJ[(codes) comp] Tf(original),
/// so following ops keep their pen position exactly.
#[allow(clippy::too_many_arguments)]
fn replace_with_fallback(
    doc: &mut Document,
    page_id: ObjectId,
    page_no: u32,
    mut content: lopdf::content::Content,
    seg: Seg,
    with: &str,
    old_adv: f32,
    ttf: &TtfFont,
) -> Result<ReplaceReport, ReplaceError> {
    let op_kind = content.operations[seg.op_idx].operator.clone();
    // For a multi-string TJ, the untouched sibling elements stay in the
    // original font: the array is split around the edited element and the
    // halves become their own TJ ops (Tf between them doesn't move the pen).
    let (before, after) = match op_kind.as_str() {
        "Tj" | "'" | "\"" => (Vec::new(), Vec::new()),
        "TJ" => {
            let arr = content.operations[seg.op_idx].operands[0]
                .as_array()
                .map_err(ReplaceError::Pdf)?;
            let pos = arr
                .iter()
                .enumerate()
                .filter(|(_, o)| matches!(o, Object::String(..)))
                .map(|(i, _)| i)
                .nth(seg.str_idx)
                .ok_or_else(|| ReplaceError::UnsupportedOperator("TJ".into()))?;
            (arr[..pos].to_vec(), arr[pos + 1..].to_vec())
        }
        other => return Err(ReplaceError::UnsupportedOperator(other.to_string())),
    };

    let chars: Vec<char> = with.chars().collect();
    let fb = build_type3_font(doc, ttf, &chars).ok_or(ReplaceError::Unencodable)?;
    let res_name = add_font_resource(doc, page_id, fb.font_id)?;

    let codes: Vec<u8> = with.chars().map(|c| fb.codes[&c]).collect();
    let new_adv: f32 = with
        .chars()
        .map(|c| fb.advances[&c] / fb.units_per_em * 1000.0)
        .sum();
    let delta = new_adv - old_adv;

    let mut ops: Vec<Operation> = Vec::new();
    match op_kind.as_str() {
        "'" => ops.push(Operation::new("T*", vec![])),
        "\"" => {
            let o = &content.operations[seg.op_idx].operands;
            ops.push(Operation::new("Tw", vec![o[0].clone()]));
            ops.push(Operation::new("Tc", vec![o[1].clone()]));
            ops.push(Operation::new("T*", vec![]));
        }
        _ => {}
    }
    if !before.is_empty() {
        ops.push(Operation::new("TJ", vec![Object::Array(before)]));
    }
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(res_name.into_bytes()), Object::Real(seg.font_size)],
    ));
    let mut arr = vec![Object::String(codes, lopdf::StringFormat::Literal)];
    if delta.abs() > 0.01 {
        arr.push(Object::Real(delta));
    }
    ops.push(Operation::new("TJ", vec![Object::Array(arr)]));
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(seg.font.clone().into_bytes()), Object::Real(seg.font_size)],
    ));
    if !after.is_empty() {
        ops.push(Operation::new("TJ", vec![Object::Array(after)]));
    }

    content.operations.splice(seg.op_idx..seg.op_idx + 1, ops);
    let encoded = content.encode()?;
    doc.change_page_content(page_id, encoded)?;

    let mut bbox = seg.bbox;
    if old_adv > 0.0 && new_adv > old_adv {
        bbox[2] = bbox[0] + (bbox[2] - bbox[0]) * (new_adv / old_adv);
    }
    Ok(ReplaceReport {
        page: page_no,
        old_text: seg.text,
        new_text: with.to_string(),
        bbox,
    })
}

/// Register `font_id` under a fresh name in the page's /Resources /Font,
/// creating dictionaries as needed. Handles both inline and referenced
/// Resources/Font dicts (adding a key to a shared dict is harmless).
fn add_font_resource(doc: &mut Document, page_id: ObjectId, font_id: ObjectId) -> Result<String, ReplaceError> {
    enum Loc {
        Inline,
        Ref(ObjectId),
        Missing,
    }

    let (res_loc, font_loc, existing) = {
        let page = doc.get_object(page_id).map_err(ReplaceError::Pdf)?.as_dict().map_err(ReplaceError::Pdf)?;
        let res_loc = match page.get(b"Resources") {
            Ok(Object::Reference(id)) => Loc::Ref(*id),
            Ok(_) => Loc::Inline,
            Err(_) => Loc::Missing,
        };
        let res_dict = match &res_loc {
            Loc::Ref(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
            Loc::Inline => page.get(b"Resources").ok().and_then(|o| o.as_dict().ok()),
            Loc::Missing => None,
        };
        let font_loc = match res_dict.map(|r| r.get(b"Font")) {
            Some(Ok(Object::Reference(id))) => Loc::Ref(*id),
            Some(Ok(_)) => Loc::Inline,
            _ => Loc::Missing,
        };
        let existing: Vec<Vec<u8>> = match (&font_loc, res_dict) {
            (Loc::Ref(id), _) => doc
                .get_object(*id)
                .ok()
                .and_then(|o| o.as_dict().ok())
                .map(|d| d.iter().map(|(k, _)| k.clone()).collect())
                .unwrap_or_default(),
            (Loc::Inline, Some(r)) => r
                .get(b"Font")
                .ok()
                .and_then(|o| o.as_dict().ok())
                .map(|d| d.iter().map(|(k, _)| k.clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        (res_loc, font_loc, existing)
    };

    let mut n = 0;
    let name = loop {
        let candidate = format!("PFB{n}");
        if !existing.iter().any(|k| k == candidate.as_bytes()) {
            break candidate;
        }
        n += 1;
    };

    let font_ref = Object::Reference(font_id);
    match (res_loc, font_loc) {
        (_, Loc::Ref(fid)) => {
            let d = doc
                .get_object_mut(fid)
                .map_err(ReplaceError::Pdf)?
                .as_dict_mut()
                .map_err(ReplaceError::Pdf)?;
            d.set(name.as_bytes(), font_ref);
        }
        (Loc::Ref(rid), font_loc) => {
            let r = doc
                .get_object_mut(rid)
                .map_err(ReplaceError::Pdf)?
                .as_dict_mut()
                .map_err(ReplaceError::Pdf)?;
            match font_loc {
                Loc::Inline => {
                    if let Ok(f) = r.get_mut(b"Font").and_then(|o| o.as_dict_mut()) {
                        f.set(name.as_bytes(), font_ref);
                    }
                }
                _ => {
                    let mut f = lopdf::Dictionary::new();
                    f.set(name.as_bytes(), font_ref);
                    r.set(b"Font", Object::Dictionary(f));
                }
            }
        }
        (res_loc, font_loc) => {
            let page = doc
                .get_object_mut(page_id)
                .map_err(ReplaceError::Pdf)?
                .as_dict_mut()
                .map_err(ReplaceError::Pdf)?;
            match res_loc {
                Loc::Inline => {
                    let r = page
                        .get_mut(b"Resources")
                        .and_then(|o| o.as_dict_mut())
                        .map_err(ReplaceError::Pdf)?;
                    match font_loc {
                        Loc::Inline => {
                            if let Ok(f) = r.get_mut(b"Font").and_then(|o| o.as_dict_mut()) {
                                f.set(name.as_bytes(), font_ref);
                            }
                        }
                        _ => {
                            let mut f = lopdf::Dictionary::new();
                            f.set(name.as_bytes(), font_ref);
                            r.set(b"Font", Object::Dictionary(f));
                        }
                    }
                }
                _ => {
                    let mut f = lopdf::Dictionary::new();
                    f.set(name.as_bytes(), font_ref);
                    let mut r = lopdf::Dictionary::new();
                    r.set(b"Font", Object::Dictionary(f));
                    page.set(b"Resources", Object::Dictionary(r));
                }
            }
        }
    }
    Ok(name)
}
