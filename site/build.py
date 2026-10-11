#!/usr/bin/env python3
"""Build the static documentation site for omavrything.

Renders ``docs/*.md`` (and the hand-written landing page in ``site/index.md``)
into ``site/dist/`` as a self-contained static site with no external requests.

Usage::

    python3 -m venv .venv-site
    .venv-site/bin/pip install -r site/requirements.txt
    .venv-site/bin/python site/build.py [--out DIR]
"""

from __future__ import annotations

import argparse
import html
import json
import re
import shutil
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
DOCS = ROOT / "docs"
ASSETS = ROOT / "assets"

REPO = "https://github.com/brrrassica/omavrything"
SITE_URL = "https://omavrything.alatcerdas.tech"
DEFAULT_DESC = (
    "Instant, index-backed file search for Omarchy 4.x — Everything for Linux. "
    "A search bar in omarchy-shell backed by a root index daemon."
)

# Sidebar groups: (heading, [(slug, source, nav title)])
NAV: list[tuple[str, list[tuple[str, str, str]]]] = [
    ("Getting started", [
        ("install", "docs/install.md", "Install"),
        ("troubleshooting", "docs/troubleshooting.md", "Troubleshooting"),
    ]),
    ("Using it", [
        ("search-syntax", "docs/search-syntax.md", "Search syntax"),
        ("omarchy-shell-integration", "docs/omarchy-shell-integration.md", "Shell integration"),
    ]),
    ("Under the hood", [
        ("daemon", "docs/daemon.md", "The daemon"),
        ("architecture", "docs/architecture.md", "Architecture"),
        ("decisions", "docs/decisions.md", "Decisions"),
        ("memory-optimization", "docs/memory-optimization.md", "Memory"),
        ("ffi-audit", "docs/ffi-audit.md", "FFI audit"),
        ("profiling", "docs/profiling.md", "Profiling"),
    ]),
    ("Contributing", [
        ("development", "docs/development.md", "Development"),
        ("e2e-checklist", "docs/e2e-checklist.md", "E2E checklist"),
    ]),
]

MD_EXTENSIONS = ["extra", "toc", "sane_lists", "codehilite", "admonition"]
MD_CONFIG = {"codehilite": {"css_class": "highlight", "guess_lang": False}}


def version() -> str:
    try:
        return json.loads((ROOT / "manifest.json").read_text())["version"]
    except Exception:
        return "dev"


def render_markdown(text: str) -> str:
    return markdown.markdown(text, extensions=MD_EXTENSIONS, extension_configs=MD_CONFIG)


def split_title(text: str, fallback: str) -> tuple[str, str]:
    """Pull the leading ``# H1`` out of a markdown document."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if line.startswith("# "):
            title = line[2:].strip()
            del lines[i]
            while i < len(lines) and not lines[i].strip():
                del lines[i]
            return title, "\n".join(lines)
    return fallback, text


def rewrite_links(fragment: str) -> str:
    """Point ``.md`` links at rendered pages, source-tree links at GitHub.

    Documentation links to repository files that are not published as part of the
    site (``../crates/...``, ``../.github/...`` and friends) are sent to GitHub so
    they resolve to the real source instead of 404ing.
    """

    def repl(m: re.Match[str]) -> str:
        href, frag = m.group(1), m.group(2) or ""
        if href.startswith(("http://", "https://", "mailto:", "#", "data:")):
            return m.group(0)
        if href.startswith("../") or href.startswith("/"):
            rel = href[3:] if href.startswith("../") else href[1:]
            if rel.endswith(".md"):
                return f'href="{REPO}/blob/main/{rel}{frag}"'
            kind = "tree" if rel.endswith("/") else "blob"
            return f'href="{REPO}/{kind}/main/{rel.rstrip("/")}{frag}"'
        if href.endswith(".md"):
            base = href[:-3]
            if base in ("README", "index"):
                base = "index"
            return f'href="{base}.html{frag}"'
        return m.group(0)

    return re.sub(r'href="([^"#]+)(#[^"]*)?"', repl, fragment)


def page(title: str, body: str, *, depth: int, desc: str = DEFAULT_DESC,
         sidebar: str = "", page_url: str = "", edit: str = "") -> str:
    root = "../" * depth
    full_title = title if title.startswith("omavrything") else f"{title} — omavrything"
    desc_e = html.escape(desc, quote=True)
    article = f'<article class="article">\n{body}\n{edit}\n</article>'
    inner = f'<div class="docs">\n{sidebar}\n{article}\n</div>' if sidebar else article
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark">
<title>{html.escape(full_title)}</title>
<meta name="description" content="{desc_e}">
<link rel="canonical" href="{SITE_URL}/{page_url}">
<meta property="og:type" content="website">
<meta property="og:title" content="{html.escape(title)}">
<meta property="og:description" content="{desc_e}">
<meta property="og:url" content="{SITE_URL}/{page_url}">
<meta name="theme-color" content="#0a0c10">
<link rel="stylesheet" href="{root}theme.css">
</head>
<body>
<header class="topbar">
  <div class="topbar-inner">
    <a class="brand" href="{root}index.html">omavrything<span class="dot">/</span></a>
    <nav class="topnav">
      <a href="{root}docs/index.html">Docs</a>
      <a href="{root}docs/install.html">Install</a>
      <a href="{REPO}">GitHub</a>
      <a class="cta" href="{REPO}/releases">v{version()}</a>
    </nav>
  </div>
</header>
<main class="wrap">
{inner}
</main>
<footer class="site">
  <div class="wrap cols">
    <span>MIT licensed</span>
    <a href="{REPO}">Source</a>
    <a href="{REPO}/releases">Releases</a>
    <a href="{root}docs/index.html">Docs</a>
    <span class="spacer">Omavrything — “Everything for Linux.”</span>
  </div>
</footer>
</body>
</html>
"""


