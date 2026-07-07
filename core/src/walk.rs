//! Content-stream interpreter: walks text-showing operators while tracking
//! the graphics/text state, yielding one segment per shown string with an
//! approximate bounding box in PDF user space.

use crate::matrix::Mat;
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object};
use std::collections::BTreeMap;

/// One shown string (a Tj operand, or one string element of a TJ array).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Seg {
    pub page: u32,
    /// Index of the operation in the page's decoded content stream.
    pub op_idx: usize,
    /// For TJ: index of this string among the array's string elements. 0 otherwise.
    pub str_idx: usize,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub text: String,
    pub font: String,
    pub font_size: f32,
    /// x0, y0, x1, y1 in PDF user space (y up).
    pub bbox: [f32; 4],
    /// True when the font is a Type0/CID font (multi-byte codes).
    pub cid: bool,
}

pub struct FontInfo<'a> {
    pub dict: &'a Dictionary,
    pub cid: bool,
    /// Per-byte-code advance widths (simple fonts only), in 1000ths of em.
    pub widths: Option<(i64, Vec<f32>)>, // (FirstChar, widths)
    /// Standard-14 metrics used when the font dict carries no Widths.
    pub std_widths: Option<&'static [f32; 256]>,
    pub default_width: f32,
}

/// Map a BaseFont name (possibly subset-prefixed or an Arial/TimesNewRoman
/// style alias) onto standard-14 metrics.
fn std14_lookup(base_font: &[u8]) -> Option<&'static [f32; 256]> {
    let s = String::from_utf8_lossy(base_font);
    let s = s.rsplit('+').next().unwrap_or(&s); // strip "ABCDEF+" subset tag
    let lower = s.to_ascii_lowercase();
    let bold = lower.contains("bold");
    let italic = lower.contains("italic") || lower.contains("oblique");
    let name = if lower.contains("courier") || lower.contains("mono") {
        match (bold, italic) {
            (false, false) => "Courier",
            (true, false) => "Courier-Bold",
            (false, true) => "Courier-Oblique",
            (true, true) => "Courier-BoldOblique",
        }
    } else if lower.contains("times") {
        match (bold, italic) {
            (false, false) => "Times-Roman",
            (true, false) => "Times-Bold",
            (false, true) => "Times-Italic",
            (true, true) => "Times-BoldItalic",
        }
    } else if lower.contains("helvetica") || lower.contains("arial") {
        match (bold, italic) {
            (false, false) => "Helvetica",
            (true, false) => "Helvetica-Bold",
            (false, true) => "Helvetica-Oblique",
            (true, true) => "Helvetica-BoldOblique",
        }
    } else if lower.contains("zapf") || lower.contains("dingbat") {
        "ZapfDingbats"
    } else if lower == "symbol" {
        "Symbol"
    } else {
        return None;
    };
    crate::std14::std14_widths(name)
}

impl<'a> FontInfo<'a> {
    pub fn advance(&self, bytes: &[u8], decoded_chars: usize) -> f32 {
        if self.cid {
            // v0: assume 1000/1000 em per decoded char (typical for CJK).
            return decoded_chars as f32 * 1000.0;
        }
        let mut total = 0.0;
        for &b in bytes {
            let w = self
                .widths
                .as_ref()
                .and_then(|(first, ws)| ws.get((b as i64 - first).max(0) as usize).copied())
                .filter(|w| *w > 0.0)
                .or_else(|| self.std_widths.map(|ws| ws[b as usize]).filter(|w| *w > 0.0))
                .unwrap_or(self.default_width);
            total += w;
        }
        total
    }

