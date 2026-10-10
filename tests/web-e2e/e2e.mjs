// Browser run of the Keel web client against a fixture keel-daemon (tests/web-e2e/run.sh
// starts it). The client is an egui canvas, so there are no DOM elements to select: the
// run drives it with the keyboard and mouse and checks what the page does on its
// WebSocket (the JSON-RPC methods it sends), what the daemon holds, and the console.
// Click positions depend on egui's layout, so each click is tried over a short range of
// positions until the page sends the request that click should cause. Screenshots of every
// step go to ./out (uploaded by CI when the run fails).
// usage: node e2e.mjs <base url> <token file> <fixture dir>
import { mkdirSync, readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const [, , base, tokenFile, fixture] = process.argv;
const token = readFileSync(tokenFile, "utf8").trim();
const out = fileURLToPath(new URL("./out", import.meta.url));
mkdirSync(out, { recursive: true });

const sent = []; // {method, params} of every request the page sent
const errors = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sentCount = (pred) => sent.filter(pred).length;
const until = async (pred, ms = 800) => {
  for (let t = 0; t < ms; t += 50) {
    if (pred()) return true;
    await sleep(50);
  }
  return pred();
};

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
const page = await context.newPage();
page.on("console", (m) => m.type() === "error" && errors.push("console: " + m.text()));
page.on("pageerror", (e) => errors.push("pageerror: " + e.message));
page.on("websocket", (ws) =>
  ws.on("framesent", (f) => {
    try {
      const m = JSON.parse(f.payload);
      if (m.method && m.method !== "auth") sent.push({ method: m.method, params: m.params ?? {} });
    } catch {}
  }),
);
const shot = (name) => page.screenshot({ path: `${out}/${name}.png` });
const fail = async (msg) => {
  await shot("failure").catch(() => {});
  throw new Error(msg);
};

// Tries `act(x, y)` over `points` until `done()` holds.
async function tryClicks(label, points, act, done) {
  for (const [x, y] of points) {
    await act(x, y);
    if (await until(done)) return [x, y];
  }
  return fail(`${label}: no click position worked`);
}
const range = (from, to, step) => Array.from({ length: Math.floor((to - from) / step) + 1 }, (_, i) => from + i * step);
const click = (x, y) => page.mouse.click(x, y);

// 1. The shell loads and is installable.
const res = await page.goto(base + "/");
if (!res?.ok()) await fail(`GET / answered ${res?.status()}`);
if ((await page.title()) !== "Keel") await fail("the page title is not Keel");
const manifest = await (await page.request.get(base + "/manifest.webmanifest")).json();
if (manifest.name !== "Keel" || manifest.display !== "standalone") await fail("manifest.webmanifest is wrong");
await page.waitForSelector("canvas#keel");
await sleep(1500); // wasm start
await shot("1-login");

// 2. Sign in with the token from daemon.token (typed, never in the address).
await click(640, 600); // focus the canvas on blank space
await page.keyboard.press("Tab"); // the token field is the first widget
await page.keyboard.type(token, { delay: 5 });
await page.keyboard.press("Enter");
if (!(await until(() => sentCount((m) => m.method === "sources.list") > 0, 8000))) {
  await fail("signing in did not load the sources");
}
if (page.url().includes(token)) await fail("the token is in the address");
await sleep(500);
await shot("2-signed-in");

// 3. Open the fixture source from the sidebar.
const lists = () => sentCount((m) => m.method === "list" && String(m.params.path).startsWith("library://"));
await tryClicks("open the source", range(46, 260, 8).map((y) => [90, y]), click, () => lists() > 0);
await shot("3-source");

// 4. Search for a file name (the Search tab, then its field).
const searches = () => sent.filter((m) => m.method === "search" && String(m.params.query).includes("alpha"));
await tryClicks(
  "search",
  range(100, 340, 12).map((x) => [x, 17]),
  async (tabX) => {
    await click(tabX, 17); // the Search tab, somewhere along the top bar
    await click(600, 52); // the query field
    await page.keyboard.press("Control+A");
    await page.keyboard.type("alpha");
    await page.keyboard.press("Enter");
  },
  () => searches().length > 0,
);
await shot("4-search");

// 5. A text preview: select the hit.
const previews = () => sentCount((m) => m.method === "preview.render" || m.method === "stat");
const before = previews();
await tryClicks("preview", range(72, 160, 8).map((y) => [300, y]), click, () => previews() > before);
await sleep(800);
await shot("5-preview");

// 6. A delete preview, then Cancel: right-click the hit, then its Delete… entry.
const deletes = () => sent.filter((m) => m.method === "plan" && m.params.op === "delete");
const hit = [300, 0];
for (const y of range(72, 160, 8)) {
  hit[1] = y;
  await page.mouse.click(hit[0], y, { button: "right" });
  const opened = await tryClicks("delete preview", range(60, 150, 6).map((dy) => [hit[0] + 30, y + dy]), click, () => deletes().length > 0).catch(() => null);
  if (opened) break;
  await page.keyboard.press("Escape");
}
if (deletes().length === 0) await fail("no delete preview was asked for");
await sleep(500);
await shot("6-delete-preview");
// Cancel is right of Execute, on the dialog's bottom row: sweep that row from the right
// (never the other way) until the picture changes, i.e. the dialog closed.
const region = { x: 340, y: 300, width: 600, height: 300 };
const dialogShot = () => page.screenshot({ clip: region });
const open = await dialogShot();
let closed = false;
for (const y of range(440, 560, 8)) {
  for (const x of range(900, 480, -8)) {
    await click(x, y);
    await sleep(60);
    if (!(await dialogShot()).equals(open)) {
      closed = true;
      break;
    }
  }
  if (closed) break;
}
if (!closed) await fail("the delete preview could not be cancelled");
await shot("7-cancelled");
if (sentCount((m) => m.method === "execute") > 0) await fail("something was executed");
if (!existsSync(`${fixture}/alpha-notes.txt`)) await fail("the fixture file is gone");

// 7. The service worker is ready (the shell is installable offline).
const sw = await page.evaluate(async () => {
  const reg = await navigator.serviceWorker.ready;
  return reg.active?.scriptURL ?? null;
});
if (!sw || !sw.endsWith("/sw.js")) await fail("the service worker is not active");

// 8. Nothing went wrong in the page.
if (errors.length) await fail("console errors:\n" + errors.join("\n"));
await browser.close();
console.log("web e2e ok:", sent.map((m) => m.method).join(" "));
