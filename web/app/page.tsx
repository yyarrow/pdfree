"use client";

import { useCallback, useEffect, useRef, useState } from "react";

type Run = {
  page: number;
  text: string;
  font: string;
  font_size: number;
  bbox: [number, number, number, number];
  cid: boolean;
  visible: boolean;
  type3: boolean;
};

type Engine = {
  extract: (data: Uint8Array) => string;
  replace: (
    data: Uint8Array,
    page: number,
    find: string,
    with_text: string,
    fallback_font?: Uint8Array,
  ) => { pdf: Uint8Array; report: string };
};

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

type Popover = { run: Run; x: number; y: number; value: string };

// pdf.js and our WASM engine are loaded at runtime from /public so the
// bundler never sees them (worker files and .wasm confuse it for no gain).
let enginePromise: Promise<Engine> | null = null;
function loadEngine(): Promise<Engine> {
  enginePromise ??= (async () => {
    const mod = await import(/* webpackIgnore: true */ "/wasm/pdfree_wasm.js" as string);
    await mod.default({ module_or_path: "/wasm/pdfree_wasm_bg.wasm" });
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
    return "这个字体的嵌入子集里缺少替换所需的字形，暂时改不了这个词（兜底字体功能开发中）";
  }
  if (msg.includes("not found on page")) {
    return "没有在页面上定位到这段文字，可能刚被改过，请重试";
  }
  return `引擎报错：${msg}`;
}

export default function Home() {
  const [pdfBytes, setPdfBytes] = useState<Uint8Array | null>(null);
  const [fileName, setFileName] = useState("");
  const [runs, setRuns] = useState<Run[]>([]);
  const [pageCount, setPageCount] = useState(0);
  const [page, setPage] = useState(1);
  const [busy, setBusy] = useState("");
  const [toast, setToast] = useState<{ text: string; err: boolean } | null>(null);
  const [popover, setPopover] = useState<Popover | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [rects, setRects] = useState<{ run: Run; l: number; t: number; w: number; h: number }[]>([]);
  const [edited, setEdited] = useState(false);

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const docRef = useRef<any>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const showToast = useCallback((text: string, err = false) => {
    setToast({ text, err });
    if (toastTimer.current) clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(null), err ? 5200 : 2600);
  }, []);

  const openPdf = useCallback(
    async (bytes: Uint8Array, name: string) => {
      setBusy("正在解析…");
      setPopover(null);
      try {
        const engine = await loadEngine();
        const parsed = JSON.parse(engine.extract(bytes));
        const pdfjs = await loadPdfjs();
        // pdf.js transfers the buffer to its worker; keep our copy intact.
        // standardFontDataUrl/cMapUrl are required for non-embedded fonts.
        const doc = await pdfjs.getDocument({
          data: bytes.slice(),
          standardFontDataUrl: "/pdfjs/standard_fonts/",
          cMapUrl: "/pdfjs/cmaps/",
          cMapPacked: true,
        }).promise;
        docRef.current?.destroy?.();
        docRef.current = doc;
        setPdfBytes(bytes);
        setFileName(name);
        setRuns(parsed.runs);
        setPageCount(doc.numPages);
        setPage(1);
      } catch (e) {
        showToast(`打不开这个文件：${e instanceof Error ? e.message : e}`, true);
      } finally {
        setBusy("");
      }
    },
    [showToast],
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
      const pageRects = runs
        .filter((r) => r.page === page && r.visible)
        .map((run) => {
          const [x0, y0, x1, y1] = run.bbox;
          const [ax, ay] = viewport.convertToViewportPoint(x0, y0);
          const [bx, by] = viewport.convertToViewportPoint(x1, y1);
          return {
            run,
            l: Math.min(ax, bx),
            t: Math.min(ay, by),
            w: Math.abs(bx - ax),
            h: Math.abs(by - ay),
          };
        });
      setRects(pageRects);
    })();
    return () => {
      cancelled = true;
    };
  }, [page, runs, pdfBytes]);

  const clickRun = useCallback(
    (r: { run: Run; l: number; t: number; w: number; h: number }) => {
      if (r.run.cid) {
        showToast("中文/CJK 文本编辑正在开发中，敬请期待", true);
        return;
      }
      setPopover({ run: r.run, x: r.l, y: r.t + r.h + 6, value: r.run.text });
    },
    [showToast],
  );

  const applyEdit = useCallback(async () => {
    if (!popover || !pdfBytes) return;
    const { run, value } = popover;
    if (value === run.text) {
      setPopover(null);
      return;
    }
    setBusy("正在改写…");
    try {
      const engine = await loadEngine();
      let result;
      try {
        result = engine.replace(pdfBytes, run.page, run.text, value);
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (!msg.includes("cannot represent")) throw e;
        // Original font lacks the glyphs — retry with the fallback font.
        setBusy("原字体缺字形，正在加载兜底字体…");
        const font = await loadFallbackFont();
        result = engine.replace(pdfBytes, run.page, run.text, value, font);
      }
      setPopover(null);
      await openPdf(result.pdf, fileName);
      setEdited(true);
      setPage(run.page);
      showToast("改好了，编辑点之外一个像素都没动");
    } catch (e) {
      showToast(friendlyError(e instanceof Error ? e.message : String(e)), true);
    } finally {
      setBusy("");
    }
  }, [popover, pdfBytes, fileName, openPdf, showToast]);

  const download = useCallback(() => {
    if (!pdfBytes) return;
    const blob = new Blob([pdfBytes.slice()], { type: "application/pdf" });
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
                docRef.current?.destroy?.();
                docRef.current = null;
                setPdfBytes(null);
                setRuns([]);
                setRects([]);
                setPopover(null);
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
            <button className="btn accent" disabled={!edited} onClick={download}>
              下载修改后的 PDF
            </button>
          </div>

          <div ref={stageRef} className="stage">
            <canvas ref={canvasRef} />
            <div className="overlay" onClick={() => setPopover(null)}>
              {rects.map((r, i) => (
                <div
                  key={i}
                  className={`run${r.run.cid ? " disabled" : ""}`}
                  style={{ left: r.l, top: r.t, width: r.w, height: r.h }}
                  title={r.run.cid ? "中文编辑开发中" : "点击编辑这段文字"}
                  onClick={(e) => {
                    e.stopPropagation();
                    clickRun(r);
                  }}
                />
              ))}
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
        暂不支持：中文编辑、扫描件（图片型 PDF）、加密文件 — 都在路上
      </footer>
    </div>
  );
}
