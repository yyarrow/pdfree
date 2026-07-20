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
    /// Text rendering mode (Tr): fill/stroke/clip. Regeneration must not
    /// change it, so lines with mixed Tr refuse.
    #[serde(skip)]
    pub render_mode: i64,
    /// The exact nonstroking-color operator + numeric operands active for
    /// this segment (e.g. ("g",[0.0]) or ("rg",[1,0,0])), so regeneration
    /// replays the ORIGINAL color space instead of flattening to rg.
    #[serde(skip)]
    pub fill_op: (String, Vec<f32>),
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
    /// Original nonstroking-color operator + operands, for faithful replay.
    fill_op: (String, Vec<f32>),
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
            fill_op: ("g".to_string(), vec![0.0]), // initial nonstroking color is black
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
    /// Lazily resolved embedded TrueType program (/FontDescriptor
    /// /FontFile2). `None` until first consulted; `Some(None)` when absent
    /// or unparseable (CFF, corrupt). Resolution goes through a small
    /// thread-local cache keyed by a stream fingerprint, so a fragmented
    /// run edit that rebuilds FontInfo per segment re-parses a
    /// multi-megabyte font once, not once per segment.
    embedded: std::cell::OnceCell<Option<std::rc::Rc<crate::ttf::TtfFont>>>,
    /// FontDescriptor /Flags says Symbolic (bit 3 set, bit 6 Nonsymbolic
    /// clear): such fonts resolve codes through their OWN byte-keyed cmap
    /// (ISO 32000 9.6.6.4), not through unicode.
    symbolic: bool,
    /// Whether the font dict carries an /Encoding entry at all. Per ISO
    /// 32000 9.6.6.4 a TrueType font with NO /Encoding resolves codes
    /// through its built-in (3,0)/(1,0) cmap exactly like the symbolic
    /// case, regardless of the Symbolic flag.
    has_encoding: bool,
}

