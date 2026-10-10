// Keel's service worker: keeps the app shell (page, script, wasm, style, icons) for a
// quick, offline-capable start. It never caches data: /rpc, /file/ downloads, /share and
// anything else not in SHELL go straight to the daemon. scripts/build-web.* puts a hash
// of the wasm in VERSION, so a new build replaces the old shell.
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
  event.respondWith(
    caches
      .open(CACHE)
      .then((c) => c.match(req, { ignoreSearch: true }))
      .then((hit) => hit || fetch(req)),
  );
});
