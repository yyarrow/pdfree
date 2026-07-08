#!/usr/bin/env python3
"""
Skia (Chrome print-to-PDF) corpus generator.

Generates ~40 varied HTML documents and prints each to PDF using Chrome headless.
Outputs to /Users/ian/Work/pdfree/harness/corpus/skia/ in the main tree.
"""

import os
import subprocess
import tempfile
import random
import shutil
from pathlib import Path


def generate_html_documents(temp_dir, count=40):
    """
    Generate varied HTML documents with mix of Chinese/English, different structures, and CSS.
    Returns list of (html_path, description) tuples.
    """
    rng = random.Random(42)  # Deterministic seeding

    # Sample content snippets
    chinese_words = [
        "简历", "发票", "报告", "文章", "合同", "声明", "通知", "更新",
        "销售", "费用", "收入", "支出", "日期", "客户", "产品", "服务"
    ]

    english_words = [
        "Resume", "Invoice", "Report", "Article", "Contract", "Statement", "Notice", "Update",
        "Sales", "Expenses", "Revenue", "Budget", "Date", "Client", "Product", "Service"
    ]

    font_stacks = [
        "system-ui",
        '"PingFang SC", sans-serif',
        "serif",
        "Georgia, serif",
        "monospace",
        '"Courier New", monospace',
        "Arial, sans-serif",
        '"Microsoft YaHei", sans-serif',
    ]

    doc_types = ["resume", "invoice", "report", "article"]

    docs = []

    for i in range(count):
        doc_type = rng.choice(doc_types)
        font_stack = rng.choice(font_stacks)
        font_size = rng.randint(10, 24)
        include_bold = rng.choice([True, False])
        include_italic = rng.choice([True, False])
        include_table = rng.choice([True, False])
        include_list = rng.choice([True, False])

        # Build HTML content
        html_parts = [
            '<!DOCTYPE html>',
            '<html>',
            '<head>',
            '<meta charset="UTF-8">',
            f'<meta name="viewport" content="width=device-width, initial-scale=1.0">',
            '<title>Skia Test Document</title>',
            '<style>',
            f'body {{ font-family: {font_stack}; font-size: {font_size}px; line-height: 1.6; margin: 20px; }}',
            'h1 { font-size: 1.5em; margin-bottom: 10px; }',
            'h2 { font-size: 1.2em; margin-top: 15px; margin-bottom: 8px; }',
            'table { border-collapse: collapse; width: 100%; margin: 10px 0; }',
            'td, th { border: 1px solid #ccc; padding: 8px; text-align: left; }',
            'th { background-color: #f5f5f5; }',
            '.date { text-align: right; margin-top: 5px; }',
            'ul { margin: 10px 0; padding-left: 20px; }',
            'li { margin: 5px 0; }',
            '</style>',
            '</head>',
            '<body>',
        ]

        # Title
        title = rng.choice(chinese_words) if rng.choice([True, False]) else rng.choice(english_words)
        html_parts.append(f'<h1>{title} - Doc {i:03d}</h1>')

        # Add content sections
        for section_num in range(rng.randint(2, 5)):
            heading = rng.choice(chinese_words) if rng.choice([True, False]) else rng.choice(english_words)
            html_parts.append(f'<h2>{heading} {section_num}</h2>')

            # Random paragraph content
            para_text = " ".join(
                rng.choice(chinese_words) if rng.choice([True, False]) else rng.choice(english_words)
                for _ in range(rng.randint(10, 30))
            )

            if include_bold:
                para_text = para_text.replace(para_text.split()[-3], f"<b>{para_text.split()[-3]}</b>", 1)
            if include_italic:
                para_text = para_text.replace(para_text.split()[-2], f"<i>{para_text.split()[-2]}</i>", 1)

            html_parts.append(f'<p>{para_text}</p>')

            # Optionally add a table
            if include_table and section_num == 1:
                html_parts.append('<table>')
                html_parts.append('<tr><th>Name</th><th>Value</th><th>Status</th></tr>')
                for row in range(rng.randint(3, 6)):
                    name = rng.choice(chinese_words) if rng.choice([True, False]) else rng.choice(english_words)
                    value = rng.randint(100, 9999)
                    status = "Active" if rng.choice([True, False]) else "Inactive"
                    html_parts.append(f'<tr><td>{name}</td><td>{value}</td><td>{status}</td></tr>')
                html_parts.append('</table>')

            # Optionally add a list
            if include_list and section_num == len(range(rng.randint(2, 5))) - 1:
                html_parts.append('<ul>')
                for item_num in range(rng.randint(3, 6)):
                    item = rng.choice(chinese_words) if rng.choice([True, False]) else rng.choice(english_words)
                    html_parts.append(f'<li>{item} item {item_num}</li>')
                html_parts.append('</ul>')

        # Add date
        date_str = f"2024-{rng.randint(1,12):02d}-{rng.randint(1,28):02d}"
        html_parts.append(f'<div class="date">{date_str}</div>')

        html_parts.extend(['</body>', '</html>'])

        html_content = '\n'.join(html_parts)
        html_path = os.path.join(temp_dir, f'doc_{i:03d}.html')

        with open(html_path, 'w', encoding='utf-8') as f:
            f.write(html_content)

        docs.append((html_path, f'{doc_type}_{i:03d}'))

    return docs


