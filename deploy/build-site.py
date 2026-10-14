#!/usr/bin/env python3
"""Render docs/whitepaper.md to web/whitepaper/index.html. Run after editing the Markdown."""
import pathlib, markdown
root = pathlib.Path(__file__).resolve().parent.parent
md = (root / "docs/whitepaper.md").read_text()
body = markdown.markdown(md, extensions=["tables", "fenced_code"])
html = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Bollard white paper</title>
<meta name="description" content="Bollard protocol design: a cross-chain order book finalized on Bitcoin.">
<link rel="stylesheet" href="/style.css">
</head>
<body>
<main>
  <nav>
    <a href="/">Bollard</a>
    <a href="/dashboard/">Dashboard</a>
    <a href="/whitepaper/" class="here">White paper</a>
    <a href="https://github.com/masonforest/bollard">Source</a>
    <a href="https://mainnet.bollard.exchange/status">API</a>
  </nav>
{body}
  <footer>Source for this page: <a href="https://github.com/masonforest/bollard/blob/main/docs/whitepaper.md">docs/whitepaper.md</a>.</footer>
</main>
</body>
</html>
"""
out = root / "web/whitepaper/index.html"
out.write_text(html)
print(f"wrote {out} ({len(html)} bytes)")
