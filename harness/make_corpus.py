#!/usr/bin/env python3
"""Generate a synthetic PDF corpus with varied fonts/sizes/layouts.

Synthetic files go to harness/corpus/synthetic/. Real-world local files can be
collected separately into harness/corpus/local/ (gitignored).
"""

import random
from pathlib import Path

from reportlab.lib.pagesizes import letter, A4
from reportlab.pdfgen import canvas

OUT = Path(__file__).parent / "corpus" / "synthetic"

FONTS = [
    "Helvetica", "Helvetica-Bold", "Helvetica-Oblique",
    "Times-Roman", "Times-Bold", "Times-Italic",
    "Courier", "Courier-Bold",
]

WORDS = (
    "invoice contract agreement payment schedule delivery warranty liability "
    "customer provider service quality standard material equipment inspection "
    "certificate signature effective termination renewal notice address company "
    "amount total balance interest penalty deadline shipment insurance claim"
).split()


def paragraph(rng, n):
    return " ".join(rng.choice(WORDS) for _ in range(n)).capitalize() + "."


def make_pdf(path: Path, seed: int):
    rng = random.Random(seed)
    page = rng.choice([letter, A4])
    c = canvas.Canvas(str(path), pagesize=page)
    w, h = page
    npages = rng.randint(1, 3)
    for _ in range(npages):
        y = h - 60
        # title
        c.setFont(rng.choice(FONTS), rng.choice([16, 18, 22]))
        c.drawString(60, y, paragraph(rng, 3)[:-1])
        y -= 40
        # body blocks
        for _ in range(rng.randint(3, 8)):
            font = rng.choice(FONTS)
            size = rng.choice([8, 9, 10, 11, 12, 14])
            c.setFont(font, size)
            for _ in range(rng.randint(1, 5)):
                if y < 60:
                    break
                c.drawString(rng.choice([50, 60, 80]), y, paragraph(rng, rng.randint(4, 10)))
                y -= size + 4
            y -= 12
        # a right-aligned line and a centered line for variety
        c.setFont(rng.choice(FONTS), 10)
        c.drawRightString(w - 50, 40, paragraph(rng, 4))
        c.drawCentredString(w / 2, 25, paragraph(rng, 3))
        c.showPage()
    c.save()


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    n = 60
    for i in range(n):
        make_pdf(OUT / f"syn_{i:03d}.pdf", seed=1000 + i)
    print(f"generated {n} PDFs in {OUT}")


if __name__ == "__main__":
    main()
