# pdfree

浏览器里免费改 PDF 正文。引擎 MIT 开源，WASM 本地运行不上传。定位与决策背景见根目录 README。

## 怎么跑

- 引擎：`cd core && cargo build`（产物 `core/target/debug/pdfree`）
- 闭环：`harness/.venv/bin/python harness/run.py harness/corpus/synthetic harness/corpus/local --fresh`
  - 失败样本自动归档到 `harness/failures/<case>/`（in.pdf / out.pdf / diff.png / case.json），修完必须重跑全量确认没退步
  - `harness/corpus/local/` 是从本机 Spotlight 收集的真实 PDF，**私人文件，永远不进 git**（已 gitignore）
- 依赖：qpdf（brew）、harness/.venv（pypdfium2 + pillow + reportlab）

## 铁律

- 引擎代码**禁止参考 AGPL/GPL 实现**（MuPDF、Ghostscript、iText 等），只准对着 ISO 32000 规范和测试结果写。允许的依赖：lopdf（MIT）、pdfium（BSD，只用于 harness 渲染裁判）
- `core/src/std14.rs` 是 make_corpus 同款 venv 里 reportlab AFM 数据生成的，别手改；重新生成的脚本在 git history 里
- 通过率是唯一 KPI：改引擎前先跑基线，改完对比，任何类别变差都要查

## Web 端（web/ + wasm/）

- 线上：https://pdfree.vercel.app（Vercel 项目 `pdfree`，team ian1995）；自定义域名 pdfree.warmbeing.com 待绑定（Vercel `vercel domains add` + Spaceship CNAME → cname.vercel-dns.com）
- 本地跑：`pnpm --dir web dev`；引擎改了之后 `cd wasm && wasm-pack build --target web --release && cd ../web && ./scripts/sync-assets.sh`（public/ 里的 wasm 是提交进 git 的，Vercel 构建不装 Rust）
- 部署：`cd web && vercel deploy --prod`
- 坑：pdf.js v6 没有 convertToViewportRectangle（用 convertToViewportPoint×2）；渲染要传 `intent:"print"`（display 路径走 rAF，隐藏标签页会冻死）；标准字体/CMap 必须给 standardFontDataUrl/cMapUrl
- `wasm/target/` 427MB 构建产物，已 gitignore，千万别提交

## 下一步（按优先级）

1. 兜底字体嵌入（55 例 fail_unencodable，也是中文编辑的地基）
2. 扩语料到万级（Common Crawl / govdocs1），中文语料单独收
3. 合并/拆分/压缩功能补进 web 端
