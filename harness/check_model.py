#!/usr/bin/env python3
"""Text model invariant checker: validate the heuristic block/line/run/glyph
reconstruction across a PDF corpus.

For each PDF, extracts the page count, then runs the model command on each
page and validates structural invariants:

  1. bbox sanity    — all bboxes have x0<=x1, y0<=y1, values are finite
  2. nesting        — run bbox contained in line bbox, line in block (1.0pt slack)
  3. line ordering  — baselines strictly decreasing top-to-bottom (0.01 tolerance)
  4. run ordering   — runs sorted left-to-right by x0 (0.01 tolerance)
  5. glyph consistency — run.text equals concatenation of glyph texts
  6. glyph geometry — glyph x values non-decreasing (0.1pt tolerance)

Per-file verdict: ok / N violations / engine_error.
Exit code 0 if all files clean, 1 otherwise.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

ROOT = Path(__file__).parent
# Use engine from main tree, not worktree
ENGINE = Path("/Users/ian/Work/pdfree/core/target/debug/pdfree")
SLACK_NESTING = 1.0  # points
TOLERANCE_BASELINE = 0.01
TOLERANCE_RUN_ORDER = 0.01
TOLERANCE_GLYPH_X = 0.1


def sh(args, timeout=30):
    """Run a subprocess with timeout; return (success, stdout, stderr)."""
    try:
        r = subprocess.run(
            args,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=timeout,
        )
        return r.returncode == 0, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return False, "", "timeout"


def get_page_count(pdf_path: Path) -> Optional[int]:
    """Extract the page count from the PDF via the extract command."""
    success, stdout, stderr = sh([str(ENGINE), "extract", str(pdf_path)])
    if not success:
        return None
    try:
        data = json.loads(stdout)
        return data.get("pages")
    except (json.JSONDecodeError, KeyError):
        return None


def is_finite(v: float) -> bool:
    """Check if a value is finite (not inf or nan)."""
    return isinstance(v, (int, float)) and not (v != v or abs(v) == float("inf"))


def validate_bbox(bbox: List[float], name: str) -> List[str]:
    """Validate a single bbox: x0<=x1, y0<=y1, all finite."""
    violations = []
    if len(bbox) != 4:
        violations.append(f"{name}: bbox length != 4")
        return violations
    x0, y0, x1, y1 = bbox
    if not all(is_finite(v) for v in bbox):
        violations.append(f"{name}: non-finite values")
    if x0 > x1:
        violations.append(f"{name}: x0={x0} > x1={x1}")
    if y0 > y1:
        violations.append(f"{name}: y0={y0} > y1={y1}")
    return violations


def bbox_contains(outer: List[float], inner: List[float], slack: float) -> bool:
    """Check if inner bbox is contained in outer bbox (with slack tolerance)."""
    if len(outer) != 4 or len(inner) != 4:
        return False
    ox0, oy0, ox1, oy1 = outer
    ix0, iy0, ix1, iy1 = inner
    return (
        ix0 >= ox0 - slack
        and iy0 >= oy0 - slack
        and ix1 <= ox1 + slack
        and iy1 <= oy1 + slack
    )


def validate_page(blocks: List[Dict[str, Any]], page_no: int) -> List[str]:
    """Validate all invariants for a single page's model."""
    violations = []

    for block_idx, block in enumerate(blocks):
        block_bbox = block.get("bbox", [])
        violations.extend(
            validate_bbox(block_bbox, f"page {page_no} block {block_idx} bbox")
        )

        lines = block.get("lines", [])
        if not isinstance(lines, list):
            violations.append(f"page {page_no} block {block_idx}: lines not a list")
            continue

        # Track baselines for ordering check
        prev_baseline = float("inf")
        for line_idx, line in enumerate(lines):
            line_bbox = line.get("bbox", [])
            violations.extend(
                validate_bbox(line_bbox, f"page {page_no} block {block_idx} line {line_idx} bbox")
            )

            # Nesting: line bbox in block bbox
            if block_bbox and line_bbox and not bbox_contains(block_bbox, line_bbox, SLACK_NESTING):
                violations.append(
                    f"page {page_no} block {block_idx} line {line_idx}: "
                    f"line bbox {line_bbox} not contained in block bbox {block_bbox}"
                )

            # Line ordering: baselines strictly decreasing (y-up)
            baseline = line.get("baseline")
            if baseline is not None and is_finite(baseline):
                if baseline > prev_baseline + TOLERANCE_BASELINE:
                    violations.append(
                        f"page {page_no} block {block_idx} line {line_idx}: "
                        f"baseline {baseline} not strictly <= previous {prev_baseline}"
                    )
                prev_baseline = baseline

            runs = line.get("runs", [])
            if not isinstance(runs, list):
                violations.append(
                    f"page {page_no} block {block_idx} line {line_idx}: runs not a list"
                )
                continue

            # Track run x0 for ordering check
            prev_x0 = float("-inf")
            for run_idx, run in enumerate(runs):
                run_bbox = run.get("bbox", [])
                violations.extend(
                    validate_bbox(
                        run_bbox,
                        f"page {page_no} block {block_idx} line {line_idx} run {run_idx} bbox",
                    )
                )

                # Nesting: run bbox in line bbox
                if line_bbox and run_bbox and not bbox_contains(line_bbox, run_bbox, SLACK_NESTING):
                    violations.append(
                        f"page {page_no} block {block_idx} line {line_idx} run {run_idx}: "
                        f"run bbox {run_bbox} not contained in line bbox {line_bbox}"
                    )

                # Run ordering: x0 ascending (allow tolerance)
                if run_bbox and len(run_bbox) >= 1:
                    x0 = run_bbox[0]
                    if is_finite(x0):
                        if x0 < prev_x0 - TOLERANCE_RUN_ORDER:
                            violations.append(
                                f"page {page_no} block {block_idx} line {line_idx} run {run_idx}: "
                                f"x0={x0} not >= previous x0={prev_x0}"
                            )
                        prev_x0 = x0

                # Glyph consistency: run.text == concatenation of glyph texts
                run_text = run.get("text", "")
                glyphs = run.get("glyphs", [])
                if not isinstance(glyphs, list):
                    violations.append(
                        f"page {page_no} block {block_idx} line {line_idx} run {run_idx}: "
                        f"glyphs not a list"
                    )
                    continue

                glyph_text = "".join(g.get("text", "") for g in glyphs)
                if glyph_text != run_text:
                    violations.append(
                        f"page {page_no} block {block_idx} line {line_idx} run {run_idx}: "
                        f"run.text='{run_text}' != glyph concatenation '{glyph_text}'"
                    )

                # Glyph geometry: x values non-decreasing (with tolerance)
                prev_x = float("-inf")
                for glyph_idx, glyph in enumerate(glyphs):
                    glyph_x = glyph.get("x")
                    if glyph_x is not None and is_finite(glyph_x):
                        if glyph_x < prev_x - TOLERANCE_GLYPH_X:
                            violations.append(
                                f"page {page_no} block {block_idx} line {line_idx} "
                                f"run {run_idx} glyph {glyph_idx}: "
                                f"x={glyph_x} not >= previous x={prev_x}"
                            )
                        prev_x = glyph_x

    return violations


