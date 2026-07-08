#!/bin/sh
# Copy runtime-loaded assets into public/: the WASM engine build and pdf.js.
# Run after `wasm-pack build --target web --release` in ../wasm, or after
# bumping pdfjs-dist. The copies are committed so Vercel builds need no Rust.
set -e
cd "$(dirname "$0")/.."

mkdir -p public/wasm public/pdfjs
cp ../wasm/pkg/pdfree_wasm.js ../wasm/pkg/pdfree_wasm_bg.wasm public/wasm/
cp node_modules/pdfjs-dist/build/pdf.min.mjs node_modules/pdfjs-dist/build/pdf.worker.min.mjs public/pdfjs/
# standard font data + cmaps: pdf.js needs these to render non-embedded
# standard-14 fonts and CJK encodings
rm -rf public/pdfjs/standard_fonts public/pdfjs/cmaps
cp -R node_modules/pdfjs-dist/standard_fonts public/pdfjs/standard_fonts
cp -R node_modules/pdfjs-dist/cmaps public/pdfjs/cmaps
echo "synced: $(ls public/wasm public/pdfjs | tr '\n' ' ')"
