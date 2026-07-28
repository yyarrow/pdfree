//! Phase B: variable-length run replacement via LINE REGENERATION.
//!
//! A length-changing edit moves everything after it on the same line, and
//! PDF positions the NEXT line relative to state left by this one — so the
//! whole line's show ops are deleted and re-emitted with absolute (Tm)
//! positioning, and the original text-line matrix is restored afterwards so
//! following operations continue exactly as before.
//!
//! v1 scope (each guard falls back to an honest NeedsReflow error):
//! - the line's ops must be plain Tj/TJ (no '/" line-advance side effects)
//! - the line's ops must not carry segments of other lines
//! - the edited run must own whole segments (no straddling)
//! - the pushed line must still fit its block's width (block rewrap is
//!   Phase D)

use crate::matrix::Mat;
use crate::model::{build_page_model, MRun};
use crate::replace::{add_font_resource, set_page_content, ReplaceError, ReplaceReport};
use crate::ttf::TtfFont;
use crate::type3gen::build_type3_font;
use crate::walk::{gs_fonts_for_restore, load_fonts, walk_page_with_blanks, Seg};
use lopdf::content::Operation;
use lopdf::{Document, Object, ObjectId};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// How the replacement text will be rendered.
struct NewRunPlan {
    /// /Font resource name for Tf.
    res_name: String,
    string_bytes: Vec<u8>,
    /// Tf size to emit.
    size: f32,
    /// Advance in em-1000 units at `size`.
    adv_em: f32,
    /// True when the CHOSEN font is CID (2-byte codes) — may differ from the
    /// original segment's font, so spacing counts use this, not the seg's.
    cid: bool,
    /// False only for a CID font whose advances are still the 1000-per-char
    /// guess (no /W, or a non-Identity CMap we can't resolve to CIDs).
    widths_trusted: bool,
}

