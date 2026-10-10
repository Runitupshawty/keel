// Browser run of the Keel web client against a fixture keel-daemon (tests/web-e2e/run.sh
// starts it). The client is an egui canvas without DOM elements to select, so the bundle is
// built with the `e2e` feature (scripts/build-web.sh --features e2e) and the page opened
// with ?e2e=1: `window.__keel = { select(name), act(name, arg), state() }` drives the app
// through its own actions. The run checks what the page sends on its WebSocket (the
// JSON-RPC methods), what the daemon holds, and the console. Screenshots of every step go
// to ./out (uploaded by CI when the run fails).
// usage: node e2e.mjs <base url> <token file> <fixture dir>
import { mkdirSync, readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const [, , base, tokenFile, fixture] = process.argv;
const token = readFileSync(tokenFile, "utf8").trim(); // never printed
const out = fileURLToPath(new URL("./out", import.meta.url));
mkdirSync(out, { recursive: true });

const sent = []; // {method, params} of every request the page sent (auth left out)
const errors = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sentCount = (pred) => sent.filter(pred).length;

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
  await browser.close().catch(() => {});
  throw new Error(msg);
};

// The app's state (`__keel.state()`, refreshed every frame).
const state = async () => JSON.parse(await page.evaluate(() => window.__keel.state()));
// Waits until `pred(state)` holds (and the page sent what `sentPred` asks, when given).
async function until(label, pred, sentPred = () => true, ms = 10000) {
  const end = Date.now() + ms;
  for (;;) {
    const s = await state();
    if (pred(s) && sentPred()) return s;
    if (Date.now() > end) return fail(`${label}: not reached; state ${JSON.stringify({ ...s, error: s.error })}`);
    await sleep(100);
  }
}
const act = (name, arg) => page.evaluate(([n, a]) => window.__keel.act(n, a), [name, arg]);
const select = (name) => page.evaluate((n) => window.__keel.select(n), name);

// 1. The shell loads and is installable.
const res = await page.goto(base + "/?e2e=1");
if (!res?.ok()) await fail(`GET / answered ${res?.status()}`);
if ((await page.title()) !== "Keel") await fail("the page title is not Keel");
const manifest = await (await page.request.get(base + "/manifest.webmanifest")).json();
if (manifest.name !== "Keel" || manifest.display !== "standalone") await fail("manifest.webmanifest is wrong");
await page.waitForSelector("canvas#keel");
await page.waitForFunction(() => typeof window.__keel?.state === "function", null, { timeout: 15000 }).catch(() =>
  fail("no window.__keel: build the bundle with scripts/build-web.sh --features e2e"),
);
await shot("1-login");

// 2. Sign in with the token from daemon.token (typed, never in the address).
await page.mouse.click(640, 600); // focus the canvas on blank space
await page.keyboard.press("Tab"); // the token field is the first widget
await page.keyboard.type(token, { delay: 5 });
await page.keyboard.press("Enter");
await until(
  "signed in",
  (s) => s.signed_in && s.sources.length > 0,
  () => sentCount((m) => m.method === "sources.list") > 0,
);
if (page.url().includes(token)) await fail("the token is in the address");
await shot("2-signed-in");

// 3. Open the fixture source.
await act("open-source");
await until(
  "source listed",
  (s) => s.entries.includes("alpha-notes.txt"),
  () => sentCount((m) => m.method === "list" && String(m.params.path).startsWith("library://")) > 0,
);
await shot("3-source");

// 4. Search for a file name.
await act("search", "alpha");
await until(
  "search hits",
  (s) => s.hits.includes("alpha-notes.txt"),
  () => sentCount((m) => m.method === "search" && String(m.params.query).includes("alpha")) > 0,
);
await shot("4-search");

// 5. A text preview of the hit.
await select("alpha-notes.txt");
await act("open-preview");
await until(
  "text preview",
  (s) => s.selected === "alpha-notes.txt" && s.preview === "text",
  () => sentCount((m) => m.method === "preview.render") > 0,
);
await shot("5-preview");

// 6. A delete preview, then Cancel.
await act("delete-preview");
await until(
  "delete preview",
  (s) => s.plan?.state === "review",
  () => sentCount((m) => m.method === "plan" && m.params.op === "delete") > 0,
);
await shot("6-delete-preview");
await act("cancel");
await until("delete preview cancelled", (s) => s.plan === null);

// 7. A rename preview, then Cancel.
await act("rename-preview", "renamed-notes.txt");
await until(
  "rename preview",
  (s) => s.plan?.state === "review",
  () => sentCount((m) => m.method === "plan" && m.params.op === "rename") > 0,
);
await act("cancel");
await until("rename preview cancelled", (s) => s.plan === null);
await shot("7-cancelled");

// Nothing was executed; the fixture is as it was.
await sleep(500);
if (sentCount((m) => m.method === "execute") > 0) await fail("something was executed");
if (!existsSync(`${fixture}/alpha-notes.txt`)) await fail("the fixture file is gone");
if (existsSync(`${fixture}/renamed-notes.txt`)) await fail("the fixture file was renamed");
const { error } = await state();
if (error) await fail(`the app shows an error: ${error}`);

// 8. The service worker is ready (the shell is installable offline).
const sw = await page.evaluate(async () => {
  const reg = await navigator.serviceWorker.ready;
  return reg.active?.scriptURL ?? null;
});
if (!sw || !sw.endsWith("/sw.js")) await fail("the service worker is not active");

// 9. Nothing went wrong in the page.
if (errors.length) await fail("console errors:\n" + errors.join("\n"));
await browser.close();
console.log("web e2e ok:", sent.map((m) => m.method).join(" "));
