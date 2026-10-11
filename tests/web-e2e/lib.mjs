// Shared by the browser flows (desktop.mjs, phone.mjs): the fixture keel-daemon, and a page
// driven through the bundle's `window.__keel` hook (built with the `e2e` feature, opened
// with ?e2e=1). The client is an egui canvas without DOM elements to select, so the flows
// act through the app's own actions, read its state, and touch the canvas only where a
// gesture is the point (taps, long-press, pull to refresh) at the rectangles the hook
// reports. Every flow takes a screenshot into ./out.
import { spawn } from "node:child_process";
import { mkdirSync, openSync } from "node:fs";
import { fileURLToPath } from "node:url";

export const out = fileURLToPath(new URL("./out", import.meta.url));
mkdirSync(out, { recursive: true });
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// keel-daemon --web on `addr` (env: KEEL_CONFIG_DIR, KEEL_DATA_DIR, KEEL_NET_SECRET from
/// run.sh), its output appended to out/daemon.log; resolves once it serves the page.
export async function startDaemon(bin, addr) {
  const log = openSync(`${out}/daemon.log`, "a");
  const child = spawn(bin, ["--web", addr], { stdio: ["ignore", log, log] });
  let exited = false;
  child.on("exit", () => (exited = true));
  for (let i = 0; i < 150; i++) {
    if (exited) throw new Error("keel-daemon exited: see out/daemon.log");
    try {
      if ((await fetch(`http://${addr}/`)).ok) return child;
    } catch {}
    await sleep(200);
  }
  child.kill();
  throw new Error("keel-daemon did not serve the web client in time");
}

export async function stopDaemon(child) {
  if (child.exitCode !== null) return;
  const gone = new Promise((r) => child.once("exit", r));
  child.kill();
  await gone;
}

/// A browser page on the client, with the helpers the flows use.
export async function openPage(browser, base, token, options = {}, query = "") {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1, ...options });
  const page = await context.newPage();
  const t = { context, page, base, errors: [], logs: [], sent: [] };
  page.on("console", (m) => {
    t.logs.push(m.text());
    if (m.type() === "error") t.errors.push("console: " + m.text());
  });
  page.on("pageerror", (e) => t.errors.push("pageerror: " + e.message));
  page.on("websocket", (ws) =>
    ws.on("framesent", (f) => {
      try {
        const m = JSON.parse(f.payload);
        if (m.method && m.method !== "auth") t.sent.push({ method: m.method, params: m.params ?? {} });
      } catch {}
    }),
  );
  t.shot = (name) => page.screenshot({ path: `${out}/${name}.png` });
  t.fail = async (msg) => {
    await t.shot("failure").catch(() => {});
    throw new Error(msg);
  };
  t.sentCount = (pred) => t.sent.filter(pred).length;
  t.state = async () => JSON.parse(await page.evaluate(() => window.__keel.state()));
  // Waits until `pred(state)` holds (and the page sent what `sentPred` asks, when given).
  t.until = async (label, pred, sentPred = () => true, ms = 10000) => {
    const end = Date.now() + ms;
    for (;;) {
      const s = await t.state();
      if (pred(s) && sentPred()) return s;
      if (Date.now() > end) {
        const { rects, ...shown } = s;
        return t.fail(`${label}: not reached; state ${JSON.stringify(shown)}`);
      }
      await sleep(100);
    }
  };
  t.act = (name, arg) => page.evaluate(([n, a]) => window.__keel.act(n, a), [name, arg ?? null]);
  t.select = (name) => page.evaluate((n) => window.__keel.select(n), name);
  // Loads the page with the hook (the client removes the query from the address at start).
  t.load = async (q = query) => {
    const res = await page.goto(`${base}/?e2e=1${q}`);
    if (!res?.ok()) await t.fail(`GET / answered ${res?.status()}`);
    await page.waitForSelector("canvas#keel");
    await page
      .waitForFunction(() => typeof window.__keel?.state === "function", null, { timeout: 15000 })
      .catch((e) => t.fail(`no window.__keel (${e.message}): build the bundle with scripts/build-web.sh --features e2e`));
  };
  // Types `value` into the real password box over the login form and presses Enter.
  t.enterToken = async (value) => {
    const box = page.locator("input#token[type=password]");
    await box.waitFor({ state: "visible" }).catch(() => t.fail("no visible password box for the token"));
    await box.fill(value);
    await box.press("Enter");
  };
  t.signIn = async () => {
    await t.until("login shown", (s) => s.login);
    await t.enterToken(token);
    await t.until("signed in", (s) => s.signed_in && s.sources.length > 0);
  };
  // The token must not leak: address, console, page text, the hook's state.
  t.noLeak = async () => {
    if (page.url().includes(token)) await t.fail("the token is in the address");
    if (t.logs.some((l) => l.includes(token))) await t.fail("the token is in the console");
    const text = await page.evaluate(() => document.body.innerText + document.title);
    if (text.includes(token)) await t.fail("the token is in the page text");
    if (JSON.stringify(await t.state()).includes(token)) await t.fail("the token is in the app state");
  };
  t.close = async () => {
    if (t.errors.length) await t.fail("console errors:\n" + t.errors.join("\n"));
    await context.close();
  };
  await t.load();
  return t;
}

/// A touch gesture on the canvas through the DevTools protocol (a context with
/// `hasTouch`): `points` are [x, y] positions, `holdMs` the wait after touching down.
export async function touch(page, points, holdMs = 0, stepMs = 16) {
  const cdp = await page.context().newCDPSession(page);
  const at = ([x, y]) => [{ x, y, id: 1 }];
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: at(points[0]) });
  await sleep(holdMs);
  for (const p of points.slice(1)) {
    await cdp.send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: at(p) });
    await sleep(stepMs);
  }
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await cdp.detach();
}

/// The centre of a named entry the phone pane drew.
export function centre(state, name) {
  const r = state.rects[name];
  if (!r) throw new Error(`no ${name} on screen: ${Object.keys(state.rects)}`);
  return [r[0] + r[2] / 2, r[1] + r[3] / 2];
}
