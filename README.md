# Leal

A fast, low-memory CSV viewer and editor for macOS that never changes the bytes
you didn't edit.

> **Status:** pre-alpha, being built (foundations done, the viewer is next).
> Nothing to download yet.

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

Requirements: Xcode 27 (CI pins 27.0; older versions aren't tested), Rust
via rustup, XcodeGen, just and cargo-nextest. The app runs on macOS 14 or
later. From a fresh clone:

```sh
brew install just xcodegen cargo-nextest
rustup toolchain install   # the toolchain and targets in rust-toolchain.toml
just run                   # build the Rust library, bindings and Leal.app, then launch it
```

`just check` runs the Rust checks (including rustdoc with warnings as
errors), `just check-all` adds the app's tests, and `just` lists every
recipe. How the build fits together is described in
[`docs/tasks/0.3.md`](docs/tasks/0.3.md).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.
