import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "pdfree — 免费在线改 PDF 正文",
  description:
    "在浏览器里直接编辑 PDF 里的文字，排版不乱。完全本地处理，文件不上传，开源免费。",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="zh-CN">
      <body>{children}</body>
    </html>
  );
}
