"use client";

import { useCallback, useEffect, useRef, useState } from "react";

// A run from the engine's text model (block -> line -> run hierarchy).
type MRun = {
  text: string;
  bbox: [number, number, number, number];
  font: string;
  font_size: number;
  color: [number, number, number];
  cid: boolean;
  type3: boolean;
};
type MBlock = { bbox: [number, number, number, number]; lines: { baseline: number; runs: MRun[] }[] };

// The engine keeps the parsed document resident (parse once, edit many,
// serialize only when bytes are actually needed).
type DocSession = {
  page_count: () => number;
  set_fallback_font: (bytes: Uint8Array) => void;
  has_fallback: () => boolean;
  extract_model: (page: number) => string;
  replace_run: (page: number, block: number, line: number, run: number, with_text: string) => string;
  save: () => Uint8Array;
  free: () => void;
};

type Engine = { DocSession: new (data: Uint8Array) => DocSession };

// Fallback glyph source (Noto Sans SC, OFL) — fetched only when an edit
// needs glyphs the document's own fonts lack, then kept for the session.
let fallbackFontCache: Uint8Array | null = null;
async function loadFallbackFont(): Promise<Uint8Array> {
  if (!fallbackFontCache) {
    const res = await fetch("/fonts/NotoSansSC.ttf");
    if (!res.ok) throw new Error("fallback font unavailable");
    fallbackFontCache = new Uint8Array(await res.arrayBuffer());
  }
  return fallbackFontCache;
}

type RunRect = { run: MRun; b: number; l: number; r: number; left: number; top: number; w: number; h: number };
type Popover = { rect: RunRect; x: number; y: number; value: string };

// pdf.js and our WASM engine are loaded at runtime from /public so the
// bundler never sees them (worker files and .wasm confuse it for no gain).
let enginePromise: Promise<Engine> | null = null;
function loadEngine(): Promise<Engine> {
  enginePromise ??= (async () => {
    const mod = await import(/* webpackIgnore: true */ "/wasm/pdfree_wasm.js" as string);
    await mod.default({ module_or_path: "/wasm/pdfree_wasm_bg.wasm" });
    // The wasm is fetched at runtime while the page bundle may be a stale
    // browser-cached copy from an earlier deploy — surface the mismatch as
    // a refresh hint instead of a cryptic missing-function error.
    if (typeof (mod as { DocSession?: unknown }).DocSession !== "function") {
      throw new Error("页面已更新，请强制刷新后重试（Cmd+Shift+R）");
    }
    return mod as unknown as Engine;
  })();
  return enginePromise;
}

let pdfjsPromise: Promise<any> | null = null;
function loadPdfjs(): Promise<any> {
  pdfjsPromise ??= (async () => {
    const lib = await import(/* webpackIgnore: true */ "/pdfjs/pdf.min.mjs" as string);
    lib.GlobalWorkerOptions.workerSrc = "/pdfjs/pdf.worker.min.mjs";
    return lib;
  })();
  return pdfjsPromise;
}

function friendlyError(msg: string): string {
  if (msg.includes("cannot represent")) {
    return "字体缺少所需字形且兜底失败，这段暂时改不了";
  }
  // The engine names WHY a length-changing edit was refused; the reasons
  // are not interchangeable — reporting them all as "doesn't fit" told
  // users to shorten text that had been refused for a structural reason.
  if (msg.includes("reflow refused") || msg.includes("length differs")) {
    if (msg.includes("width-overflow") || msg.includes("push-target-overflow")) {
      return "改完这一行放不下了（超出页面或段落宽度）；自动换行还在开发中";
    }
    if (msg.includes("pattern-fill")) {
      return "这行文字用了特殊颜色或底纹（专色/图案填充），改长度会串色，暂时不支持";
    }
    if (msg.includes("clipping-render-mode")) {
      return "这行文字带裁剪效果，改长度暂时不支持";
    }
    return "这一行的排版结构比较特殊（多段拼接或状态穿插），改长度暂时不支持；等长替换可以用";
  }
  if (msg.includes("not found")) {
    return "没有定位到这段文字，可能刚被改过，请重试";
  }
  return `引擎报错：${msg}`;
}

