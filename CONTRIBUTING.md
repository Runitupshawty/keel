# Contributing to Keel

Thank you for helping improve Keel.

1. Fork [the repository](https://github.com/Runitupshawty/keel) and create a focused branch from `main`.
2. Make one logical change per pull request.
3. Add or update tests for behavior changes. New filesystem, search, preview, remote, or cloud providers must include provider tests.
4. Before opening the pull request, run:

   ```powershell
   cargo fmt --all --check
   cargo clippy --all-targets -- -D warnings
   cargo test --workspace
   ```

5. Explain the problem, the solution, and how you verified it in the pull request description.

Do not commit machine-specific configuration, hostnames, IP addresses, usernames, credentials, access tokens, or OAuth client secrets. Personal profiles belong under `%APPDATA%\Keel`, outside the repository.

## Screenshots

The pictures in `docs/screenshots/` (used by the README and [docs/getting-started.md](docs/getting-started.md)) are rendered by the real UI on a fixture folder, through the egui_kittest harness on wgpu. They need a GPU, so the test is `#[ignore]`d and CI does not run it. To make them again after a UI change:

```sh
scripts/fetch-deps.sh        # once (scripts/fetch-deps.ps1 on Windows): pdfium for the PDF preview
scripts/screenshots.sh       # or: cargo test -p keel-app --bin keel docs_screenshots -- --ignored --nocapture
```

The test writes its fixture to `C:\Keel demo` (Windows) or `/tmp/Keel demo`, or `KEEL_SHOTS_ROOT`, keeps its settings, library, search index and device identity there, and deletes it afterwards; it never uses your own Keel configuration or library, and its search covers only the fixture (it never indexes your home folder). It prints each picture it wrote and any it had to skip. Open every changed PNG before committing it: it must show only the fixture.

## QA walkthrough

`crates/keel-app/src/walkthrough_tests.rs` drives the window the way a user does, through the same egui_kittest harness, on the screenshot fixture: typing a path, tabs, selection, previews, copy and its conflicts, rename, archives, every palette command, the guide's keys, every Settings page, the library, pairing and the Recycle Bin. Each flow asserts what the user sees. The fast flows run with `cargo test -p keel-app`; the slow ones are `#[ignore]`d. To run them all with a 1280x800 PNG per flow in `target/walkthrough/` (needs a GPU):

```sh
scripts/fetch-deps.sh        # once: pdfium for the PDF preview (ffmpeg on the PATH for the video one)
scripts/walkthrough.sh       # or one flow: scripts/walkthrough.sh copy_conflicts
```

Settings, the library, the search index and the device identity stay in temp folders; no flow uses the keychain, your Keel configuration or your home folder. The palette and keyboard flows use the system clipboard, and on Windows the Recycle Bin flow moves one file of its own to the Recycle Bin, restores it and deletes it from there (it filters the Recycle Bin's view to that file). Look at the PNGs after a UI change; [docs/qa/](docs/qa/) keeps what a walkthrough found and what became of it.
