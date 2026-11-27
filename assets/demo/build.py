#!/usr/bin/env python3
"""Builds the demo PDFs in this folder from the HTML sources in `src/`.

    python3 assets/demo/build.py

Needs `google-chrome` (or `chromium`, or the program in $CHROME) and `pdftotext` (poppler-utils).
Chrome runs headless with a throw-away profile; no window opens.

Per document:
1. Keep the parts of the source that belong to the version (`<!--v1.0-->...<!--/v1.0-->`).
2. Print it with Chrome, which writes the outline (bookmarks) and the internal links.
3. Find the page of every heading with `pdftotext` and print again with the table of contents
   filled in and the body pages numbered from 1.
4. Append an incremental update that adds page labels (i, ii, ... for the front matter, 1, 2, ...
   for the body) and replaces the document information with fixed values, so that no tool
   version, path or time of the build ends up in the file.
"""

from __future__ import annotations

import html
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zlib
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE / "src"


@dataclass
class Doc:
    source: str
    output: str
    version: str
    date: str  # ISO date, also the creation date written into the PDF
    doc_id: str
    title: str  # running header


DOCS = [
    Doc("process-manager.html", "process-manager-v1.0.pdf", "1.0", "2026-03-02",
        "EP-SWS-PM", "Example Platform — Process Manager Specification"),
    Doc("process-manager.html", "process-manager-v1.1.pdf", "1.1", "2026-09-14",
        "EP-SWS-PM", "Example Platform — Process Manager Specification"),
    Doc("watchdog-interface.html", "watchdog-interface-v1.0.pdf", "1.0", "2026-01-15",
        "EP-SWS-WDG", "Example Platform — Watchdog Interface Specification"),
]

VERSIONS = ("1.0", "1.1")

# The top page margin of the body pages in style.css (22 mm), in points.
TOP_MARGIN_PT = 22 / 25.4 * 72
# Then a little further, so that a link to a heading lands just below the outline entry of the
# heading and viewers name the heading's own section, not the one before it.
TARGET_DROP_PT = 4.0

HEADING = re.compile(r'<h([123]) id="([^"]+)">(.*?)</h\1>', re.S)
TAG = re.compile(r"<[^>]+>")


def find_chrome() -> str:
    for name in (os.environ.get("CHROME"), "google-chrome", "chromium", "chromium-browser"):
        if name and shutil.which(name):
            return name
    sys.exit("build.py: no Chrome found; set $CHROME")


def select_version(text: str, version: str) -> str:
    for v in VERSIONS:
        block = re.compile(r"<!--v%s-->(.*?)<!--/v%s-->" % (re.escape(v), re.escape(v)), re.S)
        text = block.sub(lambda m: m.group(1) if v == version else "", text)
    if "<!--v" in text or "<!--/v" in text:
        sys.exit("build.py: unbalanced version marker")
    return text


