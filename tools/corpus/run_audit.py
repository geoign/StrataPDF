"""Run the reflow_audit example over a sample of a PDF corpus and summarise.

    python run_audit.py run --corpus corpus.jsonl --out RUN_DIR (--sample N --seed S | --ids FILE) [--jobs 4]
    python run_audit.py report RUN_DIR
    python run_audit.py compare BASE_DIR NEW_DIR

`corpus.jsonl` comes from build_corpus.py (one record per PDF with its matched
BibTeX entry). Each run directory holds results.jsonl (one audit record per
document, extended with title/abstract agreement against the BibTeX entry) and
dumps/<id>.txt (the reflowed text, one node per line).
"""

import argparse
import json
import os
import random
import re
import statistics
import subprocess
import sys
import time
import unicodedata
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

DEFAULT_EXE = r"C:\tmp\cargo-target\StrataPDF\release\examples\reflow_audit.exe"


def load_corpus(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(l) for l in f if l.strip()]


def toks(s):
    s = unicodedata.normalize("NFKC", s or "").lower()
    s = re.sub(r"(?<=\w)[-\u2010\u2011\u00ad'’](?=\w)", "", s)
    out = []
    for m in re.finditer(r"[\u3040-\u30ff\u3400-\u9fff\uf900-\ufaff]|[^\W_]+", s):
        out.append(m.group(0))
    return out


def ngrams(ts, n=3):
    return [tuple(ts[i : i + n]) for i in range(len(ts) - n + 1)]


def bib_agreement(rec, dump_text):
    """Title and abstract of the BibTeX entry against the reflowed text."""
    bib = rec.get("bib") or {}
    res = {}
    nodes = []  # (kind, tokens)
    for line in dump_text.splitlines():
        if line.startswith("=== ") or "\t" not in line:
            continue
        k, t = line.split("\t", 1)
        nodes.append((k, toks(t)))
    title = bib.get("title")
    if title:
        tt = toks(title)
        best = 0.0
        for k, ts in nodes[:60]:
            if k.startswith("H") and ts:
                inter = len(set(tt) & set(ts))
                best = max(best, inter / max(len(set(tt)), len(set(ts))))
        res["title_heading"] = round(best, 3)
        # The title the engine chose for the document.
        chosen = set(toks(rec.get("_reflow_title") or ""))
        if chosen:
            res["title_match"] = round(len(set(tt) & chosen) / max(len(set(tt)), len(chosen)), 3)
        else:
            res["title_match"] = 0.0
    abstract = bib.get("abstract")
    if abstract:
        a = abstract.rstrip("…").rstrip(".").rstrip()
        a = re.sub(r"^\s*(abstract|summary)\b[:.]?", "", a, flags=re.I)
        at = toks(a)
        if bib.get("abstract_truncated"):
            at = at[:-1]
        if len(at) >= 30:
            grams = ngrams(at)
            # Where each trigram occurs in the output (first occurrence).
            pos = {}
            flat = []
            for ni, (k, ts) in enumerate(nodes):
                if k in ("P", "L", "N", "H1", "H2", "H3", "H4", "H5", "H6", "FIG", "TAB"):
                    for j, t in enumerate(ts):
                        flat.append((t, ni))
            seq = [t for t, _ in flat]
            index = {}
            for i, g in enumerate(ngrams(seq)):
                index.setdefault(g, i)
            found = [index[g] for g in grams if g in index]
            res["abs_found"] = round(len(found) / len(grams), 3)
            if len(found) >= 10:
                # In order: consecutive found trigrams that advance by a small step.
                ok = sum(1 for x, y in zip(found, found[1:]) if 0 < y - x <= 4)
                res["abs_order"] = round(ok / (len(found) - 1), 3)
                # The shortest stretch of output holding 80% of the found trigrams:
                # foreign text interleaved into the abstract lengthens it.
                pos = sorted(found)
                k = max(2, int(len(pos) * 0.8))
                i0 = min(range(len(pos) - k + 1), key=lambda i: pos[i + k - 1] - pos[i])
                lo, hi = pos[i0], pos[i0 + k - 1]
                res["abs_nodes"] = len({flat[i][1] for i in range(lo, hi + 1)})
                expected = k / len(grams) * len(at)
                res["abs_extra"] = round(max(0, (hi - lo) - expected) / len(at), 3)
    return res


