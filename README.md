# pdfree

Edit PDF text in your browser, for free. MIT-licensed engine, no upload — files never leave your machine.

## Why

Every decent PDF text editor costs money. The technical moat isn't algorithmic depth — it's a long tail of compatibility grind that nobody wanted to fund as open source. pdfree attacks that moat with an automated verification loop: real-world PDFs go in, random edits are made through the engine, and four automatic judges verify the output. Failures become regression tests; the engine improves until the long tail is paved.

Clean-room: built against ISO 32000 and test behavior only. No AGPL/GPL code is read or used. Object layer is [lopdf](https://github.com/J-F-Liu/lopdf) (MIT).

## Layout

- `core/` — Rust engine (`pdfree-core`): content-stream interpreter with position tracking, text extraction, in-place text replacement. Compiles to WASM for the web app.
- `harness/` — the oracle loop (Python): corpus management + four judges:
  1. **structure** — `qpdf --check` must not get worse than the input
  2. **isolation** — rendered pixels outside the edit bbox must be byte-identical
  3. **visibility** — pixels inside the edit bbox must actually change
  4. **semantics** — extracted text must contain the replacement
- `web/` — Next.js app (WASM build of the engine). Not started yet.

## Quickstart

```sh
# engine
cd core && cargo build

# harness
python3 -m venv harness/.venv
harness/.venv/bin/pip install pypdfium2 pillow reportlab
brew install qpdf

# generate corpus + run the loop
harness/.venv/bin/python harness/make_corpus.py
harness/.venv/bin/python harness/run.py harness/corpus/synthetic --fresh
```

CLI:

```sh
core/target/debug/pdfree extract doc.pdf
core/target/debug/pdfree replace doc.pdf out.pdf --page 1 --find "old" --with "new"
```

## Status

- [x] Extract text runs with positions (simple + Type0 fonts, ToUnicode CMaps)
- [x] Same-length-ish replacement with TJ width compensation (text after the
      edit stays pixel-identical)
- [x] Standard-14 font metrics (Helvetica/Times/Courier + Arial/TimesNewRoman aliases)
- [x] Glyph-availability check: refuses edits a subset font can't render
      instead of silently corrupting output
- [x] Xref salvage loader: recovers files with broken/missing cross-reference
      tables (all 27 such files in the pdf.js corpus load)
- [x] Corpus: 1100 files incl. the Mozilla pdf.js torture suite; 80.1% pass,
      94.7% of attempted edits correct (the rest are honest refusals)
- [ ] Fallback font embedding for out-of-subset replacements (kills the
      biggest remaining failure bucket, 55 cases)
- [ ] Scale corpus to 10k+ real-world PDFs; CJK corpus + replacement
- [ ] Cross-segment matches, reflow for longer replacements
- [ ] WASM build + web UI (edit / merge / split / compress)

## License

MIT
