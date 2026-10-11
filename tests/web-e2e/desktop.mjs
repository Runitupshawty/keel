// The desktop layout (1280×800): sign-in, the remembered token, sources, folders, search,
// previews, tags, the plan previews, Devices, the Spacedrop inbox and a share to claim.
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { openPage, sleep } from "./lib.mjs";

/// Every file under `dir` with its contents, to show that nothing else changed.
export function tree(dir, at = "") {
  const all = {};
  for (const name of readdirSync(dir).sort()) {
    const p = `${dir}/${name}`;
    if (statSync(p).isDirectory()) Object.assign(all, tree(p, `${at}${name}/`));
    else all[at + name] = readFileSync(p, "latin1");
  }
  return all;
}

export async function run({ browser, base, token, work, ids }) {
  const fixture = `${work}/fixture`;
  const before = tree(fixture);
  const t = await openPage(browser, base, token);
  const { page } = t;

  // The shell loads and is installable.
  if ((await page.title()) !== "Keel") await t.fail("the page title is not Keel");
  const manifest = await (await page.request.get(base + "/manifest.webmanifest")).json();
  if (manifest.name !== "Keel" || manifest.display !== "standalone") await t.fail("manifest.webmanifest is wrong");
  await t.until("login", (s) => s.login && s.layout === "desktop");
  await t.shot("01-login");

  // A paste lands in the password box (eframe cancels pastes elsewhere).
  const box = page.locator("input#token[type=password]");
  await box.focus();
  await page.evaluate(() => {
    const data = new DataTransfer();
    data.setData("text/plain", " pasted ");
    document.getElementById("token").dispatchEvent(
      new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }),
    );
  });
  if ((await box.inputValue()) !== "pasted") await t.fail("a paste did not land in the token box");

  // A wrong token is refused and the form asks again; nothing is remembered.
  await t.enterToken("not-the-token");
  // It says why (it used to say "the first message must be auth with the daemon token").
  await t.until("wrong token refused", (s) => s.login && /refused.*not the token in daemon\.token/.test(s.notice ?? ""));
  await t.shot("02-wrong-token");
  if (await page.evaluate(() => localStorage.length)) await t.fail("a refused token was stored");

  // The right one, remembered on this device: a new load signs in by itself.
  await t.act("remember", "on");
  await t.enterToken(token);
  await t.until(
    "signed in",
    (s) => s.signed_in && s.sources.length === 2,
    () => t.sentCount((m) => m.method === "sources.list") > 0,
  );
  if (await box.isVisible()) await t.fail("the token box is still shown after signing in");
  // The preview panel keeps its width with nothing selected (it shrank to its one line,
  // and then cut the buttons of a selected file).
  await t.until("preview panel width", (s) => s.preview_width >= 340 && s.columns === 2);
  // Empty panes say how to fill them (they were blank).
  await t.until("empty pane hint", (s) => s.notes.includes("Open a source from the sidebar, or type a path above."));
  await t.shot("03-signed-in");
  await t.load();
  await t.until("signed in from the remembered token", (s) => s.signed_in && s.remember);
  // Sign out forgets it: the next load asks again.
  await t.act("sign-out");
  await t.until("signed out", (s) => s.login);
  if (await page.evaluate(() => localStorage.length)) await t.fail("sign out left the token stored");
  await t.load();
  await t.until("asks again after sign out", (s) => s.login && !s.remember);
  await t.signIn();
  await t.noLeak();

  // Each source of the sidebar.
  for (const label of ["Media", "Fixture"]) {
    await t.act("open-source", label);
    await t.until(`${label} listed`, (s) => s.path === `library://${ids[label]}/` && s.entries.length > 0);
    await t.shot(`04-source-${label.toLowerCase()}`);
  }

  // Folders: down two levels, then up again.
  await t.act("open", "docs");
  await t.until("docs", (s) => s.path.endsWith("/docs") && s.entries.includes("report.pdf"));
  await t.act("open", "deep");
  await t.until("docs/deep", (s) => s.path.endsWith("/docs/deep") && s.entries.includes("delta.txt"));
  await t.shot("05-folder");
  await t.act("up");
  await t.until("up to docs", (s) => s.path.endsWith("/docs"));
  await t.act("up");
  await t.until("up to the source", (s) => s.path === `library://${ids.Fixture}/`);

  // Search: an `ext:` filter, then a query that finds nothing.
  await t.act("search", "ext:pdf");
  await t.until("ext: search", (s) => s.tab === "search" && s.hits.join() === "report.pdf");
  await t.shot("06-search-ext");
  // An empty result says so (the tab used to stay blank).
  const none = "zzqx-nothing-like-this";
  await t.act("search", none);
  await t.until("empty search", (s) => s.searched === none && s.hits.length === 0 && s.notes.includes(`Nothing matches ${none}.`));
  await t.shot("07-search-empty");

  // Previews: text, image, PDF (an image of the page, or the previewer's message when the
  // daemon has no pdfium), a file inside a zip.
  await t.act("search", "alpha");
  await t.until("alpha hit", (s) => s.hits.includes("alpha-notes.txt"));
  await t.act("open-preview", "alpha-notes.txt");
  await t.until("text preview", (s) => s.selected === "alpha-notes.txt" && s.preview === "text" && s.preview_width >= 340);
  await t.shot("08-preview-text");
  await t.act("open-source", "Media");
  await t.until("media listed", (s) => s.entries.includes("keel.png"));
  await t.act("open-preview", "keel.png");
  await t.until("image preview", (s) => s.selected === "keel.png" && s.preview === "image");
  await t.shot("09-preview-image");
  await t.act("go", `library://${ids.Fixture}/docs`);
  await t.until("docs again", (s) => s.entries.includes("report.pdf"));
  await t.act("open-preview", "report.pdf");
  const pdf = await t.until("pdf preview", (s) => s.selected === "report.pdf" && ["image", "message"].includes(s.preview));
  console.log("  pdf preview:", pdf.preview);
  await t.shot("10-preview-pdf");
  await t.act("go", `${fixture}/docs/bundle.zip!/`);
  await t.until("inside the zip", (s) => s.entries.includes("a.txt"));
  await t.act("open-preview", "a.txt");
  await t.until("zip entry preview", (s) => s.selected === "a.txt" && s.preview === "text");
  await t.shot("11-preview-zip-entry");

  // A tag, previewed and executed (library metadata only), then found by `tag:`.
  await t.act("go", `library://${ids.Fixture}/`);
  await t.until("fixture again", (s) => s.entries.includes("alpha-notes.txt"));
  await t.act("open-preview", "alpha-notes.txt");
  await t.until("alpha selected", (s) => s.selected === "alpha-notes.txt" && Array.isArray(s.tags));
  await t.act("tag", "qa-walk");
  await t.until("tag preview", (s) => s.plan?.state === "review" && s.plan.operation === "tags.add");
  await t.shot("12-tag-preview");
  await t.act("execute");
  await t.until("tagged", (s) => s.plan?.state === "done");
  await t.act("cancel");
  await t.until("tag shown", (s) => s.tags?.includes("qa-walk"));
  await t.act("search", "tag:qa-walk");
  await t.until("tag: search", (s) => s.hits.join() === "alpha-notes.txt");
  await t.shot("13-tagged");

  // A delete preview and a rename preview, both cancelled.
  await t.act("open-preview", "alpha-notes.txt");
  await t.until("alpha selected", (s) => s.selected === "alpha-notes.txt");
  await t.act("delete-preview");
  await t.until("delete preview", (s) => s.plan?.state === "review" && s.plan.changes[0]?.action === "delete");
  await t.shot("14-delete-preview");
  await t.act("cancel");
  await t.until("delete preview cancelled", (s) => s.plan === null);
  await t.act("rename-preview", "renamed-notes.txt");
  await t.until("rename preview", (s) => s.plan?.state === "review" && s.plan.changes[0]?.action === "rename");
  await t.shot("15-rename-preview");
  await t.act("cancel");
  await t.until("rename preview cancelled", (s) => s.plan === null);

  // Devices: this device (devices on, nothing paired), and the empty Spacedrop inbox.
  const dev = await t.until("devices", (s) => s.devices.id && s.inbox.entries);
  if (dev.devices.peers.length) await t.fail("a fresh profile lists paired devices");
  if (dev.inbox.pending || dev.inbox.entries.length) await t.fail("the inbox of a fresh profile is not empty");
  // This device's id shows, and the empty list says how to pair (it said nothing).
  await t.until(
    "devices notes",
    (s) =>
      s.notes.includes(`id ${dev.devices.id}`) &&
      s.notes.some((n) => n.startsWith("No paired devices")) &&
      s.notes.includes("Nothing received yet."),
  );
  await t.shot("16-devices-inbox");

  // Nothing but the tag was executed; the fixture is as it was.
  await sleep(300);
  const executed = t.sent.filter((m) => m.method === "execute").length;
  if (executed !== 1) await t.fail(`${executed} executes, expected 1 (the tag)`);
  if (JSON.stringify(tree(fixture)) !== JSON.stringify(before)) await t.fail("the fixture changed");
  if (existsSync(`${fixture}/renamed-notes.txt`)) await t.fail("the fixture file was renamed");
  const { error } = await t.state();
  if (error) await t.fail(`the app shows an error: ${error}`);

  // The service worker is ready (the shell is installable offline).
  const sw = await page.evaluate(async () => (await navigator.serviceWorker.ready).active?.scriptURL ?? null);
  if (!sw || !sw.endsWith("/sw.js")) await t.fail("the service worker is not active");
  await t.noLeak();
  await t.close();

  // Share → Keel: a parked upload, claimed after signing in; nothing is paired to send to.
  const form = new FormData();
  form.append("files", new Blob(["shared from the test\n"], { type: "text/plain" }), "shared-note.txt");
  const posted = await fetch(`${base}/share`, { method: "POST", body: form, redirect: "manual" });
  const location = posted.headers.get("location") ?? "";
  if (posted.status !== 303 || !location.startsWith("/?share=")) throw new Error(`POST /share answered ${posted.status}`);
  const s = await openPage(browser, base, token, {}, "&" + location.slice(2));
  await s.until("login asks for the share", (st) => st.login && st.share === "Asking");
  await s.shot("17-share-login");
  if (s.page.url().includes("share=")) await s.fail("the share id stayed in the address");
  await s.signIn();
  await s.until("share question", (st) => st.share === "Asking");
  await s.shot("18-share-ask");
  await s.act("share-open");
  await s.until("share claimed", (st) => st.share === "Claimed");
  await s.shot("19-share-claimed");
  await s.act("share-close");
  await s.until("share closed", (st) => st.share === "None");
  await s.noLeak();
  await s.close();
}
