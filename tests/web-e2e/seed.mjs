// Adds the fixture folder as a library source through the daemon's JSON-RPC (a preview,
// then the execute that confirms it) and waits until it is indexed.
// usage: node seed.mjs <rpc url> <token file> <fixture dir>
import { readFileSync } from "node:fs";

const [, , url, tokenFile, root] = process.argv;
const token = readFileSync(tokenFile, "utf8").trim();
const ws = new WebSocket(url);
const pending = new Map();
let next = 1;
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  pending.get(m.id)?.(m);
};
const call = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = next++;
    pending.set(id, (m) => (m.error ? reject(new Error(`${method}: ${m.error.message}`)) : resolve(m.result)));
    ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  });
const confirm = async (method, params) => {
  const p = await call(method, params);
  return (await call("execute", { plan_id: p.plan_id, input_hash: p.input_hash })).result;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

await new Promise((resolve, reject) => {
  ws.onopen = resolve;
  ws.onerror = () => reject(new Error("cannot connect to " + url));
});
await call("auth", { token });
const added = await confirm("sources.add", { root, label: "Fixture" });
await confirm("sources.index", { id: added.id });
for (let i = 0; i < 120; i++) {
  const s = (await call("sources.list")).find((x) => x.id === added.id);
  if (s?.status === "online" && s.indexed_at) break;
  if (i === 119) throw new Error("the fixture source was not indexed in time");
  await sleep(500);
}
console.log("seeded source", added.id);
ws.close();
