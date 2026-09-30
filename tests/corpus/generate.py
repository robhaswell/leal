#!/usr/bin/env python3
"""Regenerate Leal's hand-made test corpus, byte for byte.

    python3 tests/corpus/generate.py          # write every file
    python3 tests/corpus/generate.py --check  # verify the committed files match

Each case below gives the exact bytes of a data file and, next to them, what
a parser should find: the sidecar `<file>.expected.toml`. Offsets in the
sidecars are located by searching the bytes (`at(...)`), never computed by
parsing, so the sidecars stay an independent statement of intent. The
Rust tests in `crates/leal-testkit/tests/corpus.rs` check every sidecar
against a reference parser.

Conventions (see README.md): rows are 0-based physical rows; offsets are
0-based byte offsets into the file as stored, including any BOM.

This script only writes the files it defines. It never deletes anything, so
real exports added by hand are safe. Python 3.9+, standard library only.
"""

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
BOM_UTF8 = b"\xef\xbb\xbf"
BOM_UTF16LE = b"\xff\xfe"
BOM_UTF16BE = b"\xfe\xff"

CASES = []


def at(data, needle, nth=0, plus=0):
    """Offset of the nth (0-based) occurrence of `needle` in `data`, plus `plus`."""
    pos = -1
    for _ in range(nth + 1):
        pos = data.find(needle, pos + 1)
        if pos < 0:
            raise ValueError(f"{needle!r} occurrence {nth} not found")
    return pos + plus


def case(path, data, description, *, rows, fields, delimiter=",", line_ending="lf",
         mixed=False, bom="none", encoding="utf-8", trailing_newline=True, header=True,
         diagnostics=(), cells=()):
    """Register one corpus file.

    fields:      the field count of each row (a list of `rows` ints).
    diagnostics: (kind, [(row, offset), ...]) pairs; count = number of locations.
    cells:       (row, field, display_value, quoted-or-None) tuples.
    """
    assert len(fields) == rows, f"{path}: {len(fields)} field counts for {rows} rows"
    CASES.append(dict(path=path, data=data, description=description, rows=rows,
                      fields=fields, delimiter=delimiter, line_ending=line_ending,
                      mixed=mixed, bom=bom, encoding=encoding,
                      trailing_newline=trailing_newline, header=header,
                      diagnostics=list(diagnostics), cells=list(cells)))


def bom_present():
    return ("bom_present", [(0, 0)])


# ---------------------------------------------------------------------------
# Dialects
# ---------------------------------------------------------------------------

d = b"id,name,city\n1,Ada,London\n2,Grace,New York\n3,Linus,Helsinki\n"
case("dialect/comma.csv", d, "Comma-separated, LF, header row.", rows=4, fields=[3] * 4)

d = b"product;price;qty\nApple;1,20;3\nPear;0,95;12\nPlum;2,05;7\n"
case("dialect/semicolon.csv", d,
     "Semicolon-separated with decimal commas, as European locales write it.",
     rows=4, fields=[3] * 4, delimiter=";")

d = b"name\tage\tnote\nAda\t36\tfirst, programmer\nAlan\t41\tcodebreaker\n"
case("dialect/tab.tsv", d, "Tab-separated; a comma in an unquoted field is plain text.",
     rows=3, fields=[3] * 3, delimiter="\t",
     cells=[(1, 2, "first, programmer", False)])

d = b"sku|description|stock\nA-1|Blue mug|12\nA-2|Red mug, large|0\n"
case("dialect/pipe.txt", d, "Pipe-separated.", rows=3, fields=[3] * 3, delimiter="|")

for name, le, sep in [("lf", "lf", b"\n"), ("crlf", "crlf", b"\r\n"), ("cr", "cr", b"\r")]:
    d = b"name,qty" + sep + b"apple,1" + sep + b"pear,2" + sep
    case(f"dialect/line-endings-{name}.csv", d, f"Every row ends with {name.upper()}.",
         rows=3, fields=[2] * 3, line_ending=le)

d = b"name,qty\r\napple,1\npear,2\r\nplum,3\r\n"
case("dialect/line-endings-mixed.csv", d,
     "Mostly CRLF with one LF row; CRLF is the most common, so it is the dialect's.",
     rows=4, fields=[2] * 4, line_ending="crlf", mixed=True,
     diagnostics=[("mixed_line_endings", [(1, at(d, b"apple,1", plus=7))])])