def run_one(exe, rec, out_dir, timeout):
    dump = out_dir / "dumps" / f"{rec['id']}.txt"
    t0 = time.time()
    try:
        p = subprocess.run([exe, rec["path"], "--dump", str(dump)], capture_output=True, timeout=timeout)
        line = p.stdout.decode("utf-8", "replace").strip().splitlines()
        if p.returncode != 0 or not line:
            err = p.stderr.decode("utf-8", "replace")[-600:]
            r = {"path": rec["path"], "error": f"exit {p.returncode}: {err}"}
        else:
            r = json.loads(line[-1])
    except subprocess.TimeoutExpired:
        r = {"path": rec["path"], "error": f"timeout {timeout}s"}
    r["id"] = rec["id"]
    r["wall"] = round(time.time() - t0, 2)
    for k in ("folder", "scanned_guess", "ocr_layer_guess", "vertical_cjk"):
        r[k] = rec.get(k)
    bib = rec.get("bib") or {}
    r["bib_type"] = bib.get("type")
    r["bib_year"] = bib.get("year")
    r["bib_key"] = bib.get("key")
    if "error" not in r and dump.exists():
        r.update(bib_agreement(dict(rec, _reflow_title=r.get("title")), dump.read_text(encoding="utf-8")))
    return r


def cmd_run(a):
    corpus = load_corpus(a.corpus)
    by_id = {r["id"]: r for r in corpus}
    if a.ids:
        ids = [l.split()[0] for l in Path(a.ids).read_text(encoding="utf-8").splitlines() if l.strip() and not l.startswith("#")]
        recs = [by_id[i] for i in ids if i in by_id]
    else:
        pool = [r for r in corpus if not r.get("error") and not r.get("encrypted")]
        if a.exclude:
            ex = set()
            for f in a.exclude:
                ex |= {l.split()[0] for l in Path(f).read_text(encoding="utf-8").splitlines() if l.strip()}
            pool = [r for r in pool if r["id"] not in ex]
        if a.max_pages:
            pool = [r for r in pool if (r.get("pages") or 0) <= a.max_pages]
        rng = random.Random(a.seed)
        recs = rng.sample(pool, min(a.sample, len(pool)))
    out = Path(a.out)
    (out / "dumps").mkdir(parents=True, exist_ok=True)
    (out / "ids.txt").write_text("".join(f"{r['id']}\t{r['rel']}\n" for r in recs), encoding="utf-8")
    done = {}
    res_path = out / "results.jsonl"
    if res_path.exists() and a.resume:
        for l in res_path.read_text(encoding="utf-8").splitlines():
            if l.strip():
                r = json.loads(l)
                done[r["id"]] = r
    todo = [r for r in recs if r["id"] not in done]
    t0 = time.time()
    with open(res_path, "a" if a.resume else "w", encoding="utf-8") as f, ThreadPoolExecutor(a.jobs) as ex:
        futs = {ex.submit(run_one, a.exe, r, out, max(120, int((r.get("pages") or 20) * a.sec_per_page))): r for r in todo}
        for n, fu in enumerate(as_completed(futs), 1):
            r = fu.result()
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
            f.flush()
            if n % 10 == 0 or n == len(todo):
                print(f"{n}/{len(todo)} {time.time() - t0:.0f}s", flush=True)
    cmd_report(argparse.Namespace(run=str(out)))


def load_results(run):
    rs = []
    for l in (Path(run) / "results.jsonl").read_text(encoding="utf-8").splitlines():
        if l.strip():
            rs.append(json.loads(l))
    return {r["id"]: r for r in rs}


# Per-document measures, as rates so that long documents do not dominate.
def measures(r):
    if r.get("error"):
        return None
    paras = max(1, r.get("paras", 0))
    words = max(1, r.get("out_tokens", 0))
    m = {
        "lost%": 100 * r["lost"] / max(1, r["raw_tokens"]),
        "body_lost%": 100 * r["body_lost"] / max(1, r["body_tokens"]),
        "dup%": 100 * r["dup"] / max(1, r["raw_tokens"]),
        "cut_lower/100p": 100 * r["cut_lower"] / paras,
        "cut_upper/100p": 100 * r["cut_upper"] / paras,
        "hyphen/10kw": 1e4 * r["hyphen_left"] / words,
        "glued/10kw": 1e4 * r["long_tokens"] / words,
        "leak/100p": 100 * r["leak"] / paras,
        "caption_para/100p": 100 * r["caption_para"] / paras,
        "short_paras%": 100 * r["short_paras"] / paras,
        "heading_bad": r["heading_long"] + r["heading_sentence"],
        "ref_multi%": 100 * r["ref_multi"] / max(1, r["ref_paras"]) if r.get("ref_paras") else None,
        "ref_frag%": 100 * r["ref_frag"] / max(1, r["ref_paras"]) if r.get("ref_paras") else None,
        "sec/page": r["ms"] / 1000 / max(1, r["pages"]),
    }
    for k in ("title_heading", "title_match", "abs_found", "abs_order", "abs_nodes", "abs_extra"):
        if k in r:
            m[k] = r[k]
    return m


