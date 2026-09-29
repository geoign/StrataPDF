"""Review packets: page images next to the reflowed text of the same pages.

    python review_packets.py RUN_DIR OUT_DIR [--ids ID ...] [--pages-per-doc 3] [--scale 1.4]

For each audited document, picks the first page, a page in the middle, and the
page where the reference list starts (or the last page), plus the pages named in
the audit's examples (lost text, cut paragraphs, leaks), and writes
OUT_DIR/<id>/p<N>.png and OUT_DIR/<id>/reflow.txt (the reflowed nodes of the
picked pages; the node on a page is the one that starts there).
"""

import argparse
import json
import re
from pathlib import Path

import pymupdf


def pages_of_dump(dump):
    pages = {}
    cur = None
    for line in dump.splitlines():
        m = re.match(r"=== p(\d+)", line)
        if m:
            cur = int(m.group(1))
            pages.setdefault(cur, [])
        elif cur is not None:
            pages[cur].append(line)
    return pages


def pick_pages(r, dump_pages, per_doc):
    n = r.get("pages") or 1
    picks = [1]
    ref = None
    for p, lines in sorted(dump_pages.items()):
        if any(re.match(r"H\d\t.*(references|bibliography|literature cited|参考文献|引用文献)\s*$", l, re.I) for l in lines):
            ref = p
            break
    flagged = []
    for k in ("lost_examples", "cut_examples", "leak_examples"):
        for e in r.get(k) or []:
            m = re.match(r"p(\d+)", e)
            if m:
                flagged.append(int(m.group(1)))
    for p in flagged:
        if p not in picks:
            picks.append(p)
            break
    mid = max(1, n // 2)
    for p in (mid, ref or n):
        if p not in picks and len(picks) < per_doc:
            picks.append(p)
    return sorted(set(p for p in picks if 1 <= p <= n))[: per_doc + 1]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("run")
    ap.add_argument("out")
    ap.add_argument("--ids", nargs="*")
    ap.add_argument("--pages-per-doc", type=int, default=3)
    ap.add_argument("--scale", type=float, default=1.4)
    a = ap.parse_args()
    run = Path(a.run)
    out = Path(a.out)
    results = [json.loads(l) for l in (run / "results.jsonl").read_text(encoding="utf-8").splitlines() if l.strip()]
    for r in results:
        if a.ids and r["id"] not in a.ids:
            continue
        if r.get("error"):
            continue
        dump_path = run / "dumps" / f"{r['id']}.txt"
        if not dump_path.exists():
            continue
        dump_pages = pages_of_dump(dump_path.read_text(encoding="utf-8"))
        picks = pick_pages(r, dump_pages, a.pages_per_doc)
        d = out / r["id"]
        d.mkdir(parents=True, exist_ok=True)
        doc = pymupdf.open(r["path"])
        text = [f"# {Path(r['path']).name}", f"# pages {r['pages']}, picked {picks}", ""]
        for p in picks:
            page = doc[p - 1]
            # Keep images near 1100 px on the long side.
            s = min(a.scale, 1100 / max(page.rect.width, page.rect.height))
            page.get_pixmap(matrix=pymupdf.Matrix(s, s)).save(d / f"p{p}.png")
            text.append(f"=== p{p}")
            text.extend(dump_pages.get(p, ["(no nodes start on this page)"]))
            text.append("")
        (d / "reflow.txt").write_text("\n".join(text), encoding="utf-8")
        print(d, picks)


if __name__ == "__main__":
    main()
