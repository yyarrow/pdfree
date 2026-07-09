//! Read-only TrueType (glyf-based sfnt) font parser.
//!
//! Extracts glyph outlines and advances for individual characters, to feed a
//! Type3 PDF font generator producing fallback glyphs. Written from the
//! OpenType/TrueType specification only (no GPL/FTL sources consulted).
//!
//! Supports: sfnt header + table directory, `head`, `maxp`, `hhea`/`hmtx`,
//! `cmap` formats 4 and 12, `loca` (short/long), and `glyf` simple and
//! composite glyphs. CFF-flavored (`OTTO`) fonts and variation tables
//! (`fvar`/`gvar`) are intentionally not supported: variable fonts fall back
//! to the default (non-varied) outlines already stored in `glyf`, which is
//! exactly what we want here.

use std::collections::HashMap;

/// A single point on a TrueType quadratic outline contour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlinePoint {
    pub x: f32,
    pub y: f32,
    pub on_curve: bool,
}

/// A glyph's outline and horizontal metrics, in font design units.
#[derive(Debug, Clone)]
pub struct GlyphOutline {
    /// Horizontal advance in font units.
    pub advance: f32,
    /// Closed contours of TrueType quadratic outline points, in font units.
    pub contours: Vec<Vec<OutlinePoint>>,
}

/// Which `cmap` subtable format we picked as the best unicode source.
enum CmapSubtable {
    /// Absolute byte offset into `data` of the format-4 subtable header.
    Format4(usize),
    /// Absolute byte offset into `data` of the format-12 subtable header.
    Format12(usize),
}

/// A parsed (but not decompressed/interpreted beyond what we need) TrueType
/// font. Owns the raw bytes; all lookups index back into them.
pub struct TtfFont {
    data: Vec<u8>,
    units_per_em: u16,
    /// 0 = short (`loca` entries are u16 offsets/2), 1 = long (u32 offsets).
    index_to_loc_format: i16,
    num_glyphs: u16,
    num_h_metrics: u16,
    hmtx_offset: u32,
    loca_offset: u32,
    glyf_offset: u32,
    cmap_subtable: CmapSubtable,
}

// ---------------------------------------------------------------------
// Bounds-checked big-endian primitive readers. Every offset that ultimately
// derives from font data (untrusted input) must go through these rather than
// direct slice indexing, so malformed fonts fail with `None` instead of
// panicking.
// ---------------------------------------------------------------------

fn u8_at(d: &[u8], off: usize) -> Option<u8> {
    d.get(off).copied()
}

fn i8_at(d: &[u8], off: usize) -> Option<i8> {
    u8_at(d, off).map(|v| v as i8)
}

fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    let b = d.get(off..end)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn i16_at(d: &[u8], off: usize) -> Option<i16> {
    u16_at(d, off).map(|v| v as i16)
}

fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let b = d.get(off..end)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// F2Dot14 fixed-point (used for composite-glyph transform coefficients).
fn f2dot14_at(d: &[u8], off: usize) -> Option<f32> {
    Some(i16_at(d, off)? as f32 / 16384.0)
}

const MAX_COMPONENT_DEPTH: u32 = 5;

impl TtfFont {
    pub fn parse(data: Vec<u8>) -> Option<TtfFont> {
        let sfnt_version = u32_at(&data, 0)?;
        // Only the 0x00010000 (TrueType glyf outlines) magic is supported.
        // This also rejects 'OTTO' (CFF-flavored) fonts, whose outlines we
        // deliberately don't parse.
        if sfnt_version != 0x0001_0000 {
            return None;
        }
        let num_tables = u16_at(&data, 4)? as usize;

        let mut tables: HashMap<[u8; 4], (u32, u32)> = HashMap::with_capacity(num_tables);
        for i in 0..num_tables {
            let entry_off = 12usize.checked_add(i.checked_mul(16)?)?;
            let tag_bytes = data.get(entry_off..entry_off + 4)?;
            let tag: [u8; 4] = tag_bytes.try_into().ok()?;
            let offset = u32_at(&data, entry_off + 8)?;
            let length = u32_at(&data, entry_off + 12)?;
            tables.insert(tag, (offset, length));
        }

        let head_range = *tables.get(b"head")?;
        let maxp_range = *tables.get(b"maxp")?;
        let hhea_range = *tables.get(b"hhea")?;
        let hmtx_range = *tables.get(b"hmtx")?;
        let cmap_range = *tables.get(b"cmap")?;
        let loca_range = *tables.get(b"loca")?;
        let glyf_range = *tables.get(b"glyf")?;

        // head: unitsPerEm at offset 18, indexToLocFormat at offset 50.
        let units_per_em = u16_at(&data, head_range.0 as usize + 18)?;
        if units_per_em == 0 {
            return None;
        }
        let index_to_loc_format = i16_at(&data, head_range.0 as usize + 50)?;
        if index_to_loc_format != 0 && index_to_loc_format != 1 {
            return None;
        }

        // maxp: numGlyphs at offset 4.
        let num_glyphs = u16_at(&data, maxp_range.0 as usize + 4)?;
        if num_glyphs == 0 {
            return None;
        }

        // hhea: numberOfHMetrics at offset 34.
        let num_h_metrics = u16_at(&data, hhea_range.0 as usize + 34)?;
        if num_h_metrics == 0 {
            return None;
        }

        let cmap_subtable = Self::find_cmap_subtable(&data, cmap_range)?;

        Some(TtfFont {
            data,
            units_per_em,
            index_to_loc_format,
            num_glyphs,
            num_h_metrics,
            hmtx_offset: hmtx_range.0,
            loca_offset: loca_range.0,
            glyf_offset: glyf_range.0,
            cmap_subtable,
        })
    }

