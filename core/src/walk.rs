//! Content-stream interpreter: walks text-showing operators while tracking
//! the graphics/text state, yielding one segment per shown string with an
//! approximate bounding box in PDF user space.

use crate::matrix::Mat;
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object};
use std::collections::BTreeMap;

/// One positioned glyph (one byte code, or a 2-byte code for CID fonts).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Glyph {
    /// Decoded character(s) for this code.
    pub text: String,
    /// Byte range within the owning segment's string.
    pub byte_start: usize,
    pub byte_len: usize,
    /// Baseline origin in PDF user space.
    pub x: f32,
    pub y: f32,
    /// Advance to the next glyph's origin, user space (x direction).
    pub w: f32,
}

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
    /// False for invisible text (render mode 3, e.g. OCR layers).
    pub visible: bool,
    /// True for Type3 fonts, whose glyphs are inline drawing procedures;
    /// editing them safely needs CharProcs verification we don't do yet.
    pub type3: bool,
    /// Fill color as approximate RGB (0..1 each).
    pub color: [f32; 3],
    /// True when the text matrix has no rotation/skew component.
    pub horizontal: bool,
    #[serde(skip)]
    pub glyphs: Vec<Glyph>,
    /// Full text-rendering matrix (Tm x CTM) at the segment's start — the
    /// anchor for regenerating this segment's ops with absolute positions.
    #[serde(skip)]
    pub trm: [f32; 6],
    /// CTM at the segment, for converting user-space targets back to Tm.
    #[serde(skip)]
    pub ctm: [f32; 6],
    /// Text line matrix AFTER this segment's op completed — the state the
    /// following operations' Td/TD/T* derive from.
    #[serde(skip)]
    pub tlm_after: [f32; 6],
    /// Text-spacing state active for this segment (needed to reproduce its
    /// exact advance when regenerating): char spacing, word spacing,
    /// horizontal scale (Tz/100).
    #[serde(skip)]
    pub char_spacing: f32,
    #[serde(skip)]
    pub word_spacing: f32,
    #[serde(skip)]
    pub h_scale: f32,
    /// True when the fill color came from a pattern/separation space we
    /// only approximated (scn with a name operand) — such runs can't be
    /// faithfully re-emitted as rg.
    pub pattern_fill: bool,
}

/// Graphics state saved/restored by q/Q — the CTM plus the text state set
/// by Tf/TL/Tc/Tw/Tz/Tr and the fill color.
#[derive(Clone)]
struct GfxState {
    ctm: Mat,
    font_key: Vec<u8>,
    font_size: f32,
    leading: f32,
    char_spacing: f32,
    word_spacing: f32,
    h_scale: f32,
    render_mode: i64,
    fill_color: [f32; 3],
    pattern_fill: bool,
    /// True while the nonstroking color space (set by `cs`) is a plain
    /// device space we can reproduce with rg/g/k. Separation/DeviceN/Pattern/
    /// ICC set this false, so any scn under them is flagged unreproducible.
    fill_device_cs: bool,
}

impl GfxState {
    fn new() -> Self {
        GfxState {
            ctm: Mat::identity(),
            font_key: Vec::new(),
            font_size: 0.0,
            leading: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            h_scale: 1.0,
            render_mode: 0,
            fill_color: [0.0, 0.0, 0.0],
            pattern_fill: false,
            fill_device_cs: true,
        }
    }
}

fn cmyk_to_rgb(c: f32, m: f32, y: f32, k: f32) -> [f32; 3] {
    [(1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k)]
}

pub struct FontInfo<'a> {
    pub dict: &'a Dictionary,
    pub cid: bool,
    pub type3: bool,
    /// Per-byte-code advance widths (simple fonts only), in 1000ths of em.
    pub widths: Option<(i64, Vec<f32>)>, // (FirstChar, widths)
    /// Standard-14 metrics used when the font dict carries no Widths.
    pub std_widths: Option<&'static [f32; 256]>,
    /// Converts Widths entries to 1000ths of em. 1.0 for simple fonts;
    /// Type3 widths are in glyph space and need FontMatrix[0] * 1000.
    pub width_scale: f32,
    /// Type3 only: glyph names that actually have a CharProcs procedure.
    pub charprocs: Option<std::collections::HashSet<Vec<u8>>>,
    /// Type3 only: Encoding /Differences map from byte code to glyph name.
    pub differences: Option<std::collections::HashMap<u8, Vec<u8>>>,
    /// Parsed ToUnicode CMap — the authoritative text meaning of byte codes
    /// (ISO 32000 9.10.3); /Encoding glyph names can be meaningless (Skia).
    pub tounicode: Option<crate::tounicode::ToUnicodeMap>,
    pub default_width: f32,
    /// Type3 only: vertical extent from /FontBBox mapped through FontMatrix,
    /// per unit of font size (ascent above baseline, descent below).
    pub t3_vertical: Option<(f32, f32)>,
}

