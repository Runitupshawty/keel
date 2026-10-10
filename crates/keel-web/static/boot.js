// Loads the wasm module (a file, not an inline script: the page's CSP allows only 'self').
import init from "./keel_web.js";
init();
// The service worker (installable app, shell cached, data never) needs a secure context:
// https (a reverse proxy, a tailnet certificate) or localhost.
if ("serviceWorker" in navigator && window.isSecureContext) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}