text = "city,population\nZürich,421878\nKraków,804237\n東京,13960000\n"
case("dialect/encoding-utf8.csv", text.encode("utf-8"),
     "UTF-8 without a BOM, with 2- and 3-byte characters.",
     rows=4, fields=[2] * 4, cells=[(1, 0, "Zürich", False), (3, 0, "東京", False)])
case("dialect/encoding-utf8-bom.csv", BOM_UTF8 + text.encode("utf-8"),
     "UTF-8 with a BOM. The BOM belongs to no row; row 0 starts at offset 3.",
     rows=4, fields=[2] * 4, bom="utf-8", diagnostics=[bom_present()],
     cells=[(0, 0, "city", False), (2, 0, "Kraków", False)])
case("dialect/encoding-utf16le-bom.csv", BOM_UTF16LE + text.encode("utf-16-le"),
     "UTF-16 LE with a BOM (read-only in v1). Only counts are compared, not offsets.",
     rows=4, fields=[2] * 4, bom="utf-16le", encoding="utf-16le",
     diagnostics=[bom_present()], cells=[(3, 0, "東京", False)])
case("dialect/encoding-utf16be-bom.csv", BOM_UTF16BE + text.encode("utf-16-be"),
     "UTF-16 BE with a BOM (read-only in v1). Only counts are compared, not offsets.",
     rows=4, fields=[2] * 4, bom="utf-16be", encoding="utf-16be",
     diagnostics=[bom_present()], cells=[(1, 0, "Zürich", False)])

d = "name,price\nCafé,€3.50\n“Crème brûlée”,£4.20\n".encode("cp1252")
case("dialect/encoding-windows-1252.csv", d,
     "Windows-1252: é, €, £ and curly quotes are single bytes, so the file is not "
     "valid UTF-8. Curly quotes are not the quote character.",
     rows=3, fields=[2] * 3, encoding="windows-1252",
     cells=[(1, 0, "Café", False), (1, 1, "€3.50", False), (2, 0, "“Crème brûlée”", False)])

d = b'"id","name","note"\r\n"1","Ada","likes ""maths"""\r\n"2","Alan",""\r\n'
case("dialect/all-quoted.csv", d, "Every field quoted, CRLF, including an empty quoted field.",
     rows=3, fields=[3] * 3, line_ending="crlf",
     cells=[(0, 0, "id", True), (1, 2, 'likes "maths"', True), (2, 2, "", True)])

d = b"name,qty\napple,1\npear,2"
case("dialect/no-trailing-newline.csv", d, "The last row has no line ending.",
     rows=3, fields=[2] * 3, trailing_newline=False, cells=[(2, 1, "2", False)])

case("dialect/empty.csv", b"", "A zero-byte file: no rows, default dialect.",
     rows=0, fields=[], line_ending="none", trailing_newline=False, header=False)

d = b"score\n12\n7\n30\n"
case("dialect/single-column.csv", d,
     "One column, so no delimiter appears; the default (comma) is expected.",
     rows=4, fields=[1] * 4)

d = b"Ada,36,London\n"
case("dialect/single-row.csv", d,
     "A single row. With no later rows to compare, no header is detected.",
     rows=1, fields=[3], header=False)

d = b"date,amount,description\n2026-01-03,12.50,Coffee\n2026-01-04,8.00,Lunch\n"
case("dialect/header-row.csv", d, "A text header over typed rows (dates, numbers).",
     rows=3, fields=[3] * 3)

d = b"2026-01-03,12.50,Coffee\n2026-01-04,8.00,Lunch\n2026-01-05,3.20,Bus\n"
case("dialect/no-header-row.csv", d, "Every row has the same types, so no header.",
     rows=3, fields=[3] * 3, header=False)

d = (b'id,comment\n1,"first line\nsecond line"\n2,"windows\r\nline"\n'
     b'3,"old mac\rline"\n4,plain\n')
case("dialect/quoted-newlines.csv", d,
     "LF, CRLF and CR inside quoted fields do not end rows and are not line endings.",
     rows=5, fields=[2] * 5,
     cells=[(1, 1, "first line\nsecond line", True), (2, 1, "windows\r\nline", True),
            (3, 1, "old mac\rline", True), (4, 1, "plain", False)])

