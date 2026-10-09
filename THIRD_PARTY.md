# Third-party components

Keel uses or plans to use the following separately distributed components. Their licenses remain with their respective authors.

| Component | Use | License | Distribution |
| --- | --- | --- | --- |
| [Everything SDK](https://www.voidtools.com/support/everything/sdk/) by voidtools | Fast Windows file search | MIT | Downloaded by `scripts/fetch-deps.ps1`; `Everything64.dll` is placed in `target/deps/` and the SDK license text in `target/deps/licenses/everything/` |
| [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) / PDFium | PDF rendering | PDFium: BSD 3-Clause; pdfium-binaries build scripts: MIT | Downloaded (pinned to `chromium/7543`) by `scripts/fetch-deps.ps1` / `fetch-deps.sh`; `pdfium.dll` (Windows), `libpdfium.dylib` (macOS) or `libpdfium.so` (Linux) is placed in `target/deps/`, and its license bundle is preserved in `target/deps/licenses/pdfium/` for releases |
| [Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme) by Material Extensions (repository `main` and npm `material-icon-theme` 5.39.0, fetched 2026-10-08) | Default file and folder icons | MIT | Committed as SVG in `assets/icons/default/` with its license in `assets/icons/default/LICENSE`; embedded in the binary. Files: audio, c, console, cpp, csharp, css, database, document, exe, file, folder-open, folder, font, git, go, html, image, java, javascript, json, lock, log, markdown, pdf, powerpoint, python, rust, settings, svg, table, toml, typescript, video, word, xml, yaml, zip |
| [libunrar](https://www.rarlab.com/rar_add.htm) | Read-only RAR support planned for Phase 2 | UnRAR license | Not bundled in Phase 1 |

The `two-face` (MIT) syntax definitions and the `trash` and `sysinfo` crates are MIT/Apache-licensed like the rest of the Rust dependencies.

Rust crate dependencies and their license metadata are recorded by Cargo and will be included in release attribution as the dependency set stabilizes.
