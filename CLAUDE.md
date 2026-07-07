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

## 下一步（按优先级）

1. 扩语料到万级（Common Crawl / govdocs1），中文语料单独收
2. CJK 替换：嵌入子集字体缺字检测 + 思源黑体兜底
3. wasm-pack 构建 + web/（Next.js，部署 Vercel，域名 pdfree.warmbeing.com）
