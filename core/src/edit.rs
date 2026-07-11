//! Model-level editing: operations addressed at the reconstructed text
//! hierarchy (block/line/run) rather than at raw content-stream segments.
//!
//! v1 supports same-length replacement of a run's text. Each affected
//! segment is rewritten through the proven single-segment path; segments
//! are edited in descending (op_idx, str_idx) order so provenance indices
//! from the initial walk stay valid for the remaining targets. All work
//! happens on a clone of the document — either every sub-edit lands or
//! none do.

use crate::model::build_page_model;
use crate::replace::{replace_seg_internal, ReplaceError, ReplaceReport};
use crate::ttf::TtfFont;
use crate::walk::walk_page;
use lopdf::Document;
use std::collections::BTreeMap;

/// Replace the full text of run `[block][line][run]` on `page_no` with
/// `new_text` (must decode to the same number of characters — length
/// changes need Phase B line reflow).
pub fn replace_run_text(
    doc: &mut Document,
    page_no: u32,
    block: usize,
    line: usize,
    run: usize,
    new_text: &str,
    fallback: Option<&TtfFont>,
) -> Result<ReplaceReport, ReplaceError> {
    crate::replace::reject_encrypted(doc)?;
    let mut work = doc.clone();
    let report = replace_run_inner(&mut work, page_no, block, line, run, new_text, fallback)?;
    *doc = work;
    Ok(report)
}

fn replace_run_inner(
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
    let (_, segs) = walk_page(doc, page_id, page_no)?;
    let blocks = build_page_model(&segs);
    let mrun = blocks
        .get(block)
        .and_then(|b| b.lines.get(line))
        .and_then(|l| l.runs.get(run))
        .ok_or(ReplaceError::RunNotFound)?;

    if mrun.text == new_text {
        return Ok(ReplaceReport {
            page: page_no,
            old_text: mrun.text.clone(),
            new_text: new_text.to_string(),
            bbox: mrun.bbox,
        });
    }

    // Walk the run glyph by glyph, giving each glyph its slice of the new
    // text (glyphs can decode to several chars, e.g. ligatures). Any length
    // mismatch is a reflow job, not a patch job.
    let new_chars: Vec<char> = new_text.chars().collect();
    let mut cursor = 0usize;
    // seg index -> (byte_start -> replacement text)
    let mut per_seg: BTreeMap<usize, BTreeMap<usize, String>> = BTreeMap::new();
    for g in &mrun.glyphs {
        let n = g.text.chars().count();
        if cursor + n > new_chars.len() {
            return Err(ReplaceError::NeedsReflow);
        }
        let piece: String = new_chars[cursor..cursor + n].iter().collect();
        if piece != g.text {
            per_seg.entry(g.seg).or_default().insert(g.byte_start, piece);
        }
        cursor += n;
    }
    if cursor != new_chars.len() {
        return Err(ReplaceError::NeedsReflow);
    }

    // Build each affected segment's full new text from its own glyph list.
    let mut plan: Vec<(usize, usize, String)> = Vec::new(); // (op_idx, str_idx, new seg text)
    let mut bbox = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for (seg_idx, changes) in per_seg {
        let seg = &segs[seg_idx];
        let new_seg_text: String = seg
            .glyphs
            .iter()
            .map(|g| changes.get(&g.byte_start).cloned().unwrap_or_else(|| g.text.clone()))
            .collect();
        plan.push((seg.op_idx, seg.str_idx, new_seg_text));
        bbox[0] = bbox[0].min(seg.bbox[0]);
        bbox[1] = bbox[1].min(seg.bbox[1]);
        bbox[2] = bbox[2].max(seg.bbox[2]);
        bbox[3] = bbox[3].max(seg.bbox[3]);
    }

    // Descending order: rescue paths may splice extra ops, which only
    // shifts operations AFTER the edited one — untouched earlier targets
    // keep their (op_idx, str_idx) identity across re-walks.
    plan.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));

    let old_text = mrun.text.clone();
    for (op_idx, str_idx, new_seg_text) in plan {
        let (content, segs2) = walk_page(doc, page_id, page_no)?;
        let idx = segs2
            .iter()
            .position(|s| s.op_idx == op_idx && s.str_idx == str_idx)
            .ok_or(ReplaceError::RunNotFound)?;
        let rep = replace_seg_internal(doc, page_id, page_no, content, segs2, idx, new_seg_text, fallback)?;
        bbox[0] = bbox[0].min(rep.bbox[0]);
        bbox[1] = bbox[1].min(rep.bbox[1]);
        bbox[2] = bbox[2].max(rep.bbox[2]);
        bbox[3] = bbox[3].max(rep.bbox[3]);
    }

    Ok(ReplaceReport {
        page: page_no,
        old_text,
        new_text: new_text.to_string(),
        bbox,
    })
}