/// Map a BaseFont name (possibly subset-prefixed or an Arial/TimesNewRoman
/// style alias) onto standard-14 metrics.
fn std14_lookup(base_font: &[u8]) -> Option<&'static [f32; 256]> {
    let s = String::from_utf8_lossy(base_font);
    let s = s.rsplit('+').next().unwrap_or(&s); // strip "ABCDEF+" subset tag
    let lower = s.to_ascii_lowercase();
    let bold = lower.contains("bold");
    let italic = lower.contains("italic") || lower.contains("oblique");
    let name = if lower.contains("courier")
        || lower.contains("mono")
        || lower.contains("monaco")
        || lower.contains("menlo")
        || lower.contains("consolas")
    {
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
                .map(|w| w * self.width_scale)
                .or_else(|| self.std_widths.map(|ws| ws[b as usize]).filter(|w| *w > 0.0))
                .unwrap_or(self.default_width);
            total += w;
        }
        total
    }

    /// Whether byte code `b` maps to a glyph this font can actually render.
    /// Conservative: no verifiable metrics means we can't guarantee it.
    pub fn glyph_available(&self, b: u8) -> bool {
        if self.type3 {
            // A Type3 glyph exists iff its code maps to a name that has an
            // actual drawing procedure (width may legitimately be anything).
            return match (&self.differences, &self.charprocs) {
                (Some(diffs), Some(procs)) => diffs.get(&b).is_some_and(|name| procs.contains(name)),
                _ => false,
            };
        }
        if let Some((first, ws)) = &self.widths {
            let idx = b as i64 - first;
            return idx >= 0 && (idx as usize) < ws.len() && ws[idx as usize] > 0.0;
        }
        if let Some(ws) = self.std_widths {
            return ws[b as usize] > 0.0;
        }
        false
    }

    pub fn decode(&self, doc: &Document, bytes: &[u8]) -> String {
        if let Some(tu) = &self.tounicode {
            if let Some(text) = tu.decode(bytes) {
                return text;
            }
        }
        match self.dict.get_font_encoding(doc) {
            Ok(enc) => Document::decode_text(&enc, bytes).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// Encode replacement text to byte codes, verifying the roundtrip: the
    /// bytes must decode back to exactly the requested text. None when the
    /// font's encoding can't express it.
    pub fn encode(&self, doc: &Document, text: &str) -> Option<Vec<u8>> {
        if let Some(tu) = &self.tounicode {
            return tu.encode(text).filter(|b| !b.is_empty());
        }
        let enc = self.dict.get_font_encoding(doc).ok()?;
        let bytes = Document::encode_text(&enc, text);
        let roundtrip = Document::decode_text(&enc, &bytes).ok()?;
        if roundtrip == text && !bytes.is_empty() {
            Some(bytes)
        } else {
            None
        }
    }
}

pub fn load_fonts<'a>(doc: &'a Document, page_id: lopdf::ObjectId) -> BTreeMap<Vec<u8>, FontInfo<'a>> {
    let mut out = BTreeMap::new();
    if let Ok(fonts) = doc.get_page_fonts(page_id) {
        for (name, dict) in fonts {
            out.insert(name, build_font_info(doc, dict));
        }
    }
    // ExtGState dicts can also set the font (ISO 32000 8.4.5, /Font entry);
    // register those under a synthetic "gs:<name>" key so the interpreter
    // and the rescue paths can resolve them like ordinary resources.
    for (gs_name, font_dict, _, _) in gs_font_entries(doc, page_id) {
        let mut key = b"gs:".to_vec();
        key.extend_from_slice(&gs_name);
        // PDF names may legally contain ':' — if a real /Font resource is
        // literally named "gs:GS1", it wins (Tf references it directly);
        // the pathological collision loses gs-tracking, not correctness.
        out.entry(key).or_insert_with(|| build_font_info(doc, font_dict));
    }
    out
}

