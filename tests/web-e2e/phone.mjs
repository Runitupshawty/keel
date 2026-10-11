// The phone layout (390×844, touch, dark), a tablet (820×1180), the light theme, and a
// daemon that goes away and comes back.
import { centre, openPage, sleep, touch } from "./lib.mjs";

// Device scale 1: headless Chromium's ResizeObserver reports `devicePixelContentBoxSize`
// without the emulated scale, so eframe would draw at twice the size (a real phone's
// browser does not); egui points are CSS pixels either way.
const PHONE = { viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, isMobile: true, hasTouch: true };

export async function run({ browser, base, token, ids, daemon }) {
  const p = await openPage(browser, base, token, { ...PHONE, colorScheme: "dark" });
  const { page } = p;
  await p.until("phone login", (s) => s.login && s.layout === "phone" && s.dark);
  await p.shot("20-phone-login");
  await p.signIn();
  await p.noLeak();

  // The bottom bar, tapped.
  const tabs = ["Browse", "Search", "Library", "Devices"];
  for (const [i, name] of tabs.entries()) {
    await touch(page, [[((i + 0.5) * 390) / tabs.length, 844 - 30]]);
    const s = await p.until(`${name} tab`, (s) => s.phone_tab === name && !s.sheet);
    if (name === "Devices" && !s.devices.id) await p.until("devices listed", (s) => s.devices.id);
    // The first screen is not blank: it says where the sources are.
    if (name === "Browse")
      await p.until("browse hint", (s) => s.notes.includes("Open a source under Library, or type a path above."));
    await sleep(200);
    await p.shot(`21-phone-tab-${name.toLowerCase()}`);
  }

  // Browse a source; a tapped file opens the preview as a full-screen sheet; back closes it.
  await p.act("phone-tab", "Browse");
  await p.act("open-source", "Fixture");
  let s = await p.until("phone list", (s) => s.phone_tab === "Browse" && s.rects["alpha-notes.txt"]);
  await touch(page, [centre(s, "alpha-notes.txt")]);
  await p.until("preview sheet", (s) => s.sheet && s.selected === "alpha-notes.txt" && s.preview === "text");
  await sleep(200);
  await p.shot("22-phone-sheet");
  await p.act("back");
  s = await p.until("sheet closed", (s) => !s.sheet && s.rects["bravo-readme.txt"]);
  // The phone's Back (the browser's history) closes the sheet too, and stays in the app
  // (it left the app: the sheet was no history entry). Closed in the app, the sheet
  // leaves no entry behind.
  if (await page.evaluate(() => history.state) === "keel-sheet") await p.fail("the closed sheet left a history entry");
  await touch(page, [centre(s, "alpha-notes.txt")]);
  await p.until("sheet again", (s) => s.sheet);
  await page.goBack();
  s = await p.until("Back closed the sheet", (s) => !s.sheet && s.rects["bravo-readme.txt"]);
  if (!page.url().startsWith(base)) await p.fail(`Back left the app: ${page.url()}`);

  // A long-press opens the entry's menu (and changes nothing).
  await touch(page, [centre(s, "bravo-readme.txt")], 900);
  await p.until("long-press menu", (s) => s.menu === "bravo-readme.txt");
  await sleep(200);
  await p.shot("23-phone-long-press");
  await page.keyboard.press("Escape");
  s = await p.until("menu closed", (s) => !s.menu && !s.plan);

  // Pull to refresh: the listing and the library are read again; nothing else changes.
  const entries = s.entries.join();
  const asked = p.sentCount((m) => m.method === "sources.list");
  const [x, y] = centre(s, "alpha-notes.txt");
  await touch(page, Array.from({ length: 16 }, (_, i) => [x, y + i * 16]));
  s = await p.until("pulled", (s) => !s.sheet, () => p.sentCount((m) => m.method === "sources.list") > asked);
  await sleep(300);
  s = await p.state();
  if (s.entries.join() !== entries || s.error || s.plan) await p.fail(`pull to refresh changed something: ${JSON.stringify(s.entries)}`);
  await p.shot("24-phone-pulled");

  // The media grid.
  await p.act("grid", "on");
  await p.act("open-source", "Media");
  await p.until("grid", (s) => s.grid && s.rects["keel.png"]);
  await sleep(1000);
  await p.shot("25-phone-grid");
  await p.act("grid", "off");

  // Search on the phone, and an image in the sheet.
  await p.act("search", "ext:png");
  await p.until("phone search", (s) => s.phone_tab === "Search" && s.hits.join() === "keel.png");
  await p.shot("26-phone-search");
  await p.act("open-preview", "keel.png");
  await p.until("image sheet", (s) => s.sheet && s.preview === "image");
  await sleep(200);
  await p.shot("27-phone-image-sheet");
  await p.act("back");
  await p.noLeak();
  await p.close();

  // A small phone (320 points): the bottom bar's labels fit their buttons (they broke
  // mid-word: "Brow/se", "Devic/es").
  const small = await openPage(browser, base, token, { ...PHONE, viewport: { width: 320, height: 568 } });
  await small.signIn();
  await small.until("small phone", (s) => s.layout === "phone" && s.phone_tab === "Browse");
  await sleep(200);
  await small.shot("27b-phone-320");
  if ((await small.state()).tabs_cut) await small.fail("a bottom bar label does not fit its button at 320 wide");
  await small.close();

  // A tablet in portrait is wide enough for the desktop layout, with one pane (two were
  // squeezed to a few letters, names under their sizes and dates).
  const tab = await openPage(browser, base, token, { viewport: { width: 820, height: 1180 }, hasTouch: true, colorScheme: "dark" });
  await tab.signIn();
  await tab.until("tablet layout", (s) => s.layout === "desktop" && s.dark);
  await tab.act("open-source", "Fixture");
  await tab.until("tablet list", (s) => s.entries.includes("alpha-notes.txt"));
  await tab.act("open-preview", "alpha-notes.txt");
  await tab.until("tablet preview", (s) => s.preview === "text" && s.columns === 1);
  await sleep(200);
  await tab.shot("28-tablet");

  // The daemon goes away: the client says so in a strip over the stale contents (it was
  // only the small dot's label) and keeps retrying; once it is back the client signs in
  // again and reads everything again.
  const offline = (s) => s.notes.some((n) => n.startsWith("Offline: the daemon cannot be reached."));
  await daemon.stop();
  await tab.until("offline", (s) => !s.signed_in && !s.login && /offline/.test(s.conn) && offline(s), () => true, 15000);
  await sleep(300);
  await tab.shot("29-offline");
  const listed = tab.sentCount((m) => m.method === "list");
  await daemon.start();
  await tab.until(
    "reconnected",
    (s) => s.signed_in && s.entries.includes("alpha-notes.txt"),
    () => tab.sentCount((m) => m.method === "list") > listed,
    45000,
  );
  await sleep(300);
  if (offline(await tab.state())) await tab.fail("the offline strip stayed after reconnecting");
  await tab.shot("30-reconnected");
  await tab.noLeak();
  await tab.close();

  // The light theme.
  const light = await openPage(browser, base, token, { colorScheme: "light" });
  await light.until("light login", (s) => s.login && !s.dark);
  // The password box over the form is light too (it was a dark box on the light form).
  const bg = await light.page.locator("#token").evaluate((e) => getComputedStyle(e).backgroundColor);
  const [r, g, b] = bg.match(/\d+/g).map(Number);
  if (r + g + b < 600) await light.fail(`the token box is dark in the light theme: ${bg}`);
  await light.shot("31-light-login");
  await light.signIn();
  await light.act("open-source", "Fixture");
  await light.until("light list", (s) => s.entries.includes("alpha-notes.txt"));
  await light.act("open-preview", "alpha-notes.txt");
  await light.until("light preview", (s) => s.preview === "text");
  await sleep(200);
  await light.shot("32-light");
  await light.noLeak();
  await light.close();
}
