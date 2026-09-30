# Leal

A fast, low-memory CSV viewer and editor for macOS that never changes the bytes
you didn't edit.

> **Status:** pre-alpha, in design. Nothing to download yet.

- **Faithful:** saving changes only the cells you edited. Quoting, line
  endings, encoding and odd formatting are left exactly as they were.
- **Fast:** comfortable with 100 MB files and a million rows.
- **Light:** the file isn't copied into memory; Leal reads it in place.
- **Honest about messy files:** irregular rows, stray quotes and bad encoding
  are shown as they are, with a clear warning, and never silently "fixed".
- **Filter and sort** without reordering the file.

*Leal* is an old Scots and English word for *faithful*.

## Development

See [`docs/DESIGN.md`](docs/DESIGN.md) and [`docs/PLAN.md`](docs/PLAN.md).

Requirements: macOS 14+, Xcode, Rust via rustup, XcodeGen, just.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.
