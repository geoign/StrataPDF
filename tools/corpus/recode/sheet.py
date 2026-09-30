"""Contact sheets of (family, ucs) glyphs: outline raster + page crops.

python sheet.py chars2.jsonl mine_all.pkl OUTDIR FAMILY[,FAMILY...] [--sim 0.5] [--per 3] [--min-docs 2] [--all]
"""
import json, re, sys, pickle, collections, os, argparse
import numpy as np
import pymupdf
from PIL import Image, ImageDraw, ImageFont
sys.path.insert(0, os.path.dirname(__file__))
from mine import raster, fam

ap = argparse.ArgumentParser()
ap.add_argument("chars"); ap.add_argument("mine"); ap.add_argument("out"); ap.add_argument("families")
ap.add_argument("--sim", type=float, default=0.5); ap.add_argument("--per", type=int, default=3)
ap.add_argument("--min-docs", type=int, default=2); ap.add_argument("--all", action="store_true")
ap.add_argument("--max-rows", type=int, default=16); ap.add_argument("--skip", type=int, default=0); ap.add_argument("--skip-ledger", action="store_true"); ap.add_argument("--ctx", type=float, default=2.2)
a = ap.parse_args()
fams = set(a.families.split(","))
res = pickle.load(open(a.mine, "rb"))
keys = {k for k, v in res.items() if k[0] in fams and (a.all or v["sim"] < a.sim) and len(v["docs"]) >= a.min_docs}
inst = collections.defaultdict(list)
for l in open(a.chars, encoding="utf-8"):
    if not any(f in l for f in fams):
        continue
    try:
        r = json.loads(l)
    except Exception:
        continue
    k = (fam(r["font"]), r["ucs"], r["ohash"])
    if k in keys and r.get("path") and len({x["id"] for x in inst[k]}) < a.per + 2:
        if r["id"] not in {x["id"] for x in inst[k]}:
            inst[k].append(r)
paths = {}
for l in open("C:/tmp/strata_corpus/corpus.jsonl", encoding="utf-8"):
    r = json.loads(l); paths[r["id"]] = r["path"]
font = ImageFont.load_default()
big = ImageFont.truetype("C:/Windows/Fonts/arial.ttf", 22)
os.makedirs(a.out, exist_ok=True)
docs = {}


def crop(r, zoom_px=44):
    d = r["id"]
    if d not in docs:
        docs[d] = pymupdf.open(paths[d])
    pg = docs[d][r["page"]]
    x, y, s = r["x"], r["y"], max(r["size"], 3)
    bb = r["bb"]
    x0, x1 = x + bb[0] * s, x + bb[2] * s
    rect = pymupdf.Rect(x0 - s * a.ctx, y - bb[3] * s - s * 0.5, x1 + s * a.ctx, y - bb[1] * s + s * 0.5)
    z = min(10, zoom_px / s)
    pix = pg.get_pixmap(matrix=pymupdf.Matrix(z, z), clip=rect, alpha=False)
    img = Image.frombytes("RGB", (pix.width, pix.height), pix.samples)
    dr = ImageDraw.Draw(img)
    gx0, gx1 = (x0 - rect.x0) * z, (x1 - rect.x0) * z
    dr.line([(gx0, img.height - 2), (gx1, img.height - 2)], fill=(255, 0, 0), width=2)
    return img


for f in sorted(fams):
    ks = sorted([k for k in keys if k[0] == f], key=lambda k: -len(res[k]["docs"]))
    if a.skip_ledger:
        from ledger import L as _L
        ks = [k for k in ks if chr(k[1]) not in _L.get(f, {})]
    ks = ks[a.skip: a.skip + a.max_rows]
    if not ks:
        continue
    rows = []
    for k in ks:
        v = res[k]
        ims = []
        for r in inst[k][: a.per]:
            try:
                ims.append(crop(r))
            except Exception as e:
                pass
        m = raster(inst[k][0]["path"], inst[k][0]["bb"]) if inst[k] else None
        gi = None
        if m is not None:
            gi = Image.fromarray((~m * 255).astype(np.uint8)).convert("RGB")
            sc = 80 / max(gi.height, gi.width)
            gi = gi.resize((max(1, int(gi.width * sc)), max(1, int(gi.height * sc))))
        rows.append((k, v, gi, ims))
    H = sum(max([90] + [i.height for i in ims]) + 6 for _, _, _, ims in rows)
    W = 210 + 110 + sum(1 for _ in range(a.per)) * 320
    sheet = Image.new("RGB", (W, H), "white")
    dr = ImageDraw.Draw(sheet)
    y = 0
    for k, v, gi, ims in rows:
        h = max([90] + [i.height for i in ims]) + 6
        ch = chr(k[1])
        dr.text((4, y + 4), f"{ch}  U+{k[1]:04X}", fill=(0, 0, 200), font=big)
        nm = list(v["names"])[0]
        dr.text((4, y + 34), f"name {nm}", fill=(60, 60, 60), font=font)
        dr.text((4, y + 46), f"docs {len(v['docs'])} n {v['n']}", fill=(60, 60, 60), font=font)
        dr.text((4, y + 58), f"sim {v['sim']}", fill=(60, 60, 60), font=font)
        if gi:
            sheet.paste(gi, (150, y + 4))
        x = 260
        for im in ims:
            if x + im.width > W:
                break
            sheet.paste(im, (x, y + 4)); x += im.width + 8
        dr.line([(0, y + h - 1), (W, y + h - 1)], fill=(200, 200, 200))
        y += h
    fn = os.path.join(a.out, re.sub(r"[^A-Za-z0-9_.-]", "_", f) + ".png")
    sheet.save(fn)
    print(fn, sheet.size, len(rows))
