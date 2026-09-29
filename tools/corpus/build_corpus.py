#!/usr/bin/env python3
"""Build a corpus index that pairs a library of academic PDFs with BibTeX entries.

The index drives quality evaluation of the reflow engine: every PDF gets cheap
layout/scan metadata (PyMuPDF, first three pages only) plus the BibTeX entry it
most likely belongs to (title, authors, abstract, ...), matched from the
Paperpile-style file name.

    python build_corpus.py --library <pdf root> --bib <library.bib> --out <dir>

Paperpile file name convention: ``<Authors> <Year> - <Title>.pdf`` where Authors
is ``Smith`` / ``Smith and Jones`` / ``Smith et al.``; long titles are shortened
by cutting the middle out (`` ... ``), file names that are still too long are cut
at the end and end in ``[...]`` (the extension is lost, so such files are sniffed
by their ``%PDF-`` magic and flagged ``ext_missing``); characters that are illegal
in file names are replaced (``&`` becomes ``&amp_`` and so on).

Outputs (in --out):
    corpus.jsonl        one JSON object per PDF (see README of the fields below)
    summary.md          counts, distributions, failures, unmatched list
    unmatched.txt       PDFs with no acceptable bib entry (with best guesses)
    ambiguous.txt       PDFs where several bib entries tie
    meta_cache.jsonl    per-file PyMuPDF results keyed by rel path + size + mtime,
                        so an interrupted or time-boxed run resumes where it stopped

Per-PDF fields: id, path, rel, folder, size, ext_missing, pages, producer,
creator, creation_year, encrypted, chars_p, image_cover_p, page_size,
vertical_cjk (fraction of CJK characters on the first 3 pages),
vertical_lines_frac (share of CJK-bearing text lines that are tall and narrow,
i.e. vertically written; null when the pages hold little CJK text),
scanned_guess, ocr_layer_guess, bib, match, candidates, error.

Only the standard library and PyMuPDF are needed.
"""
import argparse
import collections
import hashlib
import html
import json
import math
import multiprocessing as mp
import os
import random
import re
import statistics
import sys
import time
import unicodedata
from difflib import SequenceMatcher

# ---------------------------------------------------------------------------
# BibTeX parsing
# ---------------------------------------------------------------------------

MONTHS = {
    "jan": "January", "feb": "February", "mar": "March", "apr": "April",
    "may": "May", "jun": "June", "jul": "July", "aug": "August",
    "sep": "September", "oct": "October", "nov": "November", "dec": "December",
}
_ENTRY_RE = re.compile(r"@\s*([A-Za-z]+)\s*[{(]")
_FIELD_RE = re.compile(r"[\s,]*([A-Za-z_][\w\-:.]*)\s*=\s*")
# A closing double quote is only accepted when what follows looks like the end of
# the field (`,` + next field name, `}` or `#`); this survives stray quotes in
# abstracts.
_QUOTE_END_RE = re.compile(r"\s*(?:,\s*(?:[A-Za-z_][\w\-:.]*\s*=|\})|\}|#|,\s*$)")


def _read_braced(text, pos):
    """text[pos] == '{'; return (content, index after the matching '}')."""
    depth = 0
    i = pos
    n = len(text)
    while i < n:
        c = text[i]
        if c == "\\":
            i += 2
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return text[pos + 1:i], i + 1
        i += 1
    return text[pos + 1:], n


def _read_quoted(text, pos):
    """text[pos] == '"'; return (content, index after the closing quote)."""
    depth = 0
    i = pos + 1
    n = len(text)
    while i < n:
        c = text[i]
        if c == "\\":
            i += 2
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        elif c == '"' and depth <= 0 and _QUOTE_END_RE.match(text, i + 1):
            return text[pos + 1:i], i + 1
        i += 1
    return text[pos + 1:], n


def _read_value(text, pos):
    parts = []
    n = len(text)
    while pos < n:
        while pos < n and text[pos].isspace():
            pos += 1
        if pos >= n:
            break
        c = text[pos]
        if c == '"':
            v, pos = _read_quoted(text, pos)
        elif c == "{":
            v, pos = _read_braced(text, pos)
        else:
            m = re.compile(r"[^,\s}#]+").match(text, pos)
            if not m:
                break
            tok = m.group(0)
            v = MONTHS.get(tok.lower(), tok)
            pos = m.end()
        parts.append(v)
        while pos < n and text[pos].isspace():
            pos += 1
        if pos < n and text[pos] == "#":
            pos += 1
            continue
        break
    return re.sub(r"\s+", " ", "".join(parts)).strip(), pos


def parse_bib(text):
    """Parse BibTeX text into a list of dicts {'type','key','fields'} (fields lower-cased)."""
    entries = []
    pos = 0
    n = len(text)
    while True:
        at = text.find("@", pos)
        if at < 0:
            break
        m = _ENTRY_RE.match(text, at)
        if not m:
            pos = at + 1
            continue
        etype = m.group(1).lower()
        if etype in ("comment", "string", "preamble"):
            opener = text[m.end() - 1]
            if opener == "{":
                _, pos = _read_braced(text, m.end() - 1)
            else:
                pos = m.end()
            continue
        comma = text.find(",", m.end())
        if comma < 0:
            break
        key = text[m.end():comma].strip()
        pos = comma + 1
        fields = {}
        while pos < n:
            fm = _FIELD_RE.match(text, pos)
            if not fm:
                # end of entry, or garbage: resync at the closing brace
                rest = text[pos:pos + 40].lstrip(" \t\r\n,")
                if rest.startswith("}"):
                    pos = text.find("}", pos) + 1
                break
            name = fm.group(1).lower()
            value, pos = _read_value(text, fm.end())
            if name not in fields:
                fields[name] = value
        entries.append({"type": etype, "key": key, "fields": fields})
    return entries


