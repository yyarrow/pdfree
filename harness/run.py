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
    can't encode one replacement doesn't end the whole case."""
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
        find = rng.choice(words)
        # same-length replacement: shift letters, preserve case
        repl = "".join(
            chr((ord(ch.lower()) - 97 + 7) % 26 + 97).upper() if ch.isupper()
            else chr((ord(ch) - 97 + 7) % 26 + 97)
            for ch in find
        )
        if repl != find:
            picked += 1
            yield run, find, repl


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


def _bbox_overlap_pred(bbox, pad=2.0):
    """Closure testing whether a char bbox (PDF user-space) overlaps an
    edit's reported bbox, with a small point-space pad for float slop."""
    bx0, by0, bx1, by1 = bbox

    def pred(cbox):
        cx0, cy0, cx1, cy1 = cbox
        return not (cx1 < bx0 - pad or cx0 > bx1 + pad or cy1 < by0 - pad or cy0 > by1 + pad)

    return pred


def match_untouched_neighbors(before_spans, after_spans, edit_bbox, tol=2.0, pixel_ctx=None):
    """ONE-TO-ONE greedy matching of in-bbox after spans to in-bbox before
    spans: a pair qualifies when char, base font, and all four bbox coords
    (each within `tol` pt) agree; closest pairs (by max coord delta) are
    consumed first and every before span is consumed AT MOST ONCE.

    This is the shared foundation of checks 2 and 3. Matched after spans
    are "untouched neighbors" — text the edit bbox merely grazes, which
    rendered identically before and after — and are excluded from both
    checks. Unmatched after spans are edit products. One-to-one consumption
    is load-bearing twice over: (a) a single before neighbor can no longer
    exempt SEVERAL after spans, so a genuine replacement char landing
    within tol of a same-char/same-font neighbor still shows up as an edit
    product; (b) when the engine drops the whole edited segment, leftover
    neighbor spans are consumed by their own before twins and cannot be
    mistaken for surviving replacement text.

    PIXEL VERIFICATION (pixel_ctx = (before_img, after_img, page_box)):
    distance alone cannot be trusted — narrow glyphs advance less than tol
    at ordinary sizes (Helvetica 'i' is 222/1000 em = 1.998pt at 9pt), so
    a rewritten char CAN land within tol of a different same-char/same-font
    twin's old position. "Untouched" is therefore verified against its own
    definition: the UNION rectangle of the before twin's quad and the after
    span's quad (expanded 2px for antialias bleed) must render RAW-identical
    between the before and after pages (ImageChops.difference(...).getbbox()
    is None — same no-threshold standard as the identity check). The union
    matters: comparing only the after quad would miss a shifted glyph whose
    OLD ink lies entirely outside the new quad (a ~2pt shift of a narrow
    glyph separates the two quads completely), letting a blank new glyph
    masquerade as its inked twin. Any pixel difference cancels the match
    and the span is treated as an edit product. Mis-cancelling is cheap: a
    genuinely untouched char with ink still passes the downstream ink test
    as an edit product, and blank (whitespace) chars are skipped by that
    test anyway; whereas the dangerous case — a newly written BLANK glyph
    near an inked twin's old position — always differs somewhere in the
    union region and is forced through the ink test, where it fails. When
    pixel_ctx is None (span-only fixtures) matching is distance-only.

    Returns (before_in, after_in, matched, had_candidate) where before_in /
    after_in are the in-bbox subsets (original span tuples, order kept),
    matched maps after_in index -> before_in index of its consumed twin,
    and had_candidate is the set of after_in indices that had at least one
    distance candidate (same char + same base font within tol) — an
    unmatched index in had_candidate was either pixel-vetoed or lost its
    twin to another span, which downstream checks classify differently
    from a plain edit product (see detect_glyph_tofu's three-way
    classification).
    """
    in_bbox = _bbox_overlap_pred(edit_bbox)
    before_in = [(c, f, box) for c, f, box in before_spans if in_bbox(box)]
    after_in = [(c, f, box) for c, f, box in after_spans if in_bbox(box)]

    candidates = []
    for i, (ac, af, abox) in enumerate(after_in):
        a_base = strip_subset_prefix(af)
        for j, (bc, bf, bbox_) in enumerate(before_in):
            if ac != bc or a_base != strip_subset_prefix(bf):
                continue
            delta = max(abs(a - b) for a, b in zip(abox, bbox_))
            if delta <= tol:
                candidates.append((delta, i, j))
    candidates.sort(key=lambda t: t[0])  # distance-first greedy: order-stable

    pixel_ok_cache = {}

    def pixel_identical(i, j):
        """Whether the UNION of before_in[j]'s and after_in[i]'s quads
        rendered identically across the two pages. The union (not just the
        after quad) catches a shifted glyph whose old ink lies entirely
        outside the new quad. Cached per (i, j) pair — the region depends
        on both quads."""
        if pixel_ctx is None:
            return True
        if (i, j) in pixel_ok_cache:
            return pixel_ok_cache[(i, j)]
        before_img, after_img, page_box = pixel_ctx
        w, h = after_img.size
        ax0, ay0, ax1, ay1 = after_in[i][2]
        bx0, by0, bx1, by1 = before_in[j][2]
        union = (min(ax0, bx0), min(ay0, by0), max(ax1, bx1), max(ay1, by1))
        px0, py0, px1, py1 = bbox_to_pixels(union, page_box, pad=2)  # 2px antialias slack
        px0, py0 = max(px0, 0), max(py0, 0)
        px1, py1 = min(px1, w), min(py1, h)
        if px1 <= px0 or py1 <= py0:
            ok = False  # degenerate/offpage region: nothing verifiable
        else:
            rect = (px0, py0, px1, py1)
            ok = ImageChops.difference(
                before_img.crop(rect), after_img.crop(rect)).getbbox() is None
        pixel_ok_cache[(i, j)] = ok
        return ok

    had_candidate = {i for _, i, _ in candidates}
    matched, used_before = {}, set()
    for _, i, j in candidates:
        if i in matched or j in used_before:
            continue
        if not pixel_identical(i, j):
            continue  # renders differ: not an untouched neighbor
        matched[i] = j
        used_before.add(j)
    return before_in, after_in, matched, had_candidate


