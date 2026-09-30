import json, sys
sys.path.insert(0, '.')
from ledger import L

chk = json.load(open('ledger_check.json', encoding='utf-8'))
docs = {}
for f, ch, to, d, n, t in chk:
    if t == 'NO DATA' or d < 2:
        continue
    docs[(f, ch)] = d


def rs_char(c):
    if c == "'":
        return "'\\''"
    if c == '\\':
        return "'\\\\'"
    if ord(c) < 0x7f and c.isprintable():
        return "'" + c + "'"
    if c.isalpha() or c in '°±×·¼½¾‘’“”':
        return "'" + c + "'"
    return "'\\u{%x}'" % ord(c)


def rs_str(s):
    return '"' + s.replace('\\', '\\\\').replace('"', '\\"') + '"'


fams = sorted({f for (f, _) in docs}, key=str.lower)
lines = []
nrows = 0
for f in fams:
    rows = [(ch, L[f][ch], docs[(f, ch)]) for ch in L[f] if (f, ch) in docs]
    rows.sort(key=lambda r: -r[2])
    nrows += len(rows)
    key = f.lower()
    cmt = ' '.join(f"{c if (ord(c) < 0x7f and c.isprintable()) else 'U+%04X' % ord(c)}={d}" for c, t, d in rows)
    lines.append(f"    // {f}, papers per row: {cmt}")
    lines.append(f"    (\"{key}\", &[{', '.join('(' + rs_char(c) + ', ' + rs_str(t) + ')' for c, t, d in rows)}]),")
print(len(fams), nrows, file=sys.stderr)
open('recode_table.rs.inc', 'w', encoding='utf-8').write('\n'.join(lines) + '\n')
