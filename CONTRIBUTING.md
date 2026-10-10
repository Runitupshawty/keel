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
