# Third-party components

Keel uses the following separately distributed components. Their licenses remain with their respective authors.

| Component | Use | License | Distribution |
| --- | --- | --- | --- |
| [Everything SDK](https://www.voidtools.com/support/everything/sdk/) by voidtools | Fast Windows file search | MIT | Downloaded by `scripts/fetch-deps.ps1`; `Everything64.dll` is placed in `target/deps/` and the SDK license text in `target/deps/licenses/everything/` (the `Everything.h` header, which carries the MIT notice, when the SDK zip has no separate license file) |
| [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) / PDFium | PDF rendering | PDFium: BSD 3-Clause; pdfium-binaries build scripts: MIT | Downloaded (pinned to `chromium/7543`) by `scripts/fetch-deps.ps1` / `fetch-deps.sh`; `pdfium.dll` (Windows), `libpdfium.dylib` (macOS) or `libpdfium.so` (Linux) is placed in `target/deps/`, and its license bundle is preserved in `target/deps/licenses/pdfium/` for releases |
| [Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme) by Material Extensions (repository `main` and npm `material-icon-theme` 5.39.0, fetched 2026-10-08) | Default file and folder icons | MIT | Committed as SVG in `assets/icons/default/` with its license in `assets/icons/default/LICENSE`; embedded in the binary. Files: audio, c, console, cpp, csharp, css, database, document, exe, file, folder-open, folder, font, git, go, html, image, java, javascript, json, lock, log, markdown, pdf, powerpoint, python, rust, settings, svg, table, toml, typescript, video, word, xml, yaml, zip |
| [libunrar](https://www.rarlab.com/rar_add.htm) via the `unrar_sys` crate 0.5 (crate: MIT OR Apache-2.0) | Read-only RAR listing and extraction (`keel-vfs` feature `rar`, on by default) | UnRAR license (free use; may not be used to re-create the RAR compression algorithm) | Compiled from source by `unrar_sys` and linked statically; never used to create RAR archives |
| [`sevenz-rust`](https://crates.io/crates/sevenz-rust) 0.6 | 7z reading | Apache-2.0 | Rust crate, compiled in |
| [`zip`](https://crates.io/crates/zip) 2.4 | zip reading and Add to zip | MIT | Rust crate, compiled in |
| [`tar`](https://crates.io/crates/tar) 0.4 | tar reading | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`lzma-rs`](https://crates.io/crates/lzma-rs) 0.3 | `.tar.xz` decompression | MIT | Rust crate, compiled in |
| [`ruzstd`](https://crates.io/crates/ruzstd) 0.8 | `.tar.zst` decompression | MIT | Rust crate, compiled in |
| [`portable-pty`](https://crates.io/crates/portable-pty) 0.9 | Pseudo-terminal for the terminal pane | MIT | Rust crate, compiled in |
| [`vt100`](https://crates.io/crates/vt100) 0.16 | Terminal escape-sequence parser and screen grid | MIT | Rust crate, compiled in |
| [`russh`](https://crates.io/crates/russh) 0.50 | SSH client for SFTP remotes | Apache-2.0 | Rust crate, compiled in |
| [`russh-sftp`](https://crates.io/crates/russh-sftp) 2.4 | SFTP protocol for remotes | Apache-2.0 | Rust crate, compiled in |
| [`keyring`](https://crates.io/crates/keyring) 3.6 | Passwords and passphrases in the OS keychain | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`rfd`](https://crates.io/crates/rfd) 0.15 | Native folder and file pickers | MIT | Rust crate, compiled in |

Licenses above are those declared in each crate's `Cargo.toml` at the version pinned in `Cargo.lock`. The `two-face` (MIT) syntax definitions and the `trash` and `sysinfo` crates are MIT/Apache-licensed like the rest of the Rust dependencies.

Release archives (`keel-<version>-<platform>.zip` / `.tar.gz`) ship these libraries next to the binary together with `licenses/` (the notices above), `LICENSE-MIT`, `LICENSE-APACHE` and this file; the release job fails when a library or its notice is missing.

Rust crate dependencies and their license metadata are recorded by Cargo and will be included in release attribution as the dependency set stabilizes.