pub(crate) fn replace_run_reflow(
    doc: &mut Document,
    page_no: u32,
    block: usize,
    line: usize,
    run: usize,
    new_text: &str,
    fallback: Option<&TtfFont>,
) -> Result<ReplaceReport, ReplaceError> {
    let pages = doc.get_pages();
    let page_id = *pages.get(&page_no).ok_or(ReplaceError::PageNotFound(page_no))?;
    let (mut content, segs, blank_ops) = walk_page_with_blanks(doc, page_id, page_no)?;
    let blocks = build_page_model(&segs);
    let mblock = blocks.get(block).ok_or(ReplaceError::RunNotFound)?;
    let mline = mblock.lines.get(line).ok_or(ReplaceError::RunNotFound)?;
    let mrun = mline.runs.get(run).ok_or(ReplaceError::RunNotFound)?;

    // --- Guards ---------------------------------------------------------
    let line_segs: BTreeSet<usize> = mline.runs.iter().flat_map(|r| r.glyphs.iter().map(|g| g.seg)).collect();
    let run_segs: BTreeSet<usize> = mrun.glyphs.iter().map(|g| g.seg).collect();
    let line_ops: BTreeSet<usize> = line_segs.iter().map(|&i| segs[i].op_idx).collect();
    for (i, s) in segs.iter().enumerate() {
        if line_ops.contains(&s.op_idx) {
            // Every segment of a touched op must belong to this line.
            if !line_segs.contains(&i) {
                return Err(ReplaceError::NeedsReflow("op-segments-outside-line"));
            }
            let op = &content.operations[s.op_idx];
            if op.operator != "Tj" && op.operator != "TJ" {
                return Err(ReplaceError::NeedsReflow("non-tj-show-op"));
            }
        }
    }
    // The edited run must own its segments completely (no straddling).
    for other in mline.runs.iter().enumerate().filter(|(i, _)| *i != run).map(|(_, r)| r) {
        if other.glyphs.iter().any(|g| run_segs.contains(&g.seg)) {
            return Err(ReplaceError::NeedsReflow("run-straddles-segment"));
        }
    }
    // Anchor from the VISUALLY leftmost segment of the run, not the lowest
    // segment index: paint order may draw the right chunk first, and the
    // replacement must start where the run visually begins.
    let first_run_seg = *run_segs
        .iter()
        .min_by(|&&a, &&b| {
            segs[a].trm[4].partial_cmp(&segs[b].trm[4]).unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or(ReplaceError::RunNotFound)?;
    let anchor_ref = Mat(segs[first_run_seg].trm);

    // Clipping render modes (Tr 4–7) accumulate a text clip path that each ET
    // applies; regenerating the shows before one ET would union clips from
    // several text objects. We don't model clip paths — refuse.
    if line_segs.iter().any(|&si| segs[si].render_mode >= 4) {
        return Err(ReplaceError::NeedsReflow("clipping-render-mode"));
    }

    // Byte-coverage: a segment the run owns may contain glyphs whose decoded
    // text is empty (dropped from the model). If the run's modeled glyphs
    // don't account for every byte of a segment it deletes, those bytes —
    // possibly still painting — would be silently erased. Refuse.
    for &si in &run_segs {
        let covered: usize = mrun.glyphs.iter().filter(|g| g.seg == si).map(|g| g.byte_len).sum();
        if covered != segs[si].bytes.len() {
            return Err(ReplaceError::NeedsReflow("byte-coverage-gap"));
        }
    }

    // Paint-order contiguity: if a NON-run segment was painted between two of
    // the edited run's segments, collapsing the run to one show would flip
    // its stacking order — refuse. (Overlapping colored/annotation overlays.)
    let run_lo = run_segs.iter().map(|&i| (segs[i].op_idx, segs[i].str_idx)).min().unwrap();
    let run_hi = run_segs.iter().map(|&i| (segs[i].op_idx, segs[i].str_idx)).max().unwrap();
    for (i, s) in segs.iter().enumerate() {
        let pos = (s.op_idx, s.str_idx);
        if pos > run_lo && pos < run_hi && !run_segs.contains(&i) {
            return Err(ReplaceError::NeedsReflow("paint-order-interleaved"));
        }
    }

    // Non-UTF-8 font names: Seg.font is a lossy String, so a resource name
    // with non-UTF-8 bytes round-trips to replacement characters and would
    // name a nonexistent /Font. Refuse rather than emit a broken Tf.
    if line_segs.iter().any(|&si| segs[si].font.contains('\u{FFFD}')) {
        return Err(ReplaceError::NeedsReflow("non-utf8-font-name"));
    }

    // CTM-uniformity guard: regenerated ops are all spliced at the first
    // show position, so they execute under ONE live CTM. If the line's
    // chunks were drawn under different CTMs (cm/q between them), that's
    // unsafe — refuse. Also refuse pattern/separation fills we can only
    // approximate as rg, which would recolor the whole regenerated line.
    let anchor_ctm = Mat(segs[first_run_seg].ctm).0;
    for &si in &line_segs {
        let c = segs[si].ctm;
        if (0..6).any(|i| (c[i] - anchor_ctm[i]).abs() > 1e-4) {
            return Err(ReplaceError::NeedsReflow("ctm-differs"));
        }
        if segs[si].pattern_fill {
            return Err(ReplaceError::NeedsReflow("pattern-fill"));
        }
    }

    // Pushing runs that follow the edit only works when they share the
    // edited run's text-matrix orientation and scale (same a,b,c,d); a
    // right-aligned column or a differently-transformed run would be
    // displaced wrongly. If any following run differs, refuse (v1) rather
    // than corrupt — annotation overlays and multi-column lines land here.
    for other in &mline.runs[run + 1..] {
        if let Some(g) = other.glyphs.first() {
            let m = Mat(segs[g.seg].trm).0;
            let a = anchor_ref.0;
            let diff = (0..4).map(|i| (m[i] - a[i]).abs()).fold(0.0_f32, f32::max);
            if diff > 1e-3 {
                return Err(ReplaceError::NeedsReflow("following-run-transform"));
            }
        }
    }

    // Uniform-context guard. All regenerated text is hoisted to the first
    // show op, so any state that differs between the line's segments — or any
    // state-changing operator interleaved among them — would be applied to
    // the wrong text. Require every line segment to share spacing state, and
    // forbid state-changing ops (q/Q/cm/gs/clip/marked-content) between the
    // line's first and last op. Simple positioning/text ops are fine.
    {
        let s0 = &segs[first_run_seg];
        for &si in &line_segs {
            let s = &segs[si];
            if (s.char_spacing - s0.char_spacing).abs() > 1e-4
                || (s.word_spacing - s0.word_spacing).abs() > 1e-4
                || (s.h_scale - s0.h_scale).abs() > 1e-4
                || s.render_mode != s0.render_mode
            {
                return Err(ReplaceError::NeedsReflow("spacing-state-differs"));
            }
        }
        // Tr excluded from the allowlist: a later Tr would change the render
        // mode of text we hoisted before it.
        let lo = *line_ops.iter().min().unwrap();
        let hi = *line_ops.iter().max().unwrap();
        for (i, op) in content.operations.iter().enumerate().take(hi + 1).skip(lo) {
            match op.operator.as_str() {
                // Only state ops we can reason about may sit between chunks.
                "Td" | "TD" | "Tm" | "T*" | "Tf" | "TL" | "Tc" | "Tw" | "Tz" | "rg" | "g" | "k" | "cs" | "sc"
                | "scn" | "BT" | "ET" => {}
                // Marked content (ISO 32000 14.6) is a semantic tag: it
                // changes no graphics state and no glyph position, so
                // hoisting text across it is visually exact. Chrome/Skia
                // exports wrap every line in /NonStruct BDC…EMC, which
                // used to make ALL their lines unreflowable.
                //
                // The regenerated shows land inside whichever block held
                // the line's first op; the emptied blocks stay as valid
                // (if now empty) marked content. That costs structure-tree
                // granularity, not rendering.
                "BMC" | "EMC" | "DP" | "MP" => {}
                // /ActualText is handled by `owned_marked_blocks` below: a
                // block the line fully owns is deleted WITH its text, so no
                // stale extraction override survives. A block that also
                // covers text outside the line can't be deleted, and moving
                // our text out from under its /ActualText would change what
                // the document says when copied — that case refuses there.
                "BDC" => {}
                // A show op that isn't part of this modeled line would be left
                // in place while target chunks hoist to the first op —
                // corrupting its inherited state. Refuse.
                "Tj" | "TJ" | "'" | "\"" => {
                    if !line_ops.contains(&i) {
                        // A show op that painted only whitespace produced no
                        // segment, so it can't belong to any line — but it
                        // also paints nothing. Regenerated text is placed by
                        // absolute Tm and the positioning operators around it
                        // are untouched, so leaving it where it is changes
                        // nothing visually. (Chrome/Skia emit every space as
                        // its own Tj; refusing on those made ~90% of their
                        // lines unreflowable.)
                        if blank_ops.contains(&i) && !segs.iter().any(|s| s.op_idx == i) {
                            continue;
                        }
                        return Err(ReplaceError::NeedsReflow("foreign-show-op"));
                    }
                    // Every String element of a line op must map to a modeled
                    // segment; an undecodable string yields no seg and would
                    // be silently dropped when we delete the op. Require the
                    // string count to match this op's segment count.
                    let n_strings = op
                        .operands
                        .iter()
                        .map(|o| match o {
                            Object::String(..) => 1,
                            Object::Array(a) => a.iter().filter(|x| matches!(x, Object::String(..))).count(),
                            _ => 0,
                        })
                        .sum::<usize>();
                    let n_segs = segs.iter().filter(|s| s.op_idx == i).count();
                    if n_strings != n_segs {
                        return Err(ReplaceError::NeedsReflow("string-segment-count-mismatch"));
                    }
                }
                _ => return Err(ReplaceError::NeedsReflow("state-op-between-chunks")),
            }
        }
    }

    // BT/ET-span guard. Following lines in the SAME text object are often
    // positioned RELATIVE to this one (T*/Td chains); regenerating this line
    // with absolute Tm would break that chain even with a restored line
    // matrix. v1 only reflows when the edited line is the LAST show op of
    // its BT/ET — its own line each in its own BT/ET (Skia, Chrome), or the
    // final line of a paragraph. Otherwise refuse honestly.
    {
        let last_line_op = *line_ops.iter().max().unwrap();
        // The ET that closes the edited line's text object.
        let mut et = content.operations.len();
        for (i, op) in content.operations.iter().enumerate().skip(last_line_op + 1) {
            if op.operator == "ET" {
                et = i;
                break;
            }
            if op.operator == "BT" {
                break; // no ET before a new BT: treat as span end
            }
        }
        // Any show op between the edit and that ET means a following line
        // shares this text object.
        for op in &content.operations[last_line_op + 1..et] {
            if matches!(op.operator.as_str(), "Tj" | "TJ" | "'" | "\"") {
                return Err(ReplaceError::NeedsReflow("following-show-in-text-object"));
            }
        }
    }

    // --- Plan the replacement text's rendering ---------------------------
    let plan = plan_new_run(doc, page_id, &segs, mrun, new_text, fallback)?;

    // --- Geometry: widths, shift, fit ------------------------------------
    let anchor = Mat(segs[first_run_seg].trm);
    // Text must advance in the +x direction: the SIGNED horizontal advance
    // is anchor.a * font_size * h_scale. A matrix like [-1 0 0 -1] has a
    // positive determinant yet advances left, so the determinant is not the
    // right test — check the effective x advance sign directly. (delta is
    // later applied as +x to following runs, valid only for +x text.)
    let x_dir = anchor.0[0] * plan.size * segs[first_run_seg].h_scale;
    if x_dir <= 1e-6 {
        return Err(ReplaceError::NeedsReflow("nonpositive-x-advance"));
    }
    let x_scale = (anchor.0[0].powi(2) + anchor.0[1].powi(2)).sqrt().max(1e-6);
    // Vertical user-space scale, for the affected-region height.
    let y_scale = (anchor.0[2].powi(2) + anchor.0[3].powi(2)).sqrt().max(1e-6);
    // Old width is the visual SPAN (rightmost edge − leftmost origin), not
    // the sum of advances: segments merged into one run can have TJ-kerning
    // or positioning gaps between them that a sum would miss.
    let g_min = mrun.glyphs.iter().map(|g| g.x).fold(f32::INFINITY, f32::min);
    let g_max = mrun.glyphs.iter().map(|g| g.x + g.w).fold(f32::NEG_INFINITY, f32::max);
    let old_w: f32 = (g_max - g_min).max(0.0);
    // The new run's advance must include the same Tz/Tc/Tw the walked
    // old_w already reflects, or the delta (and the overflow guard) is wrong
    // under non-default spacing. All run segments share these (same style).
    // Tc is applied once per ENCODED code, Tw once per single-byte code 32 —
    // count the emitted bytes, not Unicode scalars (a ligature code maps to
    // multiple scalars but is one code).
    let sp = &segs[first_run_seg];
    // Code length of the CHOSEN font (Tc counts per code, Tw per 1-byte
    // code 32) — the plan may borrow/synthesize a font whose CID status
    // differs from the original segment's.
    let code_len = if plan.cid { 2 } else { 1 };
    let n_codes = (plan.string_bytes.len() / code_len).max(1) as f32;
    let n_spaces = if code_len == 1 {
        plan.string_bytes.iter().filter(|&&b| b == b' ').count() as f32
    } else {
        0.0
    };
    let new_w = (plan.adv_em / 1000.0 * plan.size + n_codes * sp.char_spacing + n_spaces * sp.word_spacing)
        * sp.h_scale
        * x_scale;
    // Our CID advance is a fabricated chars*1000/em placeholder, not the
    // descendant font's real /W metrics. This poisons BOTH sides of the
    // delta: the chosen font (new_w) AND the original run, whose glyph
    // positions — and thus old_w's span — were laid out with the fabricated
    // width even when the replacement font is simple. Refuse if either the
    // chosen or any source-run font is CID (until /W parsing lands).
    // A guessed CID advance poisons BOTH sides of the delta: the chosen
    // font's new_w AND the original run, whose glyph positions (and thus
    // old_w's span) were laid out with the guess. Real /W metrics on both
    // sides make the geometry sound, so only untrusted widths refuse.
    if !plan.widths_trusted || run_segs.iter().any(|&si| !segs[si].cid_widths_trusted) {
        return Err(ReplaceError::NeedsReflow("cid-width-unavailable"));
    }
    let delta = new_w - old_w;

    let line_end: f32 = mline.bbox[2];
    let edited_is_last = run + 1 == mline.runs.len();
    let new_line_end = if edited_is_last {
        (mrun.bbox[0] + new_w).max(mline.bbox[2].min(mrun.bbox[2]))
    } else {
        line_end + delta.max(0.0)
    };
    let slack = (mblock.bbox[2] - mblock.bbox[0]).abs().max(10.0) * 0.03 + 3.0;
    if new_line_end > mblock.bbox[2] + slack {
        return Err(ReplaceError::NeedsReflow("width-overflow"));
    }
    // Never push text past the page's VISIBLE right edge (CropBox, which may
    // be narrower than MediaBox): beyond it the text renders clipped.
    if let Some(page_right) = page_visible_right(doc, page_id) {
        if new_line_end > page_right - 2.0 {
            return Err(ReplaceError::NeedsReflow("push-target-overflow"));
        }
    }

    // --- Emit the regenerated line ---------------------------------------
    // gs-carried fonts have no /Font name; materialize them once.
    let gs_restore = gs_fonts_for_restore(doc, page_id);
    let mut gs_names: HashMap<String, String> = HashMap::new();
    for si in &line_segs {
        let key = &segs[*si].font;
        if key.starts_with("gs:") && !gs_names.contains_key(key) {
            let (id, dict) = gs_restore.get(key).cloned().ok_or(ReplaceError::Unencodable)?;
            let fid = id.unwrap_or_else(|| doc.add_object(Object::Dictionary(dict)));
            gs_names.insert(key.clone(), add_font_resource(doc, page_id, fid)?);
        }
    }

    let wrappers = actualtext_wrappers(doc, page_id, &content);
    let mut ops_new: Vec<Operation> = Vec::new();
    let mut emitted_new_run = false;
    // Segments in content-stream order keep the original paint order.
    let mut ordered: Vec<usize> = line_segs.iter().copied().collect();
    ordered.sort_by_key(|&i| (segs[i].op_idx, segs[i].str_idx));
    for si in ordered {
        let s = &segs[si];
        if run_segs.contains(&si) {
            if !emitted_new_run {
                emitted_new_run = true;
                // Anchor at the run's VISUALLY-leftmost segment, not this
                // paint-order-first one — they differ when the right chunk
                // was painted first.
                emit_text(
                    &mut ops_new,
                    &plan.res_name,
                    plan.size,
                    &segs[first_run_seg].fill_op,
                    &anchor,
                    &Mat(segs[first_run_seg].ctm),
                    0.0,
                    plan.string_bytes.clone(),
                );
            }
            continue; // remaining edited-run segments are covered by the plan
        }
        // Does this segment sit before or after the edited run on the line?
        let dx = if s.glyphs.first().map(|g| g.x).unwrap_or(0.0) > mrun.bbox[0] {
            delta
        } else {
            0.0
        };
        let res = match gs_names.get(&s.font) {
            Some(n) => n.clone(),
            None => s.font.clone(),
        };
        // This chunk is re-emitted VERBATIM (original font, original bytes).
        // If it was wrapped in a marked-content block carrying /ActualText,
        // that wrapper is its only machine-readable text — Chrome's Type3
        // glyphs have no usable ToUnicode — so it must travel with it.
        // (The edited run needs no wrapper: its replacement text is encoded
        // through a font whose ToUnicode we control.)
        let wrapper = wrappers.get(&s.op_idx);
        if let Some(w) = wrapper {
            ops_new.push(w.clone());
        }
        emit_text(&mut ops_new, &res, s.font_size, &s.fill_op, &Mat(s.trm), &Mat(s.ctm), dx, s.bytes.clone());
        if wrapper.is_some() {
            ops_new.push(Operation::new("EMC", vec![]));
        }
    }
    // Restore the text state the FOLLOWING ops inherit. Font, size and color
    // persist across BT/ET (each line here is its own BT/ET with no Tf), so
    // leaving our fallback font active would make the next line — which
    // relies on the inherited font — render with the wrong (glyph-less)
    // font. Reinstate the font/size/color active at the end of the original
    // line, then its text line matrix.
    let tail = last_stream_seg(&segs, &line_segs);
    let tail_seg = &segs[tail];
    let tail_res = gs_names.get(&tail_seg.font).cloned().unwrap_or_else(|| tail_seg.font.clone());
    if !tail_res.starts_with("gs:") {
        ops_new.push(Operation::new(
            "Tf",
            vec![Object::Name(tail_res.into_bytes()), Object::Real(tail_seg.font_size)],
        ));
    }
    // Replay the ORIGINAL fill operator (rg/g/k) so the nonstroking color
    // space that following content inherits is unchanged.
    let (fop, fargs) = &tail_seg.fill_op;
    ops_new.push(Operation::new(fop, fargs.iter().map(|v| Object::Real(*v)).collect()));
    let last_tlm = segs[tail].tlm_after;
    ops_new.push(Operation::new("Tm", last_tlm.iter().map(|v| Object::Real(*v)).collect()));

    // --- Splice: replace the first line op, drop the rest ----------------
    // Marked-content blocks the line fully owns go too: their show ops are
    // being removed, and an emptied /ActualText span would keep dictating
    // what the (now absent) text means to copy-paste and screen readers.
    // Deleting the block hands that job to the regenerated text's own
    // ToUnicode. Blocks with any outside content stay (and refused above).
    //
    // The insertion point stays the line's first SHOW op: it sits inside
    // the enclosing BT…ET, and text operators outside a text object are
    // invalid. Deleted marked-content ops may sit before it; removing from
    // the back keeps earlier indices stable.
    let first_op = *line_ops.iter().min().unwrap();
    let mut op_list: Vec<usize> = line_ops
        .iter()
        .copied()
        .chain(owned_marked_blocks(doc, page_id, &content, &line_ops)?)
        .collect();
    op_list.sort_unstable();
    op_list.dedup();
    for &idx in op_list.iter().rev() {
        if idx == first_op {
            content.operations.splice(idx..idx + 1, ops_new.clone());
        } else {
            content.operations.splice(idx..idx + 1, std::iter::empty());
        }
    }
    let encoded = content.encode()?;
    set_page_content(doc, page_id, encoded)?;

    // --- Report: the whole line (old and new extents) is the edit region --
    // Vertical pad covers ascenders/descenders of whichever font renders,
    // generously (with an absolute floor) — a few px short reads as a leak.
    // Vertical extent scales with the text matrix's y-scale — a 4x Tm makes
    // glyphs render 4x taller than raw font size would suggest.
    let size_pad = (mrun.font_size.max(plan.size) * 1.1 * y_scale).max(6.0);
    let bbox = [
        mline.bbox[0] - 1.0,
        mline.bbox[1].min(mrun.bbox[1]) - size_pad,
        new_line_end.max(mline.bbox[2]) + 1.0,
        mline.bbox[3].max(mrun.bbox[3]) + size_pad,
    ];
    Ok(ReplaceReport {
        page: page_no,
        old_text: mrun.text.clone(),
        new_text: new_text.to_string(),
        bbox,
    })
}

/// Choose a font that can render `new_text`: the run's own fonts first,
/// then any other page font (same behavior family as try_borrow), then the
/// synthesized Type3 fallback.
fn plan_new_run(
    doc: &mut Document,
    page_id: ObjectId,
    segs: &[Seg],
    mrun: &MRun,
    new_text: &str,
    fallback: Option<&TtfFont>,
) -> Result<NewRunPlan, ReplaceError> {
    let size = mrun.font_size;
    // Candidate order: run's own fonts, then the rest of the page's fonts.
    let mut candidates: Vec<String> = Vec::new();
    for g in &mrun.glyphs {
        let f = &segs[g.seg].font;
        if !candidates.contains(f) {
            candidates.push(f.clone());
        }
    }
    let chosen = {
        let fonts = load_fonts(doc, page_id);
        for (name, _) in &fonts {
            let name = String::from_utf8_lossy(name).into_owned();
            if !candidates.contains(&name) && !name.starts_with("gs:") {
                candidates.push(name);
            }
        }
        let mut found: Option<(String, Vec<u8>, f32, bool, bool)> = None;
        for cand in &candidates {
            let Some(font) = fonts.get(cand.as_bytes()) else { continue };
            if font.cid && !segs.iter().any(|s| &s.font == cand) {
                continue;
            }
            let Some(bytes) = font.encode(doc, new_text) else { continue };
            // Same byte↔char alignment rule as replace_seg_internal: one
            // byte per char for simple fonts, `None` (conservative) when a
            // ligature-style mapping breaks the correspondence.
            let chars: Vec<char> = new_text.chars().collect();
            let aligned = bytes.len() == chars.len();
            if !font.cid
                && !bytes
                    .iter()
                    .enumerate()
                    .all(|(i, &b)| font.glyph_available(doc, b, aligned.then(|| chars[i])))
            {
                continue;
            }
            let adv = font.advance(&bytes, new_text.chars().count());
            if adv <= 0.0 {
                continue;
            }
            found = Some((cand.clone(), bytes, adv, font.cid, font.cid_widths_trusted()));
            break;
        }
        found
    };
    if let Some((key, bytes, adv, cid, widths_trusted)) = chosen {
        // gs-carried fonts must be materialized under a real /Font name.
        let res_name = if key.starts_with("gs:") {
            let (id, dict) = gs_fonts_for_restore(doc, page_id)
                .get(&key)
                .cloned()
                .ok_or(ReplaceError::Unencodable)?;
            let fid = id.unwrap_or_else(|| doc.add_object(Object::Dictionary(dict)));
            add_font_resource(doc, page_id, fid)?
        } else {
            key
        };
        return Ok(NewRunPlan { res_name, string_bytes: bytes, size, adv_em: adv, cid, widths_trusted });
    }

    // Fallback synthesis, with the same weird-scale calibration policy as
    // the fixed-length path.
    let ttf = fallback.ok_or(ReplaceError::Unencodable)?;
    let orig_t3_scale = load_fonts(doc, page_id)
        .get(segs[mrun.glyphs[0].seg].font.as_bytes())
        .filter(|f| f.type3)
        .map(|f| f.width_scale);
    let chars: Vec<char> = new_text.chars().collect();
    let fb = build_type3_font(doc, ttf, &chars).ok_or(ReplaceError::Unencodable)?;
    let res_name = add_font_resource(doc, page_id, fb.font_id)?;
    let string_bytes: Vec<u8> = new_text.chars().map(|c| fb.codes[&c]).collect();
    let adv_em: f32 = new_text.chars().map(|c| fb.advances[&c] / fb.units_per_em * 1000.0).sum();
    let mut fb_size = size;
    if let Some(ws) = orig_t3_scale {
        if !(0.5..=2.0).contains(&ws) {
            let old_glyphs = mrun.glyphs.len().max(1) as f32;
            let old_w_user: f32 = mrun.glyphs.iter().map(|g| g.w).sum();
            let anchor = Mat(segs[mrun.glyphs[0].seg].trm);
            let x_scale = (anchor.0[0].powi(2) + anchor.0[1].powi(2)).sqrt().max(1e-6);
            // Old per-char advance in em1000 relative to the Tf size.
            let old_per_char = old_w_user / x_scale / size.max(1e-6) * 1000.0 / old_glyphs;
            if old_per_char.is_finite() && old_per_char > 0.0 && (old_per_char < 100.0 || old_per_char > 2500.0) {
                fb_size = size * old_per_char / 550.0;
            }
        }
    }
    // The synthesized fallback Type3 font is single-byte.
    Ok(NewRunPlan { res_name, string_bytes, size: fb_size, adv_em, cid: false, widths_trusted: true })
}

/// fill-color + Tf + Tm + TJ for one positioned chunk. `dx` shifts the
/// anchor in user space along x; the Tm operand is the desired TRM mapped
/// back through the CTM. `fill` replays the segment's original nonstroking
/// color operator (rg/g/k) so the color space is preserved.
#[allow(clippy::too_many_arguments)]
fn emit_text(
    ops: &mut Vec<Operation>,
    res_name: &str,
    size: f32,
    fill: &(String, Vec<f32>),
    trm: &Mat,
    ctm: &Mat,
    dx: f32,
    bytes: Vec<u8>,
) {
    let desired = {
        let mut m = *trm;
        m.0[4] += dx;
        m
    };
    let tm = match ctm.invert() {
        Some(inv) => desired.mul(&inv),
        None => desired, // degenerate CTM: emit as-is, judges will decide
    };
    ops.push(Operation::new(&fill.0, fill.1.iter().map(|v| Object::Real(*v)).collect()));
    ops.push(Operation::new(
        "Tf",
        vec![Object::Name(res_name.as_bytes().to_vec()), Object::Real(size)],
    ));
    ops.push(Operation::new("Tm", tm.0.iter().map(|v| Object::Real(*v)).collect()));
    ops.push(Operation::new(
        "TJ",
        vec![Object::Array(vec![Object::String(bytes, lopdf::StringFormat::Literal)])],
    ));
}

/// The page's visible right edge: the inherited CropBox if present, else the
/// MediaBox. Both /CropBox and /MediaBox are inheritable page-tree
/// attributes; the walk carries a visited set for cycle safety.
fn page_visible_right(doc: &Document, page_id: ObjectId) -> Option<f32> {
    let right_of = |key: &[u8]| -> Option<f32> {
        let mut id = page_id;
        let mut seen = std::collections::HashSet::new();
        while seen.insert(id) {
            let dict = doc.get_object(id).ok()?.as_dict().ok()?;
            if let Ok(b) = dict.get(key).and_then(|o| doc.dereference(o)).map(|(_, o)| o) {
                if let Ok(arr) = b.as_array() {
                    return arr.get(2)?.as_float().ok();
                }
            }
            match dict.get(b"Parent") {
                Ok(Object::Reference(p)) => id = *p,
                _ => return None,
            }
        }
        None
    };
    right_of(b"CropBox").or_else(|| right_of(b"MediaBox"))
}

/// The line's last segment in content-stream order — its text state is what
/// the operations after the line inherit.
fn last_stream_seg(segs: &[Seg], line_segs: &BTreeSet<usize>) -> usize {
    *line_segs
        .iter()
        .max_by_key(|&&i| (segs[i].op_idx, segs[i].str_idx))
        .expect("line has at least one segment")
}

/// Whether a BDC operation carries /ActualText. The property list may be an
/// inline dictionary OR a name resolved through the page's
/// /Resources/Properties (ISO 32000 14.6.2) — missing the named form would
/// let a stale extraction override slip past the guards below.
fn bdc_has_actual_text(doc: &Document, page_id: ObjectId, op: &Operation) -> bool {
    op.operands.iter().any(|o| match o {
        Object::Dictionary(d) => d.has(b"ActualText"),
        Object::Name(name) => (|| {
            let (res, res_ids) = doc.get_page_resources(page_id).ok()?;
            let mut dicts: Vec<&lopdf::Dictionary> = res.into_iter().collect();
            let owned: Vec<&lopdf::Dictionary> = res_ids
                .iter()
                .filter_map(|id| doc.get_object(*id).ok()?.as_dict().ok())
                .collect();
            dicts.extend(owned);
            for d in dicts {
                let props = d
                    .get(b"Properties")
                    .ok()
                    .and_then(|o| doc.dereference(o).ok())
                    .and_then(|(_, o)| o.as_dict().ok().cloned());
                if let Some(props) = props {
                    if let Some(entry) = props.get(name.as_slice()).ok() {
                        if let Ok((_, v)) = doc.dereference(entry) {
                            if v.as_dict().map(|d| d.has(b"ActualText")).unwrap_or(false) {
                                return Some(true);
                            }
                        }
                    }
                }
            }
            None
        })()
        .unwrap_or(false),
        _ => false,
    })
}

/// For each show op, the innermost enclosing marked-content block that
/// carries /ActualText. Chunks the edit does NOT touch are re-emitted
/// verbatim (original font, original bytes) — for Chrome's Type3 glyphs
/// that /ActualText IS their only copy-paste text, so their wrapper must
/// be re-emitted with them or the surrounding text silently changes when
/// copied.
fn actualtext_wrappers(
    doc: &Document,
    page_id: ObjectId,
    content: &lopdf::content::Content,
) -> HashMap<usize, Operation> {
    let mut stack: Vec<(Operation, bool)> = Vec::new();
    let mut map = HashMap::new();
    for (i, op) in content.operations.iter().enumerate() {
        match op.operator.as_str() {
            "BDC" => {
                let has = bdc_has_actual_text(doc, page_id, op);
                stack.push((op.clone(), has));
            }
            "BMC" => stack.push((op.clone(), false)),
            "EMC" => {
                stack.pop();
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if let Some((w, _)) = stack.iter().rev().find(|(_, has)| *has) {
                    map.insert(i, w.clone());
                }
            }
            _ => {}
        }
    }
    map
}

/// Marked-content blocks (BDC/BMC … EMC) whose shown text belongs ENTIRELY
/// to the line being regenerated — their operator indices, so the caller can
/// delete the block along with the text it wrapped.
///
/// Why delete rather than keep: an emptied `/Span <</ActualText …>>` keeps
/// telling copy-paste and screen readers what text is there after that text
/// is gone, so the document would say one thing and show another. Removing
/// the block hands that job back to the regenerated text's own ToUnicode.
/// The structure tree may end up with an unreferenced MCID, which readers
/// tolerate (rendering is unaffected).
///
/// A block that ALSO wraps text outside the line can't be deleted; if such
/// a block carries /ActualText over our text, moving the text out would
/// corrupt what the document reports, so that refuses instead.
fn owned_marked_blocks(
    doc: &Document,
    page_id: ObjectId,
    content: &lopdf::content::Content,
    line_ops: &BTreeSet<usize>,
) -> Result<Vec<usize>, ReplaceError> {
    // (start index, saw line text, saw foreign text) — nesting depth is
    // small in practice, so updating the whole stack per show op is cheap.
    let mut stack: Vec<(usize, bool, bool)> = Vec::new();
    let mut owned = Vec::new();
    for (i, op) in content.operations.iter().enumerate() {
        match op.operator.as_str() {
            "BDC" | "BMC" => stack.push((i, false, false)),
            "EMC" => {
                let Some((start, saw_line, saw_foreign)) = stack.pop() else {
                    continue; // unbalanced EMC: not ours to reason about
                };
                if !saw_line {
                    continue; // untouched block
                }
                if saw_foreign {
                    let dictates_text = bdc_has_actual_text(doc, page_id, &content.operations[start]);
                    if dictates_text {
                        return Err(ReplaceError::NeedsReflow("actualtext-span"));
                    }
                    continue; // shared block stays; no stale override on it
                }
                owned.push(start);
                owned.push(i);
            }
            "Tj" | "TJ" | "'" | "\"" => {
                let mine = line_ops.contains(&i);
                for entry in stack.iter_mut() {
                    if mine {
                        entry.1 = true;
                    } else {
                        entry.2 = true;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(owned)
}

#[cfg(test)]
mod skia_tests {
    use super::*;
    use lopdf::content::Content;
    use lopdf::{dictionary, Stream, StringFormat};

    /// Minimal doc+page so the property-list resolver has resources.
    fn empty_page() -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {},
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
        });
        (doc, page_id)
    }

    fn bdc(actual_text: bool) -> Operation {
        let mut props = dictionary! { "MCID" => 0 };
        if actual_text {
            props.set("ActualText", Object::String(b"\xfe\xff\x5d\xe5".to_vec(), StringFormat::Hexadecimal));
        }
        Operation::new("BDC", vec![Object::Name(b"Span".to_vec()), Object::Dictionary(props)])
    }
    fn tj(s: &[u8]) -> Operation {
        Operation::new("Tj", vec![Object::String(s.to_vec(), StringFormat::Literal)])
    }

    #[test]
    fn block_owned_by_the_line_is_deleted_with_its_text() {
        let (doc, page_id) = empty_page();
        // BDC … Tj … EMC where the Tj is the line's own: the block goes
        // too, so a stale /ActualText can't outlive the text it described.
        let content = Content { operations: vec![bdc(true), tj(b"a"), Operation::new("EMC", vec![])] };
        let line_ops: BTreeSet<usize> = [1].into_iter().collect();
        assert_eq!(owned_marked_blocks(&doc, page_id, &content, &line_ops).unwrap(), vec![0, 2]);
    }

    #[test]
    fn block_shared_with_outside_text_refuses_when_it_dictates_actualtext() {
        let (doc, page_id) = empty_page();
        // The block also wraps text we are NOT regenerating, so it can't be
        // deleted; moving our text out from under its /ActualText would make
        // the document report something it no longer shows.
        let content = Content {
            operations: vec![bdc(true), tj(b"a"), tj(b"b"), Operation::new("EMC", vec![])],
        };
        let line_ops: BTreeSet<usize> = [1].into_iter().collect(); // op 2 is foreign
        assert!(matches!(
            owned_marked_blocks(&doc, page_id, &content, &line_ops),
            Err(ReplaceError::NeedsReflow("actualtext-span"))
        ));
    }

    #[test]
    fn shared_block_without_actualtext_is_left_alone() {
        let (doc, page_id) = empty_page();
        let content = Content {
            operations: vec![bdc(false), tj(b"a"), tj(b"b"), Operation::new("EMC", vec![])],
        };
        let line_ops: BTreeSet<usize> = [1].into_iter().collect();
        assert_eq!(owned_marked_blocks(&doc, page_id, &content, &line_ops).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn untouched_blocks_are_not_deleted() {
        let (doc, page_id) = empty_page();
        let content = Content {
            operations: vec![bdc(false), tj(b"x"), Operation::new("EMC", vec![])],
        };
        let line_ops: BTreeSet<usize> = BTreeSet::new(); // nothing of ours inside
        assert_eq!(owned_marked_blocks(&doc, page_id, &content, &line_ops).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn untouched_chunk_keeps_its_actualtext_wrapper() {
        // crbot #14 round-1: the line's OTHER chunks are re-emitted verbatim
        // (original Type3 bytes, no usable ToUnicode), so deleting their
        // /ActualText wrapper would silently change what copying yields.
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        // Edited chunk (AA) plain; neighbour (BB) wrapped with /ActualText.
        let stream = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm (AA) Tj ( ) Tj ET \
/Span <</ActualText (ZZ)>> BDC BT /F1 12 Tf 1 0 0 1 120 700 Tm (BB) Tj ET EMC \
BT /F1 12 Tf 1 0 0 1 72 684 Tm (a much longer second line of text here) Tj ET".to_vec();
        let content_id = doc.add_object(Stream::new(dictionary! {}, stream));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(content_id),
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => Object::Reference(font) } },
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
        });
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference(pages_id) });
        doc.trailer.set("Root", Object::Reference(catalog));

        let (_, segs) = crate::walk::walk_page(&doc, page_id, 1).unwrap();
        let blocks = crate::model::build_page_model(&segs);
        let (b, l, r) = (0..blocks.len())
            .flat_map(|b| (0..blocks[b].lines.len()).map(move |l| (b, l)))
            .flat_map(|(b, l)| (0..blocks[b].lines[l].runs.len()).map(move |r| (b, l, r)))
            .find(|&(b, l, r)| blocks[b].lines[l].runs[r].text.starts_with("AA"))
            .expect("modeled run");
        replace_run_reflow(&mut doc, 1, b, l, r, "AAA", None).expect("must reflow");

        let data = doc.get_page_content(page_id).unwrap();
        let out = lopdf::content::Content::decode(&data).unwrap();
        // The neighbour's wrapper must still enclose a show op.
        let mut depth_ok = false;
        let mut inside = false;
        for op in &out.operations {
            match op.operator.as_str() {
                "BDC" => {
                    inside = op.operands.iter().any(|o| matches!(o, Object::Dictionary(d) if d.has(b"ActualText")))
                }
                "EMC" => inside = false,
                "Tj" | "TJ" if inside => depth_ok = true,
                _ => {}
            }
        }
        assert!(depth_ok, "untouched chunk must stay inside its /ActualText block");
    }

    /// End-to-end on Chrome/Skia's shape: every line wrapped in BDC…EMC and
    /// every SPACE shown by its own Tj. Both used to make the line
    /// unreflowable (state-op-between-chunks / foreign-show-op).
    #[test]
    fn skia_shaped_line_reflows() {
        let mut doc = Document::with_version("1.7");
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        // BDC / BT / Tf / Tm / "AA" / " " / "BB" / ET / EMC
        // A second, much longer line in the same block so the block's width
        // leaves room for growth (a lone short line makes ANY lengthening
        // overflow, which is a separate guard).
        let stream = b"/Span <</MCID 0>> BDC BT /F1 12 Tf 1 0 0 1 72 700 Tm (AA) Tj ( ) Tj (BB) Tj ET EMC \
/Span <</MCID 1>> BDC BT /F1 12 Tf 1 0 0 1 72 684 Tm (a much longer second line of text here) Tj ET EMC".to_vec();
        let content_id = doc.add_object(Stream::new(dictionary! {}, stream));
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => Object::Reference(content_id),
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => Object::Reference(font) } },
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
        });
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => Object::Reference(pages_id) });
        doc.trailer.set("Root", Object::Reference(catalog));

        // Find the run holding "AA" and lengthen it.
        let (_, segs) = crate::walk::walk_page(&doc, page_id, 1).unwrap();
        let blocks = crate::model::build_page_model(&segs);
        let (b, l, r) = (0..blocks.len())
            .flat_map(|b| (0..blocks[b].lines.len()).map(move |l| (b, l)))
            .flat_map(|(b, l)| (0..blocks[b].lines[l].runs.len()).map(move |r| (b, l, r)))
            .find(|&(b, l, r)| blocks[b].lines[l].runs[r].text.starts_with("AA"))
            .expect("modeled run");

        replace_run_reflow(&mut doc, 1, b, l, r, "AAA", None).expect("skia-shaped line must reflow");

        let (_, segs2) = crate::walk::walk_page(&doc, page_id, 1).unwrap();
        let text: String = segs2.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("AAA"), "edited text present, got {text:?}");
        assert!(text.contains("BB"), "trailing text survived, got {text:?}");
    }
}