# A document "has the problem" when its measure crosses these lines.
FLAGS = {
    "lost%": lambda v: v > 3,
    "body_lost%": lambda v: v > 2,
    "dup%": lambda v: v > 2,
    "cut_lower/100p": lambda v: v > 0,
    "cut_upper/100p": lambda v: v > 5,
    "hyphen/10kw": lambda v: v > 2,
    "glued/10kw": lambda v: v > 5,
    "leak/100p": lambda v: v > 0,
    "caption_para/100p": lambda v: v > 0,
    "short_paras%": lambda v: v > 25,
    "heading_bad": lambda v: v > 0,
    "ref_multi%": lambda v: v is not None and v > 10,
    "ref_frag%": lambda v: v is not None and v > 10,
    "title_heading": lambda v: v < 0.6,
    "title_match": lambda v: v < 0.6,
    "abs_found": lambda v: v < 0.8,
    "abs_order": lambda v: v < 0.9,
    "abs_extra": lambda v: v > 0.3,
    "sec/page": lambda v: v > 1.0,
}


def summarise(results):
    ok = [r for r in results.values() if not r.get("error")]
    errs = [r for r in results.values() if r.get("error")]
    ms = {r["id"]: measures(r) for r in ok}
    rows = {}
    for k, flag in FLAGS.items():
        vals = [(i, m[k]) for i, m in ms.items() if m.get(k) is not None]
        if not vals:
            continue
        flagged = [(i, v) for i, v in vals if flag(v)]
        rows[k] = {
            "n": len(vals),
            "mean": statistics.fmean(v for _, v in vals),
            "flagged": len(flagged),
            "flagged_ids": sorted(flagged, key=lambda x: -abs(x[1]))[:8],
        }
    return rows, errs, ms


def cmd_report(a):
    results = load_results(a.run)
    rows, errs, ms = summarise(results)
    n = len(results)
    lines = [f"# {a.run}", f"documents {n}, errors {len(errs)}", ""]
    lines.append(f"{'measure':<20}{'n':>5}{'mean':>10}{'flagged':>9}")
    for k, r in rows.items():
        lines.append(f"{k:<20}{r['n']:>5}{r['mean']:>10.3f}{r['flagged']:>9}")
    lines.append("")
    for e in errs:
        lines.append(f"ERROR {e['id']} {Path(e['path']).name}: {e['error'][:200]}")
    lines.append("")
    for k, r in rows.items():
        if r["flagged"]:
            lines.append(f"## {k}")
            for i, v in r["flagged_ids"]:
                lines.append(f"  {i} {v:.3f} {Path(results[i]['path']).name[:90]}")
    text = "\n".join(lines)
    (Path(a.run) / "report.txt").write_text(text, encoding="utf-8")
    print(text)


def cmd_compare(a):
    base, new = load_results(a.base), load_results(a.new)
    common = sorted(set(base) & set(new))
    rb, eb, mb = summarise({i: base[i] for i in common})
    rn, en, mn = summarise({i: new[i] for i in common})
    print(f"common documents {len(common)}; errors {len(eb)} -> {len(en)}")
    print(f"{'measure':<20}{'mean base':>11}{'mean new':>11}{'flag base':>10}{'flag new':>10}")
    for k in FLAGS:
        if k in rb or k in rn:
            b, n_ = rb.get(k, {}), rn.get(k, {})
            print(f"{k:<20}{b.get('mean', 0):>11.3f}{n_.get('mean', 0):>11.3f}{b.get('flagged', 0):>10}{n_.get('flagged', 0):>10}")
    # Documents whose flags changed.
    print()
    for i in common:
        if i not in mb or i not in mn:
            continue
        worse, better = [], []
        for k, flag in FLAGS.items():
            vb, vn = mb[i].get(k), mn[i].get(k)
            if vb is None or vn is None:
                continue
            fb, fn = flag(vb), flag(vn)
            if fn and not fb:
                worse.append(f"{k} {vb:.2f}->{vn:.2f}")
            elif fb and not fn:
                better.append(f"{k} {vb:.2f}->{vn:.2f}")
        if worse or better:
            name = Path(new[i]["path"]).name[:70]
            print(f"{i} {name}\n    worse: {', '.join(worse) or '-'}\n    better: {', '.join(better) or '-'}")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--corpus", required=True)
    r.add_argument("--out", required=True)
    r.add_argument("--sample", type=int, default=40)
    r.add_argument("--seed", type=int, default=1)
    r.add_argument("--ids")
    r.add_argument("--exclude", nargs="*")
    r.add_argument("--max-pages", type=int)
    r.add_argument("--jobs", type=int, default=4)
    r.add_argument("--sec-per-page", type=float, default=3.0)
    r.add_argument("--exe", default=DEFAULT_EXE)
    r.add_argument("--resume", action="store_true")
    p = sub.add_parser("report")
    p.add_argument("run")
    c = sub.add_parser("compare")
    c.add_argument("base")
    c.add_argument("new")
    a = ap.parse_args()
    {"run": cmd_run, "report": cmd_report, "compare": cmd_compare}[a.cmd](a)


if __name__ == "__main__":
    main()
