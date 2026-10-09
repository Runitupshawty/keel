# Keel

Keel is an open-source, cross-platform file manager written in Rust (egui + wgpu), aiming for fast local file management, rich previews, and later remote and cloud storage, with no telemetry.

## Status

Keel is in development and not usable yet.

- Working, with tests: `keel-vfs` (local filesystem provider), `keel-search` (per-OS search backends), `keel-preview` (code, image, PDF, CSV, DOCX previews; video thumbnails).
- The `keel-app` window is a stub until the UI work (plan Task 5) lands.

## Build

Common to all platforms: install stable Rust with [rustup](https://rustup.rs). CI builds and tests on Windows, macOS and Linux.

**Windows**: install the Visual Studio Build Tools (C++ workload), then:

```powershell
pwsh scripts/fetch-deps.ps1
cargo build --workspace
```

**macOS**:

```sh
scripts/fetch-deps.sh
cargo build --workspace
```

**Linux**:

```sh
sudo apt-get install libgtk-3-dev libxkbcommon-dev libwayland-dev libasound2-dev
scripts/fetch-deps.sh
cargo build --workspace
```

`fetch-deps` downloads the runtime libraries (pdfium, and Everything on Windows) into `target/deps/`; the build copies them next to the binary. Run the tests with `cargo test --workspace`.

## Search backends

| OS | Backend |
| --- | --- |
| Windows | Everything (voidtools; must be running) |
| macOS | Spotlight via `mdfind` |
| Linux | `plocate` or `locate` if installed, otherwise a directory walk |

## Optional

`ffmpeg` on `PATH` enables video thumbnails; without it, videos show no thumbnail.

## Roadmap

1. Phase 1: usable MVP (dual pane, tabs, search, previews, file jobs)
2. Phase 2: archives as folders and an embedded terminal
3. Phase 3: SFTP remotes
4. Phase 4: Google Drive, Dropbox, S3/B2
5. Phase 5: polish (profiles, icon themes, Miller columns, global hotkey, own indexer)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either the MIT License or the Apache License 2.0, at your option. Third-party components: see [THIRD_PARTY.md](THIRD_PARTY.md).