_LATEX_CMD_RE = re.compile(r"\\[A-Za-z]+\s*")
_LATEX_ESC_RE = re.compile(r"\\([%&_#$])")


def clean_latex(s):
    """Best-effort conversion of the LaTeX residue in Paperpile fields to plain text."""
    if not s:
        return s
    s = _LATEX_ESC_RE.sub(r"\1", s)
    s = _LATEX_CMD_RE.sub("", s)
    s = s.replace("{", "").replace("}", "").replace("$", "")
    return re.sub(r"\s+", " ", s).strip()


def split_authors(s):
    """Split 'Last, First and Last, First' at top-level ' and '."""
    if not s:
        return []
    out, depth, cur, i = [], 0, [], 0
    while i < len(s):
        c = s[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        if depth == 0 and s.startswith(" and ", i):
            out.append("".join(cur).strip())
            cur = []
            i += 5
            continue
        cur.append(c)
        i += 1
    tail = "".join(cur).strip()
    if tail:
        out.append(tail)
    return [a for a in out if a and a.lower() != "others"]


def surname_of(author):
    a = clean_latex(author)
    if "," in a:
        return a.split(",", 1)[0].strip()
    parts = a.split()
    return parts[-1] if parts else a


# ---------------------------------------------------------------------------
# Normalisation
# ---------------------------------------------------------------------------

_DUP_SUFFIX_RE = re.compile(r"\s*\(\d{1,2}\)\s*$")
_SQUASH_RE = re.compile(r"[\W_]+", re.UNICODE)
_WORD_RE = re.compile(r"[a-z0-9]{3,}|[\u3040-\u30ff\u3400-\u9fff\uf900-\ufaff]{2,}")
_CJK_RE = re.compile(r"[\u3040-\u30ff\u31f0-\u31ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uff66-\uff9f]")


def _strip_accents(s):
    out = []
    for ch in s:
        o = ord(ch)
        if o < 0x250 or 0x1E00 <= o <= 0x1EFF:
            d = unicodedata.normalize("NFD", ch)
            out.append("".join(c for c in d if not unicodedata.combining(c)))
        else:
            out.append(ch)
    return "".join(out)


def norm_text(s, cs=False):
    """NFKC + casefold + accent strip, `&amp_` -> `&`, punctuation -> space.
    cs=True keeps the case (used only to break ties between duplicate entries)."""
    if not s:
        return ""
    s = html.unescape(s.replace("&amp_", "&"))
    s = unicodedata.normalize("NFKC", s)
    if not cs:
        s = s.casefold()
    s = _strip_accents(s)
    s = s.replace("ß", "ss")
    return re.sub(r"\s+", " ", _SQUASH_RE.sub(" ", s)).strip()


def squash(s, cs=False):
    """Normalised text without any separator: robust to space/punctuation replacement
    by Paperpile when it makes a file name."""
    return norm_text(s, cs).replace(" ", "")


def tokens_of(s):
    """Index tokens: Latin words (>= 3 chars) and CJK bigrams."""
    out = set()
    for w in _WORD_RE.findall(norm_text(s)):
        if w[0] < "\u3000":
            out.add(w)
        else:
            out.update(w[i:i + 2] for i in range(len(w) - 1))
    return out


def ratio(a, b):
    if not a or not b:
        return 0.0
    if a == b:
        return 1.0
    sm = SequenceMatcher(None, a, b, autojunk=False)
    if sm.real_quick_ratio() < 0.5 or sm.quick_ratio() < 0.5:
        return sm.quick_ratio() * 0.5
    return sm.ratio()


def coverage(a, b):
    """Share of `a` that is found (in order, in blocks) in `b`."""
    if not a or not b:
        return 0.0
    if a in b:
        return 1.0
    sm = SequenceMatcher(None, a, b, autojunk=False)
    return sum(bl.size for bl in sm.get_matching_blocks()) / len(a)


# ---------------------------------------------------------------------------
# File-name parsing
# ---------------------------------------------------------------------------

_ELLIPSIS_RE = re.compile(r"\s*(?:\.\.\.|…)\s*")
_TAIL_CUT_RE = re.compile(r"\s*\[\.\.\.\](?:\s*\(\d+\))?\s*$|\s*\[\.\.\(\d+\)\.\]\s*$")
_AUTHOR_ELLIPSIS_RE = re.compile(r"\s*(?:\.\.\.|…)\s*$")


def _title_variant(title, tail_cut):
    parts = _ELLIPSIS_RE.split(title)
    if len(parts) >= 2 and parts[0].strip() and parts[-1].strip():
        return {"prefix": parts[0], "suffix": parts[-1], "tail_cut": tail_cut}
    return {"prefix": " ".join(p for p in parts if p.strip()), "suffix": None, "tail_cut": tail_cut}


_HEAD_YEAR_RE = re.compile(r"^(?P<authors>.+?) (?P<year>(?:1[5-9]|20)\d\d)$", re.S)


def parse_filename(name, nth=0):
    """Split a Paperpile file name into author names, year and title pieces.

    The split is at the nth " - " (0 = the first, the normal case; later ones are tried
    for entries without authors, which Paperpile names "<start of title>... - <title>").
    Returns None when the name does not follow `<Authors> [<Year>] - <Title>`.
    `variants` lists the (prefix, suffix, tail_cut) readings of the title: the middle
    of a long title is cut out (prefix + suffix), over-long names are cut at the end
    (`[...]`, tail_cut), and a trailing `(1)` may be a copy marker or part of the title."""
    stem = name[:-4] if name.lower().endswith(".pdf") else name
    seps = [m.start() for m in re.finditer(r" - ", stem)]
    if nth >= len(seps) or seps[nth] == 0:
        return None
    head, title = stem[:seps[nth]].strip(), stem[seps[nth] + 3:].strip()
    hm = _HEAD_YEAR_RE.match(head)
    a_raw, year_s = (hm.group("authors").strip(), hm.group("year")) if hm else (head, None)
    # Paperpile sometimes repeats "Authors Year - " at the start of the title
    lead = a_raw + (" " + year_s if year_s else "") + " - "
    if title.startswith(lead):
        title = title[len(lead):].strip()
    tail_cut = False
    t2 = _TAIL_CUT_RE.sub("", title)
    if t2 != title:
        tail_cut, title = True, t2
    variants = [_title_variant(title, tail_cut)]
    t3 = _DUP_SUFFIX_RE.sub("", title)
    if t3 != title and t3:
        variants.append(_title_variant(t3, tail_cut))
    a_clean = re.sub(r"\s*(?:et al\.?)$", "", a_raw).strip()
    names = [x.strip() for x in re.split(r"\s+and\s+", a_clean) if x.strip()]
    return {
        "authors": names,
        "authors_raw": a_raw,
        "etal": a_raw != a_clean,
        "year": int(year_s) if year_s else None,
        "title_raw": title,
        "variants": variants,
    }


# ---------------------------------------------------------------------------
# Bib index + matching
# ---------------------------------------------------------------------------

def _year_of(s):
    m = re.search(r"\b(1[5-9]\d\d|20\d\d)\b", s or "")
    return int(m.group(1)) if m else None


def make_record(entry):
    f = entry["fields"]
    title = clean_latex(f.get("title", ""))
    authors_full = split_authors(f.get("author", ""))
    surnames = [surname_of(a) for a in authors_full]
    abstract = clean_latex(f.get("abstract", "")) or None
    venue = ""
    for k in ("journal", "booktitle", "institution", "school", "howpublished", "series"):
        if f.get(k):
            venue = clean_latex(f[k])
            break
    year = _year_of(f.get("year"))
    return {
        "key": entry["key"],
        "type": entry["type"],
        "title": title,
        "authors": surnames,
        "authors_full": [clean_latex(a) for a in authors_full],
        "year": year,
        "venue": venue or None,
        "publisher": clean_latex(f.get("publisher", "")) or None,
        "language": f.get("language") or None,
        "abstract": abstract,
        "abstract_truncated": bool(abstract and abstract.rstrip().endswith(("…", "..."))),
        # matching helpers (not exported)
        "_tsq": squash(title),
        "_tsq_cs": squash(title, cs=True),
        "_sur_sq": [squash(s) for s in surnames],
        "_full_sq": [squash(a) for a in authors_full],
        "_tok": tokens_of(title),
    }


class BibIndex:
    def __init__(self, records):
        self.records = records
        self.by_sur = collections.defaultdict(list)
        self.post = collections.defaultdict(list)
        self.by_year = collections.defaultdict(list)
        for i, r in enumerate(records):
            self.by_year[r["year"]].append(i)
            for s in set(r["_sur_sq"][:1]):
                if s:
                    self.by_sur[s].append(i)
            for t in r["_tok"]:
                self.post[t].append(i)
        n = len(records)
        self.idf = {t: math.log(n / (1 + len(v))) for t, v in self.post.items()}

    def candidates(self, fn, limit=40):
        """Bib entries worth scoring: best token overlap with the title pieces, plus
        entries sharing the first author (and a nearby year)."""
        score = collections.Counter()
        toks = set()
        for v in fn["variants"]:
            toks |= tokens_of(v["prefix"])
            if v["suffix"]:
                toks |= tokens_of(v["suffix"])
        for t in toks:
            plist = self.post.get(t)
            if not plist or len(plist) > 600:
                continue
            w = self.idf[t]
            for i in plist:
                score[i] += w
        cands = {i for i, _ in score.most_common(limit)}
        if fn["authors"]:
            fa = squash(_AUTHOR_ELLIPSIS_RE.sub("", fn["authors"][0]))
            for i in self.by_sur.get(fa, []):
                r = self.records[i]
                if fn["year"] is None or r["year"] is None or abs(r["year"] - fn["year"]) <= 1:
                    cands.add(i)
        return cands


def _variant_score(v, T):
    P = squash(v["prefix"])
    S = squash(v["suffix"]) if v["suffix"] else None
    if not P:
        return 0.0
    if S is None:
        if v["tail_cut"]:
            s = 1.0 if T.startswith(P) else ratio(P, T[:len(P)])
        elif T == P:
            s = 1.0
        else:
            s = ratio(P, T)
            # one side has an extra tail (subtitle, edition note, journal name)
            if len(min(P, T, key=len)) >= 25 and (T.startswith(P) or P.startswith(T)):
                s = max(s, 0.97)
    else:
        if len(T) < len(P) + len(S) - 3:
            return 0.0
        rp = 1.0 if T.startswith(P) else ratio(P, T[:len(P)])
        if v["tail_cut"]:
            # the suffix is a fragment from the middle of the title: no anchoring at the end
            rest = T[len(P):]
            rs = 1.0 if S in rest else coverage(S, rest)
        else:
            rs = 1.0 if T.endswith(S) else ratio(S, T[-len(S):])
        s = (len(P) * rp + len(S) * rs) / (len(P) + len(S))
    if s < 1.0 and min(len(P), len(T)) < 15:
        s *= 0.5  # near-misses on very short titles ("A18" vs "A19") mean nothing
    return s


def title_score(fn, rec):
    """0..1: how well the (possibly shortened) file-name title fits the bib title."""
    T = rec["_tsq"]
    if not T:
        return 0.0
    Ts = [T]
    if T.endswith("pdf") and len(T) > 3:
        Ts.append(T[:-3])  # bib titles that are file names ("A01.pdf")
    return max(_variant_score(v, t) for v in fn["variants"] for t in Ts)


def case_bonus(fn, rec):
    """Tiny tie-break between duplicate entries that differ only in capitalisation."""
    T = rec["_tsq_cs"]
    for v in fn["variants"]:
        P = squash(v["prefix"], cs=True)
        S = squash(v["suffix"], cs=True) if v["suffix"] else None
        if T.startswith(P) and (S is None or v["tail_cut"] or T.endswith(S)):
            return 0.002
    return 0.0


def _eq(x, y):
    if not x or not y:
        return False
    if x == y:
        return True
    return min(len(x), len(y)) >= 4 and (x.startswith(y) or y.startswith(x))


def author_component(fn, rec):
    """1.0 first author matches; 0.8 the bib surname is contained in the file's author
    string; 0.7 another bib author matches; 0.5 the name appears somewhere in an author
    string (given/family swapped); 0 no match; 0.6 neutral when unknown."""
    raw_sq = squash(_AUTHOR_ELLIPSIS_RE.sub("", fn["authors_raw"]))
    if not rec["_sur_sq"]:
        # Paperpile names entries without authors "<start of title>... - <title>"
        if raw_sq and (rec["_tsq"].startswith(raw_sq) or raw_sq.startswith(rec["_tsq"])):
            return 1.0
        return 0.6
    if not fn["authors"]:
        return 0.6
    fa = squash(_AUTHOR_ELLIPSIS_RE.sub("", fn["authors"][0]))
    if not fa:
        return 0.6
    if _eq(fa, rec["_sur_sq"][0]) or _eq(fa, rec["_full_sq"][0]):
        return 1.0
    if len(rec["_sur_sq"][0]) >= 5 and rec["_sur_sq"][0] in fa:
        return 0.8
    for s in rec["_sur_sq"][1:]:
        if _eq(fa, s):
            return 0.7
    for a in rec["_full_sq"]:
        if len(fa) >= 2 and fa in a:
            return 0.5
    return 0.0


def year_component(fy, ry):
    if fy is None and ry is None:
        return 1.0  # consistently undated
    if fy is None or ry is None:
        return 0.7
    d = abs(fy - ry)
    if d == 0:
        return 1.0
    if d == 1:
        return 0.7
    return 0.0


def combine(t, a, y):
    return t * (0.5 + 0.25 * a + 0.25 * y)


def method_name(t, a, y, fy, ry):
    """exact*: the whole title (prefix and suffix) matches; fuzzy*: title similarity < 1;
    *_yr_pm1 / *_yr_unknown: year off by one / missing on one side (still needs the author);
    title_only / fuzzy_title_only: author or year disagree."""
    base = "exact" if t >= 0.999 else "fuzzy"
    if a >= 1.0 and y >= 1.0:
        return base
    if a >= 1.0 and y >= 0.7:
        return base + ("_yr_pm1" if (fy is not None and ry is not None) else "_yr_unknown")
    return "title_only" if t >= 0.999 else "fuzzy_title_only"


MATCH_THRESHOLD = 0.70
AMBIGUITY_MARGIN = 0.03


def parent_guess(fn, index):
    """For attachments (supplements, book chapters) whose 'title' is a file name: the bib
    entry with the same first author and year, when that is unambiguous."""
    if fn["year"] is None:
        return None
    hits = []
    for i in index.by_year.get(fn["year"], []):
        rec = index.records[i]
        if rec["_sur_sq"] and author_component(fn, rec) >= 0.8:
            hits.append(rec)
    if not hits:
        return None
    if len({h["_tsq"] for h in hits}) == 1:
        return {"key": hits[0]["key"], "title": hits[0]["title"]}
    return {"keys": [h["key"] for h in hits[:3]]}


def _score_parsed(fn, index):
    """Score every candidate entry against one reading of the file name.
    -> list of (score, t, a, y, index) sorted best first."""
    scored = []
    for i in index.candidates(fn):
        rec = index.records[i]
        t = title_score(fn, rec)
        if t < 0.5:
            continue
        a = author_component(fn, rec)
        y = year_component(fn["year"], rec["year"])
        scored.append((combine(t, a, y) + case_bonus(fn, rec), t, a, y, i))
    scored.sort(key=lambda x: (-x[0], x[4]))
    return scored


def match_file(name, index):
    """-> (record | None, match dict, candidates list)"""
    fn = parse_filename(name)
    if fn is None or not any(v["prefix"].strip() for v in fn["variants"]):
        return None, {"score": 0.0, "method": "none", "reason": "unparsable file name"}, []
    scored = _score_parsed(fn, index)
    if not scored or scored[0][0] < MATCH_THRESHOLD:
        # Entries without authors are named "<start of title>... - <title>", and the start
        # may itself contain " - ": read the name again, splitting at the later separators.
        for nth in (1, 2, 3):
            alt = parse_filename(name, nth)
            if alt is None:
                break
            s2 = _score_parsed(alt, index)
            if s2 and (not scored or s2[0][0] > scored[0][0]):
                fn, scored = alt, s2
    if not scored:
        match = {"score": 0.0, "method": "none"}
        pg = parent_guess(fn, index)
        if pg:
            match["parent_guess"] = pg
        return None, match, []
    sc, t, a, y, i = scored[0]
    rec = index.records[i]
    others = [s for s in scored[1:] if index.records[s[4]]["key"] != rec["key"]]
    cand = [{"key": index.records[s[4]]["key"], "score": round(min(1.0, s[0]), 3)} for s in others[:3]]
    tied = [s for s in others if s[0] >= sc - AMBIGUITY_MARGIN and s[0] >= MATCH_THRESHOLD]
    match = {"score": round(min(1.0, sc), 3), "method": method_name(t, a, y, fn["year"], rec["year"])}
    if tied:
        match["ambiguous"] = True
        # an identical duplicate entry in the library is not a real ambiguity
        match["equivalent_duplicates"] = all(
            index.records[s[4]]["_tsq"] == rec["_tsq"] and index.records[s[4]]["year"] == rec["year"]
            for s in tied
        )
    if sc < MATCH_THRESHOLD:
        match["method"] = "none"
        pg = parent_guess(fn, index)
        if pg:
            match["parent_guess"] = pg
        return None, match, [{"key": rec["key"], "score": round(min(1.0, sc), 3)}] + cand
    return rec, match, cand if tied else []


def public_bib(rec):
    return {
        "key": rec["key"],
        "type": rec["type"],
        "title": rec["title"],
        "authors": rec["authors"],
        "year": rec["year"],
        "venue": rec["venue"],
        "publisher": rec["publisher"],
        "language": rec["language"],
        "abstract": rec["abstract"],
        "abstract_truncated": rec["abstract_truncated"],
    }


# ---------------------------------------------------------------------------
# PDF metadata (worker)
# ---------------------------------------------------------------------------

META_VERSION = 2  # bump when extract_meta changes: cached results of older versions are re-read


def _init_worker():
    try:
        import pymupdf
        pymupdf.TOOLS.mupdf_display_errors(False)
        pymupdf.TOOLS.mupdf_display_warnings(False)
    except Exception:
        pass


def _creation_year(s):
    if not s:
        return None
    m = re.search(r"(1[89]\d\d|20\d\d)", s)
    if not m:
        return None
    y = int(m.group(1))
    return y if 1980 <= y <= 2100 else None


def extract_meta(path, n_pages=3):
    """Everything we keep from the PDF itself; never raises."""
    out = {"error": None}
    errors = []
    try:
        import pymupdf
        doc = pymupdf.open(path)
    except Exception as e:  # noqa: BLE001 - record and go on
        out["error"] = f"open: {type(e).__name__}: {e}"
        return out
    try:
        md = doc.metadata or {}
        out["pages"] = doc.page_count
        out["producer"] = md.get("producer") or None
        out["creator"] = md.get("creator") or None
        out["creation_year"] = _creation_year(md.get("creationDate"))
        out["pdf_format"] = md.get("format") or None
        out["meta_title"] = (md.get("title") or None)
        out["encrypted"] = bool(doc.is_encrypted)
        out["needs_pass"] = bool(doc.needs_pass)
        out["repaired"] = bool(getattr(doc, "is_repaired", False))  # xref rebuilt while opening
        if doc.needs_pass:
            out["error"] = "needs password"
            return out
        if doc.page_count == 0:
            errors.append("0 pages (damaged or truncated file)")
        try:
            out["toc_entries"] = len(doc.get_toc(simple=True))
        except Exception as e:  # noqa: BLE001
            out["toc_entries"] = None
            errors.append(f"toc: {type(e).__name__}: {e}")
        chars, covers, all_text = [], [], []
        page_size = None
        vert_lines = vert_total = 0
        for pno in range(min(n_pages, doc.page_count)):
            try:
                page = doc[pno]
                rect = page.rect
                if page_size is None:
                    page_size = [round(rect.width, 1), round(rect.height, 1)]
                    out["page_rotation"] = page.rotation
                txt = page.get_text()
                all_text.append(txt)
                chars.append(len("".join(txt.split())))
                area = rect.width * rect.height
                cov = 0.0
                if area > 0:
                    for info in page.get_image_info():
                        b = pymupdf.Rect(info["bbox"]) & rect
                        if not b.is_empty:
                            cov += b.width * b.height
                covers.append(round(min(1.0, cov / area), 3) if area > 0 else 0.0)
            except Exception as e:  # noqa: BLE001
                chars.append(None)
                covers.append(None)
                errors.append(f"page {pno + 1}: {type(e).__name__}: {e}")
        out["chars_p"] = chars
        out["image_cover_p"] = covers
        out["page_size"] = page_size
        joined = "".join("".join(t.split()) for t in all_text)
        cjk = len(_CJK_RE.findall(joined))
        out["vertical_cjk"] = round(cjk / len(joined), 4) if joined else 0.0
        out["vertical_lines_frac"] = None
        if cjk >= 60:
            # vertical writing shows up as tall, narrow lines of CJK text
            try:
                for pno in range(min(n_pages, doc.page_count)):
                    d = doc[pno].get_text("dict")
                    for blk in d.get("blocks", []):
                        for ln in blk.get("lines", []):
                            s = "".join(sp.get("text", "") for sp in ln.get("spans", []))
                            if len(_CJK_RE.findall(s)) < 3:
                                continue
                            x0, y0, x1, y1 = ln["bbox"]
                            dx, dy = ln.get("dir", (1.0, 0.0))
                            vert_total += 1
                            # wmode 1 = vertical writing mode of the font; otherwise a tall,
                            # narrow line whose baseline is horizontal (not merely rotated text)
                            if ln.get("wmode") == 1 or (abs(dx) >= abs(dy) and (y1 - y0) > 1.5 * (x1 - x0)):
                                vert_lines += 1
                if vert_total:
                    out["vertical_lines_frac"] = round(vert_lines / vert_total, 3)
            except Exception as e:  # noqa: BLE001
                errors.append(f"vertical: {type(e).__name__}: {e}")
    except Exception as e:  # noqa: BLE001
        errors.append(f"{type(e).__name__}: {e}")
    finally:
        try:
            doc.close()
        except Exception:  # noqa: BLE001
            pass
    if errors:
        out["error"] = "; ".join(errors)
    return out


def derive_flags(meta):
    chars = [c for c in (meta.get("chars_p") or []) if c is not None]
    covers = [c for c in (meta.get("image_cover_p") or []) if c is not None]
    if not chars or not covers:
        return {"scanned_guess": False, "ocr_layer_guess": False}
    total = sum(chars)
    cover = sum(covers) / len(covers)
    scanned = total < 200 and cover > 0.5
    ocr = total >= 200 and cover > 0.7
    return {"scanned_guess": bool(scanned), "ocr_layer_guess": bool(ocr)}


def _work(job):
    rel, path = job
    return rel, extract_meta(path)


# ---------------------------------------------------------------------------
# Library scan + cache
# ---------------------------------------------------------------------------

def sniff_pdf(path):
    try:
        with open(path, "rb") as fh:
            return fh.read(1024).find(b"%PDF-") >= 0
    except OSError:
        return False


def scan_library(root, include_extless):
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for fn in sorted(filenames):
            p = os.path.join(dirpath, fn)
            ext_missing = False
            if not fn.lower().endswith(".pdf"):
                # Paperpile cuts over-long names at the end, extension included:
                # such files still start with %PDF-. (.ai files do too; they are not papers.)
                if not include_extless or fn.lower().endswith((".ai", ".eps")) or not sniff_pdf(p):
                    continue
                ext_missing = True
            try:
                st = os.stat(p)
            except OSError:
                continue
            rel = os.path.relpath(p, root).replace("\\", "/")
            files.append({
                "rel": rel, "path": os.path.abspath(p), "size": st.st_size,
                "mtime": int(st.st_mtime), "ext_missing": ext_missing, "name": fn,
            })
    return files


def load_cache(path):
    cache = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    o = json.loads(line)
                except ValueError:
                    continue
                cache[o["rel"]] = o
    return cache


def run_meta(files, cache_path, workers, budget):
    cache = load_cache(cache_path)
    todo = []
    for f in files:
        c = cache.get(f["rel"])
        if (not c or c["size"] != f["size"] or c["mtime"] != f["mtime"]
                or c.get("v") != META_VERSION):
            todo.append(f)
    print(f"[meta] {len(files) - len(todo)} cached, {len(todo)} to read", flush=True)
    if not todo:
        return cache, 0
    by_rel = {f["rel"]: f for f in todo}
    t0 = time.time()
    done = 0
    with open(cache_path, "a", encoding="utf-8") as out, mp.Pool(workers, initializer=_init_worker) as pool:
        it = pool.imap_unordered(_work, [(f["rel"], f["path"]) for f in todo], chunksize=1)
        while True:
            try:
                rel, meta = it.next(timeout=180)
            except StopIteration:
                break
            except mp.TimeoutError:
                print("[meta] no result for 180 s (hung or very slow file); stopping this run; rerun to continue", flush=True)
                pool.terminate()
                break
            f = by_rel[rel]
            rec = {"rel": rel, "size": f["size"], "mtime": f["mtime"], "v": META_VERSION, "meta": meta}
            out.write(json.dumps(rec, ensure_ascii=False) + "\n")
            out.flush()
            cache[rel] = rec
            done += 1
            if done % 250 == 0:
                print(f"[meta] {done}/{len(todo)}  {time.time() - t0:.0f}s", flush=True)
            if budget and time.time() - t0 > budget:
                print(f"[meta] time budget of {budget}s reached after {done} files; rerun to continue", flush=True)
                pool.terminate()
                break
    remaining = sum(1 for f in todo
                    if f["rel"] not in cache or cache[f["rel"]]["mtime"] != f["mtime"]
                    or cache[f["rel"]].get("v") != META_VERSION)
    return cache, remaining


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

def pct(n, d):
    return f"{100.0 * n / d:.1f}%" if d else "n/a"


def quantile(sorted_vals, q):
    if not sorted_vals:
        return None
    k = (len(sorted_vals) - 1) * q
    lo, hi = math.floor(k), math.ceil(k)
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


def write_summary(path, rows, n_bib):
    total = len(rows)
    matched = [r for r in rows if r["bib"]]
    unmatched = [r for r in rows if not r["bib"]]
    amb = [r for r in rows if r["match"].get("ambiguous")]
    amb_real = [r for r in amb if not r["match"].get("equivalent_duplicates")]
    high = [r for r in matched if r["match"]["score"] >= 0.9]
    L = []
    L.append("# Corpus summary\n")
    L.append(f"- PDFs: {total}  (of which extension-less PDFs found by content sniffing: "
             f"{sum(1 for r in rows if r.get('ext_missing'))})")
    L.append(f"- Bib entries parsed: {n_bib}")
    L.append(f"- Matched: {len(matched)} ({pct(len(matched), total)}); high confidence (score >= 0.9): "
             f"{len(high)} ({pct(len(high), total)})")
    L.append(f"- Unmatched: {len(unmatched)} ({pct(len(unmatched), total)}); of these "
             f"{sum(1 for r in unmatched if r['match'].get('parent_guess'))} look like attachments "
             f"(supplement, chapter, ...) of a bib entry that shares first author and year "
             f"(`match.parent_guess`; `bib` stays null because the abstract does not describe that file)")
    L.append(f"- Ambiguous (two or more entries within 0.03 of the best): {len(amb)}; "
             f"of these, tied only with an identical duplicate entry: {len(amb) - len(amb_real)}; "
             f"real ambiguities: {len(amb_real)}")
    L.append("\n## Match methods\n")
    L.append("| method | files |\n|---|---:|")
    for k, v in collections.Counter(r["match"]["method"] for r in rows).most_common():
        L.append(f"| {k} | {v} |")

    L.append("\n## Matched files by bib type\n")
    L.append("| type | files |\n|---|---:|")
    for k, v in collections.Counter(r["bib"]["type"] for r in matched).most_common():
        L.append(f"| {k} | {v} |")

    L.append("\n## Matched files by publication decade (bib year)\n")
    L.append("| decade | files |\n|---|---:|")
    dec = collections.Counter((r["bib"]["year"] // 10 * 10) if r["bib"]["year"] else None for r in matched)
    for k in sorted(dec, key=lambda x: (x is None, x)):
        L.append(f"| {'unknown' if k is None else str(k) + 's'} | {dec[k]} |")

    L.append("\n## By top-level folder\n")
    L.append("| folder | files | matched | unmatched |\n|---|---:|---:|---:|")
    fold = collections.defaultdict(lambda: [0, 0])
    for r in rows:
        fold[r["folder"]][0] += 1
        fold[r["folder"]][1] += 1 if r["bib"] else 0
    for k, (n, m) in sorted(fold.items(), key=lambda kv: -kv[1][0]):
        L.append(f"| {k} | {n} | {m} | {n - m} |")

    L.append("\n## Scans and text layers (first 3 pages)\n")
    sc = sum(1 for r in rows if r.get("scanned_guess"))
    oc = sum(1 for r in rows if r.get("ocr_layer_guess"))
    L.append(f"- scanned_guess (< 200 text chars and image cover > 0.5): {sc}")
    L.append(f"- ocr_layer_guess (text present and image cover > 0.7): {oc}")
    notext = sum(1 for r in rows if r.get("chars_p") and sum(c or 0 for c in r["chars_p"]) < 50)
    L.append(f"- fewer than 50 text chars on the first 3 pages: {notext}")
    enc = sum(1 for r in rows if r.get("encrypted"))
    L.append(f"- encrypted flag set: {enc}")
    L.append(f"- opened only after MuPDF rebuilt the xref (damaged or oddly written): "
             f"{sum(1 for r in rows if r.get('repaired'))}")
    L.append(f"- with an outline / bookmarks: {sum(1 for r in rows if (r.get('toc_entries') or 0) > 0)}")

    L.append("\n## Page counts\n")
    pages = sorted(r["pages"] for r in rows if isinstance(r.get("pages"), int))
    if pages:
        L.append(f"- min {pages[0]}, median {statistics.median(pages):g}, p90 {quantile(pages, 0.9):g}, "
                 f"max {pages[-1]}  (n = {len(pages)})")
        bins = [(0, 0), (1, 1), (2, 5), (6, 10), (11, 20), (21, 50), (51, 100), (101, 300), (301, 10 ** 9)]
        L.append("\n| pages | files |\n|---|---:|")
        for lo, hi in bins:
            n = sum(1 for p in pages if lo <= p <= hi)
            L.append(f"| {lo if lo == hi else (str(lo) + '-' + str(hi) if hi < 10 ** 9 else str(lo) + '+')} | {n} |")

    L.append("\n## CJK\n")
    cjk = [r for r in rows if (r.get("vertical_cjk") or 0) > 0.10]
    L.append(f"- documents with > 10% CJK characters on the first 3 pages: {len(cjk)}")
    vert = [r for r in rows if (r.get("vertical_lines_frac") or 0) > 0.5]
    L.append(f"- of these, with mostly vertical CJK lines (vertical_lines_frac > 0.5): {len(vert)}")
    for r in vert[:30]:
        L.append(f"  - {r['rel']}")

    L.append("\n## Producers (top 15)\n")
    L.append("| producer | files |\n|---|---:|")
    for k, v in collections.Counter((r.get("producer") or "(none)")[:60] for r in rows).most_common(15):
        L.append(f"| {k} | {v} |")

    L.append("\n## Files that failed to open or read\n")
    bad = [r for r in rows if r.get("error")]
    L.append(f"{len(bad)} file(s) with an error\n")
    for r in bad:
        L.append(f"- `{r['rel']}`: {r['error']}")

    L.append("\n## Unmatched file names\n")
    L.append(f"{len(unmatched)} file(s)\n")
    for r in unmatched:
        L.append(f"- `{r['rel']}`")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(L) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--library", required=True, help="root folder of the PDFs (searched recursively)")
    ap.add_argument("--bib", required=True, help="BibTeX file")
    ap.add_argument("--out", required=True, help="output directory (created if missing)")
    ap.add_argument("--workers", type=int, default=12)
    ap.add_argument("--time-budget", type=int, default=480,
                    help="seconds of PDF reading per run before stopping cleanly (resumable); 0 = unlimited")
    ap.add_argument("--limit", type=int, default=0, help="only process the first N files (testing)")
    ap.add_argument("--skip-meta", action="store_true", help="matching only: use cached PyMuPDF results, read nothing")
    ap.add_argument("--no-extless", action="store_true",
                    help="ignore PDFs whose name lost the .pdf extension (Paperpile '[...]' truncation)")
    ap.add_argument("--sample", type=int, default=0, help="print N random matches (file name vs bib title)")
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    with open(args.bib, encoding="utf-8-sig") as fh:
        entries = parse_bib(fh.read())
    records = [make_record(e) for e in entries]
    index = BibIndex(records)
    print(f"[bib] {len(records)} entries, "
          f"{sum(1 for r in records if r['year'] is None)} without year, "
          f"{sum(1 for r in records if not r['authors'])} without author", flush=True)

    files = scan_library(args.library, include_extless=not args.no_extless)
    if args.limit:
        files = files[:args.limit]
    print(f"[scan] {len(files)} PDFs", flush=True)

    cache_path = os.path.join(args.out, "meta_cache.jsonl")
    if args.skip_meta:
        cache = load_cache(cache_path)
        remaining = 0
    else:
        cache, remaining = run_meta(files, cache_path, args.workers, args.time_budget)
        if remaining:
            print(f"[meta] {remaining} file(s) still unread; rerun the same command to continue", flush=True)
            sys.exit(2)

    rows = []
    t0 = time.time()
    for f in files:
        rec, match, cands = match_file(f["name"], index)
        c = cache.get(f["rel"])
        meta = dict(c["meta"]) if c else {"error": "meta not read"}
        row = {
            "id": hashlib.sha1(f["rel"].encode("utf-8")).hexdigest()[:10],
            "path": f["path"],
            "rel": f["rel"],
            "folder": f["rel"].split("/", 1)[0] if "/" in f["rel"] else "(root)",
            "size": f["size"],
            "ext_missing": f["ext_missing"],
        }
        row.update(meta)
        row.update(derive_flags(meta))
        row["bib"] = public_bib(rec) if rec else None
        row["match"] = match
        if cands:
            row["candidates"] = cands
        rows.append(row)
    print(f"[match] done in {time.time() - t0:.1f}s", flush=True)

    with open(os.path.join(args.out, "corpus.jsonl"), "w", encoding="utf-8") as fh:
        for r in rows:
            fh.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(os.path.join(args.out, "unmatched.txt"), "w", encoding="utf-8") as fh:
        for r in rows:
            if not r["bib"]:
                best = r.get("candidates") or []
                guess = ""
                if best:
                    g = next((x for x in records if x["key"] == best[0]["key"]), None)
                    guess = f"\tbest guess {best[0]['score']}: {g['key']} | {g['title'][:90]}" if g else ""
                pg = r["match"].get("parent_guess")
                if pg:
                    guess += "\tparent guess (same first author + year): " + (
                        f"{pg['key']} | {pg['title'][:90]}" if "key" in pg else ", ".join(pg["keys"]))
                fh.write(f"{r['rel']}{guess}\n")
    with open(os.path.join(args.out, "ambiguous.txt"), "w", encoding="utf-8") as fh:
        by_key = {x["key"]: x for x in records}
        for r in rows:
            if r["match"].get("ambiguous"):
                dup = " (identical duplicate entry)" if r["match"].get("equivalent_duplicates") else ""
                fh.write(f"{r['rel']}\n  chosen  {r['match']['score']}: {r['bib']['key'] if r['bib'] else '-'}"
                         f" | {(r['bib'] or {}).get('title', '')[:90]}{dup}\n")
                for c in r.get("candidates", []):
                    b = by_key.get(c["key"])
                    fh.write(f"  other   {c['score']}: {c['key']} | {b['title'][:90] if b else ''}"
                             f" [{b['year'] if b else ''}]\n")
    write_summary(os.path.join(args.out, "summary.md"), rows, len(records))

    m = sum(1 for r in rows if r["bib"])
    print(f"[done] {len(rows)} PDFs, matched {m} ({pct(m, len(rows))}), unmatched {len(rows) - m}", flush=True)

    if args.sample:
        rnd = random.Random(args.seed)
        pool_ = [r for r in rows if r["bib"]]
        for r in rnd.sample(pool_, min(args.sample, len(pool_))):
            print(f"{r['match']['score']:.2f} {r['match']['method']:<16} FILE: {os.path.basename(r['rel'])}\n"
                  f"{'':21}BIB : {r['bib']['authors'][:2]} {r['bib']['year']} - {r['bib']['title']}")


if __name__ == "__main__":
    mp.freeze_support()
    main()
