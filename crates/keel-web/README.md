# keel-web

Keel's browser client: an egui app compiled to `wasm32-unknown-unknown`, served by
`keel-daemon --web` from a bundle embedded at build time. See the main README ("Web client")
for how to build and use it.

## Decision: a purpose-built web app, not keel-app compiled to wasm

The Phase 9 plan prefers compiling keel-app itself to wasm with the local-only parts behind
`cfg(not(target_arch = "wasm32"))`. That was tried first and is not practical:

- `cargo check -p keel-vfs --no-default-features --target wasm32-unknown-unknown` fails in
  `mio` ("This wasm target is unsupported by mio"): keel-vfs needs tokio's network stack
  (SFTP through russh), plus `keyring`, `trash`, `notify`, `sysinfo` and `fs4`, none of
  which exist in a browser. Every UI module of keel-app holds a `keel_vfs::Router`.
- keel-core (the library) is SQLite (`rusqlite` with bundled C), jobs on threads and the
  file system; keel-api depends on it. In the browser the library lives in the daemon.
- keel-app's views are written against its `State` (2,900 lines) that owns the router,
  the library, the job queue, previews (pdfium, ffmpeg), the terminal and the watchers.
  Splitting that so views compile without them is a refactor of most of the app.

So `keel-web` is a small egui app over the daemon's JSON-RPC that mirrors the desktop's
layout instead of sharing its code:

| Desktop view | Web client |
| --- | --- |
| dual pane listing | two panes over `list` (library paths list from the index, offline too) |
| search tab | `search` with the same query language |
| preview panel | `media.thumb` for photos and videos, `preview.render` for the rest (text, PDF pages, documents), rendered by the daemon with the desktop previewers |
| library sidebar | `sources.list`; a source opens `library://<id>/` |
| jobs panel | `jobs.list`, refreshed on `job.progress` notifications |
| devices | `devices.list` (keel-net off: says so), the Spacedrop inbox (`spacedrop.inbox`, `spacedrop.answer`), Send to device (`spacedrop.send`) |
| file operations | rename / delete through `plan`, tags through `tags.add`: the preview is shown, `execute` confirms exactly that plan, and a job shows as done only when `jobs.info` says it finished |

What is shared: keel-api's parameter and result types (`crates/keel-api/src/types.rs`,
included by path, since keel-api itself cannot build for wasm), so the client decodes the
same structs the daemon encodes. The native `DaemonProvider` (a keel-vfs `Provider` over the
same JSON-RPC, for native clients) lives in keel-api.

## Layout

- `conn.rs`: the connection state machine (auth first, reconnect with backoff, token
  refused stops retrying), transport-agnostic and tested natively.
- `guard.rs`: refuses an address that carries a token (tested natively).
- `util.rs`: display helpers (tested natively).
- `layout.rs`: the phone / desktop breakpoint with hysteresis, the phone tab and the
  preview sheet (tested natively).
- `gesture.rs`: pull to refresh, swipes, pinch zoom and grid tile sizes (tested natively).
- `share.rs`: the share-sheet flow: the `?share=<id>` address, `share.claim` once signed
  in, the picked device, the `spacedrop.send` parameters (tested natively).
- `app.rs`, `web.rs`: the UI and the browser glue (WebSocket, `localStorage`, downloads);
  wasm32 only. CI builds and lints them for wasm32.
- `static/`: the page shell (`index.html`, `boot.js`, `keel.css`), the PWA manifest, the
  service worker (`sw.js`; the build script stamps its cache key with the wasm's hash) and
  the icons (`icon-192.png`, `icon-512.png`, rendered from `assets/keel.svg` by
  `cargo run -p keel-app --example gen_icon`), copied next to the wasm-bindgen output by
  `scripts/build-web.*`.
