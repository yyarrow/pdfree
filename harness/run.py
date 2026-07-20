#!/usr/bin/env python3
"""pdfree oracle loop: for each corpus PDF, make a random text edit through the
engine and judge the result with four automatic checks:

  1. structure  — qpdf --check must not get worse than the input
  2. isolation  — rendered pixels outside the edit bbox must be unchanged
  3. visibility — pixels inside the edit bbox must have changed
  4. semantics  — extracted text must contain the new string

Three supplementary font-fidelity oracles run alongside the above (they add
new verdicts to the same counts; a check's own inability to run degrades to
a skip, it never asserts pass/fail it can't back up):

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

from PIL import Image, ImageChops

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


def mutool_probe(case, pdf_path, out_pdf, run, work):
    """Shared I/O for checks 2 and 3 (font substitution / glyph tofu): one
    render (for the after-page bitmap + page_box) plus two mutool stext
    calls (before/after), so the two checks don't each re-invoke mutool.

    Returns (page_box, before_spans, after_spans, after_img), or None on
    any failure (render crash, mutool crash/timeout, unparseable XML) —
    callers treat None as skip_font_oracle.
    """
    page_no = run["page"]
    try:
        after_img, page_box, _ = render_page(out_pdf, page_no - 1, work / f"{case}_fontprobe_after.png")
    except RenderCrash:
        return None
    if after_img is None or page_box is None:
        return None
    before_spans = mutool_char_spans(pdf_path, page_no, page_box)
    after_spans = mutool_char_spans(out_pdf, page_no, page_box)
    if before_spans is None or after_spans is None:
        return None
    return page_box, before_spans, after_spans, after_img


def detect_font_substitution(before_spans, after_spans, edit_bbox):
    """Check 2: characters the ORIGINAL font already covered (drawn
    somewhere on the before-page in that font — a conservative black-box
    proof the font has the glyph) must keep the same base font after the
    edit. A character the original font never drew is exempt: falling back
    for a genuinely unsupported glyph is legitimate engine behavior.

    Pure function over already-fetched mutool spans; returns
    ("fail_font_substituted", detail) or (None, None).
    """
    in_bbox = _bbox_overlap_pred(edit_bbox)

    before_in_bbox = [(c, f) for c, f, box in before_spans if in_bbox(box)]
    if not before_in_bbox:
        return None, None

    font_counts = {}
    for _, f in before_in_bbox:
        font_counts[f] = font_counts.get(f, 0) + 1
    orig_font_raw = max(font_counts, key=font_counts.get)
    orig_font = strip_subset_prefix(orig_font_raw)

    covered_chars = {c for c, f, _ in before_spans if strip_subset_prefix(f) == orig_font}

    for c, f, box in after_spans:
        if not in_bbox(box) or c not in covered_chars:
            continue
        new_font = strip_subset_prefix(f)
        if new_font != orig_font:
            return "fail_font_substituted", {
                "char": c, "orig_font": orig_font, "new_font": new_font,
                "orig_font_raw": orig_font_raw, "new_font_raw": f,
            }
    return None, None


def detect_glyph_tofu(after_img, page_box, after_spans, edit_bbox, repl):
    """Check 3: a replacement character can be encoded into a subset font
    that keeps the SAME font name (so check 2 sees nothing wrong) but lacks
    that glyph's outline — the text layer says the character is there
    (ToUnicode round-trips) yet nothing gets painted ("tofu"). Also covers
    the renderer dropping the character from stext entirely, which is
    evidence of the same underlying problem by a different symptom.

    Aligns `repl`'s characters against the after-page's in-bbox char spans
    with difflib (tolerant of a dropped/reordered char breaking a naive
    1:1 zip), then requires at least one dark pixel (<150, matching judge's
    visibility floor) under each non-whitespace character's quad on the
    AFTER render. Pure function except for the PIL crop on the caller-
    supplied after_img; returns ("fail_glyph_tofu", detail) or (None, None).
    """
    in_bbox = _bbox_overlap_pred(edit_bbox)
    after_in_bbox = [(c, f, box) for c, f, box in after_spans if in_bbox(box)]
    if not after_in_bbox:
        return None, None

    after_str = "".join(c for c, _, _ in after_in_bbox)
    sm = difflib.SequenceMatcher(a=repl, b=after_str, autojunk=False)
    pairing = [None] * len(repl)
    for tag, a0, a1, b0, b1 in sm.get_opcodes():
        if tag == "equal":
            for i in range(a1 - a0):
                pairing[a0 + i] = after_in_bbox[b0 + i]

    w, h = after_img.size
    tofu = []
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
        region = after_img.crop((px0, py0, px1, py1))
        if region.point(lambda v: 255 if v < 150 else 0).getbbox() is None:
            tofu.append({"char": ch, "reason": "blank_glyph"})

    if tofu:
        return "fail_glyph_tofu", {"missing": tofu}
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

    diff = ImageChops.difference(before, after).point(lambda v: 255 if v >= 96 else 0)
    if diff.getbbox() is not None:
        return "fail_identity_drift", diff, out_pdf
    return "pass", None, out_pdf


def run_case(pdf_path: Path, work: Path):
    """Returns a list of (file, verdict, detail) rows — usually one (the
    primary edit's verdict), but the identity-drift probe (check 1) and a
    skip_font_oracle bookkeeping row (checks 2/3) can add extra rows for
    the SAME file into the same counts, per the task's aggregation choice.
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
            return [(pdf_path.name, "skip_invalid_input", None)]
        return [(pdf_path.name, "fail_engine_extract", r.stderr.strip()[:200])]
    if VARLEN:
        return [run_case_varlen(pdf_path, work, case, rng)]
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
            return [(pdf_path.name, "skip_no_candidate", None)]
        # engine refusing every candidate is a capability gap (usually a
        # font that can't encode the replacement), not silent corruption
        if last_err and "encrypted" in last_err:
            return [(pdf_path.name, "skip_encrypted", None)]
        if last_err and "cannot represent" in last_err:
            return [(pdf_path.name, "fail_unencodable", last_err)]
        return [(pdf_path.name, "fail_engine_replace", last_err)]
    report = json.loads(r.stdout)

    try:
        verdict, diff = judge(case, pdf_path, out_pdf, run["page"], report, find, repl, work)
    except Exception as e:
        return [(pdf_path.name, "fail_harness_error", f"{type(e).__name__}: {e}"[:200])]

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
            page_box, before_spans, after_spans, after_img = probe
            edit_bbox = tuple(report["bbox"])
            try:
                font_verdict, font_info = detect_font_substitution(before_spans, after_spans, edit_bbox)
            except Exception:
                font_verdict, font_info = None, None
            try:
                tofu_verdict, tofu_info = detect_glyph_tofu(after_img, page_box, after_spans, edit_bbox, repl)
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

    results = []
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
        results.append((pdf_path.name, final_verdict, f"find={find} with={repl} page={run['page']}"))
    else:
        results.append((pdf_path.name, final_verdict, None))
    if skip_font_oracle:
        results.append((pdf_path.name, "skip_font_oracle", None))

    # Check 1 (identity drift): independent probe on the same edit target,
    # always attempted whenever a real edit run was found above, regardless
    # of the primary/font-check outcome. Only added as its own row when it
    # isn't a plain pass, so it doesn't inflate the "passed" bucket.
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
        results.append((pdf_path.name, id_verdict, f"find={find} page={run['page']}"))
    elif id_verdict != "pass":
        results.append((pdf_path.name, id_verdict, None))

    return results


def run_case_varlen(pdf_path: Path, work: Path, case: str, rng):
    """Variable-length probing through the model path (replace-run).

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
    for pdf in pdfs:
        try:
            entries = run_case(pdf, work)
        except Exception as e:
            entries = [(pdf.name, "fail_harness_crash", f"{type(e).__name__}: {e}"[:200])]
        for name, verdict, detail in entries:
            counts[verdict] = counts.get(verdict, 0) + 1
            results.append({"file": name, "verdict": verdict, "detail": detail})
            mark = "." if verdict == "pass" else ("s" if verdict.startswith("skip") else "F")
            print(mark, end="", flush=True)

    print()
    total = len(results)
    tested = sum(v for k, v in counts.items() if not k.startswith("skip"))
    passed = counts.get("pass", 0)
    print(f"\n=== {total} files, {tested} tested, {passed} passed "
          f"({100 * passed / max(tested, 1):.1f}%) ===")
    for k in sorted(counts):
        print(f"  {k}: {counts[k]}")

    (ROOT / "report.json").write_text(json.dumps(
        {"counts": counts, "results": results}, indent=2))


if __name__ == "__main__":
    main()