/// Map from the synthetic "gs:<name>" font key to the underlying font:
/// its object id when the /Font entry is an indirect reference, plus a
/// clone of the dict so direct entries can be materialized as objects.
pub fn gs_fonts_for_restore(
    doc: &Document,
    page_id: lopdf::ObjectId,
) -> BTreeMap<String, (Option<lopdf::ObjectId>, Dictionary)> {
    gs_font_entries(doc, page_id)
        .into_iter()
        .map(|(name, dict, id, _)| {
            (format!("gs:{}", String::from_utf8_lossy(&name)), (id, dict.clone()))
        })
        .collect()
}

/// (gs resource name, font dict, font object id, size) for every ExtGState
/// with a /Font. Resource dicts are nearest-first and the FIRST occurrence
/// of a name wins — including font-less ones, so a page-level /GS1 without
/// /Font correctly shadows an ancestor's font-bearing /GS1.
#[allow(clippy::type_complexity)]
fn gs_font_entries(
    doc: &Document,
    page_id: lopdf::ObjectId,
) -> Vec<(Vec<u8>, &Dictionary, Option<lopdf::ObjectId>, Option<f32>)> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let Ok((inline_res, res_ids)) = doc.get_page_resources(page_id) else {
        return out;
    };
    let mut res_dicts: Vec<&Dictionary> = Vec::new();
    if let Some(d) = inline_res {
        res_dicts.push(d);
    }
    for id in res_ids {
        if let Ok(d) = doc.get_object(id).and_then(|o| o.as_dict()) {
            res_dicts.push(d);
        }
    }
    for res in res_dicts {
        let Some(egs) = res
            .get(b"ExtGState")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_dict().ok())
        else {
            continue;
        };
        for (gs_name, gs_obj) in egs.iter() {
            if !seen.insert(gs_name.clone()) {
                continue; // a nearer dict already defined this name
            }
            let entry = (|| {
                let gs = doc.dereference(gs_obj).ok()?.1.as_dict().ok()?;
                let arr = doc.dereference(gs.get(b"Font").ok()?).ok()?.1.as_array().ok()?;
                let font_id = arr.first()?.as_reference().ok();
                let font_dict = doc.dereference(arr.first()?).ok()?.1.as_dict().ok()?;
                // Size 0 is a legal value (distinct from a malformed entry).
                let size = arr.get(1).and_then(|o| o.as_float().ok());
                Some((gs_name.clone(), font_dict, font_id, size))
            })();
            if let Some(e) = entry {
                out.push(e);
            }
        }
    }
    out
}

/// Per-page map: gs resource name -> (synthetic font key, size).
pub fn load_gs_font_map(doc: &Document, page_id: lopdf::ObjectId) -> BTreeMap<Vec<u8>, (Vec<u8>, Option<f32>)> {
    gs_font_entries(doc, page_id)
        .into_iter()
        .map(|(name, _, _, size)| {
            let mut key = b"gs:".to_vec();
            key.extend_from_slice(&name);
            (name, (key, size))
        })
        .collect()
}