    pub fn units_per_em(&self) -> f32 {
        self.units_per_em as f32
    }

    /// Looks up the glyph for `c` via `cmap`, then resolves its outline
    /// (recursing into composite components) and advance width.
    pub fn glyph_for_char(&self, c: char) -> Option<GlyphOutline> {
        let gid = self.lookup_cmap(c as u32)?;
        if gid as u32 >= self.num_glyphs as u32 {
            return None;
        }
        let advance = self.advance_for_gid(gid)?;
        let contours = self.outline_for_gid(gid, 0)?;
        Some(GlyphOutline { advance, contours })
    }

    // -------------------------------------------------------------
    // cmap
    // -------------------------------------------------------------

    /// Scans the `cmap` encoding records and picks the subtable most likely
    /// to give full-repertoire unicode coverage: platform 3/encoding 10
    /// (Windows, full unicode incl. supplementary planes) is preferred,
    /// then platform 0 (Unicode) full-repertoire encodings, then plain
    /// platform 3/encoding 1 (Windows BMP), then any other platform-0
    /// subtable. Only format 4 and format 12 subtables are usable; other
    /// formats are skipped even if their encoding record scores highest.
    fn find_cmap_subtable(data: &[u8], range: (u32, u32)) -> Option<CmapSubtable> {
        let base = range.0 as usize;
        let num_tables = u16_at(data, base + 2)? as usize;

        let mut candidates: Vec<(u8, usize)> = Vec::with_capacity(num_tables);
        for i in 0..num_tables {
            let rec_off = base + 4 + i * 8;
            let platform_id = u16_at(data, rec_off)?;
            let encoding_id = u16_at(data, rec_off + 2)?;
            let offset = u32_at(data, rec_off + 4)?;
            let score: u8 = match (platform_id, encoding_id) {
                (3, 10) => 5,
                (0, 4) | (0, 6) => 4,
                (3, 1) => 3,
                (0, 3) => 2,
                (0, _) => 1,
                _ => 0,
            };
            if score > 0 {
                let abs_off = base.checked_add(offset as usize)?;
                candidates.push((score, abs_off));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        for (_, off) in candidates {
            match u16_at(data, off) {
                Some(4) => return Some(CmapSubtable::Format4(off)),
                Some(12) => return Some(CmapSubtable::Format12(off)),
                _ => continue,
            }
        }
        None
    }

    fn lookup_cmap(&self, c: u32) -> Option<u16> {
        match self.cmap_subtable {
            CmapSubtable::Format4(off) => Self::lookup_format4(&self.data, off, c),
            CmapSubtable::Format12(off) => Self::lookup_format12(&self.data, off, c),
        }
    }

    fn lookup_format4(data: &[u8], off: usize, c: u32) -> Option<u16> {
        if c > 0xFFFF {
            return None;
        }
        let c = c as u16;
        let seg_count_x2 = u16_at(data, off + 6)? as usize;
        if seg_count_x2 < 2 {
            return None;
        }
        let seg_count = seg_count_x2 / 2;
        let end_codes_off = off + 14;
        // + 2 to skip the reservedPad u16 after endCode[].
        let start_codes_off = end_codes_off + seg_count_x2 + 2;
        let id_delta_off = start_codes_off + seg_count_x2;
        let id_range_offset_off = id_delta_off + seg_count_x2;

        for i in 0..seg_count {
            let end_code = u16_at(data, end_codes_off + i * 2)?;
            if c > end_code {
                continue;
            }
            let start_code = u16_at(data, start_codes_off + i * 2)?;
            if c < start_code {
                return None;
            }
            let id_delta = i16_at(data, id_delta_off + i * 2)?;
            let id_range_offset = u16_at(data, id_range_offset_off + i * 2)?;

            if id_range_offset == 0 {
                let gid = ((c as i32).wrapping_add(id_delta as i32)) as u16;
                return if gid == 0 { None } else { Some(gid) };
            }
            // Per spec, the glyphIdArray slot is addressed relative to the
            // location of this segment's own idRangeOffset field.
            let this_field_off = id_range_offset_off + i * 2;
            let glyph_index_addr =
                this_field_off + id_range_offset as usize + 2 * (c - start_code) as usize;
            let raw_gid = u16_at(data, glyph_index_addr)?;
            if raw_gid == 0 {
                return None;
            }
            let gid = ((raw_gid as i32).wrapping_add(id_delta as i32)) as u16;
            return if gid == 0 { None } else { Some(gid) };
        }
        None
    }

    fn lookup_format12(data: &[u8], off: usize, c: u32) -> Option<u16> {
        let num_groups = u32_at(data, off + 12)? as usize;
        let groups_off = off + 16;

        let mut lo = 0usize;
        let mut hi = num_groups;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let g_off = groups_off.checked_add(mid.checked_mul(12)?)?;
            let start = u32_at(data, g_off)?;
            let end = u32_at(data, g_off + 4)?;
            if c < start {
                hi = mid;
            } else if c > end {
                lo = mid + 1;
            } else {
                let start_glyph = u32_at(data, g_off + 8)?;
                let gid = start_glyph.checked_add(c - start)?;
                if gid == 0 || gid > 0xFFFF {
                    return None;
                }
                return Some(gid as u16);
            }
        }
        None
    }

    // -------------------------------------------------------------
    // hmtx
    // -------------------------------------------------------------

    fn advance_for_gid(&self, gid: u16) -> Option<f32> {
        let n = self.num_h_metrics as usize;
        let idx = (gid as usize).min(n - 1);
        let rec_off = (self.hmtx_offset as usize).checked_add(idx.checked_mul(4)?)?;
        let advance = u16_at(&self.data, rec_off)?;
        Some(advance as f32)
    }

    // -------------------------------------------------------------
    // loca / glyf
    // -------------------------------------------------------------

    /// Resolves a glyph id to its absolute byte range within `data` via
    /// `loca`. Returns `Some((offset, 0))` for empty glyphs (e.g. space).
    fn glyph_range(&self, gid: u16) -> Option<(usize, usize)> {
        if gid as u32 >= self.num_glyphs as u32 {
            return None;
        }
        let gid = gid as usize;
        let (start, end) = if self.index_to_loc_format == 0 {
            let s = u16_at(&self.data, self.loca_offset as usize + gid * 2)? as u32 * 2;
            let e = u16_at(&self.data, self.loca_offset as usize + (gid + 1) * 2)? as u32 * 2;
            (s, e)
        } else {
            let s = u32_at(&self.data, self.loca_offset as usize + gid * 4)?;
            let e = u32_at(&self.data, self.loca_offset as usize + (gid + 1) * 4)?;
            (s, e)
        };
        if end < start {
            return None;
        }
        let glyf_base = self.glyf_offset as usize;
        let abs_start = glyf_base.checked_add(start as usize)?;
        let abs_end = glyf_base.checked_add(end as usize)?;
        if abs_end > self.data.len() {
            return None;
        }
        Some((abs_start, abs_end - abs_start))
    }

    fn outline_for_gid(&self, gid: u16, depth: u32) -> Option<Vec<Vec<OutlinePoint>>> {
        if depth > MAX_COMPONENT_DEPTH {
            return None;
        }
        let (glyph_off, glyph_len) = self.glyph_range(gid)?;
        if glyph_len == 0 {
            return Some(Vec::new());
        }
        // Glyph headers are always 10 bytes: numberOfContours + bbox.
        if glyph_len < 10 {
            return None;
        }
        let num_contours = i16_at(&self.data, glyph_off)?;
        if num_contours >= 0 {
            self.parse_simple_glyph(glyph_off, num_contours as usize, glyph_len)
        } else {
            self.parse_composite_glyph(glyph_off, glyph_len, depth)
        }
    }

    fn parse_simple_glyph(
        &self,
        base: usize,
        num_contours: usize,
        glyph_len: usize,
    ) -> Option<Vec<Vec<OutlinePoint>>> {
        let data = &self.data;
        let glyph_end = base.checked_add(glyph_len)?;

        let end_pts_off = base + 10;
        let mut end_pts = Vec::with_capacity(num_contours);
        for i in 0..num_contours {
            end_pts.push(u16_at(data, end_pts_off + i * 2)? as usize);
        }
        let num_points = match num_contours {
            0 => 0,
            n => end_pts[n - 1] + 1,
        };

        let instr_len_off = end_pts_off + num_contours * 2;
        let instruction_length = u16_at(data, instr_len_off)? as usize;
        let mut pos = instr_len_off.checked_add(2)?.checked_add(instruction_length)?;
        if pos > glyph_end {
            return None;
        }

        const ON_CURVE: u8 = 0x01;
        const X_SHORT: u8 = 0x02;
        const REPEAT: u8 = 0x08;
        const X_SAME_OR_POS: u8 = 0x10;
        const Y_SHORT: u8 = 0x04;
        const Y_SAME_OR_POS: u8 = 0x20;

        let mut flags: Vec<u8> = Vec::with_capacity(num_points);
        while flags.len() < num_points {
            let flag = *data.get(pos)?;
            pos += 1;
            flags.push(flag);
            if flag & REPEAT != 0 {
                let repeat = *data.get(pos)?;
                pos += 1;
                for _ in 0..repeat {
                    if flags.len() >= num_points {
                        break;
                    }
                    flags.push(flag);
                }
            }
        }
        if flags.len() != num_points {
            return None;
        }

        let mut xs = Vec::with_capacity(num_points);
        let mut x: i32 = 0;
        for &flag in &flags {
            if flag & X_SHORT != 0 {
                let dx = *data.get(pos)? as i32;
                pos += 1;
                if flag & X_SAME_OR_POS != 0 {
                    x += dx;
                } else {
                    x -= dx;
                }
            } else if flag & X_SAME_OR_POS == 0 {
                let dx = i16_at(data, pos)? as i32;
                pos += 2;
                x += dx;
            }
            // else: X_SHORT unset and X_SAME_OR_POS set => same as previous,
            // zero delta, no bytes consumed.
            xs.push(x);
        }

        let mut ys = Vec::with_capacity(num_points);
        let mut y: i32 = 0;
        for &flag in &flags {
            if flag & Y_SHORT != 0 {
                let dy = *data.get(pos)? as i32;
                pos += 1;
                if flag & Y_SAME_OR_POS != 0 {
                    y += dy;
                } else {
                    y -= dy;
                }
            } else if flag & Y_SAME_OR_POS == 0 {
                let dy = i16_at(data, pos)? as i32;
                pos += 2;
                y += dy;
            }
            ys.push(y);
        }

        if pos > glyph_end {
            return None;
        }

        let mut points = Vec::with_capacity(num_points);
        for i in 0..num_points {
            points.push(OutlinePoint {
                x: xs[i] as f32,
                y: ys[i] as f32,
                on_curve: flags[i] & ON_CURVE != 0,
            });
        }

        let mut contours = Vec::with_capacity(num_contours);
        let mut start = 0usize;
        for &end in &end_pts {
            if end < start || end >= points.len() {
                return None;
            }
            contours.push(points[start..=end].to_vec());
            start = end + 1;
        }
        Some(contours)
    }

    fn parse_composite_glyph(
        &self,
        base: usize,
        glyph_len: usize,
        depth: u32,
    ) -> Option<Vec<Vec<OutlinePoint>>> {
        const ARGS_ARE_WORDS: u16 = 0x0001;
        const ARGS_ARE_XY_VALUES: u16 = 0x0002;
        const WE_HAVE_A_SCALE: u16 = 0x0008;
        const MORE_COMPONENTS: u16 = 0x0020;
        const WE_HAVE_X_AND_Y_SCALE: u16 = 0x0040;
        const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;

        let data = &self.data;
        let glyph_end = base.checked_add(glyph_len)?;
        let mut pos = base + 10;
        let mut contours = Vec::new();

        loop {
            let flags = u16_at(data, pos)?;
            pos += 2;
            let component_gid = u16_at(data, pos)?;
            pos += 2;

            let (dx, dy) = if flags & ARGS_ARE_WORDS != 0 {
                let a1 = i16_at(data, pos)?;
                pos += 2;
                let a2 = i16_at(data, pos)?;
                pos += 2;
                if flags & ARGS_ARE_XY_VALUES != 0 {
                    (a1 as f32, a2 as f32)
                } else {
                    // Point-matching (not offsets); unsupported, treat as
                    // no translation rather than failing the whole glyph.
                    (0.0, 0.0)
                }
            } else {
                let a1 = i8_at(data, pos)?;
                pos += 1;
                let a2 = i8_at(data, pos)?;
                pos += 1;
                if flags & ARGS_ARE_XY_VALUES != 0 {
                    (a1 as f32, a2 as f32)
                } else {
                    (0.0, 0.0)
                }
            };

            let (a, b, c, d) = if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
                let a = f2dot14_at(data, pos)?;
                pos += 2;
                let b = f2dot14_at(data, pos)?;
                pos += 2;
                let c = f2dot14_at(data, pos)?;
                pos += 2;
                let d = f2dot14_at(data, pos)?;
                pos += 2;
                (a, b, c, d)
            } else if flags & WE_HAVE_X_AND_Y_SCALE != 0 {
                let xs = f2dot14_at(data, pos)?;
                pos += 2;
                let ys = f2dot14_at(data, pos)?;
                pos += 2;
                (xs, 0.0, 0.0, ys)
            } else if flags & WE_HAVE_A_SCALE != 0 {
                let s = f2dot14_at(data, pos)?;
                pos += 2;
                (s, 0.0, 0.0, s)
            } else {
                (1.0, 0.0, 0.0, 1.0)
            };

            let sub_contours = self.outline_for_gid(component_gid, depth + 1)?;
            for contour in sub_contours {
                let mut new_contour = Vec::with_capacity(contour.len());
                for p in contour {
                    new_contour.push(OutlinePoint {
                        x: a * p.x + c * p.y + dx,
                        y: b * p.x + d * p.y + dy,
                        on_curve: p.on_curve,
                    });
                }
                contours.push(new_contour);
            }

            if flags & MORE_COMPONENTS == 0 {
                break;
            }
            if pos > glyph_end {
                return None;
            }
        }

        Some(contours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const FIXTURE_PATH: &str = "/Users/ian/Work/pdfree/assets/NotoSansSC.ttf";

    /// The fixture may still be mid-download when tests start (a background
    /// fetch was in flight). Wait up to ~10 minutes for it to become a valid
    /// sfnt file before giving up.
    fn load_fixture() -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            if let Ok(data) = std::fs::read(FIXTURE_PATH) {
                if data.len() >= 12 && u32_at(&data, 0) == Some(0x0001_0000) {
                    if TtfFont::parse(data.clone()).is_some() {
                        return data;
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "fixture font at {FIXTURE_PATH} never became a valid, parseable sfnt file \
                     within 10 minutes; is the download still running or did it fail?"
                );
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn load_font() -> TtfFont {
        let data = load_fixture();
        TtfFont::parse(data).expect("fixture font should parse")
    }

    #[test]
    fn parses_header_and_units_per_em() {
        let font = load_font();
        assert_eq!(font.units_per_em(), 1000.0);
    }

    #[test]
    fn common_glyphs_have_sane_outlines() {
        let font = load_font();
        for ch in ['A', '单', '棪'] {
            let g = font
                .glyph_for_char(ch)
                .unwrap_or_else(|| panic!("expected glyph for {ch:?}"));
            assert!(g.advance > 0.0, "advance for {ch:?} should be > 0");
            assert!(
                g.contours.iter().any(|c| !c.is_empty()),
                "expected at least one non-empty contour for {ch:?}"
            );
            for contour in &g.contours {
                for p in contour {
                    assert!(
                        p.x > -2000.0 && p.x < 2000.0,
                        "x out of range for {ch:?}: {}",
                        p.x
                    );
                    assert!(
                        p.y > -2000.0 && p.y < 2000.0,
                        "y out of range for {ch:?}: {}",
                        p.y
                    );
                }
            }
        }
    }

    #[test]
    fn space_has_advance_and_no_crash() {
        let font = load_font();
        let g = font.glyph_for_char(' ').expect("space should be mapped");
        assert!(g.advance > 0.0);
    }

    #[test]
    fn unmapped_char_returns_none() {
        let font = load_font();
        assert!(font.glyph_for_char('\u{e0000}').is_none());
    }

    #[test]
    fn composite_glyph_has_contours() {
        let font = load_font();
        let g = font
            .glyph_for_char('é')
            .expect("é should be a composite glyph with an outline");
        assert!(g.advance > 0.0);
        assert!(g.contours.iter().any(|c| !c.is_empty()));
    }
}