def css_string(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def page_css(doc: Doc, first_body_page: int | None) -> str:
    """Running header and footer text, and the counter style that numbers body pages from 1."""
    if first_body_page is None:
        counter = "@counter-style body-num { system: extends decimal; }"
    else:
        symbols = " ".join('"%d"' % i for i in range(1, 200))
        counter = "@counter-style body-num { system: fixed %d; symbols: %s; }" % (
            first_body_page, symbols)
    return "\n".join([
        counter,
        "@page main {",
        "  @top-left { content: %s; }" % css_string(doc.title),
        "  @top-right { content: %s; }" % css_string("Version " + doc.version),
        "  @bottom-left { content: %s; }" % css_string(doc.doc_id),
        "}",
    ])


def headings(text: str) -> list[tuple[int, str, str]]:
    out = []
    for level, ident, inner in HEADING.findall(text):
        label = html.unescape(TAG.sub("", inner)).strip()
        out.append((int(level), ident, label))
    return out


def toc_html(items: list[tuple[int, str, str]], pages: dict[str, str]) -> str:
    rows = ['<div class="toc">']
    for level, ident, label in items:
        rows.append(
            '<a class="l%d" href="#%s"><span class="t">%s</span><span class="dots"></span>'
            '<span class="p">%s</span></a>' % (level, ident, html.escape(label),
                                               pages.get(ident, "00")))
    rows.append("</div>")
    return "\n".join(rows)


def render(chrome: str, work: Path, html_text: str, out: Path) -> None:
    page = work / "doc.html"
    page.write_text(html_text, encoding="utf-8")
    env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
    cmd = [
        chrome, "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
        "--disable-extensions", "--disable-sync", "--user-data-dir=%s" % (work / "profile"),
        "--no-pdf-header-footer", "--generate-pdf-document-outline",
        "--print-to-pdf=%s" % out, page.as_uri(),
    ]
    subprocess.run(cmd, check=True, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                   timeout=180)
    if not out.exists():
        sys.exit("build.py: Chrome wrote no PDF")


def page_texts(pdf: Path) -> list[str]:
    text = subprocess.run(["pdftotext", "-enc", "UTF-8", str(pdf), "-"], check=True,
                          capture_output=True).stdout.decode("utf-8")
    pages = text.split("\f")
    if pages and not pages[-1].strip():
        pages.pop()
    return pages


def locate(pdf: Path, items: list[tuple[int, str, str]]) -> tuple[int, dict[str, int]]:
    """Number of pages before the body, and the physical page (1-based) of each heading."""
    pages = page_texts(pdf)
    front = 0
    for i, t in enumerate(pages):
        if re.search(r"^Page \d+$", t, re.M):
            front = i
            break
    else:
        sys.exit("build.py: no body page found")
    found: dict[str, int] = {}
    cursor = front
    for _, ident, label in items:
        want = " ".join(label.split())
        for i in range(cursor, len(pages)):
            lines = {" ".join(line.split()) for line in pages[i].splitlines()}
            if want in lines:
                found[ident] = i + 1
                cursor = i
                break
        else:
            sys.exit("build.py: heading %r not found in %s" % (label, pdf.name))
    return front, found


# --- PDF post-processing ---------------------------------------------------------------------


def pdf_string(s: str) -> bytes:
    """A PDF text string: UTF-16BE with BOM, hex-encoded (safe for any character)."""
    return b"<FEFF" + s.encode("utf-16-be").hex().upper().encode() + b">"


def find_object(data: bytes, num: int) -> bytes:
    m = re.search(rb"(?<![0-9])%d 0 obj\s*(.*?)\s*endobj" % num, data, re.S)
    if not m:
        sys.exit("build.py: object %d not found" % num)
    return m.group(1)


def finish(pdf: Path, doc: Doc, front: int) -> None:
    data = pdf.read_bytes()
    tail = data[-1024:]
    trailer = re.search(rb"trailer\s*<<(.*?)>>\s*startxref\s*(\d+)\s*%%EOF\s*$", tail, re.S)
    if not trailer:
        sys.exit("build.py: unexpected trailer (Chrome changed its PDF writer?)")
    tdict, prev = trailer.group(1), int(trailer.group(2))
    size = int(re.search(rb"/Size (\d+)", tdict).group(1))
    root = int(re.search(rb"/Root (\d+) 0 R", tdict).group(1))
    info = int(re.search(rb"/Info (\d+) 0 R", tdict).group(1))

    catalog = find_object(data, root).strip()
    if not catalog.startswith(b"<<") or b"/PageLabels" in catalog:
        sys.exit("build.py: unexpected catalog")
    labels = b"/PageLabels <</Nums [0 <</S /r>> %d <</S /D>>]>>" % front
    catalog = catalog[:-2] + b"\n" + labels + b"\n/PageMode /UseOutlines>>"

    stamp = doc.date.replace("-", "")
    info_dict = b"".join([
        b"<</Title ", pdf_string("%s, version %s" % (doc.title, doc.version)),
        b"\n/Subject ", pdf_string("Fictional specification for the refdesk demo"),
        b"\n/Creator ", pdf_string("refdesk assets/demo/build.py"),
        b"\n/Producer ", pdf_string("Chrome headless print"),
        b"\n/CreationDate (D:%s000000Z)\n/ModDate (D:%s000000Z)>>" % (stamp.encode(),
                                                                       stamp.encode()),
    ])

    # The superseded information dictionary stays in the file. Give its dates the same fixed
    # value (same length, so no offset moves) to make the output byte-for-byte reproducible.
    fixed = b"(D:%s000000+00'00')" % stamp.encode()
    data = re.sub(rb"\(D:\d{14}[+-]\d\d'\d\d'\)", lambda m: fixed if len(m.group(0)) == len(fixed)
                  else m.group(0), data)

    # Chrome places named destinations (the targets of `href="#id"`) one top page margin too
    # high: a link to a heading lands on the end of the section before it. Move them down.
    replace = [(info, info_dict), (root, catalog)]
    dests_ref = re.search(rb"/Dests (\d+) 0 R", catalog)
    if dests_ref:
        num = int(dests_ref.group(1))

        def lower(m: re.Match[bytes]) -> bytes:
            y = float(m.group(2)) - TOP_MARGIN_PT - TARGET_DROP_PT
            return m.group(1) + b"%.3f" % y + m.group(3)

        dests = re.sub(rb"(\[\d+ 0 R /XYZ [-\d.]+ )([-\d.]+)( [-\d.]+\])", lower,
                       find_object(data, num))
        replace.append((num, dests))

    out = bytearray(data)
    if not out.endswith(b"\n"):
        out += b"\n"
    offsets = {}
    for num, body in replace:
        offsets[num] = len(out)
        out += b"%d 0 obj\n" % num + body + b"\nendobj\n"
    xref_at = len(out)
    out += b"xref\n"
    for num in sorted(offsets):
        out += b"%d 1\n%010d 00000 n \n" % (num, offsets[num])
    out += b"trailer\n<</Size %d\n/Root %d 0 R\n/Info %d 0 R\n/Prev %d>>\n" % (size, root, info,
                                                                                 prev)
    out += b"startxref\n%d\n%%%%EOF\n" % xref_at
    pdf.write_bytes(bytes(out))


def check_private(pdf: Path, forbidden: list[str]) -> None:
    """Fails if a forbidden string (home folder, user name, build folder) is in the file, also
    inside compressed streams."""
    data = pdf.read_bytes()
    chunks = [data]
    for m in re.finditer(rb"stream\r?\n(.*?)\r?\nendstream", data, re.S):
        try:
            chunks.append(zlib.decompress(m.group(1)))
        except zlib.error:
            pass
    for chunk in chunks:
        for word in forbidden:
            for enc in (word.encode(), word.encode("utf-16-be")):
                if enc and enc in chunk:
                    sys.exit("build.py: %s contains %r" % (pdf.name, word))


def build(chrome: str, doc: Doc, work: Path, forbidden: list[str]) -> None:
    css = (SRC / "style.css").read_text(encoding="utf-8")
    source = (SRC / doc.source).read_text(encoding="utf-8")
    text = select_version(source, doc.version)
    text = text.replace("{{VERSION}}", doc.version).replace("{{DATE}}", doc.date)
    items = headings(text)
    if not items:
        sys.exit("build.py: no headings in %s" % doc.source)

    def html_for(front: int | None, pages: dict[str, str]) -> str:
        style = "<style>\n%s\n%s\n</style>" % (css, page_css(doc, front))
        return text.replace("<!--STYLE-->", style).replace("<!--TOC-->", toc_html(items, pages))

    out = HERE / doc.output
    tmp = work / "pass.pdf"
    render(chrome, work, html_for(None, {}), tmp)
    front, physical = locate(tmp, items)
    for _ in range(3):
        labels = {k: str(p - front) for k, p in physical.items()}
        render(chrome, work, html_for(front + 1, labels), tmp)
        front2, physical2 = locate(tmp, items)
        if (front2, physical2) == (front, physical):
            break
        front, physical = front2, physical2
    else:
        sys.exit("build.py: page numbers do not settle for %s" % doc.output)
    finish(tmp, doc, front)
    check_private(tmp, forbidden)
    shutil.move(tmp, out)
    pages = len(page_texts(out))
    print("%-30s %3d pages (%d front)  %6.0f KiB" % (doc.output, pages, front,
                                                     out.stat().st_size / 1024))


def main() -> None:
    chrome = find_chrome()
    for tool in ("pdftotext",):
        if not shutil.which(tool):
            sys.exit("build.py: %s not found (poppler-utils)" % tool)
    with tempfile.TemporaryDirectory(prefix="refdesk-demo-build-") as work_dir:
        work = Path(work_dir)
        forbidden = [str(Path.home()), work_dir, str(HERE)]
        user = os.environ.get("USER") or os.environ.get("LOGNAME")
        if user and len(user) >= 3:
            forbidden.append(user)
        for doc in DOCS:
            build(chrome, doc, work, forbidden)


if __name__ == "__main__":
    main()
