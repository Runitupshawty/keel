// Browser run of the Keel web client against a fixture keel-daemon (tests/web-e2e/run.sh
// prepares the fixture and the throwaway profile). It starts keel-daemon --web, adds the
// fixture sources, then runs the flows of desktop.mjs and phone.mjs in Chromium (see
// lib.mjs for how a flow drives the canvas). Screenshots of every flow go to ./out
// (uploaded by CI when the run fails).
// usage: node e2e.mjs <keel-daemon> <address> <work dir>      (SPEC=desktop|phone: one only)
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname } from "node:path";
import { chromium } from "playwright";
import { startDaemon, stopDaemon } from "./lib.mjs";
import * as desktop from "./desktop.mjs";
import * as phone from "./phone.mjs";

const [, , bin, addr, work] = process.argv;
const base = `http://${addr}`;
const t0 = Date.now();
let child = await startDaemon(bin, addr);
// The disconnect flow stops the daemon and starts it again (same profile).
const daemon = {
  stop: () => stopDaemon(child),
  start: async () => (child = await startDaemon(bin, addr)),
};
let browser;
try {
  const wasm = Buffer.from(await (await fetch(`${base}/keel_web_bg.wasm`)).arrayBuffer());
  if (!wasm.includes("__keel"))
    throw new Error("the served bundle has no e2e hook: scripts/build-web.sh --features e2e, then build keel-daemon");
  const tokenFile = `${work}/config/daemon.token`;
  const token = readFileSync(tokenFile, "utf8").trim(); // never printed
  const seeded = execFileSync(
    process.execPath,
    ["seed.mjs", `ws://${addr}/rpc`, tokenFile, `Fixture=${work}/fixture`, `Media=${work}/media`],
    { cwd: import.meta.dirname, encoding: "utf8" },
  );
  const ids = Object.fromEntries([...seeded.matchAll(/seeded source (\S+) (\S+)/g)].map((m) => [m[1], m[2]]));
  console.log(`daemon up and seeded in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  browser = await chromium.launch();
  // PDF pages render when pdfium sits next to keel-daemon (scripts/fetch-deps, copied there).
  const pdfium = ["pdfium.dll", "libpdfium.so", "libpdfium.dylib"].some((f) => existsSync(`${dirname(bin)}/${f}`));
  const env = { browser, base, token, work, ids, daemon, pdfium };
  for (const [name, spec] of [
    ["desktop", desktop],
    ["phone", phone],
  ]) {
    if (process.env.SPEC && process.env.SPEC !== name) continue;
    const t = Date.now();
    await spec.run(env);
    console.log(`web e2e ${name} ok in ${((Date.now() - t) / 1000).toFixed(1)} s`);
  }
} finally {
  await browser?.close().catch(() => {});
  await stopDaemon(child);
}
console.log(`web e2e done in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
