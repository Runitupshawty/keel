
`keel-net` is a library on iroh 1.3, using ALPN `keel/net/1`. It owns no UI.
`Node::open` loads or creates `net/node-secret` through the supplied
`keel_vfs::cloud::SecretStore`. Only public identities leave the store boundary
for the network; the private key is used in memory by iroh and is never included
in tickets, tables, events or logs. Tests use `MemoryStore` exclusively.

## Pairing

Display a generated `PairCode` for manual entry, or encode its `ticket()` string
in a QR code. The short form carries 128 randomly generated bits in lowercase
base32. A domain-separated SHA-256 derivation creates an ephemeral iroh identity
whose address is published by iroh's public address lookup. The joiner derives
that identity from the short code, resolves it and connects. This works on a LAN
or across relays when public discovery is reachable. **Offline LAN discovery of
short codes is not provided**; use a full ticket with embedded addresses offline.
An invitation lasts ten minutes (monotonic host deadline), allows one successful
pairing, and is replaced by the next `pair_code()` call. Its endpoint is closed
on expiry, replacement or node shutdown. Wrong proofs do not consume it.
Only the inviter's monotonic deadline decides expiry; the joiner does not compare
the inviter's wall clock with its own. Full tickets bind the advertised expiry
to the authenticated transcript, so devices with different clocks can pair.

Pairing uses a temporary endpoint separate from the device endpoint. The joiner
uses its permanent TLS identity. HMAC-SHA256 proofs bind fresh nonces, both
permanent identities, both labels, both address sets, expiry and role. The host
checks the joiner's identity against the authenticated QUIC connection. The
joiner verifies the host proof before committing; the final host proof confirms
that the host committed. Possession of the code authorizes pairing: protect it
as a bearer secret. Debug output redacts it. Interrupted pairing can leave only
the host paired (distributed commits cannot be atomic); pair again with a fresh
code or forget that peer. No grants are added by pairing.

## Requests and authorization

Each request uses one bidirectional stream: big-endian u32 CBOR header length,
then the CBOR `Request` / `Response`, then exactly the stated raw body length.
Headers above one MiB, trailing CBOR data and short/long bodies are rejected.
Reads use `(offset, length)` ranges, clamp length at EOF, and reject overflowing
or beyond-EOF ranges. `read_stream` does not buffer a whole file.

The permanent endpoint rejects unpaired TLS identities. Each request checks
current grants, using source equality and slash-component subtree matching.
Both rename paths require write permission. Paths reject traversal, absolute
paths, backslashes, colon aliases, control characters, percent escapes and
trailing spaces/dots. Ping, filtered ListSources and the requesting peer's Grants
are allowed for paired devices without a grant; file operations default-deny.
Handlers must reject symlink traversal (or independently enforce the authorized
subtree after resolution), and must not decode or reinterpret paths. Transport validation
cannot inspect a Handler's filesystem. Every Handler method receives a
`RequestCtx` (requesting peer id and its label) for logging and prompts.

Writes are pushed in pieces: `Request::Write { source, path, offset, size, final_,
expect }` appends exactly `size` body bytes to the host's `.keel-partial-<id>`
staging file (`<id>` fixed per device and target). `offset` must equal the staged
length (`Request::StatPartial` answers `Response::Partial { len, .. }`), except 0,
which starts over. A `final_` piece publishes atomically once complete, after checking
`expect` (BLAKE3 of the whole file) when given; a failed check drops the staging file.
A cancelled piece leaves the staging file at its `offset`, so a dropped transfer
resumes from the last complete piece. Operations already committed before revocation
cannot be undone.

`Request::List { source, path, after, limit }` pages by name: at most `PAGE_LIMIT`
(500) entries after `after`, with `Response::Entries { entries, more }`. A 10,000-entry
folder in one header would exceed the 1 MiB limit. `EntryInfo::content_id` is CBOR
bytes.

Grant downgrades, revoke and forget synchronously cancel handlers and close **all**
of that peer's sessions, including active reads and writes. Each later request
rechecks current grants, even on existing sessions. Client operations use fresh
connections, closed when the response/body is dropped, so the next operation
does not race a cached session's remote close notification. Mutations are never
automatically retried. Already delivered bytes
cannot be recalled. Requests are traced at debug with public peer id, operation
and authorization result; paths, labels, codes and handler errors are not logged.

## Persistence, events and API additions

`<data_dir>/net/net.sqlite3` stores peers, address hints, grants and the local
label in one atomic SQLite transaction per update (bundled SQLite, rollback journal, synchronous=FULL).
The supplied data directory must be private to the application user. Reopening
retains identity, pairing and grants; live links begin Offline. Public discovery
allows reconnecting after addresses change; offline address hints only remain
usable while the remote node keeps those addresses.
Open only one node for a given data directory and secret store at a time; the
daemon/single-instance layer owns that coordination.

`events()` returns a new crossbeam subscription each time, capacity 256. Slow
subscribers may lose events and should refresh `peers()` / `grants()`. Cloning a
receiver shares its queue, as with any crossbeam receiver. Link updates reflect
observed connections; `Lan` means a direct IP path (not necessarily the same LAN).
There is no unsolicited probing of offline peers. Call `close().await` to stop
the endpoints and await all server work before dropping the node/runtime.

The requested Task 35 API is preserved, with these documented additions/details:

- `NodeOptions` and `Node::open_with_options` configure discovery, relays, binding,
  relay-only operation and header/connection timeouts. `NodeOptions::offline()`
  binds loopback with no relays or address lookup. Custom relays can be supplied
  using `iroh::RelayMode::Custom`; configure their addresses outside source code.
- `Node::write_stream(peer, source, path, body, WriteAt)` supplies the body missing
  from the specified `request(Request::Write)` signature. `request` sends an
  empty body and exposes a Read header only; use the streaming methods for data.
- `try_set_label` reports persistence errors; the required void `set_label`
  logs a generic warning and leaves the old label on failure.
- `NodeId` displays lowercase RFC 4648 base32 per the requested contract. iroh
  1.3 itself now displays hex; `FromStr` accepts both base32 and iroh hex.
- `PairCode::ticket() -> String` retains the generated full ticket. A code parsed
  from short text cannot reconstruct addresses and returns that short text.

## Library integration

`NodeProvider` is the `keel_vfs::Provider` for `node://<peer id>/<source id>/<path>`;
register it with the router (`router.register`). It lives here, not in keel-vfs,
because keel-net already depends on keel-vfs. A device's root lists the sources it
granted; listings walk every page; writes buffer to an anonymous temp file and are
pushed on `flush()` as one verified final piece; `caps` follow the grants devices
reported; `remove` is permanent from the client's view (a library host trashes).
Its calls block on the node's runtime, so call them off async tasks.

`LibraryHandler` serves a `keel_core::Library`: its non-device sources, lists and
stats from the live provider, reads/writes/mkdir/rename/remove through the router.
Each path must canonicalize to exactly `<canonical root>/<path>`, so a symlink or
junction anywhere on the way is refused; writes re-check before publishing. Device
writes go to local sources only. Every served request is appended to the library's
op log as `net.<op>` with the peer id and label.

iroh uses an explicit ring CryptoProvider, without touching the process default.
This coexists with keel-vfs's graviola provider. The offline regression test
installs graviola first, then opens nodes and completes a pairing and QUIC ping.

Run `cargo test -p keel-net` for offline tests. The ignored relay-only test must
also have `KEEL_NET_RELAY_TEST=1`; it uses short-code discovery and public relays.