    pub fn decode(&self, doc: &Document, bytes: &[u8]) -> String {
        match self.dict.get_font_encoding(doc) {
            Ok(enc) => Document::decode_text(&enc, bytes).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }
}

pub fn load_fonts<'a>(doc: &'a Document, page_id: lopdf::ObjectId) -> BTreeMap<Vec<u8>, FontInfo<'a>> {
    let mut out = BTreeMap::new();
    let Ok(fonts) = doc.get_page_fonts(page_id) else {
        return out;
    };
    for (name, dict) in fonts {
        let subtype = dict
            .get(b"Subtype")
            .and_then(|o| o.as_name())
            .map(|n| n.to_vec())
            .unwrap_or_default();
        let cid = subtype == b"Type0";
        let widths = (|| {
            let first = doc.dereference(dict.get(b"FirstChar").ok()?).ok()?.1.as_i64().ok()?;
            let arr = doc.dereference(dict.get(b"Widths").ok()?).ok()?.1.as_array().ok()?.clone();
            let ws: Vec<f32> = arr
                .iter()
                .map(|o| doc.dereference(o).ok().and_then(|(_, o)| o.as_float().ok()).unwrap_or(0.0))
                .collect();
            Some((first, ws))
        })();
        let std_widths = if cid {
            None
        } else {
            dict.get(b"BaseFont")
                .and_then(|o| o.as_name())
                .ok()
                .and_then(std14_lookup)
        };
        out.insert(
            name,
            FontInfo {
                dict,
                cid,
                widths,
                std_widths,
                default_width: 500.0,
            },
        );
    }
    out
}

fn op_f32(op: &Object) -> f32 {
    op.as_float().unwrap_or(0.0)
}

