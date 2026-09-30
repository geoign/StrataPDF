#!/usr/bin/env python3
"""Cut evaluation samples out of books: the front matter, a stretch of the body and
the end of each book, as separate PDFs (text layers and images kept).

    python book_samples.py --books books.jsonl --ids ids.txt --out samples/
        [--front 10] [--body 12] [--at 0.4] [--end 6] [--copies 1]

Writes <out>/<id>_front.pdf, <id>_body.pdf, <id>_end.pdf and samples.jsonl (id,
chunk, source pages). With --copies 2 every sample gets a twin <name>_ocr.pdf,
so that one copy can be OCR'd (the OCR cache is keyed by path) and the other
keeps its own text layer.
"""
import argparse
import json
import os
import shutil

import pymupdf


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--books", required=True)
    ap.add_argument("--ids", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--front", type=int, default=10)
    ap.add_argument("--body", type=int, default=12)
    ap.add_argument("--at", type=float, default=0.4)
    ap.add_argument("--end", type=int, default=6)
    ap.add_argument("--copies", type=int, default=1)
    a = ap.parse_args()
    books = {json.loads(l)["id"]: json.loads(l) for l in open(a.books, encoding="utf-8")}
    ids = [l.split()[0] for l in open(a.ids, encoding="utf-8") if l.strip() and not l.startswith("#")]
    os.makedirs(a.out, exist_ok=True)
    with open(os.path.join(a.out, "samples.jsonl"), "a", encoding="utf-8") as log:
        for i in ids:
            b = books[i]
            doc = pymupdf.open(b["path"])
            n = doc.page_count
            start = min(max(a.front, int(n * a.at)), max(0, n - a.body))
            chunks = {
                "front": (0, min(n, a.front) - 1),
                "body": (start, min(n, start + a.body) - 1),
                "end": (max(0, n - a.end), n - 1),
            }
            for name, (p0, p1) in chunks.items():
                if p1 < p0:
                    continue
                out = pymupdf.open()
                out.insert_pdf(doc, from_page=p0, to_page=p1)
                dest = os.path.join(a.out, f"{i}_{name}.pdf")
                out.save(dest, garbage=3, deflate=True)
                if a.copies > 1:
                    shutil.copyfile(dest, dest[:-4] + "_ocr.pdf")
                log.write(json.dumps({"id": i, "chunk": name, "pages": [p0 + 1, p1 + 1], "root": b["root"], "rel": b["rel"]}, ensure_ascii=False) + "\n")
            print(i, n, b["rel"][:60])


if __name__ == "__main__":
    main()
