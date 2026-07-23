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
- **加密 PDF 一律拒绝编辑**（`reject_encrypted`：trailer /Encrypt、lopdf 自动解密的 `was_encrypted()`、打捞通道的 `/PdfreeSalvagedEncrypted` 哨兵三路都要认）。原因：现在的保存会剥掉加密和权限限制。任何新的加载/编辑入口都必须过这道门；直到实现「原样保留加密」才允许放开
- **内部记号进公共命名空间要认命名碰撞**：`gs:` 前缀合成字体键与 PDF 合法资源名（允许含冒号）理论上可碰撞，属已评估接受的风险（PR#1 waived）——处理原则是首见者胜、真实资源优先，绝不覆盖文档自己的定义。新增内部记号（如 /PdfreeGen、/PdfreeSalvagedEncrypted）时必须按同一原则评估

## Web 端（web/ + wasm/）

- 线上：https://pdfree.vercel.app（Vercel 项目 `pdfree`，team ian1995）；自定义域名 pdfree.warmbeing.com 待绑定（Vercel `vercel domains add` + Spaceship CNAME → cname.vercel-dns.com）
- 本地跑：`pnpm --dir web dev`；引擎改了之后 `cd wasm && wasm-pack build --target web --release && cd ../web && ./scripts/sync-assets.sh`（public/ 里的 wasm 是提交进 git 的，Vercel 构建不装 Rust）
- 部署：`cd web && vercel deploy --prod`
- 坑：pdf.js v6 没有 convertToViewportRectangle（用 convertToViewportPoint×2）；渲染要传 `intent:"print"`（display 路径走 rAF，隐藏标签页会冻死）；标准字体/CMap 必须给 standardFontDataUrl/cMapUrl
- `wasm/target/` 427MB 构建产物，已 gitignore，千万别提交

## 兜底字体（已上线 v1）

- 链路：原字体表达不了 → `ttf.rs`（只读 TTF 解析，glyf/cmap4+12/复合字形）取思源黑体轮廓 → `type3gen.rs` 现场合成 Type3 字体（轮廓转 PDF 路径 + ToUnicode）→ 注入页面资源，整段改写、TJ 拆三段保排版
- 字形源：`assets/NotoSansSC.ttf`（静态 Regular 实例 10.6MB，由 google/fonts 的 glyf 版变量字体经 fontTools instancer wght=400 生成——原变量字体默认实例是 **Thin(100)**，直接用会让所有兜底文字变细体；**拉丁子集切片没有中文**，别下错）；web 端在 `web/public/fonts/` 懒加载，与 assets/ 必须同一份文件
- 段内拆分（定长路径已做）：救援顺序 = 原字体全覆盖 → **按字符拆分**（原字体可画的字符留原字体，只有缺字合成 Type3，净宽度补偿单独一个 TJ）→ 整段借字体 → 整段兜底；CID/Type3 原字体不拆（走整段路径）。reflow 变长路径仍整段兜底（待做）
- v1 限制：整段替换（find 必须等于整个 seg 文本）；字重固定 Regular（bold 匹配待做，可从原字体 FontDescriptor 推）

## 下一步（按优先级）

1. Skia/Chrome 导出件的多段匹配编辑（单字形一条指令 → 跨段 find/replace），做完解锁 40 份 skia 语料 + 中文词级编辑
2. 兜底精修：段内拆分（原有字符留原字体）、字重匹配、字体切片（17.8MB → 按需几十 KB）
3. 扩语料到万级；合并/拆分/压缩补进 web 端