def mutool_probe(case, pdf_path, out_pdf, run, work):
    """Shared I/O for checks 2 and 3 (font substitution / glyph tofu):
    before AND after page renders (the before bitmap feeds the pixel
    verification of neighbor matches, see match_untouched_neighbors) plus
    two mutool stext calls, so the two checks don't each re-invoke mutool.

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


def detect_font_substitution(before_spans, after_spans, edit_bbox, pixel_ctx=None):
    """Check 2: characters the ORIGINAL font already covered (drawn
    somewhere on the before-page in that font — a conservative black-box
    proof the font has the glyph) must keep the same base font after the
    edit. A character the original font never drew is exempt: falling back
    for a genuinely unsupported glyph is legitimate engine behavior.

    Untouched neighbors — after spans one-to-one matched to an identical
    before span by match_untouched_neighbors() — are excluded first: the
    edit bbox (plus its overlap pad) routinely grazes adjacent text in a
    different font ("label (FontA): value (FontB)" lines), and those chars
    may well be in covered_chars without the edit ever touching them. Only
    UNMATCHED after spans (edit products) are tested, and because matching
    consumes each before span once, a replacement char painted in a
    neighbor's font is still flagged even if it lands within tolerance of
    a real neighbor (the neighbor's before twin is already consumed by the
    neighbor itself).

    THIS check matches DISTANCE-ONLY, deliberately ignoring pixel_ctx (the
    tofu check uses the pixel-verified set; the two matching sets are
    separate). A distance match already requires the before twin to share
    the after span's base font, so the span's font provably did not change
    — which is the only thing this check judges. Pixel differences inside
    a neighbor's quad (italic overhang or antialias bleed from adjacent
    NEW glyphs) are therefore not treated as substitution evidence; under
    pixel-verified matching such a neighbor would be demoted to an edit
    product and, when its char happens to be in covered_chars, falsely
    flagged. Residual window of the split (correct width math this time):
    a replacement char that the engine mis-paints in a NEIGHBOR's font AND
    that lands within 2pt of a same-char span already in that font is
    distance-matched and escapes this check — reachable, since narrow
    glyph advances drop under 2pt at small sizes (Helvetica 'i' = 1.998pt
    at 9pt). Accepted as defense-in-depth: engine-side glyph validation
    (PR #7) refuses to write wrong glyphs, and the tofu check's
    pixel-verified path still scrutinizes that span.

    Pure function over already-fetched mutool spans; returns
    ("fail_font_substituted", detail) or (None, None). pixel_ctx is
    accepted for signature symmetry but intentionally unused.
    """
    del pixel_ctx  # distance-only by design; see docstring
    before_in, after_in, matched, _had_candidate = match_untouched_neighbors(
        before_spans, after_spans, edit_bbox)
    if not before_in:
        return None, None

    orig_font_raw = _majority_font(before_in)
    orig_font = strip_subset_prefix(orig_font_raw)

    covered_chars = {c for c, f, _ in before_spans if strip_subset_prefix(f) == orig_font}

    for i, (c, f, box) in enumerate(after_in):
        if i in matched or c not in covered_chars:
            continue
        new_font = strip_subset_prefix(f)
        if new_font != orig_font:
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


def _ring_contrast(img, quad_rect, glyph_rects, expand=3, glyph_pad=2):
    """Luminance extrema span of the BACKGROUND RING around a glyph quad:
    expand quad_rect by `expand` px, subtract the quad itself, and mask out
    every OTHER char quad on the page (padded by glyph_pad for antialias
    bleed) — adjacent glyphs' own ink is text, not background, and must not
    make a plain page look "busy". Returns the ring's hi-lo span, or None
    when no background pixels survive the masking (dense text or a quad
    flush against the image edge) — with no observable background there is
    no basis for trusting a quad-contrast judgement, so callers record the
    char as "unknown" (abstention), same as a busy ring.
    """
    w, h = img.size
    qx0, qy0, qx1, qy1 = quad_rect
    ex0, ey0 = max(qx0 - expand, 0), max(qy0 - expand, 0)
    ex1, ey1 = min(qx1 + expand, w), min(qy1 + expand, h)
    if ex1 <= ex0 or ey1 <= ey0:
        return None
    crop = img.crop((ex0, ey0, ex1, ey1))
    mask = Image.new("L", crop.size, 255)
    draw = ImageDraw.Draw(mask)
    # Punch out the quad under test (its own ink is what we're judging).
    draw.rectangle((qx0 - ex0, qy0 - ey0, qx1 - ex0 - 1, qy1 - ey0 - 1), fill=0)
    # Punch out every other glyph's quad that intersects the ring.
    for gx0, gy0, gx1, gy1 in glyph_rects:
        gx0, gy0 = gx0 - glyph_pad, gy0 - glyph_pad
        gx1, gy1 = gx1 + glyph_pad, gy1 + glyph_pad
        if gx1 <= ex0 or gx0 >= ex1 or gy1 <= ey0 or gy0 >= ey1:
            continue
        draw.rectangle((gx0 - ex0, gy0 - ey0, gx1 - ex0 - 1, gy1 - ey0 - 1), fill=0)
    hist = crop.histogram(mask)
    live = [v for v, n in enumerate(hist[:256]) if n]
    if not live:
        return None
    return live[-1] - live[0]


def detect_glyph_tofu(after_img, page_box, before_spans, after_spans, edit_bbox, repl,
                      before_img=None):
    """Check 3: a replacement character can be encoded into a subset font
    that keeps the SAME font name (so check 2 sees nothing wrong) but lacks
    that glyph's outline — the text layer says the character is there
    (ToUnicode round-trips) yet nothing gets painted ("tofu"). Also covers
    the renderer dropping characters from stext entirely — up to and
    including the WHOLE edited segment vanishing: untouched neighbors are
    removed first via match_untouched_neighbors(), so leftover neighbor
    spans that happen to spell out repl's characters can never stand in
    for the missing edit products (their before twins consume them). If
    the edit products are empty while the before-page bbox had spans and
    repl has visible characters, every such character is reported
    missing_from_stext.

    THREE-WAY SPAN CLASSIFICATION (matching alone is not enough):
    1. VERIFIED NEIGHBOR — distance-matched AND pixel-identical (union
       rect). Exempt. Those whose before twin is in the edit target's own
       (majority) font join the alignment string as unchanged in-place
       characters (accepted without ink test: their render is
       byte-identical to before); other-font verified neighbors are
       unrelated grazed text and are excluded.
    2. EDIT PRODUCT — no distance candidate at all, OR distance-matched
       but pixel-vetoed with a TARGET-font twin (a same-font twin nearby
       is exactly the round-4/5 capture scenario: it must face the ink
       test). Joins the alignment string and is ink-tested.
    3. AMBIGUOUS — distance-matched but pixel-vetoed (or twin consumed)
       where the twin is an OTHER-font span. Since distance candidates
       require the same base font, this means the after span ITSELF is in
       a non-target font: it is not part of the edited run, and its pixel
       difference is most likely adjacent edit ink swallowed by the union
       pad (italic overhang / antialias bleed). It is neither a trusted
       neighbor nor an edit product: excluded from the alignment string
       entirely — not ink-tested, and NOT allowed to account for repl
       characters. A blank replacement glyph therefore stays the only
       alignment candidate for its repl char and fails the ink test,
       instead of the inked bled-into neighbor absorbing the alignment.
       Residual window unchanged (documented in
       detect_font_substitution): a replacement char mis-painted in a
       neighbor's font within 2pt of a same-char span of that font is
       classified ambiguous and escapes the ink test — engine-side glyph
       validation (PR #7) covers that path.

    Ink test per aligned edit-product character: the quad's luminance
    extrema must span >= 96 (same confidence threshold as judge). A
    painted glyph always produces foreground/background contrast plus
    antialiasing transitions; a blank quad on a uniform background is
    near-flat. Background-validity heuristic: the quad's surrounding ring
    (3px frame outside the quad, with every OTHER glyph quad masked out —
    neighboring text ink is not "background") is checked first. If the
    RING's extrema span >= 96 the local background is itself busy (photo,
    gradient, dense linework), and if NO ring pixels survive the masking
    (dense text, image edge) there is no observable background at all: in
    either case the quad judgement can't be trusted and the char is
    recorded as "unknown" rather than passed or failed. Any unknown char
    without a definite tofu failure makes the whole check abstain
    ("skip_font_oracle" — counted, not failed): success is only reported
    when every ink-tested character was actually judged. Heuristic
    boundary, accepted: a background busy at exactly the glyph's scale but
    flat in the 3px ring can still fool the quad test, and unknown chars
    are simply not judged — tofu there goes undetected by THIS check
    (judge's pixel-diff checks still apply).

    Neighbor matches are pixel-verified when before_img is supplied (see
    match_untouched_neighbors): a span only counts as an untouched
    neighbor if its quad rendered raw-identical before and after, so a
    newly written blank glyph landing on an inked twin's old position
    (possible within the 2pt tolerance: narrow glyphs advance < 2pt at
    ordinary sizes) is forced through the ink test instead of being
    skipped.

    Pure function except for PIL crops on the caller-supplied images;
    returns ("fail_glyph_tofu", detail), ("skip_font_oracle", detail), or
    (None, None).
    """
    pixel_ctx = (before_img, after_img, page_box) if before_img is not None else None
    before_in, after_in, matched, had_candidate = match_untouched_neighbors(
        before_spans, after_spans, edit_bbox, pixel_ctx=pixel_ctx)
    visible_repl = [ch for ch in repl if not ch.isspace()]

    if all(i in matched for i in range(len(after_in))):
        # No edit products at all in the after bbox — everything present is
        # a pre-existing twin. If the target existed and repl needs visible
        # glyphs, the edit output vanished wholesale.
        if before_in and visible_repl:
            return "fail_glyph_tofu", {
                "missing": [{"char": ch, "reason": "missing_from_stext"} for ch in visible_repl],
            }
        return None, None

    orig_font = strip_subset_prefix(_majority_font(before_in)) if before_in else None

    # Three-way classification -> (span, is_product) alignment sequence in
    # reading order; ambiguous spans are dropped entirely (see docstring).
    seq = []
    ambiguous = []
    for i, span in enumerate(after_in):
        if i in matched:
            if orig_font is not None and strip_subset_prefix(before_in[matched[i]][1]) == orig_font:
                seq.append((span, False))  # verified neighbor, target font
            continue  # verified other-font neighbor: excluded
        span_font = strip_subset_prefix(span[1])
        if i in had_candidate and orig_font is not None and span_font != orig_font:
            ambiguous.append(span[0])  # class 3: excluded from alignment
            continue
        seq.append((span, True))  # class 2: edit product, ink-tested

    after_str = "".join(entry[0][0] for entry in seq)
    sm = difflib.SequenceMatcher(a=repl, b=after_str, autojunk=False)
    pairing = [None] * len(repl)
    for tag, a0, a1, b0, b1 in sm.get_opcodes():
        if tag == "equal":
            for i in range(a1 - a0):
                pairing[a0 + i] = seq[b0 + i]

    # Every char quad on the page in pixel space, for ring masking.
    glyph_rects = [bbox_to_pixels(box, page_box, pad=0) for _, _, box in after_spans]

    w, h = after_img.size
    tofu = []
    unknown = []
    judged = 0
    for ch, entry in zip(repl, pairing):
        if ch.isspace():
            continue
        if entry is None:
            tofu.append({"char": ch, "reason": "missing_from_stext"})
            continue
        (_, _, cbox), is_product = entry
        if not is_product:
            continue  # unchanged in-place char: rendered identically to before
        px0, py0, px1, py1 = bbox_to_pixels(cbox, page_box, pad=1)
        px0, py0 = max(px0, 0), max(py0, 0)
        px1, py1 = min(px1, w), min(py1, h)
        if px1 <= px0 or py1 <= py0:
            tofu.append({"char": ch, "reason": "offpage_quad"})
            continue
        ring = _ring_contrast(after_img, (px0, py0, px1, py1), glyph_rects)
        if ring is None or ring >= 96:
            # Busy background, or no background pixels survived masking
            # (dense text / image edge): quad contrast unreliable either
            # way — abstain on this char rather than guess.
            unknown.append(ch)
            continue
        judged += 1
        lo, hi = after_img.crop((px0, py0, px1, py1)).getextrema()
        if hi - lo < 96:
            tofu.append({"char": ch, "reason": "blank_glyph"})

    if tofu:
        detail = {"missing": tofu}
        if unknown:
            detail["unknown_background"] = unknown
        if ambiguous:
            detail["ambiguous_neighbors"] = ambiguous
        return "fail_glyph_tofu", detail
    if unknown:
        # ANY unjudgeable char without a definite failure means this edit
        # was not fully verified: abstain (counted as skip_font_oracle),
        # never report success on partial coverage.
        detail = {"unknown_background": unknown, "judged_ok": judged}
        if ambiguous:
            detail["ambiguous_neighbors"] = ambiguous
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
            pixel_ctx = (before_img, after_img, page_box)
            try:
                font_verdict, font_info = detect_font_substitution(
                    before_spans, after_spans, edit_bbox, pixel_ctx=pixel_ctx)
            except Exception:
                font_verdict, font_info = None, None
            try:
                tofu_verdict, tofu_info = detect_glyph_tofu(
                    after_img, page_box, before_spans, after_spans, edit_bbox, repl,
                    before_img=before_img)
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
            if tofu_verdict == "skip_font_oracle":
                # Every visible char sat on a busy background: the ink test
                # abstained for this file (font-name check still ran).
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
