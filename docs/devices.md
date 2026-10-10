# Devices, Spacedrop, web client and phones

Reference for pairing devices, shares, Spacedrop and library sync, and for the web client and phones. The [getting-started guide](getting-started.md#devices) introduces them.

## Devices and Spacedrop

Keel can talk directly to your other computers. There is no account and no server of ours: devices connect peer to peer over an encrypted link ([iroh](https://www.iroh.computer/)), using iroh's public relay servers only when a direct path is not possible. Devices are off until you turn them on in Settings → Devices; that creates this device's identity, kept in the OS keychain. (A configuration saved by an earlier 0.8 build, while Devices were on by default, is switched off once; turn them on again if you want them.) There you also set this device's name, the Spacedrop inbox folder (default `Downloads/Keel Drops`, or `<data dir>/inbox` without a Downloads folder; attached to keel-daemon, the daemon's `<data dir>/inbox` unless you set one) and which devices may send without asking.

**Pair two devices.**

1. On one device open the sidebar's **Devices** section and choose **Pair a device… → Show code**. Keel shows a short code and a QR code (the QR carries the full ticket).
2. On the other device choose **Pair a device… → Enter code** and type the short code, or paste the ticket.
3. Both sides now list each other under Devices with a status dot (direct, relay or offline), the device's name and a storage bar.

A code works for 10 minutes and for one pairing; showing a new code replaces the old one. Treat it like a password until it is used. Pairing grants nothing: a freshly paired device can see only its name until you share something. Each pair of devices keeps one connection, whichever side opened it.

**Share folders (grants).** **Shares…** on a device row (or the Devices menu) lists what you give that device. Add a grant for a whole source or for one folder inside it, as **Read** or **Read-write**. A grant covers the folder and everything under it, and **Revoke** takes effect at once: running transfers from that device are cut off and later requests are refused. Sources that come from another device are never re-shared. Forgetting a device ends its shares and removes its folders from the library.

**Browse a remote source.** **Browse** on a device opens a tab at `node://<device>/<source>/...`, titled with the device's name (and "<device> / <source>" in a shared folder); the breadcrumb and the tab's hover use the names too, and a device you forgot shows its id. It behaves like any other folder: listing, preview, copy and drag between panes. Add a device's source as a library source to index it and search it like local files; the content ids it reports are its word only and never count as copies for delete warnings or duplicates. Writes to a device land in its shared folders whether they are local folders, SFTP hosts or cloud accounts, with the same grant and path checks; they are checked with BLAKE3 and published whole: into a local folder through a staging file renamed into place, into an SFTP or cloud source streamed through that host's connection to the server and placed by its own upload only after the check (a read-only or unreachable source refuses the write). Sharing a source whose deletes are permanent (SFTP, S3) read-write says so in the preview.

**Spacedrop.** Drag files or folders onto a device in the sidebar, or use **Send with Spacedrop…** in a file's context menu. The receiver sees an accept prompt with the names, count and size (Accept, Decline, or "always accept from this device"). Files travel in resumable 4 MiB pieces and show up as a job in the jobs panel; if the link drops or either app restarts, the transfer continues where it stopped. Pieces are staged in a `.keel-partial-<id>` folder inside the inbox, each file is verified against a BLAKE3 hash of the whole file, and only then moved into the inbox (a name clash becomes `name (1).ext`, never an overwrite). A drop that already arrived is remembered for an hour, so a sender that lost the last reply finishes without asking you again or sending a second copy. Cancel from the jobs panel; a prompt whose sender gave up or stopped asking goes away by itself.

**Sync tags and favorites.** Your tags (names, colors, nesting), which files carry them, and your favorites can follow you between your own devices. In Settings → Devices each paired device has a **Sync library with this device** switch (off by default); turn it on on both devices. They then exchange changes when they connect and every minute (`[devices] sync_secs` in the profile's `config.toml`, 10 seconds to an hour), and the line under the switch says when this device last synced with that one; the library Overview says how many devices the tags are synced with. Only tags and favorites travel: sources, recents, saved views, settings, drives and jobs stay on each device, and so do the files themselves.

A tag on a file whose content has been hashed belongs to that content: on the other device it lands on every copy of the same bytes (hashed there too), wherever it is. A tag on a file that is not hashed yet is kept by its place on the device where you set it, and lands on the other device only in that device's folder added there as a library source (`node://`); once the first device hashes the file, the tag reaches the other device's copies of it too. Tags made on both devices with the same name (under the same parent) become one tag. When both devices change the same thing before they sync (a tag's color, a tag on one file), the later change wins on both, the same way, so they always agree; a deleted tag is remembered for 30 days so that an older copy of it never comes back. Turning the switch off, or forgetting the device, stops the exchange at once and keeps what already arrived. A device answers only devices it syncs with, never passes on what it received from a third one (each pair of devices that should sync needs its switches on), and takes at most 10,000 changes a minute from any device (the rest arrives in the next minutes). The other device learns the names and paths of the files you tag, including in folders you do not share with it. `library.sync` pulls at once (see [docs/api.md](api.md)).

**Security model, in plain words.**

- Only devices you paired can connect; everyone else is rejected before any request is read.
- Pairing reveals nothing about either device until the other side proves it knows the code, and the code works once.
- Every request is checked against your current grants, so a revoked or narrowed share applies even on an open connection. Paths that try to escape the shared folder (`..`, symlinks and junctions, Windows device names) are refused.
- Each peer is limited in connections and in concurrent requests, and a transfer that stalls is dropped after an idle timeout.
- Only one process may own a device store at a time, so grants cannot be changed behind the owner's back.
- Spacedrop needs your accept (or a standing auto-accept for that device), and the sender cannot choose where files land. One device can have at most 4 offers waiting (16 from all devices); more are refused as busy.
- Anyone holding a still-valid code can pair, so show it only to the person in front of you.

**Without internet.** The short code is found through iroh's internet discovery and, on the same network, through local network discovery (mDNS), so two devices on one Wi-Fi or LAN pair by short code with no internet, relay or DNS. A code shown while no relay answers within 5 seconds works on the same network only (the QR code and full ticket carry the device's addresses, as before). Paired devices also find each other on the same network after an address change. Local discovery advertises this device's id (and while a code is shown, the code's rendezvous id, which reveals nothing about the code) on the local network, as internet discovery already publishes them; it is on only while Devices are on. Where multicast is blocked (a firewall, some guest networks) Keel logs it once and pairs as before: by internet discovery or the full ticket.

Developer note: `KEEL_NET_SECRET=memory` keeps the device identity in memory instead of the keychain, in the window, `keel-daemon` and the `keel` subcommands alike (tests and live checks; the device is new on every run and must pair again).

## Web client

`keel-daemon --web` serves Keel in a browser: browse sources (offline ones from the index), search, preview text, images and PDF pages, tag, rename and delete (preview first, then execute, exactly as in the window), follow jobs, see your devices and the Spacedrop inbox, send files to a device, and download files. Narrower than 700 px (a phone) it switches to a phone layout; it installs as an app (see Phones).

```sh
scripts/build-web.sh                # once, or scripts\build-web.ps1: builds crates/keel-web/dist
cargo build --release -p keel-daemon  # embeds that bundle
keel-daemon --web                   # http://127.0.0.1:7421/ (or --web 127.0.0.1:PORT)
```

`build-web` needs `rustup target add wasm32-unknown-unknown` and `wasm-bindgen-cli` of the `wasm-bindgen` version in Cargo.lock (the script prints how to install it). A daemon built without the bundle serves a page saying so.

**The token.** The first visit asks for the token in `daemon.token` in Keel's configuration folder on the daemon's machine (`%APPDATA%\Keel`, `~/Library/Application Support/Keel`, `~/.config/keel`, or `KEEL_CONFIG_DIR`). Tick **Remember on this device** to keep it in that browser's local storage; leave it off on a shared computer (it then lives only in the open tab). **Sign out** forgets it. The page sends the token as its first WebSocket message; it never goes in an address, and an address that carries one is refused and scrubbed from the address bar. Download links (`/file/...`) are one-time and expire after 60 seconds. Every page is served `no-store`, without referrers, under a same-origin content security policy; nothing is loaded from a CDN.

**From another machine.** `--web` binds loopback only. To reach it from your phone or laptop, bind your tailnet address with `--web <tailnet IP>:7421 --ws-allow-remote` (the daemon has no TLS of its own: a tailnet encrypts the link; elsewhere put it behind a TLS reverse proxy). The page answers only to the bound IP (loopback names on a loopback bind); add `--web-host <name>` for each host name you use to reach it (a tailnet name, or a reverse proxy's name). Anyone who can reach the port still needs the token, and until they sign in a connection is held to small messages and few connections at once. Keep loopback when you do not need it.

**Revoking access.** `keel daemon rotate-token` writes a new token: every browser and client signs in again, and sessions signed in with the old token are closed.

## Phones

The web client is an installable app (a PWA) with a phone layout: one pane (a list, or a media grid you pinch to resize), a bottom bar with **Browse**, **Search**, **Library** and **Devices**, the preview as a full-screen sheet (pinch to zoom, double-tap, swipe left or right for the next or previous file), long-press menus (Preview, Download, Send to device…, Delete…) and pull to refresh. Above 700 px it is the desktop layout. Pull to refresh is phone-only. Install it from the browser's menu (**Install app** / **Add to Home Screen**); the service worker keeps only the app shell, never your files or the daemon's answers, and loads the shell from the daemon whenever it can (the cached copy is for starting offline), so an updated daemon's client shows at the next start. Browsers install apps and run service workers only over **https** (or on localhost), so use the tailnet's certificate or a reverse proxy for the installed app; plain http still works as a page.

**Reaching the daemon from the phone.** Pick one:

- **Same network (LAN).** `keel-daemon --web <LAN IP of the PC>:7421 --ws-allow-remote`, then open `http://<that IP>:7421/` on the phone. Plain http: anyone on that network can read the traffic (the client warns you), so use it only at home, and prefer one of the next two.
- **Tailnet (Tailscale or similar).** `keel-daemon --web <tailnet IP>:7421 --ws-allow-remote --web-host <machine name>`, then open `http://<machine name>:7421/` from the phone on the same tailnet. The tailnet encrypts the link. For an installable app with https, put `tailscale serve` (or another TLS front) before it and add that name with `--web-host`.
- **Reverse proxy with TLS.** Keep the daemon on loopback (`keel-daemon --web`), and let a proxy (Caddy, nginx) with a certificate forward `https://<name>/` to `127.0.0.1:7421`, passing the `Host` header through and upgrading WebSockets on `/rpc`; start the daemon with `--web-host <name>` so it answers to that name.

Whenever the page comes over plain http from another machine, the client shows a warning that the token and files cross the network unencrypted.

**Share → Keel (Spacedrop from the phone).** With the app installed, the phone's share sheet lists Keel. Sharing photos or files posts them to the daemon, which parks them (at most 512 MiB, 100 files; an upload slower than 64 KiB/s is cut off) and opens the app; the share request cannot carry the token, so nothing more happens until the app asks "Open N shared files?" and, on **Open**, the signed-in app claims them, once, within 5 minutes (unclaimed shares are deleted). Then pick one paired device and **Send…**: the preview lists every file and its size, and **Execute** sends exactly those files with Spacedrop through the daemon to that device only. `spacedrop.send` sends only from library sources, the inbox and opened shares, never from Keel's configuration or data folder, and never follows a link inside a folder. Drops sent to the daemon's machine show in **Devices → Inbox** with Accept / Decline for waiting offers and **Download** for what arrived; they land in `[devices] inbox` of the profile's `config.toml` (default `<data dir>/inbox`), and devices listed in `[devices] auto_accept` skip the question. Devices must be on for the daemon (Settings → Devices).
