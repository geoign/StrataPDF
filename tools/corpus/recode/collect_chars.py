"""Run char_probe over the corpus: one JSON line per glyph of the fonts with few glyphs.

python collect_chars.py OUT.jsonl [--jobs 6] [--max-pages 40] [--ids ids.txt] [--limit N]
"""
import argparse, json, os, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor

EXE = "C:/tmp/cargo-target/StrataPDF-glyphs2/release/examples/char_probe.exe"

ap = argparse.ArgumentParser()
ap.add_argument("out")
ap.add_argument("--jobs", type=int, default=6)
ap.add_argument("--max-pages", type=int, default=40)
ap.add_argument("--ids")
ap.add_argument("--limit", type=int)
ap.add_argument("--exe", default=EXE)
a = ap.parse_args()

recs = [json.loads(l) for l in open("C:/tmp/strata_corpus/corpus.jsonl", encoding="utf-8")]
recs = [r for r in recs if not r.get("error") and os.path.exists(r["path"])]
if a.ids:
    want = set(open(a.ids).read().split())
    recs = [r for r in recs if r["id"] in want]
if a.limit:
    recs = recs[: a.limit]


def run(r):
    try:
        p = subprocess.run([a.exe, r["path"], "--max-pages", str(a.max_pages)], capture_output=True, timeout=300)
    except Exception as e:
        return r["id"], [], str(e)
    lines = []
    for l in p.stdout.decode("utf-8", "replace").splitlines():
        try:
            d = json.loads(l)
        except Exception:
            continue
        d["id"] = r["id"]
        lines.append(d)
    return r["id"], lines, p.returncode if p.returncode else None


t0 = time.time()
done = 0
bad = 0
with open(a.out, "w", encoding="utf-8") as f, ThreadPoolExecutor(a.jobs) as ex:
    for id_, lines, err in ex.map(run, recs):
        for d in lines:
            f.write(json.dumps(d, ensure_ascii=False) + "\n")
        done += 1
        if err:
            bad += 1
            print("ERR", id_, err, file=sys.stderr)
        if done % 200 == 0:
            f.flush()
            print(done, len(recs), f"{time.time()-t0:.0f}s", file=sys.stderr)
print("done", done, "bad", bad, f"{time.time()-t0:.0f}s", file=sys.stderr)