def print_to_pdf(html_path, pdf_path, timeout=60):
    """
    Print HTML file to PDF using Chrome headless.
    Returns True if successful, False otherwise.
    """
    chrome_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

    if not os.path.exists(chrome_path):
        print(f"Chrome not found at {chrome_path}")
        return False

    cmd = [
        chrome_path,
        "--headless",
        "--disable-gpu",
        "--no-pdf-header-footer",
        f"--print-to-pdf={pdf_path}",
        f"file://{html_path}",
    ]

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout
        )

        # Check if PDF was created
        if os.path.exists(pdf_path) and os.path.getsize(pdf_path) > 0:
            return True
        else:
            if result.stderr:
                print(f"Chrome stderr for {os.path.basename(html_path)}: {result.stderr[:200]}")
            return False

    except subprocess.TimeoutExpired:
        print(f"Timeout printing {os.path.basename(html_path)}")
        return False
    except Exception as e:
        print(f"Error printing {os.path.basename(html_path)}: {e}")
        return False


def main():
    """Generate corpus and print all documents to PDF."""

    # Create output directory in main tree
    output_dir = Path("/Users/ian/Work/pdfree/harness/corpus/skia")
    output_dir.mkdir(parents=True, exist_ok=True)

    print(f"Output directory: {output_dir}")

    # Create temp directory for HTML files
    with tempfile.TemporaryDirectory() as temp_dir:
        print(f"Generating HTML documents in {temp_dir}...")

        # Generate HTML documents
        docs = generate_html_documents(temp_dir, count=40)
        print(f"Generated {len(docs)} HTML documents")

        # Print each to PDF
        successful = 0
        failed = 0

        for idx, (html_path, desc) in enumerate(docs):
            pdf_name = f"skia_{idx:03d}.pdf"
            pdf_path = output_dir / pdf_name

            if print_to_pdf(html_path, str(pdf_path)):
                successful += 1
                print(f"[{idx+1:2d}/{len(docs)}] ✓ {pdf_name}")
            else:
                failed += 1
                print(f"[{idx+1:2d}/{len(docs)}] ✗ {pdf_name}")

    print(f"\n{'='*60}")
    print(f"Generation complete:")
    print(f"  Successful: {successful}")
    print(f"  Failed: {failed}")
    print(f"  Total: {len(docs)}")
    print(f"  Output: {output_dir}")
    print(f"{'='*60}")

    return successful >= 30


if __name__ == "__main__":
    success = main()
    exit(0 if success else 1)