/// Walk one page's content stream and return the decoded content plus all
/// text segments. `page_no` is the 1-based page number used for labeling.
pub fn walk_page(doc: &Document, page_id: lopdf::ObjectId, page_no: u32) -> lopdf::Result<(Content, Vec<Seg>)> {
    let data = doc.get_page_content(page_id)?;
    let content = Content::decode(&data)?;
    let fonts = load_fonts(doc, page_id);

    let mut segs = Vec::new();

    let mut ctm = Mat::identity();
    let mut ctm_stack: Vec<Mat> = Vec::new();
    let mut tm = Mat::identity(); // text matrix
    let mut tlm = Mat::identity(); // text line matrix
    let mut font_key: Vec<u8> = Vec::new();
    let mut font_size: f32 = 0.0;
    let mut leading: f32 = 0.0;
    let mut char_spacing: f32 = 0.0;
    let mut word_spacing: f32 = 0.0;
    let mut h_scale: f32 = 1.0;

    for (op_idx, op) in content.operations.iter().enumerate() {
        let ops = &op.operands;
        match op.operator.as_str() {
            "q" => ctm_stack.push(ctm),
            "Q" => {
                if let Some(m) = ctm_stack.pop() {
                    ctm = m;
                }
            }
            "cm" if ops.len() == 6 => {
                let m = Mat([
                    op_f32(&ops[0]),
                    op_f32(&ops[1]),
                    op_f32(&ops[2]),
                    op_f32(&ops[3]),
                    op_f32(&ops[4]),
                    op_f32(&ops[5]),
                ]);
                ctm = m.mul(&ctm);
            }
            "BT" => {
                tm = Mat::identity();
                tlm = Mat::identity();
            }
            "Tf" if ops.len() == 2 => {
                font_key = ops[0].as_name().map(|n| n.to_vec()).unwrap_or_default();
                font_size = op_f32(&ops[1]);
            }
            "TL" if ops.len() == 1 => leading = op_f32(&ops[0]),
            "Tc" if ops.len() == 1 => char_spacing = op_f32(&ops[0]),
            "Tw" if ops.len() == 1 => word_spacing = op_f32(&ops[0]),
            "Tz" if ops.len() == 1 => h_scale = op_f32(&ops[0]) / 100.0,
            "Td" if ops.len() == 2 => {
                tlm = Mat::translate(op_f32(&ops[0]), op_f32(&ops[1])).mul(&tlm);
                tm = tlm;
            }
            "TD" if ops.len() == 2 => {
                leading = -op_f32(&ops[1]);
                tlm = Mat::translate(op_f32(&ops[0]), op_f32(&ops[1])).mul(&tlm);
                tm = tlm;
            }
            "Tm" if ops.len() == 6 => {
                tm = Mat([
                    op_f32(&ops[0]),
                    op_f32(&ops[1]),
                    op_f32(&ops[2]),
                    op_f32(&ops[3]),
                    op_f32(&ops[4]),
                    op_f32(&ops[5]),
                ]);
                tlm = tm;
            }
            "T*" => {
                tlm = Mat::translate(0.0, -leading).mul(&tlm);
                tm = tlm;
            }
            "Tj" | "'" | "\"" => {
                if op.operator == "'" || op.operator == "\"" {
                    tlm = Mat::translate(0.0, -leading).mul(&tlm);
                    tm = tlm;
                }
                // For ", operands are [aw ac string]; for Tj and ' it's [string].
                let (s_op, s_idx) = if op.operator == "\"" {
                    (ops.get(2), 2)
                } else {
                    (ops.get(0), 0)
                };
                if op.operator == "\"" {
                    word_spacing = op_f32(&ops[0]);
                    char_spacing = op_f32(&ops[1]);
                }
                if let Some(Object::String(bytes, _)) = s_op {
                    let _ = s_idx;
                    show_string(
                        doc, &fonts, &font_key, font_size, char_spacing, word_spacing, h_scale, &ctm, &mut tm,
                        bytes, page_no, op_idx, 0, &mut segs,
                    );
                }
            }
            "TJ" if ops.len() == 1 => {
                if let Ok(arr) = ops[0].as_array() {
                    let mut str_idx = 0;
                    for el in arr {
                        match el {
                            Object::String(bytes, _) => {
                                show_string(
                                    doc, &fonts, &font_key, font_size, char_spacing, word_spacing, h_scale, &ctm,
                                    &mut tm, bytes, page_no, op_idx, str_idx, &mut segs,
                                );
                                str_idx += 1;
                            }
                            _ => {
                                let adj = el.as_float().unwrap_or(0.0);
                                let tx = -adj / 1000.0 * font_size * h_scale;
                                tm = Mat::translate(tx, 0.0).mul(&tm);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok((content, segs))
}

#[allow(clippy::too_many_arguments)]
fn show_string(
    doc: &Document,
    fonts: &BTreeMap<Vec<u8>, FontInfo>,
    font_key: &[u8],
    font_size: f32,
    char_spacing: f32,
    word_spacing: f32,
    h_scale: f32,
    ctm: &Mat,
    tm: &mut Mat,
    bytes: &[u8],
    page_no: u32,
    op_idx: usize,
    str_idx: usize,
    segs: &mut Vec<Seg>,
) {
    let font = fonts.get(font_key);
    let (text, cid, advance_em) = match font {
        Some(f) => {
            let text = f.decode(doc, bytes);
            let adv = f.advance(bytes, text.chars().count());
            (text, f.cid, adv)
        }
        None => (String::new(), false, bytes.len() as f32 * 500.0),
    };

    let n_units = if cid { text.chars().count() } else { bytes.len() };
    let n_spaces = if cid { 0 } else { bytes.iter().filter(|&&b| b == b' ').count() };
    let width_text_space = advance_em / 1000.0 * font_size * h_scale
        + n_units as f32 * char_spacing * h_scale
        + n_spaces as f32 * word_spacing * h_scale;

    let trm = tm.mul(ctm);
    let (x0, y0) = trm.apply(0.0, -0.25 * font_size);
    let (x1a, y1a) = trm.apply(width_text_space, font_size);
    let bbox = [x0.min(x1a), y0.min(y1a), x0.max(x1a), y0.max(y1a)];

    if !text.trim().is_empty() {
        segs.push(Seg {
            page: page_no,
            op_idx,
            str_idx,
            bytes: bytes.to_vec(),
            text,
            font: String::from_utf8_lossy(font_key).into_owned(),
            font_size,
            bbox,
            cid,
        });
    }

    *tm = Mat::translate(width_text_space, 0.0).mul(tm);
}
