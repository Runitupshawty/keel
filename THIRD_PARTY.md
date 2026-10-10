# Third-party components

Keel uses the following separately distributed components. Their licenses remain with their respective authors.

| Component | Use | License | Distribution |
| --- | --- | --- | --- |
| `iroh`, `iroh-base`, `iroh-relay`, `iroh-dns` 1.3 | Encrypted device transport, relay fallback and public address lookup | MIT OR Apache-2.0 | Rust crates, compiled in |
| `noq`, `noq-proto`, `noq-udp` 1.3 | iroh's QUIC implementation | MIT OR Apache-2.0 | Rust crates, compiled in |
| `ciborium` 0.2 | Network protocol CBOR headers | Apache-2.0 | Rust crate, compiled in |
| `async-trait` 0.1 | Async network Handler interface | MIT OR Apache-2.0 | Rust proc macro, build dependency |
| `data-encoding` 2 | Public identity and pairing ticket base32 | MIT | Rust crate, compiled in |
| `tokio-util` 0.7 | Network cancellation and task tracking | MIT | Rust crate, compiled in |
| `serde_bytes` 0.11 | Content ids as CBOR byte strings in the network protocol | MIT OR Apache-2.0 | Rust crate, compiled in |
| `qrcode` 0.14 | Device pairing QR codes (no image renderers) | MIT OR Apache-2.0 | Rust crate, compiled in |
| [Everything SDK](https://www.voidtools.com/support/everything/sdk/) by voidtools | Fast Windows file search | MIT | Downloaded by `scripts/fetch-deps.ps1`; `Everything64.dll` is placed in `target/deps/` and the SDK license text in `target/deps/licenses/everything/` (the `Everything.h` header, which carries the MIT notice, when the SDK zip has no separate license file) |
| [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) / PDFium | PDF rendering | PDFium: BSD 3-Clause; pdfium-binaries build scripts: MIT | Downloaded (pinned to `chromium/7543`) by `scripts/fetch-deps.ps1` / `fetch-deps.sh`; `pdfium.dll` (Windows), `libpdfium.dylib` (macOS) or `libpdfium.so` (Linux) is placed in `target/deps/`, and its license bundle is preserved in `target/deps/licenses/pdfium/` for releases |
| [Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme) by Material Extensions (repository `main` and npm `material-icon-theme` 5.39.0, fetched 2026-10-08) | Default file and folder icons | MIT | Committed as SVG in `assets/icons/default/` with its license in `assets/icons/default/LICENSE`; embedded in the binary. Files: audio, c, console, cpp, csharp, css, database, document, exe, file, folder-open, folder, font, git, go, html, image, java, javascript, json, lock, log, markdown, pdf, powerpoint, python, rust, settings, svg, table, toml, typescript, video, word, xml, yaml, zip |
| [libunrar](https://www.rarlab.com/rar_add.htm) via the `unrar_sys` crate 0.5 (crate: MIT OR Apache-2.0) | Read-only RAR listing and extraction (`keel-vfs` feature `rar`, on by default) | UnRAR license (free use; may not be used to re-create the RAR compression algorithm) | Compiled from source by `unrar_sys` and linked statically; never used to create RAR archives |
| [`sevenz-rust2`](https://crates.io/crates/sevenz-rust2) 0.23 (replaces the unmaintained `sevenz-rust`; with `lzma-rust2` 0.21) | 7z reading and writing | Apache-2.0 | Rust crate, compiled in |
| [`zip`](https://crates.io/crates/zip) 2.4 | zip reading, Add to zip and Office/OpenDocument previews | MIT | Rust crate, compiled in |
| [`tar`](https://crates.io/crates/tar) 0.4 | tar reading and writing | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`lzma-rs`](https://crates.io/crates/lzma-rs) 0.3 | `.tar.xz` decompression | MIT | Rust crate, compiled in |
| [`ruzstd`](https://crates.io/crates/ruzstd) 0.8 | `.tar.zst` decompression | MIT | Rust crate, compiled in |
| [`portable-pty`](https://crates.io/crates/portable-pty) 0.9 | Pseudo-terminal for the terminal pane | MIT | Rust crate, compiled in |
| [`vt100`](https://crates.io/crates/vt100) 0.16 | Terminal escape-sequence parser and screen grid | MIT | Rust crate, compiled in |
| [`russh`](https://crates.io/crates/russh) 0.64 | SSH client for SFTP remotes | Apache-2.0 | Rust crate, compiled in |
| [`ring`](https://crates.io/crates/ring) 0.17 | SSH ciphers and key exchange (russh's crypto backend) | Apache-2.0 AND ISC | Rust crate with C and assembly, compiled in |
| [`russh-sftp`](https://crates.io/crates/russh-sftp) 3.0 | SFTP protocol for remotes | Apache-2.0 | Rust crate, compiled in |
| [`keyring`](https://crates.io/crates/keyring) 3.6 | Passwords and passphrases in the OS keychain | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`rfd`](https://crates.io/crates/rfd) 0.15 | Native folder and file pickers | MIT | Rust crate, compiled in |
| [`clap`](https://crates.io/crates/clap) 4.6 | Command-line parsing | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`interprocess`](https://crates.io/crates/interprocess) 2.4 | Local socket / named pipe for the single instance and `keel-daemon` | 0BSD OR Apache-2.0 | Rust crate, compiled in |
| [`tungstenite`](https://crates.io/crates/tungstenite) 0.30 | Optional `keel-daemon --ws` JSON-RPC WebSocket (no TLS features) | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`ctrlc`](https://crates.io/crates/ctrlc) 3.5 | `keel-daemon` graceful shutdown on Ctrl-C / SIGTERM | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`schemars`](https://crates.io/crates/schemars) 1.2 | JSON schemas of the API operations (`keel-api`: JSON-RPC, CLI, MCP tools) | MIT | Rust crate, compiled in |
| [`global-hotkey`](https://crates.io/crates/global-hotkey) 0.8 | System-wide hotkey | Apache-2.0 OR MIT | Rust crate, compiled in |
| [`rusqlite`](https://crates.io/crates/rusqlite) 0.37 | Search index and library stores, with FTS5 (bundled SQLite, which is public domain) | MIT | Rust crate, compiled in |
| [`blake3`](https://crates.io/crates/blake3) 1 | Content ids for the duplicate finder (pure-Rust build) | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | Rust crate, compiled in |
| [`notify`](https://crates.io/crates/notify) 6 | Live file watching for library sources | CC0-1.0 | Rust crate, compiled in |
| [`ignore`](https://crates.io/crates/ignore) 0.4 | Directory walking | Unlicense OR MIT | Rust crate, compiled in |
| [`starship-battery`](https://crates.io/crates/starship-battery) 0.12 | Pause hashing on battery power (`keel-core` feature `power`, on by default) | ISC | Rust crate, compiled in |
| [`regex`](https://crates.io/crates/regex) 1.13 | Regex search queries | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`encoding_rs`](https://crates.io/crates/encoding_rs) 0.8 | Windows-1252 text decoding | (Apache-2.0 OR MIT) AND BSD-3-Clause | Rust crate, compiled in |
| [`calamine`](https://crates.io/crates/calamine) 0.36 | Spreadsheet previews (xlsx, xls, xlsb, ods) | MIT | Rust crate, compiled in |
| [`reflink-copy`](https://crates.io/crates/reflink-copy) 0.1 | Copy-on-write file copies on macOS and Linux | MIT/Apache-2.0 | Rust crate, compiled in |
| [`roxmltree`](https://crates.io/crates/roxmltree) 0.19 | XML parsing for the icon theme SVG check | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`zeroize`](https://crates.io/crates/zeroize) 1.9 | Wipes secrets from memory | Apache-2.0 OR MIT | Rust crate, compiled in |
| [`fs4`](https://crates.io/crates/fs4) 0.13 | Free-space checks before copy and extract; keel-net data-directory lock | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`kamadak-exif`](https://crates.io/crates/kamadak-exif) 0.6 | EXIF metadata for media sidecars | BSD-2-Clause | Rust crate, compiled in |
| [`quick-xml`](https://crates.io/crates/quick-xml) 0.42 | XMP metadata for media sidecars; pptx and OpenDocument preview text | MIT | Rust crate, compiled in |
| [`quick-xml`](https://crates.io/crates/quick-xml) 0.41 | WebDAV listings (via `opendal-service-webdav`, cloud feature) | MIT | Rust crate, compiled in |
| [`http`](https://crates.io/crates/http) 1 | Requests of chunked cloud uploads (cloud feature; already used by reqwest and opendal) | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`cpal`](https://crates.io/crates/cpal) 0.18 | Sound output for video playback in the media viewer (WASAPI on Windows, CoreAudio on macOS) | Apache-2.0 | Rust crate, compiled in; on Linux it uses ALSA through `alsa` (Apache-2.0 OR MIT) and `alsa-sys` (MIT), which links the system's `libasound` (build: `libasound2-dev`) |
| [`wait-timeout`](https://crates.io/crates/wait-timeout) 0.2 | Time limit for `ffmpeg`/`ffprobe` runs (video sidecars; ffmpeg is not shipped, it is used when installed) | MIT OR Apache-2.0 | Rust crate, compiled in |
| [`libheif-rs`](https://crates.io/crates/libheif-rs) 2 / libheif | HEIC thumbnails (`keel-core` feature `heic`, off by default) | crate: MIT; libheif: LGPL-3.0 | Not in release builds (feature off); enabling it links libheif |
| [`winfsp`](https://crates.io/crates/winfsp) 0.13 and `winfsp-sys` 0.12 (winfsp-rs) / [WinFsp](https://winfsp.dev) 2.1 | Mounts on Windows (`keel-mount` / `keel-daemon` feature `winfsp`, off by default, on in the Windows release build of `keel-daemon`) | winfsp-rs: GPL-3.0; WinFsp: GPL-3.0 with a FLOSS exception | Compiled into the Windows release's `keel-daemon.exe`, which is therefore distributed under the GPL-3.0 (the license text ships as `licenses/winfsp-rs/COPYING`; the corresponding source is this repository at the release tag, with winfsp-rs from crates.io). `keel.exe` is built separately without it. The WinFsp DLL is not shipped: it is loaded from the user's WinFsp installation |
| [`fuser`](https://crates.io/crates/fuser) 0.18 | Mounts on Linux and macOS (`keel-mount` / `keel-daemon` feature `fuse`, off by default, on in the Linux release build of `keel-daemon`) | MIT | Compiled into the Linux release's `keel-daemon` (not macOS); uses the system's FUSE (`fusermount3`) or macFUSE, not shipped |

**User-installed icon themes.** Settings → Icons can download a VS Code icon theme extension from the Visual Studio Marketplace at the user's request, after showing the extension's license (from the package when it has one) and asking the user to accept it. The theme is unpacked into the user's own config folder (`<config dir>/icons/<extension id>/`) for that user only. Keel does not bundle, mirror or redistribute these themes; they are not part of Keel's source or release archives, and their licenses are between the user and the theme's author.

Licenses above are those declared in each crate's `Cargo.toml` at the version pinned in `Cargo.lock`. The `two-face` (MIT) syntax definitions and the `trash` and `sysinfo` crates are MIT/Apache-licensed like the rest of the Rust dependencies.

Release archives (`keel-<version>-<platform>.zip` / `.tar.gz`) ship these libraries next to the binary together with `licenses/` (the notices above), `LICENSE-MIT`, `LICENSE-APACHE` and this file; the release job fails when a library or its notice is missing.

Rust crate dependencies and their license metadata are recorded by Cargo and will be included in release attribution as the dependency set stabilizes.
