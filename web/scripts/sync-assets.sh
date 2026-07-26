#!/bin/sh
# Copy runtime-loaded assets into public/: the WASM engine build and pdf.js.
# Run after `wasm-pack build --target web --release` in ../wasm, or after
# bumping pdfjs-dist. The copies are committed so Vercel builds need no Rust.
set -e
cd "$(dirname "$0")/.."

mkdir -p public/wasm public/pdfjs
# Staleness guard: copying an old wasm/pkg silently ships a build that
# predates the engine sources — the site then runs code nobody wrote
# recently, and the mismatch is invisible in review (it happened once:
# reason labels fixed in Rust, old labels still served to users).
# Build inputs, not just sources: a dependency bump or feature toggle in a
# manifest/lockfile changes the binary while every .rs stays older.
STALE=$(find ../core/src ../wasm/src -name '*.rs' -newer ../wasm/pkg/pdfree_wasm_bg.wasm -print -quit 2>/dev/null)
if [ -z "$STALE" ]; then
  for f in ../core/Cargo.toml ../wasm/Cargo.toml ../core/Cargo.lock ../wasm/Cargo.lock ../Cargo.toml ../Cargo.lock; do
    if [ -f "$f" ] && [ "$f" -nt ../wasm/pkg/pdfree_wasm_bg.wasm ]; then
      STALE=$f
      break
    fi
  done
fi
if [ -n "$STALE" ]; then
  echo "error: wasm/pkg is older than build input $STALE — run 'wasm-pack build --target web --release' in ../wasm first" >&2
  exit 1
fi
cp ../wasm/pkg/pdfree_wasm.js ../wasm/pkg/pdfree_wasm_bg.wasm public/wasm/
cp node_modules/pdfjs-dist/build/pdf.min.mjs node_modules/pdfjs-dist/build/pdf.worker.min.mjs public/pdfjs/
# standard font data + cmaps: pdf.js needs these to render non-embedded
# standard-14 fonts and CJK encodings
rm -rf public/pdfjs/standard_fonts public/pdfjs/cmaps
cp -R node_modules/pdfjs-dist/standard_fonts public/pdfjs/standard_fonts
cp -R node_modules/pdfjs-dist/cmaps public/pdfjs/cmaps
echo "synced: $(ls public/wasm public/pdfjs | tr '\n' ' ')"
