"""Find (family, ucs) whose glyph outline does not look like the character ucs.

python mine.py chars2.jsonl OUT.pkl
Rasterises each distinct glyph outline (family, ucs, outline hash) and compares it with the
character `ucs` in a set of reference fonts; low similarity = the PDF decodes the glyph as another
character. Output: a pickle with the triples, their documents, best instance and the similarity.
"""
import json, re, sys, collections, pickle
import numpy as np
from PIL import Image, ImageDraw, ImageFont
from fontTools.ttLib import TTFont

F = "C:/Windows/Fonts/"
REF = [
    ("georgia.ttf", 0), ("georgiai.ttf", 0), ("georgiab.ttf", 0), ("pala.ttf", 0), ("palai.ttf", 0),
    ("arial.ttf", 0), ("arialbd.ttf", 0), ("ariali.ttf", 0), ("calibri.ttf", 0), ("calibrii.ttf", 0),
    ("segoeui.ttf", 0), ("cambria.ttc", 1), ("cambriai.ttf", 0), ("seguisym.ttf", 0), ("consola.ttf", 0),
    ("cour.ttf", 0), ("LSANS.TTF", 0),
]
SZ = 200  # reference glyph size in px
G = 40    # comparison grid

_ref_fonts = []
for name, idx in REF:
    try:
        tt = TTFont(F + name, fontNumber=idx) if name.endswith("ttc") else TTFont(F + name)
        cmap = tt.getBestCmap()
        pf = ImageFont.truetype(F + name, SZ, index=idx) if name.endswith("ttc") else ImageFont.truetype(F + name, SZ)
        _ref_fonts.append((name, cmap, pf))
    except Exception as e:
        print("skip", name, e, file=sys.stderr)


def norm(mask):
    """Crop to the ink, return (G x G binary array, aspect w/h) or None."""
    ys, xs = np.nonzero(mask)
    if len(xs) < 6:
        return None
    x0, x1, y0, y1 = xs.min(), xs.max() + 1, ys.min(), ys.max() + 1
    crop = mask[y0:y1, x0:x1]
    im = Image.fromarray((crop * 255).astype(np.uint8)).resize((G, G), Image.BILINEAR)
    return np.asarray(im) > 100, (x1 - x0) / max(1, (y1 - y0)), (x1 - x0, y1 - y0)


_refcache = {}


def ref_feats(ucs):
    if ucs in _refcache:
        return _refcache[ucs]
    out = []
    for name, cmap, pf in _ref_fonts:
        for cp in (ucs, ucs + 0xF000 if ucs < 0x100 and name == "seguisym.ttf" else None):
            if cp is None or cp not in cmap:
                continue
            im = Image.new("L", (SZ * 2, SZ * 2), 0)
            ImageDraw.Draw(im).text((SZ // 2, SZ // 4), chr(cp), font=pf, fill=255)
            r = norm(np.asarray(im) > 128)
            if r:
                out.append((name, r))
            break
    _refcache[ucs] = out
    return out


def raster(path, bb):
    """Rasterise an outline (ops in 1/1000 em, y up) at 0.2 px/unit; even-odd via XOR of contours."""
    toks = re.findall(r"[MLCZ]|-?\d+", path)
    contours = []
    cur = []
    i = 0
    last = (0, 0)
    while i < len(toks):
        t = toks[i]
        i += 1
        if t == "M":
            if cur:
                contours.append(cur)
            x, y = int(toks[i]), int(toks[i + 1])
            i += 2
            cur = [(x, y)]
            last = (x, y)
        elif t == "L":
            x, y = int(toks[i]), int(toks[i + 1])
            i += 2
            cur.append((x, y))
            last = (x, y)
        elif t == "C":
            x1, y1, x2, y2, x, y = (int(v) for v in toks[i:i + 6])
            i += 6
            x0, y0 = last
            for k in range(1, 9):
                u = k / 8
                a, b, c, d = (1 - u) ** 3, 3 * u * (1 - u) ** 2, 3 * u * u * (1 - u), u ** 3
                cur.append((a * x0 + b * x1 + c * x2 + d * x, a * y0 + b * y1 + c * y2 + d * y))
            last = (x, y)
        elif t == "Z":
            if cur:
                contours.append(cur)
            cur = []
    if cur:
        contours.append(cur)
    if not contours:
        return None
    xs = [p[0] for c in contours for p in c]
    ys = [p[1] for c in contours for p in c]
    x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)
    s = 0.2
    w, h = int((x1 - x0) * s) + 4, int((y1 - y0) * s) + 4
    if w > 4000 or h > 4000 or w < 3 or h < 3:
        return None
    mask = np.zeros((h, w), dtype=bool)
    for c in contours:
        if len(c) < 3:
            continue
        im = Image.new("1", (w, h), 0)
        ImageDraw.Draw(im).polygon([((px - x0) * s + 2, (y1 - py) * s + 2) for px, py in c], fill=1)
        mask ^= np.asarray(im)
    return mask


def sim(a, b):
    (ma, ara, _), (mb, arb, _) = a, b
    inter = np.logical_and(ma, mb).sum()
    uni = np.logical_or(ma, mb).sum()
    iou = inter / uni if uni else 0
    return iou * min(ara / arb, arb / ara) ** 0.5


def fam(f):
    f = re.sub(r"^[A-Z]{6}\+", "", f)
    i = f.rfind("Adv")
    if i > 0 and f[:i].isalpha():
        f = f[i:]
    return f


def main():
    trip = {}
    for l in open(sys.argv[1], encoding="utf-8"):
        try:
            r = json.loads(l)
        except Exception:
            continue
        k = (fam(r["font"]), r["ucs"], r["ohash"])
        t = trip.get(k)
        if t is None:
            t = trip[k] = {"docs": set(), "n": 0, "inst": None, "names": collections.Counter()}
        t["docs"].add(r["id"])
        t["n"] += r["n"]
        t["names"][r["name"]] += 1
        if t["inst"] is None and r.get("path"):
            t["inst"] = r
    print("triples", len(trip), file=sys.stderr)
    res = {}
    import zlib
    shard, nshard = (int(sys.argv[3]), int(sys.argv[4])) if len(sys.argv) > 4 else (0, 1)
    for j, (k, t) in enumerate(trip.items()):
        if zlib.crc32(repr(k).encode()) % nshard != shard:
            continue
        fm, ucs, oh = k
        inst = t["inst"]
        if inst is None or ucs <= 32 or ucs == 0xFFFD:
            continue
        refs = ref_feats(ucs)
        if not refs:
            continue
        m = raster(inst["path"], inst["bb"])
        if m is None:
            continue
        me = norm(m)
        if me is None:
            continue
        best = max((sim(me, rf), nm) for nm, rf in refs)
        res[k] = {"docs": sorted(t["docs"]), "n": t["n"], "sim": round(float(best[0]), 3), "ref": best[1], "names": dict(t["names"]),
                  "aspect": round(float(me[1]), 2), "inst": {kk: inst[kk] for kk in ("id", "page", "x", "y", "size", "adv", "bb", "gid", "font", "nglyphs")}}
        if j % 5000 == 0:
            print(j, len(res), file=sys.stderr)
    pickle.dump(res, open(sys.argv[2], "wb"))


if __name__ == "__main__":
    main()