/// Embedded font-program parse results, cached per thread (engine and WASM
/// are single-threaded) across FontInfo lifetimes. Tiny LRU: real documents
/// use a handful of embedded fonts per page; the win is not re-decompressing
/// a 10MB CJK font for every segment of one fragmented run edit.
///
/// Identity is exact, never sampled: an entry stores a copy of the RAW
/// (still compressed) stream PLUS its decode configuration (/Filter,
/// /DecodeParms — the same payload under different filters decodes to
/// different programs), and a first hit requires byte equality with both.
/// The sampled fingerprint is only a fast-reject filter. FAILED parses are
/// cached too (value None): a corrupt or unsupported font must not
/// re-decompress per segment either. Memory is bounded by the cap.
///
/// The full memcmp is then amortized to ONCE per font per edit operation:
/// a verified entry records (buffer ptr, len, epoch); public edit entry
/// points bump the epoch (`bump_ttf_cache_epoch`), and within one epoch
/// the probing stream's buffer identity is proof enough — the document is
/// borrowed for the whole edit, so the same allocation can't have changed
/// bytes. A new epoch (or a different document) re-verifies with one full
/// memcmp. This is exact: the O(1) shortcut never crosses an edit
/// boundary, so allocator address reuse can't alias two fonts.
const TTF_CACHE_CAP: usize = 4;
struct TtfCacheEntry {
    fingerprint: u64,
    /// Serialized /Filter + /DecodeParms of the cached stream.
    decode_cfg: String,
    raw: std::rc::Rc<Vec<u8>>,
    parsed: Option<std::rc::Rc<crate::ttf::TtfFont>>,
    /// (buffer ptr, len, epoch) of the last full-verified probe.
    verified: std::cell::Cell<(usize, usize, u64)>,
}
thread_local! {
    static TTF_CACHE: std::cell::RefCell<Vec<TtfCacheEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static TTF_CACHE_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Start a new edit operation for the embedded-font cache: pointer-identity
/// shortcuts from earlier operations stop being trusted and the next hit
/// per font re-verifies with a full byte comparison. Public edit entry
/// points call this; forgetting one costs a memcmp per segment, never
/// correctness.
pub fn bump_ttf_cache_epoch() {
    TTF_CACHE_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
}

/// Cheap fingerprint of a raw stream (length + first/last 1KB hashed) used
/// as the cache's fast-reject filter. Probing stays O(1) for multi-megabyte
/// font programs; equality is confirmed exactly on a filter hit.
fn stream_fingerprint(raw: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    raw.len().hash(&mut h);
    raw[..raw.len().min(1024)].hash(&mut h);
    raw[raw.len().saturating_sub(1024)..].hash(&mut h);
    h.finish()
}

/// The decode configuration that, together with the raw payload, fully
/// determines `decompressed_content()`'s output.
fn stream_decode_cfg(stream: &lopdf::Stream) -> String {
    format!(
        "{:?}|{:?}",
        stream.dict.get(b"Filter").ok(),
        stream.dict.get(b"DecodeParms").ok()
    )
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

    /// The embedded TrueType program, parsed on first use and shared via
    /// the thread-local cache. Only fonts whose /FontDescriptor carries
    /// /FontFile2 qualify; `TtfFont::parse` rejects CFF ('OTTO') data and
    /// fonts with no usable cmap subtable of either kind.
    fn embedded_ttf(&self, doc: &Document) -> Option<&crate::ttf::TtfFont> {
        self.embedded
            .get_or_init(|| {
                let stream = (|| {
                    let fd = doc.dereference(self.dict.get(b"FontDescriptor").ok()?).ok()?.1.as_dict().ok()?;
                    doc.dereference(fd.get(b"FontFile2").ok()?).ok()?.1.as_stream().ok()
                })()?;
                let key = stream_fingerprint(&stream.content);
                let cfg = stream_decode_cfg(stream);
                let ident = (stream.content.as_ptr() as usize, stream.content.len());
                let epoch = TTF_CACHE_EPOCH.with(std::cell::Cell::get);
                TTF_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    let pos = cache.iter().position(|e| {
                        e.fingerprint == key
                            && e.decode_cfg == cfg
                            && (e.verified.get() == (ident.0, ident.1, epoch)
                                || e.raw.as_slice() == stream.content)
                    });
                    if let Some(pos) = pos {
                        let hit = cache.remove(pos);
                        hit.verified.set((ident.0, ident.1, epoch));
                        let parsed = hit.parsed.clone();
                        cache.push(hit); // move to most-recent slot
                        return parsed;
                    }
                    // Miss: parse (or fail) ONCE and remember either way.
                    let parsed = stream
                        .decompressed_content()
                        .ok()
                        .and_then(crate::ttf::TtfFont::parse)
                        .map(std::rc::Rc::new);
                    if cache.len() >= TTF_CACHE_CAP {
                        cache.remove(0);
                    }
                    cache.push(TtfCacheEntry {
                        fingerprint: key,
                        decode_cfg: cfg,
                        raw: std::rc::Rc::new(stream.content.clone()),
                        parsed: parsed.clone(),
                        verified: std::cell::Cell::new((ident.0, ident.1, epoch)),
                    });
                    parsed
                })
            })
            .as_deref()
    }

    /// Whether byte code `b` (decoding to `c` when the caller can align
    /// bytes to characters) maps to a glyph this font can actually render.
    ///
    /// The embedded font program is the ground truth when present: a
    /// shard-subsetting generator (reportlab et al.) writes /Widths for whole
    /// 256-code shards including codes whose glyphs were never embedded, so a
    /// positive width certifies nothing and trusting it writes invisible
    /// tofu. Fonts without a parseable unicode-cmap'd FontFile2 (unembedded,
    /// CFF, symbol) keep the width heuristics below — for them the embedded
    /// check can neither confirm nor deny.
    ///
    /// Conservative both ways: no verifiable evidence means we can't
    /// guarantee the glyph, and refusing (the caller falls back or errors)
    /// beats silently emitting a blank character.
    pub fn glyph_available(&self, doc: &Document, b: u8, c: Option<char>) -> bool {
        if self.type3 {
            // A Type3 glyph exists iff its code maps to a name that has an
            // actual drawing procedure (width may legitimately be anything).
            return match (&self.differences, &self.charprocs) {
                (Some(diffs), Some(procs)) => diffs.get(&b).is_some_and(|name| procs.contains(name)),
                _ => false,
            };
        }
        if let Some(ttf) = self.embedded_ttf(doc) {
            // Pick the lookup a renderer would actually use (ISO 32000
            // 9.6.6.4) instead of OR-ing both: on a nonsymbolic dual-cmap
            // font the byte and unicode keyings disagree outside ASCII
            // (WinAnsi 0x80 is '€' while Mac (1,0) 0x80 is 'Ä'), so a
            // byte-keyed hit does NOT prove the encoding's character will
            // paint — approving it would emit the wrong glyph or tofu.
            //
            // - Symbolic fonts resolve codes through their own byte-keyed
            //   cmap; unicode is only a last resort when they lack one.
            // - Nonsymbolic fonts with a unicode subtable go through the
            //   encoding's character value; alignment lost means we can't
            //   look it up — refuse (the widths of an embedded font are
            //   exactly the evidence that lies).
            // - A byte-only table on a nonsymbolic font is the
            //   reportlab-shard shape: the generator wrote encoding and
            //   cmap together, the byte IS the font-internal code.
            let ws = c.is_some_and(char::is_whitespace);
            // Byte-keyed resolution applies when the Symbolic flag is set
            // OR the font dict has no /Encoding at all — ISO 32000 9.6.6.4
            // names both conditions; painting then goes through the
            // built-in (3,0)/(1,0) cmap with the string byte. (ToUnicode
            // still defines what the byte MEANS; the generator that wrote
            // both tables is responsible for their consistency, so a real
            // glyph at the byte is the correct renderability verdict.)
            if self.symbolic || !self.has_encoding {
                if let Some(v) = ttf.can_render_code(b, ws) {
                    return v;
                }
                // No byte-keyed table to consult: fall back to the unicode
                // verdict rather than refusing outright — viewers are
                // lenient here and treat unicode-only fonts' codes as
                // characters, so a hard refusal would push perfectly
                // renderable text into the rescue/fallback path.
                return c.map(|c| ttf.can_render_char(c)).unwrap_or(false);
            }
            if ttf.has_unicode_cmap() {
                return c.map(|c| ttf.can_render_char(c)).unwrap_or(false);
            }
            return ttf.can_render_code(b, ws).unwrap_or(false);
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
        // /Flags bit 3 (value 4) Symbolic, bit 6 (value 32) Nonsymbolic
        // (ISO 32000 9.8.2). Both set is contradictory; trust Nonsymbolic
        // then, since the unicode path is the safer verifier.
        let symbolic = (|| {
            let fd = doc.dereference(dict.get(b"FontDescriptor").ok()?).ok()?.1.as_dict().ok()?;
            let flags = doc.dereference(fd.get(b"Flags").ok()?).ok()?.1.as_i64().ok()?;
            Some(flags & 4 != 0 && flags & 32 == 0)
        })()
        .unwrap_or(false);
        let has_encoding = dict.has(b"Encoding");
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
            embedded: std::cell::OnceCell::new(),
            symbolic,
            has_encoding,
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
                gs.fill_op = ("rg".into(), vec![op_f32(&ops[0]), op_f32(&ops[1]), op_f32(&ops[2])]);
            }
            "g" if ops.len() == 1 => {
                let v = op_f32(&ops[0]);
                gs.fill_color = [v, v, v];
                gs.pattern_fill = false;
                gs.fill_device_cs = true;
                gs.fill_op = ("g".into(), vec![v]);
            }
            "k" if ops.len() == 4 => {
                gs.fill_color = cmyk_to_rgb(op_f32(&ops[0]), op_f32(&ops[1]), op_f32(&ops[2]), op_f32(&ops[3]));
                gs.pattern_fill = false;
                gs.fill_device_cs = true;
                gs.fill_op = ("k".into(), ops.iter().map(op_f32).collect());
            }
            "cs" => {
                gs.fill_color = [0.0, 0.0, 0.0]; // new colorspace resets to its initial color
                // Only the named device spaces are safe to reproduce as
                // rg/g/k; Separation, DeviceN, ICCBased, Indexed, Pattern and
                // resource-named spaces are not (numeric scn under them is a
                // tint, not device components). The INITIAL color under a
                // non-device space is likewise unreproducible, so flag now.
                let name = ops.first().and_then(|o| o.as_name().ok()).unwrap_or(b"");
                gs.fill_device_cs = matches!(name, b"DeviceRGB" | b"DeviceGray" | b"DeviceCMYK");
                gs.pattern_fill = !gs.fill_device_cs;
                // Record the initial-color fill_op in the SELECTED space, not
                // always "g 0" — regenerating "g" under a DeviceRGB line would
                // switch the space to DeviceGray for following content.
                gs.fill_op = match name {
                    b"DeviceRGB" => ("rg".into(), vec![0.0, 0.0, 0.0]),
                    b"DeviceCMYK" => ("k".into(), vec![0.0, 0.0, 0.0, 1.0]),
                    _ => ("g".into(), vec![0.0]),
                };
            }
            "sc" | "scn" => {
                // Pattern (trailing name) or a non-device color space means a
                // fill we can only approximate — flag it so reflow refuses.
                let has_name = ops.last().map(|o| o.as_name().is_ok()).unwrap_or(false);
                gs.pattern_fill = has_name || !gs.fill_device_cs;
                let nums: Vec<f32> = ops.iter().filter_map(|o| o.as_float().ok()).collect();
                match nums.len() {
                    1 => {
                        gs.fill_color = [nums[0], nums[0], nums[0]];
                        gs.fill_op = ("g".into(), vec![nums[0]]);
                    }
                    3 => {
                        gs.fill_color = [nums[0], nums[1], nums[2]];
                        gs.fill_op = ("rg".into(), nums.clone());
                    }
                    4 => {
                        gs.fill_color = cmyk_to_rgb(nums[0], nums[1], nums[2], nums[3]);
                        gs.fill_op = ("k".into(), nums.clone());
                    }
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
            render_mode: gs.render_mode,
            fill_op: gs.fill_op.clone(),
        });
    }

    *tm = Mat::translate(width_text_space, 0.0).mul(tm);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object, Stream};

    /// Strict pyftsubset of the bundled Noto Sans SC covering exactly
    /// " ()Ian元桓" (regenerate: `pyftsubset assets/NotoSansSC.ttf
    /// --text="元桓 (Ian)" --output-file=...`). OFL-licensed derivative of
    /// the font this repo already ships. Notably it has NO glyph for 'd' —
    /// the tofu-repro character used throughout these tests.
    const SUBSET_TTF: &[u8] = include_bytes!("testdata/noto_sc_subset_ian.ttf");

    /// The same subset with its unicode cmap REPLACED by a (1,0) Mac
    /// format-6 table keyed by the ASCII bytes — the exact shape
    /// reportlab-style subsetters embed (they carry no unicode subtable at
    /// all, so coverage must be provable per byte code).
    const SUBSET_TTF_MAC: &[u8] = include_bytes!("testdata/noto_sc_subset_ian_mac_cmap.ttf");

    /// The subset with BOTH a unicode cmap and a (1,0) Mac table whose byte
    /// 0x80 maps to a real glyph (元's). On a nonsymbolic WinAnsi font, byte
    /// 0x80 means '€' — absent from the unicode table — so a byte-keyed hit
    /// must NOT authorize the code (crbot #7 round-1 finding 2).
    const SUBSET_TTF_DUAL: &[u8] = include_bytes!("testdata/noto_sc_subset_dual_cmap.ttf");

    /// A simple TrueType font dict whose /Widths LIE the way shard-based
    /// subsetting generators do: every code in 32..=122 gets a positive
    /// width, including 'd' (0x64) whose glyph is absent from the program.
    fn font_dict_id_with(doc: &mut Document, fontfile: Option<&[u8]>) -> lopdf::ObjectId {
        let mut font = dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "AAAAAA+NotoSansSC-Thin",
            "FirstChar" => 32,
            "Widths" => (32..=122).map(|_| 500.into()).collect::<Vec<Object>>(),
            "Encoding" => "WinAnsiEncoding",
        };
        if let Some(bytes) = fontfile {
            let ff = doc.add_object(Stream::new(dictionary! {}, bytes.to_vec()));
            let fd = doc.add_object(dictionary! {
                "Type" => "FontDescriptor",
                "FontName" => "AAAAAA+NotoSansSC-Thin",
                "Flags" => 32,
                "FontFile2" => Object::Reference(ff),
            });
            font.set("FontDescriptor", Object::Reference(fd));
        }
        doc.add_object(font)
    }

    fn font_dict_id(doc: &mut Document, with_fontfile: bool) -> lopdf::ObjectId {
        font_dict_id_with(doc, with_fontfile.then_some(SUBSET_TTF))
    }

    fn font_info(doc: &Document, id: lopdf::ObjectId) -> FontInfo<'_> {
        build_font_info(doc, doc.get_dictionary(id).unwrap())
    }

    #[test]
    fn embedded_program_overrides_lying_widths() {
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id(&mut doc, true);
        let f = font_info(&doc, id);
        // 'd' has a positive /Widths entry but no glyph in the program:
        // trusting widths here is exactly the silent-tofu bug.
        assert!(!f.glyph_available(&doc, b'd', Some('d')));
        // Characters the subset really covers pass.
        assert!(f.glyph_available(&doc, b'I', Some('I')));
        assert!(f.glyph_available(&doc, b'a', Some('a')));
        assert!(f.glyph_available(&doc, b'n', Some('n')));
    }

    #[test]
    fn whitespace_with_empty_outline_is_renderable() {
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id(&mut doc, true);
        let f = font_info(&doc, id);
        // The space glyph legitimately has no contours; it must not be
        // mistaken for a stripped (tofu) glyph.
        assert!(f.glyph_available(&doc, b' ', Some(' ')));
    }

    #[test]
    fn cjk_beyond_widths_range_is_renderable() {
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id(&mut doc, true);
        let f = font_info(&doc, id);
        // The embedded check is unicode-keyed: a char outside the /Widths
        // range entirely still verifies through the font program.
        assert!(f.glyph_available(&doc, 0, Some('元')));
        assert!(!f.glyph_available(&doc, 0, Some('魔')));
    }

    #[test]
    fn lost_alignment_is_conservative_with_embedded_font() {
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id(&mut doc, true);
        let f = font_info(&doc, id);
        // No char to look up + an embedded font whose widths lie: refuse
        // rather than risk tofu.
        assert!(!f.glyph_available(&doc, b'I', None));
    }

    #[test]
    fn mac_byte_cmap_font_overrides_lying_widths() {
        // reportlab-style embed: (1,0) format-6 cmap only, keyed by the PDF
        // string bytes. No unicode subtable at all — coverage must be
        // provable per byte code or the check never engages.
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id_with(&mut doc, Some(SUBSET_TTF_MAC));
        let f = font_info(&doc, id);
        assert!(!f.glyph_available(&doc, b'd', Some('d')));
        assert!(f.glyph_available(&doc, b'I', Some('I')));
        assert!(f.glyph_available(&doc, b' ', Some(' ')));
        // Byte-keyed lookup doesn't need char alignment: verdicts hold even
        // when the caller lost the byte↔char correspondence.
        assert!(f.glyph_available(&doc, b'I', None));
        assert!(!f.glyph_available(&doc, b'd', None));
    }

    #[test]
    fn nonsymbolic_dual_cmap_byte_hit_does_not_authorize() {
        // Nonsymbolic + unicode subtable present: the encoding's character
        // value is the authoritative key. The Mac table's 0x80 -> real
        // glyph must not approve WinAnsi 0x80 ('€', not in the font) — the
        // old OR logic did, and the viewer would paint the wrong glyph.
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id_with(&mut doc, Some(SUBSET_TTF_DUAL));
        let f = font_info(&doc, id);
        assert!(!f.symbolic, "fixture descriptor is nonsymbolic (Flags 32)");
        assert!(!f.glyph_available(&doc, 0x80, Some('\u{20AC}')));
        // Positive control: the unicode path still verifies real coverage.
        assert!(f.glyph_available(&doc, b'I', Some('I')));
        assert!(f.glyph_available(&doc, 0, Some('元')));
    }

    #[test]
    fn symbolic_flag_prefers_byte_keyed_cmap() {
        // Same dual-cmap font but flagged Symbolic (Flags 4): such fonts
        // resolve codes through their own byte-keyed table (ISO 32000
        // 9.6.6.4), so 0x80 — mapped to a real glyph there — is renderable.
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id_with(&mut doc, Some(SUBSET_TTF_DUAL));
        {
            let font_dict = doc.get_dictionary(id).unwrap();
            let fd_id = font_dict.get(b"FontDescriptor").unwrap().as_reference().unwrap();
            doc.get_dictionary_mut(fd_id).unwrap().set("Flags", 4);
        }
        let f = font_info(&doc, id);
        assert!(f.symbolic);
        assert!(f.glyph_available(&doc, 0x80, None));
        assert!(!f.glyph_available(&doc, 0x81, None), "unmapped byte still refused");
    }

    /// The subset with ONLY a (3,0) symbol cmap whose codes live in the
    /// 0xF100 range — one of the four high-byte ranges ISO 32000 permits
    /// for symbol tables (crbot #7 round-3 finding 1).
    const SUBSET_TTF_SYM_F100: &[u8] = include_bytes!("testdata/noto_sc_subset_symbol_f100.ttf");

    #[test]
    fn symbol_cmap_f100_range_is_probed() {
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id_with(&mut doc, Some(SUBSET_TTF_SYM_F100));
        {
            let font_dict = doc.get_dictionary(id).unwrap();
            let fd_id = font_dict.get(b"FontDescriptor").unwrap().as_reference().unwrap();
            doc.get_dictionary_mut(fd_id).unwrap().set("Flags", 4);
        }
        let f = font_info(&doc, id);
        // 'I' lives at 0xF149; probing only 0xF000|b and the bare byte
        // missed it and wrongly refused the whole font.
        assert!(f.glyph_available(&doc, b'I', None));
        assert!(!f.glyph_available(&doc, b'd', None), "absent glyph still refused");
    }

    #[test]
    fn missing_encoding_routes_through_byte_cmap() {
        // ISO 32000 9.6.6.4: no /Encoding entry -> the built-in
        // (3,0)/(1,0) cmap resolves the byte, same as the symbolic case,
        // regardless of the Symbolic flag. The dual-cmap fixture maps Mac
        // byte 0x80 to a real glyph, so WITHOUT /Encoding the byte is
        // renderable; WITH /Encoding (nonsymbolic) the unicode table rules
        // and 0x80/'€' stays refused (see the dual-cmap test above).
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id_with(&mut doc, Some(SUBSET_TTF_DUAL));
        doc.get_dictionary_mut(id).unwrap().remove(b"Encoding");
        let f = font_info(&doc, id);
        assert!(!f.symbolic && !f.has_encoding);
        assert!(f.glyph_available(&doc, 0x80, Some('\u{20AC}')));
        assert!(!f.glyph_available(&doc, 0x90, None), "unmapped byte still refused");
    }

    #[test]
    fn embedded_font_parse_is_cached_across_font_infos() {
        // crbot #7 round-1 finding 1: replace_run_inner rebuilds the font
        // map per segment; the parsed program must be shared, not re-parsed
        // (decompress+copy of a multi-MB font per segment froze WASM).
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id_with(&mut doc, Some(SUBSET_TTF));
        let f1 = font_info(&doc, id);
        let f2 = font_info(&doc, id);
        let p1 = f1.embedded_ttf(&doc).unwrap() as *const _;
        let p2 = f2.embedded_ttf(&doc).unwrap() as *const _;
        assert_eq!(p1, p2, "second FontInfo must reuse the cached parse");
    }

    #[test]
    fn no_font_program_keeps_widths_heuristic() {
        let mut doc = Document::with_version("1.7");
        let id = font_dict_id(&mut doc, false);
        let f = font_info(&doc, id);
        // Without FontFile2 there is no ground truth; the pre-existing
        // widths heuristic (and its false positives) is all we have.
        assert!(f.glyph_available(&doc, b'd', Some('d')));
        assert!(!f.glyph_available(&doc, 5, Some('\u{5}')));
    }

    /// End-to-end: replacing through the real pipeline must refuse (not
    /// silently emit tofu) when the replacement introduces a character the
    /// embedded subset has no glyph for, and still succeed when every
    /// character is covered.
    #[test]
    fn replace_refuses_missing_glyph_instead_of_tofu() {
        use crate::replace::{replace_text, ReplaceError};

        let build = || {
            let mut doc = Document::with_version("1.7");
            let font_id = font_dict_id(&mut doc, true);
            let content = doc.add_object(Stream::new(
                dictionary! {},
                b"BT /F1 24 Tf 72 700 Td (Ian) Tj ET".to_vec(),
            ));
            let pages_id = doc.new_object_id();
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference(pages_id),
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Contents" => Object::Reference(content),
                "Resources" => dictionary! {
                    "Font" => dictionary! { "F1" => Object::Reference(font_id) },
                },
            });
            doc.set_object(pages_id, dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            });
            let catalog = doc.add_object(dictionary! {
                "Type" => "Catalog",
                "Pages" => Object::Reference(pages_id),
            });
            doc.trailer.set("Root", Object::Reference(catalog));
            doc
        };

        // 'd' is absent from the subset: no fallback provided -> refuse.
        let mut doc = build();
        let err = replace_text(&mut doc, 1, "Ian", "Idn", None).unwrap_err();
        assert!(matches!(err, ReplaceError::MissingGlyph), "got {err:?}");

        // All replacement chars covered by the subset -> succeeds in the
        // original font.
        let mut doc = build();
        replace_text(&mut doc, 1, "Ian", "nIa", None).unwrap();
    }

    /// Same end-to-end guarantee for the byte-keyed-cmap embed shape.
    #[test]
    fn replace_refuses_missing_glyph_with_mac_cmap_font() {
        use crate::replace::{replace_text, ReplaceError};

        let mut doc = Document::with_version("1.7");
        let font_id = font_dict_id_with(&mut doc, Some(SUBSET_TTF_MAC));
        let content = doc.add_object(Stream::new(
            dictionary! {},
            b"BT /F1 24 Tf 72 700 Td (Ian) Tj ET".to_vec(),
        ));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(content),
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => Object::Reference(font_id) },
            },
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        });
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog));

        let err = replace_text(&mut doc, 1, "Ian", "Idn", None).unwrap_err();
        assert!(matches!(err, ReplaceError::MissingGlyph), "got {err:?}");
    }
}
