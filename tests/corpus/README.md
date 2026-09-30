# Test corpus

Small, hand-made files that pin down how Leal reads each dialect and each
kind of messy input (DESIGN §3.2, §3.4, §3.5). Every data file has a
**sidecar**, `<file>.expected.toml`, saying what a parser should find in it.

The corpus is loaded by `leal_testkit::corpus::load()`. The tests in
`crates/leal-testkit/tests/corpus.rs` check that every data file has a
sidecar, every sidecar parses, and every sidecar agrees with a small
reference parser. Later tasks test the real detector (1.2), parser (1.4) and
diagnostics (1.5) against the same sidecars.

## Layout

```
tests/corpus/
├── README.md        this file (not data)
├── generate.py      writes every file below, byte for byte (not data)
├── dialect/         one file per delimiter, line ending, encoding and shape
├── diagnostics/     one file per diagnostic kind
└── exports/         imitations of real tools' exports
```

Every file in a subdirectory is data unless its name ends in
`.expected.toml`. Files directly in `tests/corpus/` and hidden files (such as
`.DS_Store`) are ignored.

The files in `exports/` are **imitations**, named `imitation-<tool>-...`.
They follow each tool's documented or commonly seen output, but were written
by `generate.py`, not exported from the tool. Real exports will be added
later under names starting with `real-`.

## Conventions

These follow DESIGN §3.4–§3.5 as clarified by ADR-0003.

- **Rows** are *physical rows* (records), numbered from 0. A newline inside a
  quoted field does not start a row.
- **Offsets** are byte offsets into the file as stored on disk, from 0,
  **including the BOM**, for every encoding, UTF-16 included. In a file with
  a UTF-8 BOM, row 0 starts at offset 3; with a UTF-16 BOM, at offset 2.
- **Text after a closing quote** (`"a"b`) is part of the field, and the field
  displays raw: its value is `"a"b`. Quotes in that text are literal, so
  `"a"b"c",` is one field with raw bytes `"a"b"c"`.
- **BOM.** A BOM belongs to no row. A quote straight after it opens a quoted
  first field.
- **End of file.** A line ending at the very end ends the last row; it does
  not start an empty one. An empty file, or one holding only a BOM, has no
  rows.
- **Trailing newline** means *the last row has a line ending*. A file whose
  last field is an unterminated quote has none, even if its last byte is a
  newline, because that newline is inside the field.
- **Blank lines.** A row with no bytes before its line ending is a blank line,
  with one empty field. Blank lines are reported as `blank_lines` wherever
  they are (including at the end), and are never counted as ragged. The
  final line ending is not a blank line: `a\nb\n` has two rows, `a\nb\n\n`
  three, the last blank.
- **Line ending** in the sidecar is the most common one; ties go to the one
  seen first. `none` means no row has a line ending.
- **Field-count mode** (for ragged rows) is the most common field count among
  non-blank rows; ties go to the count seen first.
- **Delimiter.** A file with no delimiter at all (one column, or empty)
  expects the default, `,`.
- **Header.** A single row, or rows that all look alike, have no header.
- **Encoding.** A BOM decides. Otherwise pure ASCII is UTF-8; a file with at
  least one valid multibyte UTF-8 sequence, and more of them than invalid
  bytes, is UTF-8 (with `invalid_encoding` if any bytes are invalid);
  anything else is Windows-1252. The corpus tests check every sidecar's
  `encoding` against this rule (`leal_testkit::dialect::expected_encoding`).
- **Windows-1252** follows the WHATWG mapping: every byte decodes, so
  Windows-1252 files never expect `invalid_encoding`.

## Sidecar format

```toml
description = "One short row and one long row."

[dialect]
delimiter = ","              # "," ";" "\t" "|"
line_ending = "lf"           # most common: "lf" "crlf" "cr", or "none"
mixed_line_endings = false
bom = "none"                 # "none" "utf-8" "utf-16le" "utf-16be"
encoding = "utf-8"           # "utf-8" "utf-16le" "utf-16be" "windows-1252"
trailing_newline = true
header = true                # what the header-row heuristic should decide

[rows]
count = 5
# Either every row's field count...
#   fields = [3, 3, 2, 4, 3]
# ...or the most common count plus the rows that differ:
fields_mode = 3
fields_exceptions = [
  { row = 2, fields = 2 },
  { row = 3, fields = 4 },
]

# One table per diagnostic kind the file has. Kinds not listed must NOT be
# reported.
[[diagnostics]]
kind = "ragged_rows"
count = 2                    # occurrences in the whole file
first = [                    # the first locations, in file order
  { row = 2, offset = 12 },
  { row = 3, offset = 16 },
]

# Optional: display values (unquoted, unescaped, decoded) of chosen cells.
[[cells]]
row = 1
field = 0
value = "1"
quoted = false               # optional
```

`first` may list fewer locations than `count` (DESIGN §3.5 keeps at most
1,000). A parser's locations must *start with* the listed ones. The files
here list every location.

Unknown keys are errors, so typos are caught.

### Diagnostic kinds

What counts as one occurrence (ADR-0003 decision 4), and which byte its
offset points at. The definitions live in
`leal_testkit::diagnostics::DiagnosticKind`.

| Kind | One occurrence per | Offset |
|---|---|---|
| `unterminated_quote` | file (the field whose quote never closes) | the opening quote |
| `ragged_rows` | non-blank row whose field count differs from the mode | row start |
| `text_after_closing_quote` | field with bytes after its closing quote | first such byte |
| `invalid_encoding` | field containing invalid UTF-8 (UTF-8 files only) | first invalid byte in the field |
| `nul_bytes` | field containing a NUL byte | first NUL in the field |
| `mixed_line_endings` | row whose line ending differs from the most common | first byte of that line ending |
| `blank_lines` | blank row | row start |
| `bom_present` | file with a BOM (at most one) | 0 (row 0) |

## Adding a file

**A hand-made file:** add a `case(...)` to `generate.py` with the exact bytes
and the expected results, then run:

```sh
python3 tests/corpus/generate.py          # writes the data file and its sidecar
just check                                # the corpus tests check the sidecar
```

Locate offsets with `at(data, b"needle")` rather than counting by hand. Keep
files tiny: the whole corpus must stay well under 1 MB (a test enforces it).

**A real export:** copy the file in unchanged, as `exports/real-<tool>-<variant>.csv`,
and write its sidecar by hand. Don't add it to `generate.py`. If it is
large, trim it to the rows that matter.

## Byte-exactness

`.gitattributes` marks everything here `-text`, so git never converts line
endings. Data files are also `binary` (no diffs); the sidecars, this README
and `generate.py` are diffable. To confirm the committed files match the
script:

```sh
python3 tests/corpus/generate.py --check
```