export default function Home() {
  const [pdfBytes, setPdfBytes] = useState<Uint8Array | null>(null);
  const [fileName, setFileName] = useState("");
  const [pageCount, setPageCount] = useState(0);
  const [page, setPage] = useState(1);
  const [busy, setBusy] = useState("");
  const [toast, setToast] = useState<{ text: string; err: boolean } | null>(null);
  const [popover, setPopover] = useState<Popover | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [rects, setRects] = useState<RunRect[]>([]);
  const [edited, setEdited] = useState(false);
  // True from the moment an edit is submitted until the engine has
  // serialized it: the downloadable bytes are stale for that stretch.
  const [committing, setCommitting] = useState(false);
  // Shown instantly at the edit position while the real render catches up.
  const [optimistic, setOptimistic] = useState<{ l: number; t: number; w: number; h: number; text: string } | null>(
    null,
  );

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const docRef = useRef<any>(null);
  const sessionRef = useRef<DocSession | null>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Serialized bytes of the newest committed edit. `pdfBytes` only catches
  // up once the matching pdf.js document has loaded, so downloading during
  // that window would hand back a file missing the edit the user just made.
  const latestBytesRef = useRef<Uint8Array | null>(null);
  // Generation token for document-changing operations. Every open/edit
  // takes a number; an async continuation may only install its results
  // while it still holds the newest one, so an abandoned load can never
  // resurrect a replaced document or desync docRef/sessionRef/pdfBytes.
  const opSeqRef = useRef(0);

  const showToast = useCallback((text: string, err = false) => {
    setToast({ text, err });
    if (toastTimer.current) clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(null), err ? 5200 : 2600);
  }, []);

  const loadPdfjsDoc = useCallback(async (bytes: Uint8Array) => {
    const pdfjs = await loadPdfjs();
    // pdf.js transfers the buffer to its worker; keep our copy intact.
    // standardFontDataUrl/cMapUrl are required for non-embedded fonts.
    const doc = await pdfjs.getDocument({
      data: bytes.slice(),
      standardFontDataUrl: "/pdfjs/standard_fonts/",
      cMapUrl: "/pdfjs/cmaps/",
      cMapPacked: true,
    }).promise;
    return doc;
  }, []);

  /// Install a freshly loaded document as the render source, retiring the
  /// previous one. Callers must check their generation token first.
  const installDoc = useCallback((doc: any) => {
    docRef.current?.destroy?.();
    docRef.current = doc;
  }, []);

  const openPdf = useCallback(
    async (bytes: Uint8Array, name: string) => {
      setBusy("正在解析…");
      setPopover(null);
      const myOp = ++opSeqRef.current;
      try {
        const engine = await loadEngine();
        if (opSeqRef.current !== myOp) return; // superseded while loading
        const doc = await loadPdfjsDoc(bytes);
        if (opSeqRef.current !== myOp) {
          doc.destroy?.();
          return;
        }
        sessionRef.current?.free?.();
        sessionRef.current = new engine.DocSession(bytes);
        installDoc(doc);
        latestBytesRef.current = bytes;
        setPdfBytes(bytes);
        setFileName(name);
        setPageCount(doc.numPages);
        setPage(1);
      } catch (e) {
        showToast(`打不开这个文件：${e instanceof Error ? e.message : e}`, true);
      } finally {
        setBusy("");
      }
    },
    [showToast, loadPdfjsDoc, installDoc],
  );

  const onFile = useCallback(
    async (f: File | undefined | null) => {
      if (!f) return;
      if (!f.name.toLowerCase().endsWith(".pdf")) {
        showToast("请选择 PDF 文件", true);
        return;
      }
      const bytes = new Uint8Array(await f.arrayBuffer());
      setEdited(false);
      await openPdf(bytes, f.name);
    },
    [openPdf, showToast],
  );

  const loadSample = useCallback(async () => {
    setBusy("正在加载示例…");
    const res = await fetch("/sample.pdf");
    const bytes = new Uint8Array(await res.arrayBuffer());
    setEdited(false);
    await openPdf(bytes, "sample.pdf");
  }, [openPdf]);

  // Render the current page and compute overlay rectangles.
  useEffect(() => {
    const doc = docRef.current;
    const canvas = canvasRef.current;
    if (!doc || !canvas || !pdfBytes) return;
    let cancelled = false;
    (async () => {
      const p = await doc.getPage(page);
      const container = stageRef.current?.parentElement;
      // clientWidth can be 0 in hidden/backgrounded tabs — fall back to full size
      const maxW = Math.min(container?.clientWidth || 900, 900);
      const base = p.getViewport({ scale: 1 });
      const scale = maxW / base.width;
      const viewport = p.getViewport({ scale });
      const dpr = window.devicePixelRatio || 1;
      canvas.width = Math.floor(viewport.width * dpr);
      canvas.height = Math.floor(viewport.height * dpr);
      canvas.style.width = `${Math.floor(viewport.width)}px`;
      canvas.style.height = `${Math.floor(viewport.height)}px`;
      const ctx = canvas.getContext("2d")!;
      // intent "print": the display path schedules via requestAnimationFrame,
      // which browsers freeze in hidden tabs — print renders everywhere.
      await p
        .render({ canvasContext: ctx, viewport, transform: [dpr, 0, 0, dpr, 0, 0], intent: "print" })
        .promise;
      if (cancelled) return;
      // Click targets come from the engine's text model for this page.
      const blocks: MBlock[] = JSON.parse(sessionRef.current?.extract_model(page) ?? "[]");
      const pageRects: RunRect[] = [];
      blocks.forEach((blk, b) =>
        blk.lines.forEach((line, l) =>
          line.runs.forEach((run, r) => {
            const [x0, y0, x1, y1] = run.bbox;
            const [ax, ay] = viewport.convertToViewportPoint(x0, y0);
            const [bx, by] = viewport.convertToViewportPoint(x1, y1);
            pageRects.push({
              run,
              b,
              l,
              r,
              left: Math.min(ax, bx),
              top: Math.min(ay, by),
              w: Math.abs(bx - ax),
              h: Math.abs(by - ay),
            });
          }),
        ),
      );
      setRects(pageRects);
      setOptimistic(null); // real render is on screen now
    })();
    return () => {
      cancelled = true;
    };
  }, [page, pdfBytes]);

  const clickRun = useCallback(
    (rect: RunRect) => {
      // Edits are strictly serial: a submitted edit isn't in the engine
      // until save() returns, and starting another one meanwhile would
      // supersede the first — silently dropping a change the user already
      // confirmed. The window is only human-visible when the fallback font
      // has to be fetched, which is exactly when it must not be lost.
      if (committing) {
        showToast("上一处修改还在提交，请稍候");
        return;
      }
      setPopover({ rect, x: rect.left, y: rect.top + rect.h + 6, value: rect.run.text });
    },
    [committing, showToast],
  );

  const applyEdit = useCallback(async () => {
    const session = sessionRef.current;
    if (!popover || !session) return;
    const { rect, value } = popover;
    if (value === rect.run.text) {
      setPopover(null);
      return;
    }
    // Optimistic: paint the new text over the old spot immediately; the
    // canvas refresh below swaps in the real render and clears it.
    setOptimistic({ l: rect.left, t: rect.top, w: rect.w, h: rect.h, text: value });
    setPopover(null);
    const myOp = ++opSeqRef.current;
    // The engine holds this edit only from save() onwards; until then the
    // downloadable bytes still predate it, so exporting must wait (the
    // fallback-font load below can make that window human-visible).
    setCommitting(true);
    try {
      try {
        session.replace_run(page, rect.b, rect.l, rect.r, value);
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (!msg.includes("cannot represent") || session.has_fallback()) throw e;
        // Fonts lack the glyphs — load the fallback once and retry.
        setBusy("原字体缺字形，正在加载兜底字体…");
        const fallbackFont = await loadFallbackFont();
        // Re-check BEFORE touching the session: a newer edit (or a reset)
        // may have landed during the load, and this stale continuation
        // must not mutate the shared session or publish its bytes.
        if (opSeqRef.current !== myOp || sessionRef.current !== session) return;
        setBusy("");
        session.set_fallback_font(fallbackFont);
        session.replace_run(page, rect.b, rect.l, rect.r, value);
      }
      // pdf.js needs fresh full bytes. Load the new document BEFORE the
      // state change: the render effect reads docRef (a ref, not state),
      // so publishing pdfBytes first would let it repaint from the stale
      // document and leave the new one with nothing to trigger a redraw —
      // the edit would only appear on the NEXT edit's render pass.
      const bytes = session.save();
      // Publish the downloadable bytes IMMEDIATELY: the edit is committed
      // in the engine, so a download during the render load below must
      // include it. Only the on-screen state waits for the document.
      latestBytesRef.current = bytes;
      setEdited(true);
      setCommitting(false); // bytes now include this edit; export is safe
      const doc = await loadPdfjsDoc(bytes);
      if (opSeqRef.current !== myOp) {
        // Superseded (another edit, or a different file was opened) —
        // discard this render, whoever came last owns the screen.
        doc.destroy?.();
        return;
      }
      installDoc(doc);
      setPdfBytes(bytes);
    } catch (e) {
      setOptimistic(null);
      showToast(friendlyError(e instanceof Error ? e.message : String(e)), true);
    } finally {
      // Only the operation that still owns the generation may clear the
      // shared busy/committing flags — a superseded continuation would
      // otherwise unblock export while the newer edit is mid-flight.
      if (opSeqRef.current === myOp) {
        setBusy("");
        setCommitting(false);
      }
    }
  }, [popover, page, showToast, loadPdfjsDoc, installDoc]);

  const download = useCallback(() => {
    // The freshest committed bytes, which may be one render ahead of the
    // on-screen document (see latestBytesRef).
    const bytes = latestBytesRef.current ?? pdfBytes;
    if (!bytes) return;
    const blob = new Blob([bytes.slice()], { type: "application/pdf" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = fileName.replace(/\.pdf$/i, "") + "-edited.pdf";
    a.click();
    URL.revokeObjectURL(url);
  }, [pdfBytes, fileName]);

  return (
    <div className="shell">
      <header className="topbar">
        <span className="brand">
          pdf<em>ree</em>
        </span>
        <span className="tagline">免费改 PDF 正文，排版不乱</span>
        <span className="spacer" />
        <a className="gh" href="https://github.com/yyarrow/pdfree" target="_blank" rel="noreferrer">
          开源 · MIT
        </a>
      </header>

      {!pdfBytes && (
        <section className="hero">
          <h1>
            直接点住 PDF 里的字，改掉它
            <br />
            不需要 Word 原件，也不花一分钱
          </h1>
          <p>
            所有处理都在你的浏览器里完成，文件不会上传到任何服务器。
            <br />
            合同、简历、发票……改个日期、名字、金额，排版原样不动。
          </p>

          <div
            className={`drop${dragOver ? " over" : ""}`}
            onDragOver={(e) => {
              e.preventDefault();
              setDragOver(true);
            }}
            onDragLeave={() => setDragOver(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragOver(false);
              onFile(e.dataTransfer.files?.[0]);
            }}
          >
            <div className="big">把 PDF 拖到这里</div>
            <div className="hint">或者</div>
            <div className="actions">
              <button className="btn" onClick={() => fileInputRef.current?.click()}>
                选择文件
              </button>
              <button className="btn secondary" onClick={loadSample}>
                用示例文件试试
              </button>
            </div>
            <input
              ref={fileInputRef}
              type="file"
              accept=".pdf,application/pdf"
              hidden
              onChange={(e) => onFile(e.target.files?.[0])}
            />
          </div>
          <p className="privacy">
            <b>●</b> 纯本地处理 — 引擎以 WebAssembly 运行在你的浏览器里，断网也能用
          </p>
        </section>
      )}

      {pdfBytes && (
        <section className="editor">
          <div className="toolbar">
            <button
              className="btn secondary"
              onClick={() => {
                // Reset is a document-changing operation too: bump the
                // generation so an edit still awaiting its render can't
                // reinstall the document we are dropping here.
                opSeqRef.current += 1;
                docRef.current?.destroy?.();
                docRef.current = null;
                sessionRef.current?.free?.();
                sessionRef.current = null;
                latestBytesRef.current = null;
                setPdfBytes(null);
                setRects([]);
                setPopover(null);
                setOptimistic(null);
                setEdited(false);
                // The superseded operation's finally deliberately skips
                // shared-flag cleanup, so reset owns it: otherwise the
                // fallback-font message would sit on the drop screen and
                // `committing` would follow into the next document.
                setBusy("");
                setCommitting(false);
              }}
            >
              ← 换个文件
            </button>
            <span className="name">{fileName}</span>
            <span className="spacer" />
            <button className="pagebtn" disabled={page <= 1} onClick={() => setPage((p) => p - 1)}>
              ‹
            </button>
            <span className="pages">
              {page} / {pageCount}
            </span>
            <button
              className="pagebtn"
              disabled={page >= pageCount}
              onClick={() => setPage((p) => p + 1)}
            >
              ›
            </button>
            <span className="spacer" />
            <button className="btn accent" disabled={!edited || committing} onClick={download}>
              下载修改后的 PDF
            </button>
          </div>

          <div ref={stageRef} className="stage">
            <canvas ref={canvasRef} />
            <div className="overlay" onClick={() => setPopover(null)}>
              {rects.map((r, i) => (
                <div
                  key={i}
                  className="run"
                  style={{ left: r.left, top: r.top, width: r.w, height: r.h }}
                  title="点击编辑这段文字"
                  onClick={(e) => {
                    e.stopPropagation();
                    clickRun(r);
                  }}
                />
              ))}
              {optimistic && (
                <div
                  className="optimistic"
                  style={{
                    left: optimistic.l,
                    top: optimistic.t,
                    minWidth: optimistic.w,
                    height: optimistic.h,
                    fontSize: optimistic.h * 0.72,
                  }}
                >
                  {optimistic.text}
                </div>
              )}
              {popover && (
                <div className="popover" style={{ left: popover.x, top: popover.y }} onClick={(e) => e.stopPropagation()}>
                  <label>编辑文字（改完点确认，其余排版保持原样）</label>
                  <input
                    autoFocus
                    value={popover.value}
                    onChange={(e) => setPopover({ ...popover, value: e.target.value })}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") applyEdit();
                      if (e.key === "Escape") setPopover(null);
                    }}
                  />
                  <div className="row">
                    <button className="btn secondary" onClick={() => setPopover(null)}>
                      取消
                    </button>
                    <button className="btn accent" onClick={applyEdit}>
                      确认修改
                    </button>
                  </div>
                </div>
              )}
            </div>
          </div>
        </section>
      )}

      {busy && <div className="loading">{busy}</div>}
      {toast && <div className={`toast${toast.err ? " err" : ""}`}>{toast.text}</div>}

      <footer className="footer">
        pdfree 是 MIT 开源项目 · 引擎与网站均免费 ·{" "}
        <a href="https://github.com/yyarrow/pdfree">GitHub</a>
        <br />
        暂不支持：扫描件（图片型 PDF）、加密文件、增删字数（等长替换以外）— 都在路上
      </footer>
    </div>
  );
}
