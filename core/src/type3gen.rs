//! Synthesize a Type3 PDF font from TrueType glyph outlines, for characters
//! the document's own (subsetted) fonts can't render. Type3 glyphs are plain
//! content-stream drawing procedures, so unlike embedding a real font program
//! this needs no font-file writer — the same trick Chrome/Skia uses when
//! exporting PDFs.

use crate::ttf::TtfFont;
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::collections::HashMap;

pub struct FallbackFont {
    pub font_id: ObjectId,
    /// Char -> byte code assigned inside the generated font.
    pub codes: HashMap<char, u8>,
    /// Advance widths in glyph units (same space as FontMatrix), per char.
    pub advances: HashMap<char, f32>,
    pub units_per_em: f32,
}

/// Build a Type3 font covering exactly `chars` (deduped; max 255).
pub fn build_type3_font(doc: &mut Document, ttf: &TtfFont, chars: &[char]) -> Option<FallbackFont> {
    let upem = ttf.units_per_em();
    let mut codes: HashMap<char, u8> = HashMap::new();
    let mut advances: HashMap<char, f32> = HashMap::new();
    let mut charprocs = lopdf::Dictionary::new();
    let mut differences: Vec<Object> = vec![Object::Integer(1)];
    let mut widths: Vec<Object> = Vec::new();
    let mut bfchars: Vec<(u8, char)> = Vec::new();

    let mut next_code: u8 = 1;
    for &c in chars {
        if codes.contains_key(&c) {
            continue;
        }
        if next_code == 0 {
            return None; // >255 distinct chars in one edit — split upstream
        }
        let glyph = ttf.glyph_for_char(c)?;
        let name = glyph_name(c);
        charprocs.set(name.clone(), Object::Reference(doc.add_object(charproc_stream(&glyph))));
        differences.push(Object::Name(name.into_bytes()));
        widths.push(Object::Real(glyph.advance));
        bfchars.push((next_code, c));
        codes.insert(c, next_code);
        advances.insert(c, glyph.advance);
        next_code = next_code.wrapping_add(1);
    }
    if codes.is_empty() {
        return None;
    }
    let last_char = codes.len() as i64; // codes are 1..=N

    let tounicode = doc.add_object(Stream::new(dictionary! {}, tounicode_cmap(&bfchars)));
    let charprocs_id = doc.add_object(charprocs);
    let encoding = dictionary! {
        "Type" => "Encoding",
        "Differences" => Object::Array(differences),
    };
    let scale = 1.0 / upem;
    let font = dictionary! {
        "Type" => "Font",
        "Subtype" => "Type3",
        // All-zero FontBBox is explicitly allowed (ISO 32000 9.6.5): no clip.
        "FontBBox" => Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]),
        "FontMatrix" => Object::Array(vec![
            Object::Real(scale), 0.into(), 0.into(), Object::Real(scale), 0.into(), 0.into(),
        ]),
        "CharProcs" => Object::Reference(charprocs_id),
        "Encoding" => encoding,
        "FirstChar" => 1,
        "LastChar" => last_char,
        "Widths" => Object::Array(widths),
        "ToUnicode" => Object::Reference(tounicode),
    };
    let font_id = doc.add_object(font);

    Some(FallbackFont {
        font_id,
        codes,
        advances,
        units_per_em: upem,
    })
}

fn glyph_name(c: char) -> String {
    // uniXXXX for the BMP, uXXXXXX beyond — names only need to be unique.
    let cp = c as u32;
    if cp <= 0xFFFF {
        format!("uni{cp:04X}")
    } else {
        format!("u{cp:06X}")
    }
}

/// One glyph drawing procedure: advance declaration + filled outline.
fn charproc_stream(glyph: &crate::ttf::GlyphOutline) -> Stream {
    let mut s = String::new();
    s.push_str(&format!("{} 0 d0\n", fmt(glyph.advance)));
    for contour in &glyph.contours {
        emit_contour(&mut s, contour);
    }
    if !glyph.contours.is_empty() {
        s.push_str("f\n");
    }
    Stream::new(dictionary! {}, s.into_bytes())
}