d = b'name,address\nAda,"12 High St, London"\nBob,"a;b|c\td"\nCy,","\n'
case("dialect/quoted-delimiters.csv", d,
     "Delimiters (all four kinds) inside quoted fields are text.",
     rows=4, fields=[2] * 4,
     cells=[(1, 1, "12 High St, London", True), (2, 1, "a;b|c\td", True), (3, 1, ",", True)])

d = b'id,quote\n1,"She said ""hi"""\n2,""""\n3,""\n4,"""quoted"" start"\n'
case("dialect/quoted-escaped-quotes.csv", d, 'Doubled quotes ("") inside quoted fields.',
     rows=5, fields=[2] * 5,
     cells=[(1, 1, 'She said "hi"', True), (2, 1, '"', True), (3, 1, "", True),
            (4, 1, '"quoted" start', True)])

# ---------------------------------------------------------------------------
# Diagnostics (DESIGN §3.5)
# ---------------------------------------------------------------------------

d = b'id,note\n1,ok\n2,"never closed\n3,lost\n'
case("diagnostics/unterminated-quote.csv", d,
     "A quote opened and never closed: the rest of the file is one field. The final "
     "LF is inside it, so structurally there is no trailing newline.",
     rows=3, fields=[2] * 3, trailing_newline=False,
     diagnostics=[("unterminated_quote", [(2, at(d, b'"never'))])],
     cells=[(2, 1, "never closed\n3,lost\n", True)])

d = b"a,b,c\n1,2,3\n4,5\n6,7,8,9\n10,11,12\n"
case("diagnostics/ragged-rows.csv", d, "One short row and one long row.",
     rows=5, fields=[3, 3, 2, 4, 3],
     diagnostics=[("ragged_rows", [(2, at(d, b"4,5")), (3, at(d, b"6,7"))])])

d = b'id,name\n1,"Ada"x\n2,"Bob" \n3,"Cy"d"e\n'
case("diagnostics/text-after-closing-quote.csv", d,
     "Text after a closing quote is kept in the field. A quote in that text is literal.",
     rows=4, fields=[2] * 4,
     diagnostics=[("text_after_closing_quote", [
         (1, at(d, b'"Ada"', plus=5)),
         (2, at(d, b'"Bob"', plus=5)),
         (3, at(d, b'"Cy"', plus=4)),
     ])],
     cells=[(1, 1, "Adax", True), (2, 1, "Bob ", True), (3, 1, 'Cyd"e', True)])

d = ("name,city,visits\nJosé,Málaga,3\n".encode("utf-8") + b"Ren\xe9,Paris,5\n"
     + "Zoë,Köln,2\n".encode("utf-8") + b"bad\xff\xfe,x,1\n")
case("diagnostics/invalid-utf8.csv", d,
     "Mostly valid UTF-8 (so detected as UTF-8) with three invalid bytes: a Latin-1 é, "
     "then 0xFF 0xFE (each its own invalid sequence).",
     rows=5, fields=[3] * 5,
     diagnostics=[("invalid_encoding", [
         (2, at(d, b"\xe9")), (4, at(d, b"\xff")), (4, at(d, b"\xfe")),
     ])],
     cells=[(2, 0, "Ren�", False), (4, 0, "bad��", False),
            (1, 1, "Málaga", False)])

d = b"id,value\n1,ab\x00cd\n2,\x00\n3,ok\n"
case("diagnostics/nul-bytes.csv", d, "Two NUL bytes inside fields.",
     rows=4, fields=[2] * 4,
     diagnostics=[("nul_bytes", [(1, at(d, b"\x00")), (2, at(d, b"\x00", nth=1))])],
     cells=[(1, 1, "ab\x00cd", False)])

d = b"id,v\n1,a\r\n2,b\n3,c\r4,d\n"
case("diagnostics/mixed-line-endings.csv", d,
     "LF is most common; one CRLF row and one CR row differ from it.",
     rows=5, fields=[2] * 5, mixed=True,
     diagnostics=[("mixed_line_endings", [(1, at(d, b"1,a", plus=3)),
                                          (3, at(d, b"3,c", plus=3))])])