def check_file(pdf_path: Path) -> Tuple[str, List[str]]:
    """Check a single PDF file; return (verdict, violations_list)."""
    # Get page count
    page_count = get_page_count(pdf_path)
    if page_count is None:
        return "engine_error", []

    all_violations = []
    for page_no in range(1, page_count + 1):
        success, stdout, stderr = sh([str(ENGINE), "model", str(pdf_path), "--page", str(page_no)])
        if not success:
            # timeout or crash: count as engine error for the whole file
            return "engine_error", []

        try:
            blocks = json.loads(stdout)
            if not isinstance(blocks, list):
                return "engine_error", []
            page_violations = validate_page(blocks, page_no)
            all_violations.extend(page_violations)
        except (json.JSONDecodeError, TypeError):
            return "engine_error", []

    if all_violations:
        return f"{len(all_violations)} violations", all_violations
    return "ok", []


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "corpus",
        nargs="*",
        default=["/Users/ian/Work/pdfree/harness/corpus/synthetic"],
        help="corpus directories (default: synthetic)",
    )
    args = ap.parse_args()

    if not ENGINE.exists():
        sys.exit(f"engine not built: {ENGINE} (run: cargo build in core/)")

    # Collect all PDFs
    pdfs = sorted(p for d in args.corpus for p in Path(d).glob("*.pdf"))
    if not pdfs:
        sys.exit(f"no PDFs found in {args.corpus}")

    # Run checks
    results = []
    verdict_counts = {}
    all_violations_flat = []
    for pdf in pdfs:
        verdict, violations = check_file(pdf)
        verdict_counts[verdict] = verdict_counts.get(verdict, 0) + 1
        results.append({"file": pdf.name, "verdict": verdict, "violations": violations})
        all_violations_flat.extend(violations)

        # Print per-file verdict
        if verdict == "ok":
            print(f"{pdf.name}: ok")
        elif verdict == "engine_error":
            print(f"{pdf.name}: engine_error")
        else:
            print(f"{pdf.name}: {verdict}")

    # Print summary
    print()
    total = len(results)
    clean = verdict_counts.get("ok", 0)
    print(f"=== {total} files checked, {clean} clean ===")
    for verdict in sorted(verdict_counts):
        print(f"  {verdict}: {verdict_counts[verdict]}")

    # Print violations by type if any
    if all_violations_flat:
        print(f"\nViolation summary by type:")
        violation_types = {}
        for v in all_violations_flat:
            # Extract violation type: key keyword from the violation string
            if "not finite" in v:
                vtype = "bbox_non_finite"
            elif "not contained" in v:
                vtype = "bbox_nesting"
            elif "strictly" in v or "not <=" in v:
                vtype = "line_ordering"
            elif "not >=" in v and "x0=" in v:
                vtype = "run_ordering"
            elif "!=" in v and "glyph" not in v:
                vtype = "glyph_consistency"
            elif "not >=" in v and "glyph" in v:
                vtype = "glyph_geometry"
            else:
                vtype = "other"
            violation_types[vtype] = violation_types.get(vtype, 0) + 1
        for vtype in sorted(violation_types):
            print(f"  {vtype}: {violation_types[vtype]}")

        # Print a few examples
        print(f"\nExample violations (first 5):")
        for v in all_violations_flat[:5]:
            print(f"  {v}")

    # Return appropriate exit code
    return 0 if clean == total else 1


if __name__ == "__main__":
    sys.exit(main())
