#!/usr/bin/env python3
"""pdfree oracle loop: for each corpus PDF, make a random text edit through the
engine and judge the result with four automatic checks:

  1. structure  — qpdf --check must not get worse than the input
  2. isolation  — rendered pixels outside the edit bbox must be unchanged
  3. visibility — pixels inside the edit bbox must have changed
  4. semantics  — extracted text must contain the new string

Results: summary to stdout + report.json; failing cases archived under
harness/failures/<case>/ with in/out PDFs and a diff image.
"""

import argparse
import hashlib
import json
import random
import shutil
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageChops

ROOT = Path(__file__).parent
ENGINE = ROOT.parent / "core" / "target" / "debug" / "pdfree"
FAILURES = ROOT / "failures"
SCALE = 2.0
BBOX_PAD = 6  # pixels of slack around the reported edit bbox


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


def bbox_to_pixels(bbox, page_box):
    """PDF user space (y up) -> pixel rect (y down), padded.

    page_box is the rendered region's (left, bottom, right, top): cropped
    pages don't start at the user-space origin.
    """
    import math
    left, _, _, top = page_box
    x0, y0, x1, y1 = bbox
    px0 = math.floor((x0 - left) * SCALE) - BBOX_PAD
    px1 = math.ceil((x1 - left) * SCALE) + BBOX_PAD
    py0 = math.floor((top - y1) * SCALE) - BBOX_PAD
    py1 = math.ceil((top - y0) * SCALE) + BBOX_PAD
    return px0, py0, px1, py1


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


def run_case(pdf_path: Path, work: Path):
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
            return pdf_path.name, "skip_invalid_input", None
        return pdf_path.name, "fail_engine_extract", r.stderr.strip()[:200]
    if VARLEN:
        return run_case_varlen(pdf_path, work, case, rng)
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
            return pdf_path.name, "skip_no_candidate", None
        # engine refusing every candidate is a capability gap (usually a
        # font that can't encode the replacement), not silent corruption
        if last_err and "encrypted" in last_err:
            return pdf_path.name, "skip_encrypted", None
        if last_err and "cannot represent" in last_err:
            return pdf_path.name, "fail_unencodable", last_err
        return pdf_path.name, "fail_engine_replace", last_err
    report = json.loads(r.stdout)

    try:
        verdict, diff = judge(case, pdf_path, out_pdf, run["page"], report, find, repl, work)
    except Exception as e:
        return pdf_path.name, "fail_harness_error", f"{type(e).__name__}: {e}"[:200]

    if verdict.startswith("fail"):
        dest = FAILURES / f"{pdf_path.stem}_{verdict}"
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy(pdf_path, dest / "in.pdf")
        shutil.copy(out_pdf, dest / "out.pdf")
        (dest / "case.json").write_text(json.dumps(
            {"find": find, "with": repl, "report": report}, indent=2))
        if diff is not None:
            diff.save(dest / "diff.png")
        return pdf_path.name, verdict, f"find={find} with={repl} page={run['page']}"

    return pdf_path.name, verdict, None


def run_case_varlen(pdf_path: Path, work: Path, case: str, rng):
    """Variable-length probing through the model path (replace-run)."""
    out_pdf = work / f"{case}_out.pdf"
    for page in (1, 2):
        r = sh([str(ENGINE), "model", str(pdf_path), "--page", str(page)])
        if r.returncode != 0:
            continue
        for b, l, run_i, old, repl in pick_model_edits(r.stdout, rng):
            rr = sh([
                str(ENGINE), "replace-run", str(pdf_path), str(out_pdf),
                "--page", str(page), "--block", str(b), "--line", str(l), "--run", str(run_i),
                "--with", repl, "--fallback-font", str(ROOT.parent / "assets" / "NotoSansSC.ttf"),
            ])
            if rr.returncode != 0:
                err = rr.stderr.strip()[:200]
                if "reflow" in err or "length differs" in err:
                    return pdf_path.name, "fail_needs_reflow", f"{old!r}->{repl!r}"
                if "encrypted" in err:
                    return pdf_path.name, "skip_encrypted", None
                if "cannot represent" in err:
                    return pdf_path.name, "fail_unencodable", err
                return pdf_path.name, "fail_engine_replace", err
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
            return pdf_path.name, verdict, None
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
            name, verdict, detail = run_case(pdf, work)
        except Exception as e:
            name, verdict, detail = pdf.name, "fail_harness_crash", f"{type(e).__name__}: {e}"[:200]
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
