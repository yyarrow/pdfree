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

import pypdfium2 as pdfium
from PIL import Image, ImageChops

ROOT = Path(__file__).parent
ENGINE = ROOT.parent / "core" / "target" / "debug" / "pdfree"
FAILURES = ROOT / "failures"
SCALE = 2.0
BBOX_PAD = 6  # pixels of slack around the reported edit bbox


def sh(args):
    return subprocess.run(args, capture_output=True, text=True)


def qpdf_status(path):
    r = sh(["qpdf", "--check", str(path)])
    return r.returncode


def render_page(pdf_path, page_index):
    doc = pdfium.PdfDocument(str(pdf_path))
    try:
        page = doc[page_index]
        if page.get_rotation() != 0:
            return None, None
        bitmap = page.render(scale=SCALE)
        img = bitmap.to_pil().convert("L")
        size = page.get_size()
        return img, size
    finally:
        doc.close()


def extract_page_text(pdf_path, page_index):
    doc = pdfium.PdfDocument(str(pdf_path))
    try:
        return doc[page_index].get_textpage().get_text_bounded()
    finally:
        doc.close()


def pick_edit(runs, rng):
    """Pick a run and a word inside it to replace. Returns (run, find, with_)."""
    candidates = [
        r for r in runs
        if not r["cid"] and len(r["text"]) >= 4 and any(c.isalpha() for c in r["text"])
    ]
    rng.shuffle(candidates)
    for run in candidates:
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
            return run, find, repl
    return None, None, None


def bbox_to_pixels(bbox, page_size):
    """PDF user space (y up) -> pixel rect (y down), padded."""
    _, page_h = page_size
    x0, y0, x1, y1 = bbox
    px0 = int(x0 * SCALE) - BBOX_PAD
    px1 = int(x1 * SCALE) + BBOX_PAD
    py0 = int((page_h - y1) * SCALE) - BBOX_PAD
    py1 = int((page_h - y0) * SCALE) + BBOX_PAD
    return px0, py0, px1, py1


def judge(case, in_pdf, out_pdf, page_no, report, find, repl):
    page_index = page_no - 1

    # 1. structure
    if qpdf_status(out_pdf) > max(qpdf_status(in_pdf), 0):
        return "fail_structure", None

    # 2/3. render diff
    before, size_b = render_page(in_pdf, page_index)
    after, size_a = render_page(out_pdf, page_index)
    if before is None or after is None:
        return "skip_rotated", None
    if before.size != after.size:
        return "fail_page_size_changed", None

    diff = ImageChops.difference(before, after)
    diff_bbox = diff.getbbox()  # None if identical
    if diff_bbox is None:
        return "fail_no_visual_change", None

    px0, py0, px1, py1 = bbox_to_pixels(report["bbox"], size_b)
    dx0, dy0, dx1, dy1 = diff_bbox
    if dx0 < px0 or dy0 < py0 or dx1 > px1 + 1 or dy1 > py1 + 1:
        return "fail_leak_outside_bbox", diff

    # 4. semantics
    text = extract_page_text(out_pdf, page_index)
    if repl not in text:
        return "fail_text_semantics", diff

    return "pass", None


def run_case(pdf_path: Path, work: Path):
    case = hashlib.sha1(pdf_path.name.encode()).hexdigest()[:10]
    rng = random.Random(case)

    r = sh([str(ENGINE), "extract", str(pdf_path)])
    if r.returncode != 0:
        return pdf_path.name, "fail_engine_extract", r.stderr.strip()[:200]
    runs = json.loads(r.stdout)["runs"]
    run, find, repl = pick_edit(runs, rng)
    if run is None:
        return pdf_path.name, "skip_no_candidate", None

    out_pdf = work / f"{case}_out.pdf"
    r = sh([
        str(ENGINE), "replace", str(pdf_path), str(out_pdf),
        "--page", str(run["page"]), "--find", find, "--with", repl,
    ])
    if r.returncode != 0:
        return pdf_path.name, "fail_engine_replace", r.stderr.strip()[:200]
    report = json.loads(r.stdout)

    try:
        verdict, diff = judge(case, pdf_path, out_pdf, run["page"], report, find, repl)
    except Exception as e:  # renderer blew up on our output
        return pdf_path.name, "fail_render_crash", str(e)[:200]

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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus", nargs="+", help="corpus directories")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--fresh", action="store_true", help="clear failures dir first")
    args = ap.parse_args()

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
        name, verdict, detail = run_case(pdf, work)
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
