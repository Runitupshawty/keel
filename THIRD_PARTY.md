# Third-party components

Keel uses or plans to use the following separately distributed components. Their licenses remain with their respective authors.

| Component | Use | License | Distribution |
| --- | --- | --- | --- |
| [Everything SDK](https://www.voidtools.com/support/everything/sdk/) by voidtools | Fast Windows file search | Freeware | Downloaded by `scripts/fetch-deps.ps1`; `Everything64.dll` is placed in `target/deps/` |
| [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) / PDFium | PDF rendering | BSD 3-Clause and upstream notices | Downloaded by `scripts/fetch-deps.ps1`; `pdfium.dll` is placed in `target/deps/`, and its license bundle is preserved in `target/deps/licenses/pdfium/` for releases |
| [libunrar](https://www.rarlab.com/rar_add.htm) | Read-only RAR support planned for Phase 2 | UnRAR license | Not bundled in Phase 1 |

Rust crate dependencies and their license metadata are recorded by Cargo and will be included in release attribution as the dependency set stabilizes.
