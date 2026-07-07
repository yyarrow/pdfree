#!/usr/bin/env python3
"""Render one PDF page and extract its text, in an isolated process.

pdfium is native code and some corpus files (fuzzer artifacts from the pdf.js
suite) segfault it; running per-page in a child process turns a harness-killing
crash into a per-case verdict. Prints JSON to stdout.
"""

import json
import sys

import pypdfium2 as pdfium


def main():
    pdf_path, page_index, out_png, scale = sys.argv[1], int(sys.argv[2]), sys.argv[3], float(sys.argv[4])
    doc = pdfium.PdfDocument(pdf_path)
    page = doc[page_index]
    if page.get_rotation() != 0:
        print(json.dumps({"rotated": True}))
        return
    bitmap = page.render(scale=scale)
    bitmap.to_pil().convert("L").save(out_png)
    w, h = page.get_size()
    text = page.get_textpage().get_text_bounded()
    print(json.dumps({"rotated": False, "size": [w, h], "text": text}))


if __name__ == "__main__":
    main()