def screenshot_block() -> str:
    """Render a screenshot figure if one has been dropped into assets/."""
    for name, cap in (
        ("screenshot.png", "The omavrything overlay, summoned over the desktop."),
        ("screenshot.webp", "The omavrything overlay, summoned over the desktop."),
        ("screenshot.jpg", "The omavrything overlay, summoned over the desktop."),
    ):
        if (ASSETS / name).exists():
            return (
                f'<figure><img src="assets/{name}" alt="The omavrything search overlay">'
                f'<figcaption>{cap}</figcaption></figure>'
            )
    return ""


def sidebar_html(active: str | None) -> str:
    out = ['<aside class="sidebar">']
    out.append(f'<div class="group"><h4>Overview</h4><ul>'
               f'<li><a href="index.html"'
               f'{" class=active" if active is None else ""}>All documentation</a></li></ul></div>')
    for heading, items in NAV:
        out.append(f'<div class="group"><h4>{heading}</h4><ul>')
        for slug, _src, name in items:
            cls = ' class="active"' if slug == active else ""
            out.append(f'<li><a href="{slug}.html"{cls}>{html.escape(name)}</a></li>')
        out.append("</ul></div>")
    out.append("</aside>")
    return "\n".join(out)


def build(out: Path) -> None:
    if out.exists():
        shutil.rmtree(out)
    (out / "docs").mkdir(parents=True)

    # Static assets
    shutil.copy(SITE / "theme.css", out / "theme.css")
    (out / ".nojekyll").write_text("")
    if (ASSETS / "file-managers.jpg").exists():
        (out / "assets").mkdir(exist_ok=True)
        shutil.copy(ASSETS / "file-managers.jpg", out / "assets" / "file-managers.jpg")
    for shot in ("screenshot.png", "screenshot.jpg", "screenshot.webp"):
        src = ASSETS / shot
        if src.exists():
            (out / "assets").mkdir(exist_ok=True)
            shutil.copy(src, out / "assets" / src.name)

    # Landing page
    landing_src = (SITE / "index.md").read_text()
    landing_body = rewrite_links(render_markdown(landing_src))
    landing_body = landing_body.replace("<!--screenshot-->", screenshot_block())
    (out / "index.html").write_text(page("omavrything — instant file search for Omarchy",
                                         landing_body, depth=0, page_url="index.html"))

    # Documentation index + pages
    flat = [item for _h, items in NAV for item in items]
    generated: dict[str, tuple[str, str]] = {}
    for slug, src, name in flat:
        text = (ROOT / src).read_text()
        title, body_md = split_title(text, name)
        generated[slug] = (title, rewrite_links(render_markdown(body_md)))

    for i, (slug, src, name) in enumerate(flat):
        title, body = generated[slug]
        pager = ['<nav class="pager">']
        if i > 0:
            prev = flat[i - 1]
            pager.append(f'<a class="prev" href="{prev[0]}.html">← {html.escape(generated[prev[0]][0])}</a>')
        if i < len(flat) - 1:
            nxt = flat[i + 1]
            pager.append(f'<a class="next" href="{nxt[0]}.html">{html.escape(generated[nxt[0]][0])} →</a>')
        pager.append("</nav>")
        edit = (f'<a class="edit" href="{REPO}/blob/main/{src}">'
                f'Edit this page on GitHub ↗</a>')
        heading = f'<h1>{html.escape(title)}</h1>'
        (out / "docs" / f"{slug}.html").write_text(
            page(title, heading + "\n" + body + "\n" + "\n".join(pager), depth=1,
                 sidebar=sidebar_html(slug), page_url=f"docs/{slug}.html", edit=edit)
        )

    # Docs landing
    cards = []
    for heading, items in NAV:
        lis = "".join(
            f'<li><a href="{slug}.html">{html.escape(generated[slug][0])}</a></li>'
            for slug, _s, _n in items
        )
        cards.append(f'<div class="card"><h3>{html.escape(heading)}</h3><ul>{lis}</ul></div>')
    docs_index = (
        "<h1>Documentation</h1>\n"
        "<p>Everything about installing, using, and hacking on omavrything.</p>\n"
        f'<div class="grid">{"".join(cards)}</div>\n'
        f'<p>New here? Start with <a href="install.html">Install</a>, then '
        f'<a href="search-syntax.html">Search syntax</a>. The release notes live in '
        f'<a href="{REPO}/blob/main/CHANGELOG.md">CHANGELOG.md</a>.</p>'
    )
    (out / "docs" / "index.html").write_text(
        page("Documentation", docs_index, depth=1, sidebar=sidebar_html(None),
             page_url="docs/index.html")
    )

    print(f"built {out}")
    for p in sorted(out.rglob("*.html")):
        print(f"  {p.relative_to(out)}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=SITE / "dist")
    build(ap.parse_args().out.resolve())


if __name__ == "__main__":
    main()