fn build_font_info<'a>(doc: &'a Document, dict: &'a Dictionary) -> FontInfo<'a> {
    {
        let subtype = dict
            .get(b"Subtype")
            .and_then(|o| o.as_name())
            .map(|n| n.to_vec())
            .unwrap_or_default();
        let cid = subtype == b"Type0";
        let type3 = subtype == b"Type3";
        let widths = (|| {
            let first = doc.dereference(dict.get(b"FirstChar").ok()?).ok()?.1.as_i64().ok()?;
            let arr = doc.dereference(dict.get(b"Widths").ok()?).ok()?.1.as_array().ok()?.clone();
            let ws: Vec<f32> = arr
                .iter()
                .map(|o| doc.dereference(o).ok().and_then(|(_, o)| o.as_float().ok()).unwrap_or(0.0))
                .collect();
            Some((first, ws))
        })();
        let std_widths = if cid || type3 {
            None
        } else {
            dict.get(b"BaseFont")
                .and_then(|o| o.as_name())
                .ok()
                .and_then(std14_lookup)
        };
        let (width_scale, charprocs, differences) = if type3 {
            let scale = (|| {
                let fm = doc.dereference(dict.get(b"FontMatrix").ok()?).ok()?.1.as_array().ok()?;
                // Per the PDF reference, a Type3 width contributes only the
                // HORIZONTAL component of its FontMatrix transform (signed
                // `a`) — hypot would overstate shear by sqrt(2) and give a
                // nonzero advance for 90-degree rotations.
                Some(fm.first()?.as_float().ok()? * 1000.0)
            })()
            .unwrap_or(1.0);
            let procs = (|| {
                let cp = doc.dereference(dict.get(b"CharProcs").ok()?).ok()?.1.as_dict().ok()?;
                Some(cp.iter().map(|(k, _)| k.to_vec()).collect::<std::collections::HashSet<_>>())
            })();
            let diffs = (|| {
                let enc = doc.dereference(dict.get(b"Encoding").ok()?).ok()?.1.as_dict().ok()?;
                let arr = doc.dereference(enc.get(b"Differences").ok()?).ok()?.1.as_array().ok()?;
                let mut map = std::collections::HashMap::new();
                let mut code: i64 = 0;
                for el in arr {
                    match el {
                        Object::Integer(n) => code = *n,
                        Object::Name(n) => {
                            if (0..=255).contains(&code) {
                                map.insert(code as u8, n.clone());
                            }
                            code += 1;
                        }
                        _ => {}
                    }
                }
                Some(map)
            })();
            (scale, procs, diffs)
        } else {
            (1.0, None, None)
        };
        let tounicode = if cid {
            None // CID text keeps lopdf's multi-byte path for now
        } else {
            (|| {
                let stream = doc.dereference(dict.get(b"ToUnicode").ok()?).ok()?.1.as_stream().ok()?;
                crate::tounicode::ToUnicodeMap::parse(&stream.decompressed_content().ok()?)
            })()
        };
        // Type3 vertical extent: FontBBox is required for Type3 and, mapped
        // through the FULL FontMatrix (rotation/shear included — y' depends
        // on b*x + d*y + f, not just d), gives real ascent/descent per unit
        // size. Tf sizes for these fonts can be arbitrary (0.24pt with huge
        // glyph coordinates), so em-fraction guesses are meaningless.
        let t3_vertical = if type3 {
            (|| {
                let fm = doc.dereference(dict.get(b"FontMatrix").ok()?).ok()?.1.as_array().ok()?;
                let m: Vec<f32> = fm.iter().map(|o| o.as_float().unwrap_or(0.0)).collect();
                if m.len() != 6 {
                    return None;
                }
                let bb = doc.dereference(dict.get(b"FontBBox").ok()?).ok()?.1.as_array().ok()?;
                let x0 = bb.first()?.as_float().ok()?;
                let y0 = bb.get(1)?.as_float().ok()?;
                let x1 = bb.get(2)?.as_float().ok()?;
                let y1 = bb.get(3)?.as_float().ok()?;
                let ys = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)].map(|(x, y)| m[1] * x + m[3] * y + m[5]);
                let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
                let (asc, desc) = (max_y.max(0.0), (-min_y).max(0.0));
                if asc + desc > 0.0 { Some((asc, desc)) } else { None }
            })()
        } else {
            None
        };
        // Out-of-range codes fall back to /MissingWidth when the descriptor
        // provides one (ISO 32000 9.8.1), else the old 500 guess.
        let default_width = (|| {
            let fd = doc.dereference(dict.get(b"FontDescriptor").ok()?).ok()?.1.as_dict().ok()?;
            let mw = doc.dereference(fd.get(b"MissingWidth").ok()?).ok()?.1.as_float().ok()?;
            Some(mw * if type3 { width_scale } else { 1.0 })
        })()
        .unwrap_or(500.0);
        FontInfo {
            dict,
            cid,
            type3,
            widths,
            std_widths,
            width_scale,
            charprocs,
            differences,
            tounicode,
            default_width,
            t3_vertical,
        }
    }
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
    let gs_fonts = load_gs_font_map(doc, page_id);

    let mut segs = Vec::new();

    let mut gs = GfxState::new();
    let mut tm = Mat::identity(); // text matrix
    let mut tlm = Mat::identity(); // text line matrix
    let mut gs_stack: Vec<GfxState> = Vec::new();

    for (op_idx, op) in content.operations.iter().enumerate() {
        let ops = &op.operands;
        let segs_before_op = segs.len();
        match op.operator.as_str() {
            "q" => gs_stack.push(gs.clone()),
            "Q" => {
                if let Some(s) = gs_stack.pop() {
                    gs = s;
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
                gs.ctm = m.mul(&gs.ctm);
            }
            "BT" => {
                tm = Mat::identity();
                tlm = Mat::identity();
            }
            "Tf" if ops.len() == 2 => {
                gs.font_key = ops[0].as_name().map(|n| n.to_vec()).unwrap_or_default();
                gs.font_size = op_f32(&ops[1]);
            }
            "TL" if ops.len() == 1 => gs.leading = op_f32(&ops[0]),
            "Tc" if ops.len() == 1 => gs.char_spacing = op_f32(&ops[0]),
            "Tw" if ops.len() == 1 => gs.word_spacing = op_f32(&ops[0]),
            "Tz" if ops.len() == 1 => gs.h_scale = op_f32(&ops[0]) / 100.0,
            "Tr" if ops.len() == 1 => gs.render_mode = ops[0].as_i64().unwrap_or(0),
            // ExtGState can carry a /Font entry that sets font + size.
            "gs" if ops.len() == 1 => {
                if let Some((key, size)) = ops[0].as_name().ok().and_then(|n| gs_fonts.get(n)) {
                    gs.font_key = key.clone();
                    if let Some(sz) = size {
                        gs.font_size = *sz; // 0 is a legal size, apply it too
                    }
                }
            }
            // Fill color (approximated to RGB; stroke color doesn't matter
            // for the text model).
            "rg" if ops.len() == 3 => {
                gs.fill_color = [op_f32(&ops[0]), op_f32(&ops[1]), op_f32(&ops[2])];
                gs.pattern_fill = false;
                gs.fill_device_cs = true;
            }
            "g" if ops.len() == 1 => {
                let v = op_f32(&ops[0]);
                gs.fill_color = [v, v, v];
                gs.pattern_fill = false;
                gs.fill_device_cs = true;
            }
            "k" if ops.len() == 4 => {
                gs.fill_color = cmyk_to_rgb(op_f32(&ops[0]), op_f32(&ops[1]), op_f32(&ops[2]), op_f32(&ops[3]));
                gs.pattern_fill = false;
                gs.fill_device_cs = true;
            }
            "cs" => {
                gs.fill_color = [0.0, 0.0, 0.0]; // new colorspace resets to its initial color
                gs.pattern_fill = false;
                // Only the named device spaces are safe to reproduce as
                // rg/g/k; Separation, DeviceN, ICCBased, Indexed, Pattern and
                // resource-named spaces are not (numeric scn under them is a
                // tint, not device components).
                let name = ops.first().and_then(|o| o.as_name().ok()).unwrap_or(b"");
                gs.fill_device_cs = matches!(name, b"DeviceRGB" | b"DeviceGray" | b"DeviceCMYK");
            }
            "sc" | "scn" => {
                // Pattern (trailing name) or a non-device color space means a
                // fill we can only approximate — flag it so reflow refuses.
                let has_name = ops.last().map(|o| o.as_name().is_ok()).unwrap_or(false);
                gs.pattern_fill = has_name || !gs.fill_device_cs;
                let nums: Vec<f32> = ops.iter().filter_map(|o| o.as_float().ok()).collect();
                match nums.len() {
                    1 => gs.fill_color = [nums[0], nums[0], nums[0]],
                    3 => gs.fill_color = [nums[0], nums[1], nums[2]],
                    4 => gs.fill_color = cmyk_to_rgb(nums[0], nums[1], nums[2], nums[3]),
                    _ => {}
                }
            }
            "Td" if ops.len() == 2 => {
                tlm = Mat::translate(op_f32(&ops[0]), op_f32(&ops[1])).mul(&tlm);
                tm = tlm;
            }
            "TD" if ops.len() == 2 => {
                gs.leading = -op_f32(&ops[1]);
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
                tlm = Mat::translate(0.0, -gs.leading).mul(&tlm);
                tm = tlm;
            }
            "Tj" | "'" | "\"" => {
                if op.operator == "'" || op.operator == "\"" {
                    tlm = Mat::translate(0.0, -gs.leading).mul(&tlm);
                    tm = tlm;
                }
                // For ", operands are [aw ac string]; for Tj and ' it's [string].
                let s_op = if op.operator == "\"" { ops.get(2) } else { ops.get(0) };
                if op.operator == "\"" {
                    gs.word_spacing = op_f32(&ops[0]);
                    gs.char_spacing = op_f32(&ops[1]);
                }
                if let Some(Object::String(bytes, _)) = s_op {
                    show_string(doc, &fonts, &gs, &mut tm, bytes, page_no, op_idx, 0, &mut segs);
                }
            }
            "TJ" if ops.len() == 1 => {
                if let Ok(arr) = ops[0].as_array() {
                    let mut str_idx = 0;
                    for el in arr {
                        match el {
                            Object::String(bytes, _) => {
                                show_string(doc, &fonts, &gs, &mut tm, bytes, page_no, op_idx, str_idx, &mut segs);
                                str_idx += 1;
                            }
                            _ => {
                                let adj = el.as_float().unwrap_or(0.0);
                                let tx = -adj / 1000.0 * gs.font_size * gs.h_scale;
                                tm = Mat::translate(tx, 0.0).mul(&tm);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // Backfill: segments emitted by this op record the text line matrix
        // as it stands once the op is done — what following Td/T* build on.
        for seg in &mut segs[segs_before_op..] {
            seg.tlm_after = tlm.0;
        }
    }

    Ok((content, segs))
}

#[allow(clippy::too_many_arguments)]
fn show_string(
    doc: &Document,
    fonts: &BTreeMap<Vec<u8>, FontInfo>,
    gs: &GfxState,
    tm: &mut Mat,
    bytes: &[u8],
    page_no: u32,
    op_idx: usize,
    str_idx: usize,
    segs: &mut Vec<Seg>,
) {
    let font = fonts.get(&gs.font_key);
    let (cid, type3) = font.map(|f| (f.cid, f.type3)).unwrap_or((false, false));
    let code_len = if cid { 2 } else { 1 };

    let trm = tm.mul(&gs.ctm);
    // Horizontal means no rotation/skew in the combined matrix.
    let horizontal = trm.0[1].abs() < 1e-4 && trm.0[2].abs() < 1e-4;

    // Walk code units, accumulating pen position in text space and emitting
    // one positioned glyph per code.
    let mut glyphs: Vec<Glyph> = Vec::new();
    let mut text = String::new();
    let mut pen: f32 = 0.0; // text-space x
    let mut i = 0;
    while i < bytes.len() {
        let n = code_len.min(bytes.len() - i);
        let code = &bytes[i..i + n];
        let g_text = match font {
            Some(f) => f.decode(doc, code),
            None => String::new(),
        };
        let adv_em = match font {
            Some(f) => f.advance(code, g_text.chars().count().max(1)),
            None => 500.0,
        };
        let mut adv = adv_em / 1000.0 * gs.font_size * gs.h_scale + gs.char_spacing * gs.h_scale;
        if !cid && code[0] == b' ' {
            adv += gs.word_spacing * gs.h_scale;
        }
        let (gx, gy) = trm.apply(pen, 0.0);
        let (gx1, _) = trm.apply(pen + adv, 0.0);
        text.push_str(&g_text);
        glyphs.push(Glyph {
            text: g_text,
            byte_start: i,
            byte_len: n,
            x: gx,
            y: gy,
            w: gx1 - gx,
        });
        pen += adv;
        i += n;
    }
    let width_text_space = pen;

    // Vertical extent: Type3 fonts get real FontBBox-derived metrics (their
    // Tf sizes can be arbitrary); others use generous em-fraction estimates.
    // Take the max of both for Type3 — glyphs overshooting their declared
    // FontBBox is a common spec violation, so the declared box only ever
    // WIDENS the estimate, never shrinks it.
    let (asc, desc) = match font.and_then(|f| f.t3_vertical) {
        Some((a, d)) => ((a * 1.1).max(1.05), (d * 1.1).max(a * 0.1).max(0.35)),
        None => (1.05, 0.35),
    };
    let (x0, y0) = trm.apply(0.0, -desc * gs.font_size);
    let (x1a, y1a) = trm.apply(width_text_space, asc * gs.font_size);
    let bbox = [x0.min(x1a), y0.min(y1a), x0.max(x1a), y0.max(y1a)];

    if !text.trim().is_empty() {
        segs.push(Seg {
            page: page_no,
            op_idx,
            str_idx,
            bytes: bytes.to_vec(),
            text,
            font: String::from_utf8_lossy(&gs.font_key).into_owned(),
            font_size: gs.font_size,
            bbox,
            cid,
            visible: gs.render_mode != 3,
            type3,
            color: gs.fill_color,
            horizontal,
            glyphs,
            trm: trm.0,
            ctm: gs.ctm.0,
            tlm_after: [0.0; 6], // filled in by walk_page once the op ends
            char_spacing: gs.char_spacing,
            word_spacing: gs.word_spacing,
            h_scale: gs.h_scale,
            pattern_fill: gs.pattern_fill,
        });
    }

    *tm = Mat::translate(width_text_space, 0.0).mul(tm);
}
