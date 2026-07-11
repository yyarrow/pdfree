//! The text model: reconstructs an editable hierarchy — Glyph → Run → Line
//! → Block — from the flat segment stream the interpreter produces.
//!
//! PDF stores only ink placement; every level here is a heuristic
//! reconstruction, which is exactly why the verification loop judges the
//! model's edits rather than trusting the grouping. Reading order, baseline
//! clustering and block splits will be wrong sometimes; they must fail
//! honestly (NeedsReflow / no-match), never corrupt output.

use crate::walk::Seg;
use serde::Serialize;

/// A glyph in the model, with provenance back to the content stream.
#[derive(Debug, Clone, Serialize)]
pub struct MGlyph {
    pub text: String,
    /// Baseline origin and advance, PDF user space.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Provenance: index into the page's seg list + byte range inside it.
    pub seg: usize,
    pub byte_start: usize,
    pub byte_len: usize,
}

/// Maximal style-continuous glyph sequence within a line.
#[derive(Debug, Clone, Serialize)]
pub struct MRun {
    pub text: String,
    pub bbox: [f32; 4],
    pub font: String,
    pub font_size: f32,
    pub color: [f32; 3],
    pub cid: bool,
    pub type3: bool,
    pub glyphs: Vec<MGlyph>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MLine {
    pub baseline: f32,
    pub bbox: [f32; 4],
    pub runs: Vec<MRun>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MBlock {
    pub bbox: [f32; 4],
    pub lines: Vec<MLine>,
}

/// Working record before grouping.
struct G {
    text: String,
    x: f32,
    y: f32,
    w: f32,
    size: f32,
    font: String,
    color: [f32; 3],
    cid: bool,
    type3: bool,
    seg: usize,
    byte_start: usize,
    byte_len: usize,
}

pub fn build_page_model(segs: &[Seg]) -> Vec<MBlock> {
    // Flatten to positioned glyphs. Invisible (OCR layers) and rotated text
    // stay out of the model for now — they can't be judged visually.
    let mut gs: Vec<G> = Vec::new();
    for (si, seg) in segs.iter().enumerate() {
        if !seg.visible || !seg.horizontal {
            continue;
        }
        for gl in &seg.glyphs {
            if gl.text.is_empty() {
                continue;
            }
            gs.push(G {
                text: gl.text.clone(),
                x: gl.x,
                y: gl.y,
                w: gl.w,
                size: seg.font_size,
                font: seg.font.clone(),
                color: seg.color,
                cid: seg.cid,
                type3: seg.type3,
                seg: si,
                byte_start: gl.byte_start,
                byte_len: gl.byte_len,
            });
        }
    }
    if gs.is_empty() {
        return Vec::new();
    }

    // --- Lines: cluster by baseline y, tolerance scaled to glyph size. ---
    gs.sort_by(|a, b| b.y.partial_cmp(&a.y).unwrap_or(std::cmp::Ordering::Equal));
    let mut line_groups: Vec<Vec<G>> = Vec::new();
    for g in gs {
        let tol = (g.size * 0.25).max(1.0);
        match line_groups.last_mut() {
            Some(group) if (group[0].y - g.y).abs() <= tol => group.push(g),
            _ => line_groups.push(vec![g]),
        }
    }

    // --- Runs within each line: sort by x, split on style change or gap. ---
    let mut lines: Vec<MLine> = Vec::new();
    for mut group in line_groups {
        group.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        let baseline = group.iter().map(|g| g.y).sum::<f32>() / group.len() as f32;

        let mut runs: Vec<MRun> = Vec::new();
        let mut cur: Vec<G> = Vec::new();
        for g in group {
            // Style identity is visual (size + color + continuity), NOT the
            // font resource name: Skia gives every glyph chunk its own Type3
            // font, and per-glyph provenance makes mixed-font runs editable
            // anyway.
            let split = match cur.last() {
                None => false,
                Some(p) => {
                    let gap = g.x - (p.x + p.w);
                    // Word gaps (~0.25em+) split runs: Latin text becomes
                    // word-level click targets; CJK (no gaps) stays whole.
                    (p.size - g.size).abs() > 0.01
                        || p.color != g.color
                        || gap > (g.size * 0.24).max(1.0)
                        || gap < -(g.size * 0.4) // overlap: separate positioning context
                }
            };
            if split {
                runs.push(finish_run(std::mem::take(&mut cur)));
            }
            cur.push(g);
        }
        if !cur.is_empty() {
            runs.push(finish_run(cur));
        }

        let bbox = union(runs.iter().map(|r| r.bbox));
        lines.push(MLine { baseline, bbox, runs });
    }

    // --- Blocks: consecutive lines with compatible leading and x-overlap. ---
    let mut blocks: Vec<MBlock> = Vec::new();
    for line in lines {
        let joined = blocks.last_mut().is_some_and(|b| {
            let prev = b.lines.last().unwrap();
            let size = prev.runs.first().map(|r| r.font_size).unwrap_or(12.0);
            let gap = prev.baseline - line.baseline;
            let x_overlap = line.bbox[2].min(prev.bbox[2]) - line.bbox[0].max(prev.bbox[0]);
            gap > 0.0 && gap < size * 2.2 && x_overlap > 0.0
        });
        if joined {
            let b = blocks.last_mut().unwrap();
            b.bbox = union([b.bbox, line.bbox].into_iter());
            b.lines.push(line);
        } else {
            blocks.push(MBlock { bbox: line.bbox, lines: vec![line] });
        }
    }
    blocks
}

fn finish_run(gs: Vec<G>) -> MRun {
    let first = &gs[0];
    let size = first.size;
    let x0 = gs.iter().map(|g| g.x).fold(f32::INFINITY, f32::min);
    let x1 = gs.iter().map(|g| g.x + g.w).fold(f32::NEG_INFINITY, f32::max);
    let y = first.y;
    MRun {
        text: gs.iter().map(|g| g.text.as_str()).collect(),
        bbox: [x0, y - 0.35 * size, x1, y + 1.05 * size],
        font: first.font.clone(),
        font_size: size,
        color: first.color,
        cid: first.cid,
        type3: first.type3,
        glyphs: gs
            .into_iter()
            .map(|g| MGlyph {
                text: g.text,
                x: g.x,
                y: g.y,
                w: g.w,
                seg: g.seg,
                byte_start: g.byte_start,
                byte_len: g.byte_len,
            })
            .collect(),
    }
}

fn union(boxes: impl Iterator<Item = [f32; 4]>) -> [f32; 4] {
    let mut out = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for b in boxes {
        out[0] = out[0].min(b[0]);
        out[1] = out[1].min(b[1]);
        out[2] = out[2].max(b[2]);
        out[3] = out[3].max(b[3]);
    }
    out
}