d = b"id,v\n1,a\n\n2,b\n\n\n3,c\n"
case("diagnostics/blank-lines.csv", d,
     "Blank lines in the middle. A blank line is a row with one empty field, and is "
     "not counted as ragged.",
     rows=7, fields=[2, 2, 1, 2, 1, 1, 2],
     diagnostics=[("blank_lines", [(2, at(d, b"1,a\n", plus=4)),
                                   (4, at(d, b"2,b\n", plus=4)),
                                   (5, at(d, b"2,b\n", plus=5))])])

d = b"id,v\n1,a\n\n"
case("diagnostics/blank-line-at-end.csv", d,
     "A blank line at the end: the first LF after 1,a ends that row, the second ends "
     "a blank row.",
     rows=3, fields=[2, 2, 1],
     diagnostics=[("blank_lines", [(2, at(d, b"1,a\n", plus=4))])])

d = BOM_UTF8 + b"id,v\n1,a\n"
case("diagnostics/bom-present.csv", d, "A UTF-8 BOM (info only).",
     rows=2, fields=[2, 2], bom="utf-8", diagnostics=[bom_present()])

# ---------------------------------------------------------------------------
# Imitation exports. These are stand-ins written to match each tool's
# documented or commonly observed output. They are NOT real exports; real
# ones will be added later under their own names.
# ---------------------------------------------------------------------------

d = BOM_UTF8 + ("Name,Amount,Date,Notes\r\n"
                'Ada Lovelace,1234.5,03/01/2026,"Said ""hello"""\r\n'
                'José Núñez,-12,04/01/2026,"Line one\nLine two"\r\n'
                ",0,05/01/2026,\r\n").encode("utf-8")
case("exports/imitation-excel-utf8-bom.csv", d,
     'Imitation of Excel "CSV UTF-8": BOM, CRLF rows, LF inside quoted cells, minimal '
     "quoting, empty cells unquoted.",
     rows=4, fields=[4] * 4, line_ending="crlf", bom="utf-8", diagnostics=[bom_present()],
     cells=[(1, 3, 'Said "hello"', True), (2, 3, "Line one\nLine two", True),
            (3, 0, "", False), (2, 0, "José Núñez", False)])

d = ("Name,Amount,Notes\r\n"
     "Café Crème,3.5,“special”\r\n"
     "Müller,12,€ price\r\n"
     '"Smith, J",7,\r\n').encode("cp1252")
case("exports/imitation-excel-windows-1252.csv", d,
     'Imitation of Excel "CSV (Windows)": Windows-1252, no BOM, CRLF.',
     rows=4, fields=[3] * 4, line_ending="crlf", encoding="windows-1252",
     cells=[(1, 0, "Café Crème", False), (1, 2, "“special”", False),
            (2, 2, "€ price", False), (3, 0, "Smith, J", True)])

d = ("Task,Owner,Done,Hours\r\n"
     "Write spec,Rob,TRUE,3\r\n"
     '"Review, then merge",Mark,FALSE,1.5\r\n'
     "Ship 🚀,Rob,FALSE,").encode("utf-8")
case("exports/imitation-google-sheets.csv", d,
     "Imitation of a Google Sheets download: UTF-8, no BOM, CRLF between rows and no "
     "newline after the last row.",
     rows=4, fields=[4] * 4, line_ending="crlf", trailing_newline=False,
     cells=[(2, 0, "Review, then merge", True), (3, 0, "Ship 🚀", False),
            (3, 3, "", False)])

d = ("Item;Price;Qty\n"
     "Kaffee;3,50;2\n"
     "Brötchen;0,80;6\n"
     '"Tee; grün";2,10;1\n').encode("utf-8")
case("exports/imitation-numbers.csv", d,
     "Imitation of an Apple Numbers export in a European locale: semicolons, decimal "
     "commas, UTF-8, LF.",
     rows=4, fields=[3] * 4, delimiter=";",
     cells=[(1, 1, "3,50", False), (3, 0, "Tee; grün", True)])

d = (",name,score,passed\n"
     "0,Ada,91.5,True\n"
     "1,Alan,,False\n"
     '2,"Hopper, Grace",88.0,True\n').encode("utf-8")
