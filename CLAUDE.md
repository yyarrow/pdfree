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
- **CID 字宽（已做）**：Type0/Identity-H|V 字体解析 descendant CIDFont 的 `/W`（两种形式：`c [w1 w2…]` 逐字、`cFirst cLast w` 区间，后者不展开）+ `/DW`，替换掉原来「每字 1000/em」的假宽度；非 Identity 编码（UniGB-UCS2-H 等预定义 CMap 需码表映射）仍用近似值并**标记为不可信**，变长编辑照旧拒绝（`cid-width-unavailable`）。判据在 `FontInfo::cid_widths_trusted()` / `Seg.cid_widths_trusted`
- 段内拆分（定长路径已做）：救援顺序 = 原字体全覆盖 → **按字符拆分**（原字体可画的字符留原字体，只有缺字合成 Type3，净宽度补偿单独一个 TJ）→ 整段借字体 → 整段兜底；CID/Type3 原字体不拆（走整段路径）。reflow 变长路径仍整段兜底（待做）
- v1 限制：整段替换（find 必须等于整个 seg 文本）；字重固定 Regular（bold 匹配待做，可从原字体 FontDescriptor 推）

## Chrome/Skia 导出件（浏览器打印的 PDF——简历/网页存档最常见）

结构签名：**每个字形一条 `Tf`+`Tj`**（一页几十个 Type3 字体）、整行包在 `/NonStruct <</MCID n>> BDC … EMC` 里、**每个空格是独立的 `Tj`**、部分字形带 `/Span <</ActualText <feff…>>> BDC`（Type3 无可靠 ToUnicode，靠它提供复制文本）。

变长编辑对这类文件曾经只有 5% 成功率，三道门（都已拆）：
- `BDC/EMC` 是纯语义标记，不改图形状态（ISO 32000 14.6）→ 放行
- `/ActualText` 不能只放行：文字移走而标记留下，会让**显示的和复制出来的不一致**。做法是把「本行完全拥有的」标记块连同过时 ActualText 一起删掉（`owned_marked_blocks`），语义交给新文本自带的 ToUnicode；跨行共享的块仍拒绝
- 空白字形不进模型（`text.trim().is_empty()`），于是空格 `Tj` 被当成「外来文字」→ `walk_page_with_blanks` 记录它们，reflow 认得出这是已知空白（重生成用绝对 Tm，留在原地无害）

**教训**：按公共语料的拒绝分布挑门修，会与用户实际文档类型错位——先看用户的文件长什么样。Chrome 导出样本已放进 `harness/corpus/local/real_skia_resume.pdf`（可用 `chrome --headless --print-to-pdf` 重造）。

## 下一步（按优先级）

1. ~~Skia/Chrome 导出件的多段匹配编辑~~（已做，见上）
2. 兜底精修：段内拆分（原有字符留原字体）、字重匹配、字体切片（17.8MB → 按需几十 KB）
3. 扩语料到万级；合并/拆分/压缩补进 web 端
