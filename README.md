# Keel

Keel is an open-source Windows file manager built in Rust with egui and a wgpu renderer. It is designed for fast everyday local file management, rich previews, and a path toward remote and cloud storage without telemetry.

## Phase 1 features

- Local filesystem browsing with dual panes, tabs, a sidebar, and details or grid views
- Everything-powered search, a fuzzy folder jump, and filter-by-typing
- Previews for code, images, PDFs, CSV files, DOCX documents, and video thumbnails
- Copy, move, delete, and rename jobs with progress, plus Windows Open with support
- Dark and light themes with an icon theme
- Crash recovery and an installer-free Windows executable

## Build

Install stable Rust with the `x86_64-pc-windows-msvc` target and the Microsoft C++ build tools, then run:

```powershell
pwsh scripts/fetch-deps.ps1
cargo run --release
```

## Roadmap

- Phase 2: archive browsing and an embedded terminal
- Phase 3: SFTP remotes and transfer jobs
- Phase 4: Google Drive, Dropbox, and S3-compatible cloud providers
- Phase 5: profiles, icon-theme management, Miller columns, a drop zone, Explorer drag-out, animations, a global hotkey, and a native NTFS indexer

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

Keel is available under either the MIT License or the Apache License 2.0, at your option.