/// Convert one TrueType quadratic contour to PDF cubic path operators.
fn emit_contour(out: &mut String, pts: &[crate::ttf::OutlinePoint]) {
    if pts.is_empty() {
        return;
    }
    // Normalize to start at an on-curve point; if none exists, synthesize the
    // midpoint of the first two (all-off-curve contours are legal TrueType).
    let start_idx = pts.iter().position(|p| p.on_curve);
    let (sx, sy, order): (f32, f32, Vec<(f32, f32, bool)>) = match start_idx {
        Some(i) => {
            let mut o: Vec<_> = pts[i..].iter().chain(&pts[..i]).map(|p| (p.x, p.y, p.on_curve)).collect();
            o.rotate_left(1); // start point handled separately
            (pts[i].x, pts[i].y, o)
        }
        None => {
            let mid_x = (pts[0].x + pts[pts.len() - 1].x) / 2.0;
            let mid_y = (pts[0].y + pts[pts.len() - 1].y) / 2.0;
            let o: Vec<_> = pts.iter().map(|p| (p.x, p.y, p.on_curve)).collect();
            (mid_x, mid_y, o)
        }
    };

    out.push_str(&format!("{} {} m\n", fmt(sx), fmt(sy)));
    let (mut cx, mut cy) = (sx, sy);
    let mut pending_ctrl: Option<(f32, f32)> = None;

    let mut close_via = |out: &mut String, cx: &mut f32, cy: &mut f32, x: f32, y: f32, on: bool, ctrl: &mut Option<(f32, f32)>| {
        if on {
            match ctrl.take() {
                None => out.push_str(&format!("{} {} l\n", fmt(x), fmt(y))),
                Some((qx, qy)) => {
                    emit_quad(out, *cx, *cy, qx, qy, x, y);
                }
            }
            *cx = x;
            *cy = y;
        } else if let Some((qx, qy)) = ctrl.take() {
            // Two consecutive off-curve points: implied on-curve midpoint.
            let (mx, my) = ((qx + x) / 2.0, (qy + y) / 2.0);
            emit_quad(out, *cx, *cy, qx, qy, mx, my);
            *cx = mx;
            *cy = my;
            *ctrl = Some((x, y));
        } else {
            *ctrl = Some((x, y));
        }
    };

    for &(x, y, on) in &order {
        close_via(out, &mut cx, &mut cy, x, y, on, &mut pending_ctrl);
    }
    // Close back to the start point.
    if let Some((qx, qy)) = pending_ctrl.take() {
        emit_quad(out, cx, cy, qx, qy, sx, sy);
    }
    out.push_str("h\n");
}

/// Exact quadratic -> cubic elevation: c1 = p0 + 2/3 (q - p0), c2 = p1 + 2/3 (q - p1).
fn emit_quad(out: &mut String, x0: f32, y0: f32, qx: f32, qy: f32, x1: f32, y1: f32) {
    let c1x = x0 + 2.0 / 3.0 * (qx - x0);
    let c1y = y0 + 2.0 / 3.0 * (qy - y0);
    let c2x = x1 + 2.0 / 3.0 * (qx - x1);
    let c2y = y1 + 2.0 / 3.0 * (qy - y1);
    out.push_str(&format!(
        "{} {} {} {} {} {} c\n",
        fmt(c1x), fmt(c1y), fmt(c2x), fmt(c2y), fmt(x1), fmt(y1)
    ));
}

fn fmt(v: f32) -> String {
    // Compact fixed-point: glyph units don't need more than 2 decimals.
    let r = (v * 100.0).round() / 100.0;
    if r == r.trunc() {
        format!("{}", r as i64)
    } else {
        format!("{r:.2}")
    }
}

fn tounicode_cmap(bfchars: &[(u8, char)]) -> Vec<u8> {
    let mut body = String::new();
    for (code, c) in bfchars {
        let units: Vec<u16> = c.encode_utf16(&mut [0u16; 2]).to_vec();
        let hex: String = units.iter().map(|u| format!("{u:04X}")).collect();
        body.push_str(&format!("<{code:02X}> <{hex}>\n"));
    }
    format!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CMapName /pdfree-fallback def\n/CMapType 2 def\n\
         1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
         {} beginbfchar\n{}endbfchar\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
        bfchars.len(),
        body
    )
    .into_bytes()
}
