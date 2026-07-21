#!/usr/bin/env python3
"""pdfree oracle loop: for each corpus PDF, make a random text edit through the
engine and judge the result with four automatic checks:

  1. structure  — qpdf --check must not get worse than the input
  2. isolation  — rendered pixels outside the edit bbox must be unchanged
  3. visibility — pixels inside the edit bbox must have changed
  4. semantics  — extracted text must contain the new string

Three supplementary font-fidelity oracles run alongside the above. Font
substitution / glyph tofu failures override the file's headline verdict
(fail beats pass; case.json keeps the original as primary_verdict); the
identity-drift probe and oracle-availability skips are counted in a
separate `checks` dict in report.json (and their own stdout section) so
the headline stays exactly one verdict per corpus file. A check's own
inability to run degrades to a skip — it never asserts a pass/fail it
can't back up:

  5. identity drift      — re-encoding the SAME text (find == with) must
     render pixel-identical; any diff can only come from the engine's
     re-encoding path itself, not from edit content. See
     check_identity_drift().
  6. font substitution   — characters the ORIGINAL font already covered
     (drawn elsewhere on the before-page in that font) must keep the same
     base font after the edit. See detect_font_substitution().
  7. glyph tofu           — even when the font NAME survives an edit, the
     engine can encode a character into a subset font missing that glyph's
     outline: the text layer round-trips (ToUnicode is fine) but nothing
     gets painted. See detect_glyph_tofu().

Checks 6/7 use mutool (`mutool draw -F stext`) as a black-box oracle: only
its stdout is parsed here, never MuPDF source or internals (see
mutool_char_spans()).

Results: summary to stdout + report.json; failing cases archived under
harness/failures/<case>/ with in/out PDFs and a diff image.
"""

import argparse
import difflib
import hashlib
import json
import random
import re
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).parent
ENGINE = ROOT.parent / "core" / "target" / "debug" / "pdfree"
FAILURES = ROOT / "failures"
SCALE = 2.0
BBOX_PAD = 6  # pixels of slack around the reported edit bbox

MUTOOL = shutil.which("mutool") or "/opt/homebrew/bin/mutool"
SUBSET_PREFIX_RE = re.compile(r"^[A-Z]{6}\+")  # ISO 32000 9.6.4: subset tag


def sh(args):
    # errors="replace": qpdf/engine stderr can contain raw bytes from
    # malformed PDFs; never let output decoding kill the harness.
    return subprocess.run(args, capture_output=True, text=True, errors="replace")


def qpdf_status(path):
    r = sh(["qpdf", "--check", str(path)])
    return r.returncode


class RenderCrash(Exception):
    """pdfium failed or segfaulted in the worker process."""


def render_page(pdf_path, page_index, out_png):
    """Render + extract text via the isolated worker.

    Returns (image, page_box, text) where page_box is (left, bottom, right,
    top) in PDF user space; (None, None, None) for rotated pages.
    Raises RenderCrash if the worker dies.
    """
    r = sh([
        sys.executable, str(ROOT / "render_worker.py"),
        str(pdf_path), str(page_index), str(out_png), str(SCALE),
    ])
    if r.returncode != 0:
        raise RenderCrash(f"rc={r.returncode} {r.stderr.strip()[:120]}")
    info = json.loads(r.stdout)
    if info["rotated"]:
        return None, None, None
    return Image.open(out_png), info["box"], info["text"]


VARLEN = False  # set by --varlen: probe variable-length replacements too


def pick_edits(runs, rng, n=3):
    """Yield up to n distinct (run, find, with_) candidates, so a font that
    can't encode one replacement doesn't end the whole case.

    Only (run, find) pairs where `run` is the page's FIRST run containing
    `find` are yielded (engine_edits_this_run): the engine edits that
    first occurrence, so picking any other run would make the judge
    anchor its oracles on a segment the engine never touched. Words of a
    run that also appear in an earlier run are simply not usable for that
    run — try the run's other words before giving up on it.
    """
    candidates = [
        r for r in runs
        if not r["cid"] and r.get("visible", True)
        and len(r["text"]) >= 4 and any(c.isalpha() for c in r["text"])
    ]
    rng.shuffle(candidates)
    picked = 0
    for run in candidates:
        if picked >= n:
            return
        words = [w for w in run["text"].split() if len(w) >= 4 and w.isalpha() and w.isascii()]
        if not words:
            continue
        rng.shuffle(words)
        find = next((w for w in words if engine_edits_this_run(runs, run, w)), None)
        if find is None:
            continue
        # same-length replacement: shift letters, preserve case
        repl = "".join(
            chr((ord(ch.lower()) - 97 + 7) % 26 + 97).upper() if ch.isupper()
            else chr((ord(ch) - 97 + 7) % 26 + 97)
            for ch in find
        )
        if repl != find:
            picked += 1
            yield run, find, repl


def engine_edits_this_run(runs, run, find):
    """True iff `run` is the FIRST run (in extract order) on its page whose
    text contains `find` — which is the run the engine's `replace` command
    will actually edit. The font-fidelity oracles anchor territory and
    target font on the picked run's pre-edit bbox, so judging any other
    occurrence would anchor them on a segment the engine never touched.
    """
    first = next(
        (r for r in runs if r["page"] == run["page"] and find in r["text"]), None)
    return first is run


def pick_model_edits(engine_model_json, rng, n=3):
    """Variable-length candidates from the text model: (block, line, run,
    old_text, new_text). Always changes length — this probes line reflow."""
    blocks = json.loads(engine_model_json)
    cands = []
    for b, blk in enumerate(blocks):
        for l, ln in enumerate(blk["lines"]):
            for r, run in enumerate(ln["runs"]):
                t = run["text"]
                if len(t) >= 4 and t.isascii() and any(c.isalpha() for c in t) \
                        and not run["cid"] and not run["type3"]:
                    cands.append((b, l, r, t))
    rng.shuffle(cands)
    out = []
    for b, l, r, t in cands[:n]:
        repl = "".join(
            chr((ord(ch.lower()) - 97 + 7) % 26 + 97).upper() if ch.isupper()
            else (chr((ord(ch) - 97 + 7) % 26 + 97) if ch.isalpha() else ch)
            for ch in t
        )
        repl = repl[:-1] if rng.random() < 0.5 and len(repl) > 4 else repl + "x"
        if repl != t:
            out.append((b, l, r, t, repl))
    return out


def bbox_to_pixels(bbox, page_box, pad=BBOX_PAD):
    """PDF user space (y up) -> pixel rect (y down), padded.

    page_box is the rendered region's (left, bottom, right, top): cropped
    pages don't start at the user-space origin.
    """
    import math
    left, _, _, top = page_box
    x0, y0, x1, y1 = bbox
    px0 = math.floor((x0 - left) * SCALE) - pad
    px1 = math.ceil((x1 - left) * SCALE) + pad
    py0 = math.floor((top - y1) * SCALE) - pad
    py1 = math.ceil((top - y0) * SCALE) + pad
    return px0, py0, px1, py1


def strip_subset_prefix(name):
    """'ABCDEF+Arial-Bold' -> 'Arial-Bold' (ISO 32000 9.6.4 subset tag).

    Left untouched: mutool's 'Type3 (34 0 R)' placeholder for fonts with no
    /BaseFont (exactly what the engine's synthetic fallback font produces),
    the engine's 'gs:'-prefixed synthetic keys, anything else. None of those
    collide with a real subset-prefixed PostScript name, which is precisely
    what makes a font swap detectable below.
    """
    return SUBSET_PREFIX_RE.sub("", name or "")


