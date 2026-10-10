// Keel's service worker: keeps the app shell (page, script, wasm, style, icons) so the
// client starts offline. It never caches data: /rpc, /file/ downloads, /share and
// anything else not in SHELL go straight to the daemon. Shell files are fetched from the
// daemon first (so page, script and wasm always come from the same, current build) and
// served from the cache only when the daemon cannot be reached. scripts/build-web.* puts
// a hash of every shell file in VERSION, so a new build replaces the old cache.
const VERSION = "__KEEL_BUILD__";
const CACHE = "keel-shell-" + VERSION;
const SHELL = [
  "./",
  "index.html",
  "boot.js",
  "keel.css",
  "keel_web.js",
  "keel_web_bg.wasm",
  "manifest.webmanifest",
  "icon-192.png",
  "icon-512.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  const name = url.pathname === "/" ? "./" : url.pathname.slice(1);
  if (!SHELL.includes(name)) return;
  // `/?share=<id>` is the page too: the query is not part of the cache key.
  const key = new Request(url.origin + url.pathname);
  event.respondWith(
    fetch(req)
      .then((res) => {
        if (res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(key, copy));
        }
        return res;
      })
      .catch(() =>
        caches
          .open(CACHE)
          .then((c) => c.match(key))
          .then((hit) => hit || Response.error()),
      ),
  );
});
