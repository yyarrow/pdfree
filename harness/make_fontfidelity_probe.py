#!/usr/bin/env python3
"""Self-test for check 3 (fail_glyph_tofu) in run.py.

The synthetic corpus (reportlab + standard-14 fonts) never exercises a
subset font that's missing a glyph, so a full corpus run alone can't prove
detect_glyph_tofu() is capable of firing at all. This script manufactures
the exact scenario by hand and asserts the check catches it:

  1. pyftsubset a system TTF down to only the glyphs needed for "Ian only"
     (deliberately excludes 'd') -- no --notdef-outline, so the missing
     glyph is genuinely blank rather than a visible placeholder box.
  2. reportlab embeds that subset font and draws "Ian only".
  3. The engine's `replace` reuses the SAME embedded font resource to write
     "Idn only" -- the font name survives (check 2 stays quiet) but 'd' has
     no outline in that font, so nothing paints (check 3 should fire).

Not part of the corpus loop -- run standalone:
    harness/.venv/bin/python harness/make_fontfidelity_probe.py
Exits non-zero (and prints why) if the tofu oracle fails to trigger.
"""

import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import run as R  # noqa: E402

from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.pdfgen import canvas

WORK = R.ROOT / "work"
SYSTEM_FONT = Path("/System/Library/Fonts/Supplemental/Arial.ttf")
SUBSET_FONT = WORK / "fontfidelity_subset.ttf"
PROBE_PDF = WORK / "fontfidelity_probe.pdf"
OUT_PDF = WORK / "fontfidelity_probe_out.pdf"

TEXT = "Ian only"    # deliberately has no 'd'
REPL = "Idn only"    # needs a 'd' -- absent from the subset's glyph outlines


def build_subset_font():
    if not SYSTEM_FONT.exists():
        sys.exit(f"system font not found: {SYSTEM_FONT} (adjust SYSTEM_FONT for this machine)")
    subprocess.run(
        [
            sys.executable, "-m", "fontTools.subset", str(SYSTEM_FONT),
            f"--text={TEXT}", f"--output-file={SUBSET_FONT}",
            "--glyph-names", "--notdef-glyph",
            # deliberately NOT --notdef-outline: a missing glyph must render
            # as genuinely blank ink, not fontTools' own placeholder box
            # (which pdfium would paint visibly and the check would miss).
        ],
        check=True,
    )


def build_probe_pdf():
    pdfmetrics.registerFont(TTFont("FontFidelityProbe", str(SUBSET_FONT)))
    c = canvas.Canvas(str(PROBE_PDF), pagesize=(300, 150))
    c.setFont("FontFidelityProbe", 24)
    c.drawString(50, 80, TEXT)
    c.save()


def main():
    WORK.mkdir(exist_ok=True)
    if not R.ENGINE.exists():
        sys.exit(f"engine not built: {R.ENGINE} (run: cargo build in core/)")

    build_subset_font()
    build_probe_pdf()

    r = subprocess.run(
        [
            str(R.ENGINE), "replace", str(PROBE_PDF), str(OUT_PDF),
            "--page", "1", "--find", TEXT, "--with", REPL,
            "--fallback-font", str(R.ROOT.parent / "assets" / "NotoSansSC.ttf"),
        ],
        capture_output=True, text=True,
    )
    if r.returncode != 0:
        print(f"engine replace failed (rc={r.returncode}): {r.stderr.strip()}")
        sys.exit(1)
    report = json.loads(r.stdout)
    print(f"engine report: {report}")

    rext = subprocess.run([str(R.ENGINE), "extract", str(PROBE_PDF)],
                          capture_output=True, text=True)
    pre_edit_bbox = tuple(json.loads(rext.stdout)["runs"][0]["bbox"])

    probe = R.mutool_probe("fontfidelity_selftest", PROBE_PDF, OUT_PDF, {"page": 1}, WORK)
    if probe is None:
        print("mutool_probe returned None (mutool crash/timeout/parse failure) -- can't self-test")
        sys.exit(1)
    page_box, before_spans, after_spans, before_img, after_img = probe
    edit_bbox = tuple(report["bbox"])

    font_verdict, font_info = R.detect_font_substitution(
        before_spans, after_spans, edit_bbox,
        pre_edit_bbox=pre_edit_bbox, new_text=report.get("new_text"))
    tofu_verdict, tofu_info = R.detect_glyph_tofu(
        after_img, page_box, before_spans, after_spans, edit_bbox, REPL,
        before_img=before_img, pre_edit_bbox=pre_edit_bbox,
        new_text=report.get("new_text"))

    print(f"check 2 (font substitution): {font_verdict} {font_info}")
    print(f"check 3 (glyph tofu):        {tofu_verdict} {tofu_info}")

    ok = True
    if font_verdict == "fail_font_substituted":
        print("UNEXPECTED: check 2 also fired -- the probe wasn't isolating check 3 as intended")
        ok = False
    if tofu_verdict != "fail_glyph_tofu":
        print("FAIL: check 3 did not trigger on a hand-built blank-glyph edit")
        ok = False
    elif not any(m["char"] == "d" and m["reason"] == "blank_glyph" for m in tofu_info["missing"]):
        print(f"FAIL: check 3 fired but not on the expected 'd'/blank_glyph: {tofu_info}")
        ok = False

    if ok:
        print("PASS: check 3 (glyph tofu) correctly triggers on a hand-built blank-glyph edit, "
              "and check 2 correctly stays quiet since the font name didn't change.")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