def mutool_char_spans(pdf_path, page_no, page_box, timeout=20):
    """Char-level (char, font_name, bbox) spans for one page via mutool's
    structured-text extractor (`mutool draw -F stext`), converted into PDF
    user-space (y-up) coordinates using page_box = (left, bottom, right,
    top) from render_page, so they're directly comparable to an engine edit
    report's bbox.

    Clean-room black-box oracle: only mutool's own stdout is parsed here,
    never MuPDF source or behavior beyond what's observed in this output.
    Any failure to run, parse, or make sense of the output returns None so
    callers degrade to skip_font_oracle instead of asserting anything.
    """
    try:
        r = subprocess.run(
            [MUTOOL, "draw", "-F", "stext", "-o", "-", str(pdf_path), str(page_no)],
            capture_output=True, text=True, timeout=timeout, errors="replace",
        )
    except (subprocess.TimeoutExpired, OSError):
        return None
    if r.returncode != 0 or not r.stdout.strip():
        return None
    try:
        root = ET.fromstring(r.stdout)
    except ET.ParseError:
        return None
    page_el = root.find(".//page")
    if page_el is None:
        return None

    left, _bottom, _right, top = page_box
    spans = []
    try:
        for font_el in page_el.iter("font"):
            fname = font_el.get("name") or ""
            for char_el in font_el.findall("char"):
                c = char_el.get("c")
                quad = char_el.get("quad")
                if not c or not quad:
                    continue
                nums = [float(v) for v in quad.split()]
                if len(nums) != 8:
                    continue
                xs, ys = nums[0::2], nums[1::2]
                # mutool's stext quad is page-local and top-down (y grows
                # downward from the page's top edge, confirmed empirically
                # against pdfium's page_box); flip into the same PDF
                # user-space (y-up) frame render_page's page_box uses.
                cx0, cx1 = left + min(xs), left + max(xs)
                cy0, cy1 = top - max(ys), top - min(ys)
                spans.append((c, fname, (cx0, cy0, cx1, cy1)))
    except (ValueError, TypeError):
        return None
    return spans


def mutool_probe(case, pdf_path, out_pdf, run, work):
    """Shared I/O for checks 2 and 3 (font substitution / glyph tofu):
    before AND after page renders (the before bitmap feeds the
    unchanged-char ink-test exemption in detect_glyph_tofu) plus two
    mutool stext calls, so the two checks don't each re-invoke mutool.

    Returns (page_box, before_spans, after_spans, before_img, after_img),
    or None on any failure (render crash, size mismatch, mutool
    crash/timeout, unparseable XML) — callers treat None as
    skip_font_oracle.
    """
    page_no = run["page"]
    try:
        before_img, page_box_b, _ = render_page(pdf_path, page_no - 1, work / f"{case}_fontprobe_before.png")
        after_img, page_box, _ = render_page(out_pdf, page_no - 1, work / f"{case}_fontprobe_after.png")
    except RenderCrash:
        return None
    if after_img is None or page_box is None or before_img is None:
        return None
    if before_img.size != after_img.size:
        return None  # quad regions wouldn't be comparable pixel-for-pixel
    before_spans = mutool_char_spans(pdf_path, page_no, page_box)
    after_spans = mutool_char_spans(out_pdf, page_no, page_box)
    if before_spans is None or after_spans is None:
        return None
    return page_box, before_spans, after_spans, before_img, after_img


BAND_PAD = 2.0  # pt of slack for the territory line band