case("exports/imitation-pandas-to-csv.csv", d,
     "Imitation of pandas DataFrame.to_csv() defaults: an unnamed index column (empty "
     "header cell), NaN as an empty field, minimal quoting, LF.",
     rows=4, fields=[4] * 4,
     cells=[(0, 0, "", False), (2, 2, "", False), (3, 1, "Hopper, Grace", True)])

d = ("id,name,note,created_at\n"
     "1,Ada,,2026-01-03 10:00:00+00\n"
     '2,Alan,"",2026-01-04 11:30:00+00\n'
     '3,Grace,"multi\nline",2026-01-05 09:15:00+00\n').encode("utf-8")
case("exports/imitation-postgresql-copy.csv", d,
     "Imitation of PostgreSQL COPY ... TO STDOUT (FORMAT csv, HEADER): NULL is an "
     'empty unquoted field, an empty string is "", LF.',
     rows=4, fields=[4] * 4,
     cells=[(1, 2, "", False), (2, 2, "", True), (3, 2, "multi\nline", True)])


# ---------------------------------------------------------------------------
# Sidecar writer
# ---------------------------------------------------------------------------

def toml_str(s):
    out = ['"']
    for ch in s:
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 0x20 or ord(ch) == 0x7F:
            out.append(f"\\u{ord(ch):04X}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def toml_bool(b):
    return "true" if b else "false"


def mode_first(values):
    counts = {}
    for v in values:
        counts[v] = counts.get(v, 0) + 1
    best = None
    for v in values:  # first seen wins ties
        if best is None or counts[v] > counts[best]:
            best = v
    return best


def sidecar(c):
    lines = [
        "# Generated by tests/corpus/generate.py. Edit the script, not this file.",
        "# Conventions: tests/corpus/README.md.",
        f"description = {toml_str(c['description'])}",
        "",
        "[dialect]",
        f"delimiter = {toml_str(c['delimiter'])}",
        f"line_ending = {toml_str(c['line_ending'])}",
        f"mixed_line_endings = {toml_bool(c['mixed'])}",
        f"bom = {toml_str(c['bom'])}",
        f"encoding = {toml_str(c['encoding'])}",
        f"trailing_newline = {toml_bool(c['trailing_newline'])}",
        f"header = {toml_bool(c['header'])}",
        "",
        "[rows]",
        f"count = {c['rows']}",
    ]
    fields = c["fields"]
    if len(fields) <= 3:
        lines.append(f"fields = [{', '.join(str(n) for n in fields)}]")
    else:
        mode = mode_first(fields)
        lines.append(f"fields_mode = {mode}")
        exceptions = [(i, n) for i, n in enumerate(fields) if n != mode]
        if exceptions:
            lines.append("fields_exceptions = [")
            lines += [f"  {{ row = {i}, fields = {n} }}," for i, n in exceptions]
            lines.append("]")
    for kind, locations in c["diagnostics"]:
        lines += [
            "",
            "[[diagnostics]]",
            f"kind = {toml_str(kind)}",
            f"count = {len(locations)}",
            "first = [",
        ]
        lines += [f"  {{ row = {r}, offset = {o} }}," for r, o in locations]
        lines.append("]")
    for row, field, value, quoted in c["cells"]:
        lines += ["", "[[cells]]", f"row = {row}", f"field = {field}",
                  f"value = {toml_str(value)}"]
        if quoted is not None:
            lines.append(f"quoted = {toml_bool(quoted)}")
    return ("\n".join(lines) + "\n").encode("utf-8")


def main():
    check = "--check" in sys.argv[1:]
    paths = [c["path"] for c in CASES]
    assert len(paths) == len(set(paths)), "duplicate case path"
    stale = []
    for c in CASES:
        for rel, content in [(c["path"], c["data"]),
                             (c["path"] + ".expected.toml", sidecar(c))]:
            path = ROOT / rel
            if check:
                if not path.exists() or path.read_bytes() != content:
                    stale.append(rel)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
    if check:
        if stale:
            print("out of date (run tests/corpus/generate.py):", *stale, sep="\n  ")
            sys.exit(1)
        print(f"all {len(CASES)} corpus cases match")
    else:
        print(f"wrote {len(CASES)} corpus cases ({2 * len(CASES)} files)")


if __name__ == "__main__":
    main()
