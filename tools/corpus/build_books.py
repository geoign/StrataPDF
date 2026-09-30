#!/usr/bin/env python3
"""Index a library of books (PDF) for reflow and OCR evaluation.

    python build_books.py --root <label>=<dir> [--root ...] --out <dir>

Each PDF is sampled on up to ten pages spread over the book (PyMuPDF only) and
gets: id (sha1 of label/relative path, 10 hex digits), path, root, rel, size,
pages, producer, creator, page_size (median, pt), and over the sampled pages:
chars_p (text characters per page), image_cover_p (share of the page covered
by images), cjk_frac (CJK share of the text), vertical_frac (share of CJK text
lines written vertically), kind ("digital", "scan_ocr" = page images with a
text layer, "scan" = page images without text), lang ("ja", "en", "other",
"none").

Outputs: books.jsonl and summary.md in --out.
"""
import argparse
import collections
import hashlib
import json
import multiprocessing as mp
import os
import statistics

import pymupdf

pymupdf.TOOLS.mupdf_display_errors(False)


def is_cjk(c):
    o = ord(c)
    return 0x3040 <= o <= 0x30FF or 0x3400 <= o <= 0x9FFF or 0xF900 <= o <= 0xFAFF


def survey(task):
    label, root, rel = task
    path = os.path.join(root, rel)
    rec = {
        "id": hashlib.sha1(f"{label}/{rel}".encode("utf-8")).hexdigest()[:10],
        "path": path,
        "root": label,
        "rel": rel,
        "size": os.path.getsize(path),
    }
    try:
        doc = pymupdf.open(path)
    except Exception as e:
        rec["error"] = str(e)[:200]
        return rec
    n = doc.page_count
    rec["pages"] = n
    rec["producer"] = (doc.metadata or {}).get("producer", "")
    rec["creator"] = (doc.metadata or {}).get("creator", "")
    if n == 0:
        rec["error"] = "no pages"
        return rec
    picks = sorted({min(n - 1, round(i * (n - 1) / 9)) for i in range(10)}) if n > 10 else list(range(n))
    chars, covers, cjk, alltext, vlines, cjklines, sizes = [], [], 0, 0, 0, 0, []
    for i in picks:
        try:
            pg = doc[i]
            area = max(1.0, pg.rect.width * pg.rect.height)
            sizes.append((round(pg.rect.width), round(pg.rect.height)))
            cover = sum(abs((r["bbox"][2] - r["bbox"][0]) * (r["bbox"][3] - r["bbox"][1])) for r in pg.get_image_info())
            covers.append(min(1.0, cover / area))
            d = pg.get_text("dict")
        except Exception:
            continue
        nch = 0
        for b in d.get("blocks", []):
            for l in b.get("lines", []):
                t = "".join(s["text"] for s in l["spans"])
                k = sum(1 for c in t if is_cjk(c))
                nch += sum(1 for c in t if not c.isspace())
                cjk += k
                alltext += sum(1 for c in t if not c.isspace())
                if k >= 2:
                    cjklines += 1
                    if l.get("wmode") == 1 or abs(l["dir"][1]) > 0.7:
                        vlines += 1
        chars.append(nch)
    if not chars:
        rec["error"] = "unreadable"
        return rec
    rec["page_size"] = list(statistics.median_low(sizes)) if sizes else None
    rec["chars_p"] = round(statistics.mean(chars))
    rec["image_cover_p"] = round(statistics.mean(covers), 2)
    rec["cjk_frac"] = round(cjk / alltext, 2) if alltext else 0.0
    rec["vertical_frac"] = round(vlines / cjklines, 2) if cjklines >= 20 else None
    scanned = rec["image_cover_p"] > 0.7
    rec["kind"] = ("scan_ocr" if rec["chars_p"] >= 100 else "scan") if scanned else "digital"
    if alltext < 300:
        rec["lang"] = "none"
    elif rec["cjk_frac"] > 0.3:
        rec["lang"] = "ja"
    else:
        rec["lang"] = "en"
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", action="append", required=True, help="label=dir")
    ap.add_argument("--out", required=True)
    ap.add_argument("--jobs", type=int, default=6)
    a = ap.parse_args()
    tasks = []
    for spec in a.root:
        label, root = spec.split("=", 1)
        for dp, _, fs in os.walk(root):
            for f in fs:
                if f.lower().endswith(".pdf"):
                    tasks.append((label, root, os.path.relpath(os.path.join(dp, f), root)))
    os.makedirs(a.out, exist_ok=True)
    recs = []
    with mp.Pool(a.jobs) as pool, open(os.path.join(a.out, "books.jsonl"), "w", encoding="utf-8") as fo:
        for r in pool.imap_unordered(survey, tasks, chunksize=1):
            recs.append(r)
            fo.write(json.dumps(r, ensure_ascii=False) + "\n")
    lines = ["# Books", ""]
    for label in sorted({r["root"] for r in recs}):
        rs = [r for r in recs if r["root"] == label]
        lines.append(f"## {label}: {len(rs)} PDFs, {sum(r.get('pages', 0) for r in rs)} pages")
        lines.append("kind: " + ", ".join(f"{k} {v}" for k, v in collections.Counter(r.get("kind", "error") for r in rs).most_common()))
        lines.append("lang: " + ", ".join(f"{k} {v}" for k, v in collections.Counter(r.get("lang", "error") for r in rs).most_common()))
        vert = [r for r in rs if (r.get("vertical_frac") or 0) > 0.5]
        lines.append(f"vertical: {len(vert)}")
        lines.append("")
    open(os.path.join(a.out, "summary.md"), "w", encoding="utf-8").write("\n".join(lines))
    print("\n".join(lines))


if __name__ == "__main__":
    main()