def build_territory(before_spans, after_spans, pre_edit_bbox, new_text):
    """Territory-presumption product identification, shared by checks 2/3.

    All anchoring information is the judge's own (the run bbox recorded
    when the judge picked the edit word from engine extract, cross-checked
    against mutool's before-page text by the caller) — nothing here trusts
    the engine's post-edit report coordinates.

    - BEFORE CORE: before spans whose center lies inside pre_edit_bbox.
      This is the confirmed edit target; its majority font is the target
      font (no padded-neighbor ballot problem: grazed/adjacent text has
      its center outside the pre-edit bbox).
    - PRODUCT TERRITORY: the target line band (pre_edit y-range +-2pt)
      intersected with x >= pre_edit x0 - 2pt, taken as the x-ordered
      span sequence starting at the span closest to the target's left
      edge. Its LENGTH is bounded by difflib-aligning the band's text
      against new_text (the rewritten segment's text): spans past the
      last aligned position are unrelated same-line text to the right and
      are excluded. Everything inside the bounded sequence is presumed an
      edit product; everything outside is unrelated text that neither
      gets ink-tested nor may account for repl characters.
    - OVERLAPPING TEXT: a non-target-font span inside the product
      sequence that ALREADY EXISTED before the edit — same char + base
      font at essentially the SAME position (0.5pt tolerance) on the
      before-page band — is pre-existing foreign text overprinted/
      interleaved into the target territory. No clean judgement is
      possible there: callers abstain (skip_font_oracle,
      "overlapping_text" in the detail) rather than risk either verdict.
      The tolerance is deliberately TIGHT, not the 2pt neighbor-matching
      kind: untouched text re-renders at identical coordinates (same PDF
      operations), while a borrowed-font PRODUCT near a same-font twin
      sits at a genuinely different position — a loose tolerance would
      misclassify it as pre-existing and abstain away a valid edit. A
      non-target-font span without such a same-position twin is a
      legitimate borrowed-font product (engine used another document
      font for a glyph the target font lacks) and stays in the sequence.

    Returns (orig_font_raw, before_core, product_seq, overlapping,
    anchor_failed, ambiguous_reason) where orig_font_raw is None when
    the before core is empty, product_seq is the bounded x-ordered
    after-span list, overlapping is a list of {char, font} dicts
    (non-empty => abstain), anchor_failed reports that aligned spans
    exist in the band but none at the target's left edge (product
    identity not establishable), and ambiguous_reason
    ('gap_ambiguous' | 'crosses_preexisting' | None) reports that the
    territory geometry is undecidable and the caller must abstain.
    """
    x0, y0, x1, y1 = pre_edit_bbox
    before_core = [s for s in before_spans if _center_in(s[2], pre_edit_bbox)]
    orig_font_raw = _majority_font(before_core) if before_core else None
    orig_base = strip_subset_prefix(orig_font_raw) if orig_font_raw else None

    def in_band(box):
        cx, cy = (box[0] + box[2]) / 2, (box[1] + box[3]) / 2
        return (y0 - BAND_PAD) <= cy <= (y1 + BAND_PAD) and cx >= x0 - BAND_PAD

    after_band = sorted(
        (s for s in after_spans if in_band(s[2])),
        key=lambda s: (s[2][0] + s[2][2]) / 2)
    before_band = [s for s in before_spans if in_band(s[2])]

    before_band_sorted = sorted(before_band, key=lambda s: (s[2][0] + s[2][2]) / 2)

    def truncate_continuous(seq):
        """GEOMETRIC CONTINUITY with pre-edit gap comparison: the accepted
        product sequence must be spatially contiguous. A left anchor alone
        is not enough — if the first character survives at x0 while the
        rest of the segment vanished, alignment happily splices an
        unrelated run further right into the string, ignoring the spatial
        hole. A mid-sequence x gap exceeding max(2pt, 1.5 x the band's
        median char width) (doubled next to a SPACE char — word gaps are
        legitimately wider) is however NOT automatically a hole: wide
        tracking (Tc / TJ adjustments) legally produces large inter-glyph
        gaps. The gap is compared against the BEFORE band's gap at the
        same ordinal position (tolerance max(30%, 1pt)): consistent ->
        preserved legitimate spacing, continue; inconsistent -> a NEW hole
        the pre-edit layout did not have, and whether glyphs went missing
        or the alignment is about to splice unrelated text is undecidable
        from geometry — ABSTAIN (gap_ambiguous), never assert missing
        from a gap. (Missing stays reserved for the no-anchor-at-x0
        case.) Returns (sequence, gap_ambiguous)."""
        if len(seq) <= 1:
            return seq, False
        widths = [s[2][2] - s[2][0] for s in after_band
                  if not s[0].isspace() and s[2][2] > s[2][0]]
        med = sorted(widths)[len(widths) // 2] if widths else 0.0
        base_thr = max(2.0, 1.5 * med)
        out = [seq[0]]
        for idx, (prev, cur) in enumerate(zip(seq, seq[1:])):
            gap = cur[2][0] - prev[2][2]
            thr = base_thr * 2 if (prev[0].isspace() or cur[0].isspace()) else base_thr
            if gap > thr:
                bgap = None
                if idx + 1 < len(before_band_sorted):
                    b_prev, b_cur = before_band_sorted[idx], before_band_sorted[idx + 1]
                    bgap = b_cur[2][0] - b_prev[2][2]
                if bgap is not None and abs(gap - bgap) <= max(0.3 * abs(bgap), 1.0):
                    out.append(cur)
                    continue  # matches the pre-edit layout: legal wide tracking
                return out, True  # new hole: geometry is ambiguous
            out.append(cur)
        return out, False

    # RIGHT BOUNDARY — FINAL, deliberately conservative form (round 17).
    # This corner no longer accepts heuristic enhancement: after physical
    # shift bounds (round 16) were disproven — median-char-width estimates
    # are unreliable in proportional fonts — the rule was downgraded to a
    # pure uniqueness test. The boundary is used ONLY when it is
    # unambiguous; every ambiguity is a visible abstention:
    #
    # 0. PERFORMANCE CAPS: a band over 512 spans abstains outright
    #    (band_too_large); at most K=8 candidates are evaluated
    #    (rightmost-first), more without a decision -> abstain. Total
    #    work is O(K x 512 x len(new_text)).
    # 1. EMPTY/UNANCHORED PRODUCTS FIRST: the global alignment finding
    #    nothing, or nothing anchored at the pre-edit x0, goes straight
    #    to the missing path before any boundary reasoning.
    # 2. Rigid-shift candidates are collected with NO filtering, scoring
    #    or physical bounds. A candidate is ACCEPTABLE only with an
    #    anchored, non-empty prefix alignment (round 18): a candidate
    #    overlapping the anchor position itself is a geometric ambiguity
    #    (replacement left at x0 vs. suffix shifted onto x0) and is
    #    invalid. Exactly one acceptable hypothesis -> it is the boundary
    #    and its prefix alignment is the product sequence. Two or more
    #    DIFFERENT acceptable product hypotheses ->
    #    right_boundary_ambiguous, unconditionally (repeated text makes
    #    the product/suffix split inherently undecidable — the honest
    #    outcome is an abstention, never a truncated-products missing
    #    verdict). No acceptable candidate while before right neighbors
    #    exist -> right_boundary_lost.
    #
    # DELETION EDITS (round 17): new_text == "" declares the product
    # sequence empty — nothing to ink-test, nothing for the font check,
    # and the boundary logic must NOT run (the suffix legitimately
    # shifts left into the vacated space; treating it as products would
    # false-flag substitution). new_text is None keeps the legacy
    # unbounded-band fallback for span-only fixtures.
    if new_text == "":
        return orig_font_raw, before_core, [], [], False, None

    before_rn_seq = sorted(
        (s for s in before_band if s[2][0] >= x1 - 0.5),
        key=lambda s: (s[2][0] + s[2][2]) / 2)

    if len(after_band) > 512:
        return (orig_font_raw, before_core, [], [], False, "band_too_large")

    band_str = "".join(c for c, _, _ in after_band)

    def _align(text, upto=None):
        """difflib-align `text` against the band prefix of length `upto`
        (whole band when None). Returns (first, last, covered): first/last
        aligned band indices (None/-1 when nothing aligns) and the count
        of NON-WHITESPACE chars of `text` covered by equal blocks
        (whitespace-insensitive, consistent with judge's conventions)."""
        seg = band_str if upto is None else band_str[:upto]
        smx = difflib.SequenceMatcher(a=text, b=seg, autojunk=False)
        f, l, cov = None, -1, 0
        for tag, a0, a1, b0, b1 in smx.get_opcodes():
            if tag == "equal":
                if f is None:
                    f = b0
                l = max(l, b1 - 1)
                cov += sum(1 for ch in text[a0:a1] if not ch.isspace())
        return f, l, cov

    anchor_failed = False
    ambiguous_reason = None
    product_seq = []
    chosen = None  # (first, last) of the boundary's prefix alignment

    if new_text is not None:
        g_first, g_last, _g_cov = _align(new_text)
        if g_first is None:
            product_seq = []  # nothing of the edit output found: missing path
        elif abs(after_band[g_first][2][0] - x0) > BAND_PAD:
            product_seq = []
            anchor_failed = True  # nothing anchored at x0: missing path
        elif before_rn_seq:
            n = len(before_rn_seq)
            cands = []
            for i in range(len(after_band) - n + 1):
                deltas = []
                ok = True
                for j, (bc, bf, bbox_) in enumerate(before_rn_seq):
                    ac, af, abox = after_band[i + j]
                    if ac != bc or af != bf:
                        ok = False
                        break
                    deltas.append(abox[0] - bbox_[0])
                if ok and max(deltas) - min(deltas) <= 0.5:
                    cands.append(i)
            # ACCEPTANCE (rounds 18+19, completing the uniqueness rule):
            # a candidate — including a unique one — is acceptable only
            # when its prefix alignment is
            #   1. ANCHORED at the pre-edit x0,
            #   2. NON-EMPTY (a candidate whose prefix is empty coincides
            #      with the anchor itself: replacement-left-at-x0 vs.
            #      suffix-shifted-onto-x0 is indistinguishable), and
            #   3. COMPLETE — its equal blocks cover ALL of new_text
            #      (whitespace-insensitive). An incomplete prefix means
            #      either products lost characters or the boundary is
            #      misidentified, and those two hypotheses cannot be told
            #      apart in this geometry.
            # The boundary mechanism therefore serves ONLY to scope the
            # ink verification of COMPLETE product sequences; detecting
            # missing characters when right neighbors exist is explicitly
            # ceded to a visible abstention. (Edits without right
            # neighbors — the majority, in synthetic and typical
            # documents — keep full missing detection via the global
            # path.) Rigid candidates present but none acceptable ->
            # right_boundary_ambiguous.
            K = 8
            evaluated = sorted(cands, reverse=True)[:K]
            total_ns = sum(1 for ch in new_text if not ch.isspace())
            if not cands:
                # No rigid match at all: the right-neighbor topology
                # cannot be re-established -> lost (distinct from
                # candidates existing but none acceptable -> ambiguous).
                ambiguous_reason = "right_boundary_lost"
            valid = []
            for i in evaluated:
                pf, pl, cov = _align(new_text, upto=i)
                if pf is None:
                    continue  # empty prefix: candidate sits on the anchor
                if abs(after_band[pf][2][0] - x0) > BAND_PAD:
                    continue  # prefix not anchored at the target
                if cov < total_ns:
                    continue  # incomplete products: undecidable geometry
                valid.append((pf, pl))
            hyps = set(valid)
            if ambiguous_reason is not None:
                pass  # lost already decided above
            elif not valid:
                ambiguous_reason = "right_boundary_ambiguous"
            elif len(cands) > K or len(hyps) >= 2:
                ambiguous_reason = "right_boundary_ambiguous"
            else:
                chosen = next(iter(hyps))
        # else: anchored, no right neighbors -> plain global products below

        if ambiguous_reason:
            product_seq = []
        elif chosen is not None:
            _pf, pl = chosen
            product_seq, gap_amb = truncate_continuous(after_band[: pl + 1])
            if gap_amb:
                product_seq = []
                ambiguous_reason = "gap_ambiguous"
        elif g_first is not None and not anchor_failed:
            product_seq, gap_amb = truncate_continuous(after_band[: g_last + 1])
            if gap_amb:
                product_seq = []
                ambiguous_reason = "gap_ambiguous"
    else:
        product_seq = after_band
        if product_seq and abs(product_seq[0][2][0] - x0) > BAND_PAD:
            product_seq = []
            anchor_failed = True
        else:
            product_seq, gap_amb = truncate_continuous(product_seq)
            if gap_amb:
                product_seq = []
                ambiguous_reason = "gap_ambiguous"

    overlapping = []
    for c, f, box in product_seq:
        base = strip_subset_prefix(f)
        if orig_base is None or base == orig_base:
            continue
        preexisting = any(
            c == bc and base == strip_subset_prefix(bf)
            and max(abs(p - q) for p, q in zip(box, bbox_)) <= 0.5
            for bc, bf, bbox_ in before_band)
        if preexisting:
            overlapping.append({"char": c, "font": f})
    return orig_font_raw, before_core, product_seq, overlapping, anchor_failed, ambiguous_reason


def detect_font_substitution(before_spans, after_spans, edit_bbox,
                             pre_edit_bbox=None, new_text=None):
    """Check 2: characters the ORIGINAL font already covered (drawn
    somewhere on the before-page in that font — a conservative black-box
    proof the font has the glyph) must keep the same base font after the
    edit. A character the original font never drew is exempt: falling back
    (or borrowing another document font) for a genuinely unsupported glyph
    is legitimate engine behavior.

    The checked set is the territory product sequence from
    build_territory(): adjacent text left of the target or on other lines
    never enters it, and same-line text right of the rewritten segment is
    cut off by the new_text length bounding — so grazed neighbors in other
    fonts can no longer be mistaken for substituted output. Pre-existing
    foreign text INSIDE the territory (overlapping/overprint) makes the
    check abstain instead of guessing.

    Pure function over already-fetched mutool spans; returns
    ("fail_font_substituted", detail), ("skip_font_oracle", detail), or
    (None, None).
    """
    anchor = pre_edit_bbox if pre_edit_bbox is not None else edit_bbox
    orig_font_raw, _before_core, product_seq, overlapping, _anchor_failed, ambiguous_reason = \
        build_territory(before_spans, after_spans, anchor, new_text)
    if orig_font_raw is None:
        return None, None
    if ambiguous_reason:
        return "skip_font_oracle", {"territory_ambiguous": ambiguous_reason}
    if overlapping:
        return "skip_font_oracle", {"overlapping_text": overlapping}
    orig_font = strip_subset_prefix(orig_font_raw)
    covered_chars = {c for c, f, _ in before_spans if strip_subset_prefix(f) == orig_font}

    for c, f, _box in product_seq:
        new_font = strip_subset_prefix(f)
        if new_font != orig_font and c in covered_chars:
            return "fail_font_substituted", {
                "char": c, "orig_font": orig_font, "new_font": new_font,
                "orig_font_raw": orig_font_raw, "new_font_raw": f,
            }
    return None, None


def _majority_font(spans):
    """Most frequent raw font name in a span list (spans must be non-empty)."""
    counts = {}
    for _, f, _ in spans:
        counts[f] = counts.get(f, 0) + 1
    return max(counts, key=counts.get)


def _center_in(box, bbox):
    """Whether a span box's center point lies inside bbox (both PDF user
    space)."""
    bx0, by0, bx1, by1 = bbox
    cx, cy = (box[0] + box[2]) / 2, (box[1] + box[3]) / 2
    return bx0 <= cx <= bx1 and by0 <= cy <= by1


def _masked_span(img, rect, punch_rects, glyph_pad=2, also_punch=None, protect_rect=None):
    """Luminance extrema span of img inside `rect` with every rect in
    punch_rects (padded glyph_pad px for antialias bleed) masked out;
    also_punch (unpadded) is removed too when given.

    protect_rect (unpadded) is the glyph-under-test's own core territory,
    re-included AFTER the padded punching — adjacent glyphs' pads must not
    swallow it wholesale (a narrow glyph a few px wide would otherwise
    lose its entire quad to its neighbors' pads and read as blank). But
    the protection is NOT a blanket re-light: pixels covered by another
    glyph's UNPADDED quad belong to that glyph, not this one, and are
    subtracted again — otherwise a foreign glyph overlapping the core
    would have its ink resurrected and vouch for a blank glyph under it.

    Returns (span, core_live, core_area): span is hi-lo of the surviving
    pixels (None when nothing survives), core_live/core_area are the
    surviving vs total pixel counts of protect_rect (both 0 when no
    protect_rect) so callers can refuse to judge on a sliver of
    attributable pixels."""
    w, h = img.size
    rx0, ry0, rx1, ry1 = rect
    rx0, ry0 = max(rx0, 0), max(ry0, 0)
    rx1, ry1 = min(rx1, w), min(ry1, h)
    if rx1 <= rx0 or ry1 <= ry0:
        return None, 0, 0
    crop = img.crop((rx0, ry0, rx1, ry1))
    mask = Image.new("L", crop.size, 255)
    draw = ImageDraw.Draw(mask)

    def paint(x0, y0, x1, y1, fill):
        # Clamp to the crop and skip empty/degenerate rects (zero-height
        # whitespace quads produce them) — ImageDraw raises otherwise.
        lx0, ly0 = max(x0 - rx0, 0), max(y0 - ry0, 0)
        lx1, ly1 = min(x1 - rx0, rx1 - rx0) - 1, min(y1 - ry0, ry1 - ry0) - 1
        if lx1 >= lx0 and ly1 >= ly0:
            draw.rectangle((lx0, ly0, lx1, ly1), fill=fill)

    if also_punch is not None:
        ax0, ay0, ax1, ay1 = also_punch
        paint(ax0, ay0, ax1, ay1, 0)
    for gx0, gy0, gx1, gy1 in punch_rects:
        paint(gx0 - glyph_pad, gy0 - glyph_pad, gx1 + glyph_pad, gy1 + glyph_pad, 0)

    core_live = core_area = 0
    if protect_rect is not None:
        px0, py0, px1, py1 = protect_rect
        paint(px0, py0, px1, py1, 255)
        # Foreign glyphs' own territory is not attributable to this glyph
        # — subtract it back out of the protected core with the SAME
        # glyph_pad as the primary masking (per review: soundness first —
        # an asymmetric smaller pad let the second bleed pixel of a
        # neighbor count as attributable ink). Narrow glyphs whose core
        # is consumed by symmetric pads fall to the sufficiency floor
        # below and abstain rather than get judged on neighbor bleed.
        for gx0, gy0, gx1, gy1 in punch_rects:
            paint(gx0 - glyph_pad, gy0 - glyph_pad, gx1 + glyph_pad, gy1 + glyph_pad, 0)
        cx0, cy0 = max(px0, rx0), max(py0, ry0)
        cx1, cy1 = min(px1, rx1), min(py1, ry1)
        if cx1 > cx0 and cy1 > cy0:
            core_area = (cx1 - cx0) * (cy1 - cy0)
            core_mask = mask.crop((cx0 - rx0, cy0 - ry0, cx1 - rx0, cy1 - ry0))
            core_live = core_mask.histogram()[255]

    hist = crop.histogram(mask)
    live = [v for v, n in enumerate(hist[:256]) if n]
    if not live:
        return None, core_live, core_area
    return live[-1] - live[0], core_live, core_area


def _ring_contrast(img, quad_rect, glyph_rects, expand=3):
    """Luminance extrema span of the BACKGROUND RING around a glyph quad:
    expand quad_rect by `expand` px, subtract the quad itself, and mask out
    every OTHER char quad on the page (padded for antialias bleed) —
    adjacent glyphs' own ink is text, not background, and must not make a
    plain page look "busy". Returns the ring's hi-lo span, or None when no
    background pixels survive the masking (dense text or a quad flush
    against the image edge) — with no observable background there is no
    basis for trusting a quad-contrast judgement, so callers record the
    char as "unknown" (abstention), same as a busy ring.
    """
    qx0, qy0, qx1, qy1 = quad_rect
    ring_rect = (qx0 - expand, qy0 - expand, qx1 + expand, qy1 + expand)
    span, _live, _area = _masked_span(img, ring_rect, glyph_rects, also_punch=quad_rect)
    return span


def detect_glyph_tofu(after_img, page_box, before_spans, after_spans, edit_bbox, repl,
                      before_img=None, pre_edit_bbox=None, new_text=None):
    """Check 3: a replacement character can be encoded into a subset font
    that keeps the SAME font name (so check 2 sees nothing wrong) but lacks
    that glyph's outline — the text layer says the character is there
    (ToUnicode round-trips) yet nothing gets painted ("tofu"). Also covers
    the renderer dropping characters from stext entirely — up to and
    including the WHOLE edited segment vanishing: if the territory product
    sequence is empty while the before core had text and repl has visible
    characters, every such character is reported missing_from_stext.

    PRODUCT IDENTIFICATION is territory presumption (build_territory):
    everything in the bounded target-line sequence is an edit product and
    is aligned + ink-tested; everything outside is unrelated text that
    neither gets tested nor may account for repl characters; pre-existing
    foreign text INSIDE the territory makes the check abstain
    (overlapping_text). No pixel veto or neighbor matching is involved in
    identification anymore.

    UNCHANGED-CHAR EXEMPTION (uses before_img when supplied): a product
    span whose quad renders RAW-identical across the before/after pages
    AND visibly contains ink is an unchanged in-place character (e.g. the
    untouched prefix of a same-position rewrite) — accepted as present
    without the ring/ink test, which spares it from busy-background
    abstention. Identity alone is NOT enough: a blank quad can be blank
    in both renders (a shifted narrow glyph leaves its old ink outside
    the new quad), so a blank-but-identical quad still faces the ink
    test and fails there.

    Ink test per aligned product character: the quad's luminance extrema
    must span >= 96 (same confidence threshold as judge). A painted glyph
    always produces foreground/background contrast plus antialiasing
    transitions; a blank quad on a uniform background is near-flat.
    Background-validity heuristic: the quad's surrounding ring (3px frame
    outside the quad, with every OTHER glyph quad masked out — neighboring
    text ink is not "background") is checked first. If the RING's extrema
    span >= 96 the local background is itself busy (photo, gradient,
    dense linework), and if NO ring pixels survive the masking (dense
    text, image edge) there is no observable background at all: in either
    case the quad judgement can't be trusted and the char is recorded as
    "unknown" rather than passed or failed. Any unknown char without a
    definite tofu failure makes the whole check abstain
    ("skip_font_oracle" — counted, not failed): success is only reported
    when every ink-tested character was actually judged. Heuristic
    boundary, accepted: a background busy at exactly the glyph's scale
    but flat in the 3px ring can still fool the quad test, and unknown
    chars are simply not judged — tofu there goes undetected by THIS
    check (judge's pixel-diff checks still apply).

    Pure function except for PIL crops on the caller-supplied images;
    returns ("fail_glyph_tofu", detail), ("skip_font_oracle", detail), or
    (None, None).
    """
    anchor = pre_edit_bbox if pre_edit_bbox is not None else edit_bbox
    if new_text is None:
        new_text = repl
    orig_font_raw, before_core, product_seq, overlapping, anchor_failed, ambiguous_reason = \
        build_territory(before_spans, after_spans, anchor, new_text)
    visible_repl = [ch for ch in repl if not ch.isspace()]

    if ambiguous_reason:
        # Territory geometry undecidable (new spatial hole, or the
        # alignment would cross an untouched right neighbor): abstain.
        return "skip_font_oracle", {"territory_ambiguous": ambiguous_reason}
    if overlapping:
        return "skip_font_oracle", {"overlapping_text": overlapping}
    if not product_seq:
        # Nothing left in the target territory. If the target existed and
        # repl needs visible glyphs, the edit output vanished wholesale.
        if before_core and visible_repl:
            return "fail_glyph_tofu", {
                "missing": [{"char": ch, "reason": "missing_from_stext"} for ch in visible_repl],
            }
        return None, None

    after_str = "".join(c for c, _, _ in product_seq)
    sm = difflib.SequenceMatcher(a=repl, b=after_str, autojunk=False)
    pairing = [None] * len(repl)
    for tag, a0, a1, b0, b1 in sm.get_opcodes():
        if tag == "equal":
            for i in range(a1 - a0):
                pairing[a0 + i] = product_seq[b0 + i]

    # Every char quad on the page in pixel space, for ring masking; the
    # font travels along for the risk-targeted inner margins below.
    glyph_info = [(bbox_to_pixels(box, page_box, pad=0), f) for _, f, box in after_spans]
    glyph_rects = [g for g, _f in glyph_info]

    def _overhang_risk(neighbor_font, own_font):
        """Whether a neighbor's ink can plausibly bleed further than the
        standard 2px antialias pad (reviewed ruling, round 12 follow-up):
        upright glyphs paint outside their own quad only by ~1px of
        antialiasing, which the 2px pad covers; >2px overhang comes from
        italic/oblique shear or from cross-font metric differences. The
        stricter 4px inner margin is therefore applied only to neighbors
        whose font name says Italic/Oblique (case-insensitive, covering
        faux-italic naming variants) or whose font identity differs from
        the tested glyph's — compared by the RAW mutool-reported name,
        NOT the subset-stripped base (round 13): ABCDEF+Foo and
        GHIJKL+Foo are different subsets whose metrics/overhang can
        differ, so they must not share the trusted 2px treatment.
        Base-name comparison stays reserved for the font-substitution
        check, whose question ("did the font family change?") is
        semantic, not geometric. Known residuals, accepted: rare upright
        long-swash glyphs (long-f, script faces), and italic overhang
        beyond 4px."""
        nf = neighbor_font.lower()
        if "italic" in nf or "oblique" in nf:
            return True
        return neighbor_font != own_font

    w, h = after_img.size
    tofu = []
    unknown = []
    unattributable = []
    judged = 0
    for ch, span in zip(repl, pairing):
        if ch.isspace():
            continue
        if span is None:
            tofu.append({"char": ch, "reason": "missing_from_stext"})
            continue
        _, _, cbox = span
        px0, py0, px1, py1 = bbox_to_pixels(cbox, page_box, pad=1)
        px0, py0 = max(px0, 0), max(py0, 0)
        px1, py1 = min(px1, w), min(py1, h)
        if px1 <= px0 or py1 <= py0:
            tofu.append({"char": ch, "reason": "offpage_quad"})
            continue
        rect = (px0, py0, px1, py1)
        # Ink attribution mask: every OTHER glyph's quad is punched out of
        # any contrast measured inside this quad, or an adjacent
        # character's ink at the quad edge (tight kerning) would vouch
        # for a blank glyph — in the exemption below as well as in the
        # ink test. Punch pads are RISK-TARGETED (see _overhang_risk):
        # upright same-font neighbors get the standard 2px in both masks;
        # italic/oblique or cross-font neighbors get 4px in the inner
        # mask, demanding a 2px margin from their punch boundary.
        own_rect = bbox_to_pixels(cbox, page_box, pad=0)
        own_font = span[1]  # RAW name: subset identity matters for risk

        def _padded(r, p):
            return (r[0] - p, r[1] - p, r[2] + p, r[3] + p)

        others_meta = [(g, f) for g, f in glyph_info if g != own_rect]
        outer_rects = [_padded(g, 2) for g, _f in others_meta]
        inner_rects = [_padded(g, 4 if _overhang_risk(f, own_font) else 2)
                       for g, f in others_meta]
        if before_img is not None:
            if ImageChops.difference(
                    before_img.crop(rect), after_img.crop(rect)).getbbox() is None:
                mc, _core_live, _core_area = _masked_span(
                    after_img, rect, inner_rects, glyph_pad=0, protect_rect=own_rect)
                if mc is not None and mc >= 96:
                    # Unchanged in-place char with INNER-margin
                    # attributable ink; same provenance + inner-margin
                    # argument as the ink test below — the area floor
                    # does not gate the inked direction.
                    continue
        ring = _ring_contrast(after_img, rect, glyph_rects)
        if ring is None or ring >= 96:
            # Busy background, or no background pixels survived masking
            # (dense text / image edge): quad contrast unreliable either
            # way — abstain on this char rather than guess.
            unknown.append(ch)
            continue
        # INKED requires INNER-margin ink from RISKY neighbors (round 12,
        # targeted per ruling): italic/oblique shear and cross-font
        # metric differences can bleed past the 2px punch pad, so ink
        # surviving only in the 2px band at such a neighbor's punch
        # boundary may still be that neighbor's — the inner mask expands
        # those punches to 4px. Upright same-font neighbors keep the
        # plain 2px pad (their bleed is ~1px antialiasing), so normal
        # tight body text stays judgeable. Known residuals: upright
        # long-swash glyphs, and overhang beyond 4px. The sufficiency floor
        # deliberately does NOT gate the inked branch (reviewed ruling,
        # round 11 follow-up): inner-surviving ink is attributable by
        # provenance regardless of how few pixels survive; the fringe
        # outside the glyph pads is background the ring check already
        # validated. The floor only guards the blank/unknown direction
        # (and blank has the raw<96 requirement as its own second gate).
        span_inner, _il, _ia = _masked_span(
            after_img, rect, inner_rects, glyph_pad=0, protect_rect=own_rect)
        if span_inner is not None and span_inner >= 96:
            judged += 1  # inner-margin ink: attributable with slack
            continue
        span_contrast, core_live, core_area = _masked_span(
            after_img, rect, outer_rects, glyph_pad=0, protect_rect=own_rect)
        if span_contrast is not None and span_contrast >= 96:
            # Ink exists only within the 2px boundary band next to a
            # punched foreign region: could be this glyph's, could be
            # the neighbor's overhang — unattributable, abstain.
            unattributable.append(ch)
            continue
        if core_area > 0 and core_live < max(4, 0.3 * core_area):
            # Fast abstention: the glyph's own core exists but almost
            # none of it remains attributable to THIS glyph (a foreign
            # quad covers it) and what does remain shows no ink —
            # nothing meaningful to judge. (core_area == 0 is different:
            # a BLANK glyph gets a degenerate zero-height quad from
            # mutool, so the padded rect judged below is exactly what
            # catches it.)
            unknown.append(ch)
            continue
        if span_contrast is None:
            # Fully masked by neighbors: nothing attributable — abstain.
            unknown.append(ch)
            continue
        # Attributable region is blank. A BLANK verdict additionally
        # requires the RAW quad to be blank too: raw ink with no
        # attributable survivor means the glyph's ink was swallowed by
        # the symmetric neighbor masking (narrow glyphs) or belongs to a
        # foreign glyph — either way it is UNATTRIBUTABLE, and
        # unattributable is not blank: abstain instead of guessing
        # (reviewed ruling, round 11). Soundness holds in both
        # directions: "inked" still only ever comes from attributable
        # ink above, and a genuinely blank glyph on any solid background
        # has a uniform raw quad (the ring check already validated the
        # background) and still fails here.
        raw_lo, raw_hi = after_img.crop(rect).getextrema()
        if raw_hi - raw_lo >= 96:
            unattributable.append(ch)
            continue
        judged += 1
        tofu.append({"char": ch, "reason": "blank_glyph"})

    if tofu:
        detail = {"missing": tofu}
        if unknown:
            detail["unknown_background"] = unknown
        if unattributable:
            detail["unattributable"] = unattributable
        return "fail_glyph_tofu", detail
    if unknown or unattributable:
        # ANY unjudgeable char without a definite failure means this edit
        # was not fully verified: abstain (counted as skip_font_oracle),
        # never report success on partial coverage.
        detail = {"judged_ok": judged}
        if unknown:
            detail["unknown_background"] = unknown
        if unattributable:
            detail["unattributable"] = unattributable
        return "skip_font_oracle", detail
    return None, None


# Primary judge() verdicts for which before/after render succeeded, sizes
# matched, and the edit landed on-page — i.e. the base checks the task asks
# checks 2/3 to run "after" are satisfied, regardless of the final semantic
# outcome. Excludes skip_invisible_text/skip_occluded_text (nothing visible
# to compare) and the earlier structural/render/size/offpage exits (no
# valid page_box or bbox to work with yet).
FONT_CHECKABLE_VERDICTS = {
    "pass", "fail_no_visual_change", "fail_leak_outside_bbox", "fail_text_semantics",
}


def judge(case, in_pdf, out_pdf, page_no, report, find, repl, work):
    page_index = page_no - 1

    # 1. structure
    if qpdf_status(out_pdf) > max(qpdf_status(in_pdf), 0):
        return "fail_structure", None

    # 2/3. render diff
    try:
        before, size_b, text_before = render_page(in_pdf, page_index, work / f"{case}_before.png")
    except RenderCrash:
        return "skip_invalid_input", None
    try:
        after, _, text = render_page(out_pdf, page_index, work / f"{case}_after.png")
    except RenderCrash:
        return "fail_render_crash", None
    if before is None or after is None:
        return "skip_rotated", None
    if before.size != after.size:
        return "fail_page_size_changed", None

    # Edits to text positioned outside the visible page can't be judged
    # visually (fuzzer files place text at negative coordinates).
    left, bottom, right, top = size_b
    x0, y0, x1, y1 = report["bbox"]
    if x1 < left or y1 < bottom or x0 > right or y0 > top:
        return "skip_offpage", None

    px0, py0, px1, py1 = bbox_to_pixels(report["bbox"], size_b)

    # If the edit region was blank in the ORIGINAL render (hidden OCG layer,
    # unrenderable font, occluded text), no visual judgement is possible.
    # Visibility floor 150 pairs with the diff threshold 96 below: any text
    # dark enough to count as visible (<=150 on white) produces a diff of at
    # least 255-150=105 > 96, so visible edits can never be masked as noise.
    w, h = before.size
    region = before.crop((max(px0, 0), max(py0, 0), min(px1, w), min(py1, h)))
    if region.point(lambda v: 255 if v < 150 else 0).getbbox() is None:
        return "skip_invisible_text", None

    # Threshold the diff: sub-hundredth-point compensation residue leaves
    # faint antialiasing noise (measured max ~80); real misplacements of
    # visible text differ by >=105. Only count confident pixels.
    diff = ImageChops.difference(before, after).point(lambda v: 255 if v >= 96 else 0)
    diff_bbox = diff.getbbox()  # None if identical
    if diff_bbox is None:
        # Edit landed in the text layer but is painted over (image drawn
        # after an OCR layer): correct edit, visually unjudgeable. Compare
        # occurrence COUNTS before/after — mere presence would false-pass
        # when the replacement string already existed elsewhere on the page.
        joined_before = "".join(text_before.split())
        joined_after = "".join(text.split())
        if joined_after.count(repl) > joined_before.count(repl):
            return "skip_occluded_text", None
        return "fail_no_visual_change", None
    dx0, dy0, dx1, dy1 = diff_bbox
    if dx0 < px0 or dy0 < py0 or dx1 > px1 + 1 or dy1 > py1 + 1:
        return "fail_leak_outside_bbox", diff

    # 4. semantics — whitespace-insensitive: pdfium reinserts line breaks
    # at visual wrap points, which can split the replacement word.
    if "".join(repl.split()) not in "".join(text.split()):
        return "fail_text_semantics", diff

    return "pass", None


def check_identity_drift(case, pdf_path, run, find, work):
    """Check 1: re-encode the SAME text (find == with) through the engine
    and require pixel-identical rendering across the WHOLE page. The text
    never changes, so any visible diff can only come from the engine's own
    re-encoding path (font substitution, glyph-width compensation rounding,
    etc), independent of whatever edit content judge() is separately
    grading. If the engine short-circuits find==repl into writing back the
    original bytes, this trivially passes — that's fine, not a bug.

    Reuses judge()'s skip taxonomy (offpage/invisible/rotated/size-changed)
    for cases that can't be visually judged, but with an "identity_" prefix
    on the verdict names so these never get folded into judge()'s own
    per-check counts. An engine refusal on the identity replace itself is
    also a skip (this check is about re-encoding fidelity, not about
    whether every input is editable).

    Returns (verdict, diff_image_or_None, out_pdf_or_None).
    """
    page_index = run["page"] - 1
    out_pdf = work / f"{case}_identity_out.pdf"
    r = sh([
        str(ENGINE), "replace", str(pdf_path), str(out_pdf),
        "--page", str(run["page"]), "--find", find, "--with", find,
        "--fallback-font", str(ROOT.parent / "assets" / "NotoSansSC.ttf"),
    ])
    if r.returncode != 0:
        return "skip_identity_engine_refuse", None, None
    report = json.loads(r.stdout)

    try:
        before, size_b, _ = render_page(pdf_path, page_index, work / f"{case}_identity_before.png")
    except RenderCrash:
        return "skip_identity_invalid_input", None, out_pdf
    try:
        after, _, _ = render_page(out_pdf, page_index, work / f"{case}_identity_after.png")
    except RenderCrash:
        return "fail_identity_render_crash", None, out_pdf
    if before is None or after is None:
        return "skip_identity_rotated", None, out_pdf
    if before.size != after.size:
        return "fail_identity_page_size_changed", None, out_pdf

    left, bottom, right, top = size_b
    x0, y0, x1, y1 = report["bbox"]
    if x1 < left or y1 < bottom or x0 > right or y0 > top:
        return "skip_identity_offpage", None, out_pdf

    px0, py0, px1, py1 = bbox_to_pixels(report["bbox"], size_b)
    w, h = before.size
    region = before.crop((max(px0, 0), max(py0, 0), min(px1, w), min(py1, h)))
    if region.point(lambda v: 255 if v < 150 else 0).getbbox() is None:
        return "skip_identity_invisible_text", None, out_pdf

    # STRICT pixel identity — no threshold, deliberately. Measured across
    # all 60 synthetic-corpus identity edits (2026-07-20): raw
    # unthresholded max pixel diff was 0 on every case; the identity
    # re-encode path renders pixel-identical today, with none of the
    # compensation residue that motivates judge's 96 (that residue comes
    # from actually CHANGING glyphs). If real-corpus files ever show
    # reproducible render noise here, a tolerance may be reintroduced ONLY
    # with fresh measured data (raw diff distribution across the corpus) —
    # never picked ad hoc.
    diff = ImageChops.difference(before, after)
    if diff.getbbox() is not None:
        return "fail_identity_drift", diff, out_pdf
    return "pass", None, out_pdf


def run_case(pdf_path: Path, work: Path):
    """Returns (primary_row, check_rows): primary_row is this file's ONE
    (file, verdict, detail) headline verdict (font-check failures override
    it, fail beats pass); check_rows are supplementary-oracle outcomes
    (identity drift, skip_font_oracle) that are counted and reported
    separately so the headline per-file totals stay exactly one row per
    corpus file.
    """
    case = hashlib.sha1(pdf_path.name.encode()).hexdigest()[:10]
    rng = random.Random(case)

    def input_renderable():
        try:
            img, _, _ = render_page(pdf_path, 0, work / f"{case}_probe.png")
            return img is not None
        except (RenderCrash, Exception):
            return False

    r = sh([str(ENGINE), "extract", str(pdf_path)])
    if r.returncode != 0:
        if not input_renderable():
            return (pdf_path.name, "skip_invalid_input", None), []
        return (pdf_path.name, "fail_engine_extract", r.stderr.strip()[:200]), []
    if VARLEN:
        return run_case_varlen(pdf_path, work, case, rng), []
    runs = json.loads(r.stdout)["runs"]
    out_pdf = work / f"{case}_out.pdf"
    run = find = repl = None
    last_err = None
    tried = 0
    for run_, find_, repl_ in pick_edits(runs, rng):
        # pick_edits guarantees run_ is the page's first run containing
        # find_ (engine_edits_this_run), so the engine will edit exactly
        # the run the oracles anchor on.
        tried += 1
        r = sh([
            str(ENGINE), "replace", str(pdf_path), str(out_pdf),
            "--page", str(run_["page"]), "--find", find_, "--with", repl_,
            "--fallback-font", str(ROOT.parent / "assets" / "NotoSansSC.ttf"),
        ])
        if r.returncode == 0:
            run, find, repl = run_, find_, repl_
            break
        last_err = r.stderr.strip()[:200]
    if run is None:
        if tried == 0:
            return (pdf_path.name, "skip_no_candidate", None), []
        # engine refusing every candidate is a capability gap (usually a
        # font that can't encode the replacement), not silent corruption
        if last_err and "encrypted" in last_err:
            return (pdf_path.name, "skip_encrypted", None), []
        if last_err and "cannot represent" in last_err:
            return (pdf_path.name, "fail_unencodable", last_err), []
        return (pdf_path.name, "fail_engine_replace", last_err), []
    report = json.loads(r.stdout)

    try:
        verdict, diff = judge(case, pdf_path, out_pdf, run["page"], report, find, repl, work)
    except Exception as e:
        return (pdf_path.name, "fail_harness_error", f"{type(e).__name__}: {e}"[:200]), []

    # Checks 2/3 (font substitution / glyph tofu): only meaningful once
    # judge() got as far as a valid, on-page, size-matched before/after
    # comparison. Their failure OVERRIDES the primary verdict for this row
    # (fail beats pass) but the original verdict is preserved in case.json.
    final_verdict = verdict
    case_extra = {}
    skip_font_oracle = False
    if verdict in FONT_CHECKABLE_VERDICTS:
        try:
            probe = mutool_probe(case, pdf_path, out_pdf, run, work)
        except Exception:
            probe = None
        if probe is None:
            skip_font_oracle = True
        else:
            page_box, before_spans, after_spans, before_img, after_img = probe
            edit_bbox = tuple(report["bbox"])
            # Anchor: the picked run's PRE-EDIT bbox — the judge's own
            # information from when it chose the edit word out of engine
            # extract (the post-edit report bbox is only the affected-
            # pixel scope, never an identity anchor).
            pre_edit_bbox = tuple(run["bbox"])
            # Independent confirmation by the mutool witness: the before-
            # page text inside the anchor must contain `find` (whitespace-
            # insensitive). If not, the anchor cannot be independently
            # established — abstain rather than judge on a shaky anchor.
            anchor_text = "".join(
                c for c, _f, box in before_spans if _center_in(box, pre_edit_bbox))
            font_verdict = font_info = tofu_verdict = tofu_info = None
            if "".join(find.split()) not in "".join(anchor_text.split()):
                skip_font_oracle = True
            else:
                new_text = report.get("new_text")
                try:
                    font_verdict, font_info = detect_font_substitution(
                        before_spans, after_spans, edit_bbox,
                        pre_edit_bbox=pre_edit_bbox, new_text=new_text)
                except Exception:
                    font_verdict, font_info = None, None
                try:
                    tofu_verdict, tofu_info = detect_glyph_tofu(
                        after_img, page_box, before_spans, after_spans, edit_bbox, repl,
                        before_img=before_img, pre_edit_bbox=pre_edit_bbox,
                        new_text=new_text)
                except Exception:
                    tofu_verdict, tofu_info = None, None

            # tofu (nothing rendered at all) is the more severe symptom;
            # prefer it if both somehow fire on the same edit.
            if tofu_verdict == "fail_glyph_tofu":
                final_verdict = "fail_glyph_tofu"
                case_extra["glyph_tofu"] = tofu_info
            elif font_verdict == "fail_font_substituted":
                final_verdict = "fail_font_substituted"
                case_extra["font_check"] = font_info
            if tofu_verdict == "skip_font_oracle" or font_verdict == "skip_font_oracle":
                # Abstention: busy/unobservable background under every
                # ink-tested char, or overlapping foreign text inside the
                # product territory — counted, never guessed.
                skip_font_oracle = True

    if final_verdict.startswith("fail"):
        dest = FAILURES / f"{pdf_path.stem}_{final_verdict}"
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy(pdf_path, dest / "in.pdf")
        shutil.copy(out_pdf, dest / "out.pdf")
        case_data = {"find": find, "with": repl, "report": report, "primary_verdict": verdict}
        case_data.update(case_extra)
        (dest / "case.json").write_text(json.dumps(case_data, indent=2))
        if diff is not None:
            diff.save(dest / "diff.png")
        primary = (pdf_path.name, final_verdict, f"find={find} with={repl} page={run['page']}")
    else:
        primary = (pdf_path.name, final_verdict, None)

    check_rows = []
    if skip_font_oracle:
        check_rows.append((pdf_path.name, "skip_font_oracle", None))

    # Check 1 (identity drift): independent probe on the same edit target,
    # always attempted whenever a real edit run was found above, regardless
    # of the primary/font-check outcome. Reported in the supplementary
    # checks section (never mixed into the per-file headline counts);
    # failures still get a failures/ archive like any other fail.
    id_verdict, id_diff, id_out = check_identity_drift(case, pdf_path, run, find, work)
    if id_verdict.startswith("fail"):
        dest = FAILURES / f"{pdf_path.stem}_{id_verdict}"
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy(pdf_path, dest / "in.pdf")
        if id_out is not None:
            shutil.copy(id_out, dest / "out.pdf")
        (dest / "case.json").write_text(json.dumps(
            {"find": find, "with": find, "mode": "identity", "page": run["page"]}, indent=2))
        if id_diff is not None:
            id_diff.save(dest / "diff.png")
        check_rows.append((pdf_path.name, id_verdict, f"find={find} page={run['page']}"))
    else:
        check_rows.append((pdf_path.name,
                           "identity_pass" if id_verdict == "pass" else id_verdict, None))

    return primary, check_rows


def run_case_varlen(pdf_path: Path, work: Path, case: str, rng):
    """Variable-length probing through the model path (replace-run).

    NOTE: the font-fidelity oracles (identity drift, font substitution,
    glyph tofu) currently only cover the fixed-length path in run_case();
    this varlen path runs judge() alone and returns a single row.

    An engine refusal (reflow/unencodable/encrypted) on one candidate is
    NOT a verdict for the file — keep trying later candidates and page 2, so
    one unsupported run doesn't hide a real corruption elsewhere. Returns
    only on a judged edit (pass or fail); the last seen refusal is reported
    if nothing was ever judged.
    """
    out_pdf = work / f"{case}_out.pdf"
    last_refusal = None
    tried = 0
    for page in (1, 2):
        r = sh([str(ENGINE), "model", str(pdf_path), "--page", str(page)])
        if r.returncode != 0:
            continue
        for b, l, run_i, old, repl in pick_model_edits(r.stdout, rng):
            tried += 1
            rr = sh([
                str(ENGINE), "replace-run", str(pdf_path), str(out_pdf),
                "--page", str(page), "--block", str(b), "--line", str(l), "--run", str(run_i),
                "--with", repl, "--fallback-font", str(ROOT.parent / "assets" / "NotoSansSC.ttf"),
            ])
            if rr.returncode != 0:
                err = rr.stderr.strip()[:200]
                if "reflow" in err or "length differs" in err:
                    last_refusal = ("fail_needs_reflow", f"{old!r}->{repl!r}")
                elif "encrypted" in err:
                    return pdf_path.name, "skip_encrypted", None
                elif "cannot represent" in err:
                    last_refusal = ("fail_unencodable", err)
                else:
                    last_refusal = ("fail_engine_replace", err)
                continue  # try the next candidate; don't let one refusal decide the file
            report = json.loads(rr.stdout)
            try:
                verdict, diff = judge(case, pdf_path, out_pdf, page, report, old, repl, work)
            except Exception as e:
                return pdf_path.name, "fail_harness_error", f"{type(e).__name__}: {e}"[:200]
            if verdict.startswith("fail"):
                dest = FAILURES / f"{pdf_path.stem}_{verdict}"
                dest.mkdir(parents=True, exist_ok=True)
                shutil.copy(pdf_path, dest / "in.pdf")
                shutil.copy(out_pdf, dest / "out.pdf")
                (dest / "case.json").write_text(json.dumps(
                    {"find": old, "with": repl, "report": report, "mode": "varlen"}, indent=2))
                if diff is not None:
                    diff.save(dest / "diff.png")
                return pdf_path.name, verdict, f"find={old!r} with={repl!r} page={page}"
            return pdf_path.name, verdict, None  # a judged pass
    if last_refusal:
        return pdf_path.name, last_refusal[0], last_refusal[1]
    return pdf_path.name, "skip_no_candidate", None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus", nargs="+", help="corpus directories")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--fresh", action="store_true", help="clear failures dir first")
    ap.add_argument("--varlen", action="store_true", help="probe variable-length replacements")
    args = ap.parse_args()
    global VARLEN
    VARLEN = args.varlen

    if not ENGINE.exists():
        sys.exit(f"engine not built: {ENGINE} (run: cargo build in core/)")
    if args.fresh and FAILURES.exists():
        shutil.rmtree(FAILURES)

    pdfs = sorted(p for d in args.corpus for p in Path(d).glob("*.pdf"))
    if args.limit:
        pdfs = pdfs[: args.limit]

    work = ROOT / "work"
    work.mkdir(exist_ok=True)

    results = []
    counts = {}
    check_results = []
    check_counts = {}
    for pdf in pdfs:
        try:
            primary, check_rows = run_case(pdf, work)
        except Exception as e:
            primary = (pdf.name, "fail_harness_crash", f"{type(e).__name__}: {e}"[:200])
            check_rows = []
        name, verdict, detail = primary
        counts[verdict] = counts.get(verdict, 0) + 1
        results.append({"file": name, "verdict": verdict, "detail": detail})
        mark = "." if verdict == "pass" else ("s" if verdict.startswith("skip") else "F")
        print(mark, end="", flush=True)
        for cname, cverdict, cdetail in check_rows:
            check_counts[cverdict] = check_counts.get(cverdict, 0) + 1
            check_results.append({"file": cname, "verdict": cverdict, "detail": cdetail})

    print()
    total = len(results)
    tested = sum(v for k, v in counts.items() if not k.startswith("skip"))
    passed = counts.get("pass", 0)
    print(f"\n=== {total} files, {tested} tested, {passed} passed "
          f"({100 * passed / max(tested, 1):.1f}%) ===")
    for k in sorted(counts):
        print(f"  {k}: {counts[k]}")
    if check_counts:
        print("\n--- supplementary checks (identity drift / font oracle) ---")
        for k in sorted(check_counts):
            print(f"  {k}: {check_counts[k]}")

    (ROOT / "report.json").write_text(json.dumps(
        {"counts": counts, "results": results,
         "checks": check_counts, "check_results": check_results}, indent=2))


if __name__ == "__main__":
    main()
