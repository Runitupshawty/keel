
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
as soon as a pairing completes, and on expiry, replacement or node shutdown.
Wrong proofs do not consume it. At most four unauthenticated handshakes run at
once; further connection attempts are refused until one finishes. `pair_code()`
never holds the pairing lock while it waits for a relay, so `close()` is not
delayed by a pending `pair_code()`.
Only the inviter's monotonic deadline decides expiry; the joiner does not compare
the inviter's wall clock with its own. Full tickets bind the advertised expiry
to the authenticated transcript, so devices with different clocks can pair.

Pairing uses a temporary endpoint separate from the device endpoint. The joiner
uses its permanent TLS identity. The rendezvous id is public, so the host reveals
nothing about itself until the joiner proves it knows the code: the joiner sends
its identity; the host answers with only a fresh nonce and the expiry; the joiner
sends an HMAC-SHA256 proof over its identity, that nonce and the expiry; only
after verifying it does the host commit and send its permanent identity, label
and addresses with a proof over the whole transcript (both identities, labels,
address sets and nonces, expiry and role), which also confirms the commit. The
joiner may prove first because the rendezvous endpoint's TLS key is derived from
the code, so only a code holder can answer. The host checks the joiner's identity
against the authenticated QUIC connection, and the joiner commits only after
verifying the host's proof. Labels are validated on both sides (at most 256
bytes, no control or bidirectional override/isolate characters U+202A–202E and
U+2066–2069), also for labels in Ping replies: an invalid one is ignored, a
valid change is persisted. Possession of the code authorizes pairing: protect it
as a bearer secret. Debug output redacts it. Interrupted pairing can leave only
the host paired (distributed commits cannot be atomic); pair again with a fresh
code or forget that peer. No grants are added by pairing.

## Requests and authorization

Each request uses one bidirectional stream: big-endian u32 CBOR header length,
then the CBOR `Request` / `Response`, then exactly the stated raw body length.
Headers above one MiB, trailing CBOR data and short/long bodies are rejected.
Reads use `(offset, length)` ranges, clamp length at EOF, and reject overflowing
or beyond-EOF ranges. `read_stream` does not buffer a whole file. Header buffers
grow with the bytes received, not with the advertised length.

Resource limits: a host accepts at most four connections per peer (one is
normally held) and serves at most 32 concurrent requests per connection; further
streams wait in QUIC flow control. Every body transfer, on both host and client
(Read bodies, Write uploads, `read_stream` and `write_stream`), fails once it
makes no progress for `NodeOptions::request_timeout`, so a stalled peer cannot
hold a handler's file handle or a caller forever. A denied or failed upload
still reports the host's response (normally `Response::Denied`) when it arrives
within that timeout, instead of the transport error from the stopped stream.

The permanent endpoint rejects unpaired TLS identities. Each request checks
current grants, using source equality and slash-component subtree matching.
Both rename paths require write permission. Write, StatPartial, Remove and both
Rename paths must be strictly inside the granted subtree: the granted root itself
cannot be overwritten, removed or renamed (Mkdir and reads may name it). Paths reject
traversal, absolute paths, empty components, backslashes, control characters and
bidirectional override/isolate characters. On a Windows host they also reject
`:` (alternate streams), trailing spaces/dots and reserved DOS device names (`CON`,
`PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, superscript-digit variants,
`CONIN$`, `CONOUT$`; any case, with or without an extension); other hosts accept
those as ordinary names. `%` is an ordinary character. Ping, filtered
ListSources and the requesting peer's Grants
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
rechecks current grants, even on existing sessions. A node holds one QUIC
connection per peer (whichever side dialed it) and opens one stream per request;
concurrent requests share a single dial, and a lost connection is redialed on the
next request. A read (Ping, ListSources, List, Stat, Read before its body,
Grants) that fails because the held connection closed under it, for example when
a revoke's remote close races the send, is retried once on a fresh connection.
Mutations are retried only when the stream could not even be opened, never after
their header was sent. Already delivered bytes cannot be recalled. Requests are traced at debug with public peer id, operation
and authorization result; paths, labels, codes and handler errors are not logged.

## Persistence, events and API additions

`<data_dir>/net/net.sqlite3` stores peers, address hints, grants and the local
label in one atomic SQLite transaction per update (bundled SQLite, rollback journal, synchronous=FULL).
The supplied data directory must be private to the application user. Reopening
retains identity, pairing and grants; live links begin Offline. Public discovery
allows reconnecting after addresses change; offline address hints only remain
usable while the remote node keeps those addresses.
`Node::open` takes an exclusive OS file lock on `<data_dir>/net/LOCK` (released
by `close()` or process exit); a second open of the same directory, from this or
another process, fails with "already in use". Every update re-reads the stored
row inside `BEGIN IMMEDIATE`, applies the change and commits, and only then
replaces the in-memory copy, so a stale cache can never overwrite newer rows.
Consequently a grant cannot be revoked (or resurrected) by a second process: all
grant changes go through the one node that owns the directory, normally the
daemon. Disk writes never hold the lock that request checks use.

`events()` returns a new crossbeam subscription each time. `Paired` and
`DropReceived` always arrive (the queue is unbounded for them); the other events
describe state, are dropped while 256 events wait unread, and a slow subscriber
should refresh `peers()` / `grants()`. `Request` is coalesced: the first request of
each second per device. Cloning a receiver shares its queue, as with any crossbeam
receiver. `peers()[i].link`
reflects the held connection: `Lan` means a direct IP path (not necessarily the
same LAN), `Relay` a relayed one. `PeerOnline` is emitted only when the link
changes (Offline to online, or Relay/Lan switches) and `PeerOffline` only when
the last connection to that peer closes, not per request. `last_seen` is kept
current while connected and persisted with the next update and at `close()`.
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
granted; listings walk every page; writes buffer to an anonymous temp file (the OS
deletes it with the handle; a big upload sits in the temp folder until `flush()`)
and are pushed on `flush()` as one verified final piece; `read_range` asks the device
for a byte range (the protocol's ranged `Read`); `local_copy` downloads into a
`keel-node-*` folder under `<keel_vfs::cache_dir()>/node` (`KEEL_DATA_DIR` honoured),
and folders a day old are swept when a provider is created; `caps` follow the grants
devices reported; `remove` is permanent from the
client's view (a library host trashes). Its calls block on the node's runtime, so
call them off async tasks.

A library source browsing a device (kind `Device`, root `node://`; `add_source`
refuses either without the other) keeps the content ids its host lists apart, as
claims (`remote_cas`): they show in a redundancy hover list flagged `claimed` but
never count as copies for the LastCopy warning, the protection counters or the
duplicate finder. A host lists only ids it confirmed and whose bytes did not drift.

`LibraryHandler` serves a `keel_core::Library`: its non-device sources, lists and
stats from the live provider, reads/writes/mkdir/rename/remove through the router.
Each path must canonicalize to exactly `<canonical root>/<path>` (compared exactly,
so a link or junction anywhere on the way, a short name or a case variant of an
existing name is refused; a listing shows the real names); writes re-check before
publishing. A device source or a `node://` root is never served. Device writes go to
local sources through a staging file on this machine, and to SFTP and cloud sources
through the router to their provider: a whole file streams into the provider's own
upload, placed on `flush()` only after the BLAKE3 check and a fresh path check; pieces
go to a `.keel-partial-<id>` file beside the target written in place
(`Provider::write_at`), so `StatPartial` resumes there too. A read-only provider or an
unreachable source refuses the write. Every served request is appended to the library's op log as
`net.<op>` with the peer id and label; a logger thread writes the entries in batches
(the library database syncs every commit), retries a failed batch once, and is
flushed when the node closes (`Handler::close`, called by `Node::close`) and when the
handler is dropped.

## Library sync

Tags, tag assignments, favorites and content ids of tagged files follow a user across
their own devices (keel-core's sync log; see `keel_core::Library::sync_page` and
`sync_apply`). `Node::set_sync_peers` names the devices sync is on with (Settings →
Devices, `[devices] sync`; empty by default). A device pulls from those devices on
connect, when sync is turned on and every `NodeOptions::sync_every` (60 s; zero:
only `Node::sync_now`), with `Request::SyncPull { since }` answered by
`Response::SyncEntries { entries, more, upto }`: at most `keel_core::SYNC_PAGE`
(1,000) entries and well under the header limit per page, asked again from `upto`
while `more`. A host answers only devices it syncs with (`Response::Denied` for the
others), and serves only its own changes (sync is pairwise: no relaying). The puller
checks the page (ordered sequence numbers inside `(since, upto]`), names every entry's
device from the connection, applies at most `NodeOptions::sync_rate` (10,000) entries
per device and minute (the rest waits for the next minute) through
`Handler::sync_apply`, and stops when the switch goes off or the device is forgotten
(`forget_peer` turns sync with it off; what arrived stays). A pull that changed the
library emits `NetEvent::LibrarySynced`. Sync pulls are not written to the op log.

## Spacedrop

`spacedrop::send(node, lib, peer, paths)` starts a durable keel-core job (kind
`drop`; call `spacedrop::register(lib)` before `resume_all`). It walks folders, offers
the file list (`Request::DropOffer`), and pushes each file in 4 MiB `Write` pieces to
source `drop:<id>`. The offer is answered at once: `Ok`, `Denied`, or `Pending` while
the receiving user decides; the job then polls `Request::DropStatus` (the receiver waits
up to a quarter of its request timeout, at most 5 s, for the answer, one waiter per
offer) and fails after ten minutes without one. Before each file it asks `StatPartial`
and resumes from the staged length, re-hashing the bytes it skips, so the final piece
always carries the whole file's BLAKE3; each sent file is logged as `net.drop-sent`
(`JobCtx::log_op`). A dropped link, or a closed and reopened sender node, is retried
with backoff until nothing has moved for ten minutes; a decline or any other `Denied`
(an invalid offer, a staging failure, `busy`, with the reason), a changed source file,
an unsendable name or an offer too big for one request header (about 1 MiB of names)
fails the job at once; cancelling it, or failing, sends `DropCancel`. A file the
receiver reports complete on resume (its last reply was lost) is logged as
`net.drop-sent` too, with `"resumed": true`.

The receiving node registers the offer, then hands it to `Handler::drop_offer`, which
must not wait: it passes `IncomingDrop::reply` to whoever decides and names the inbox
(default: decline; `LibraryHandler::on_drop(inbox, ask)`). An answer holds for that
device, drop id and file list only: the same id with another list is asked again, and
a declined, cancelled or forgotten drop forgets its answer. A completed drop is
remembered for `spacedrop::DONE_KEEP` (1 hour) by device, id and a hash of its file
list: a re-offer of the same list and `DropStatus` answer `Ok`, and `StatPartial`
reports `complete`, so a sender that lost the reply to the last piece neither asks the
user again nor sends a file twice. At most `PENDING_PER_DEVICE` (4) offers of one device
and `PENDING_TOTAL` (16) in all wait for an answer or staging; more get
`Denied("busy")`. `DropCancel` withdraws a prompt still waiting (`DropReply::withdrawn`),
and so does a sender that stops polling for `NodeOptions::drop_answer_wait` (default
`GIVE_UP`, 10 minutes; a `DropStatus` waiting for the answer counts as polling). Invalid
offers and staging failures are answered `Denied` with the reason (not `Error`, which
the sender retries). Staging is set up only when the sender polls an accepted offer, so
an answer given after the sender left stages nothing.
Only the accepted device may send pieces of that drop; grants are not involved. Offered names must pass the receiving host's path rules above
(so a Windows receiver declines `CON.txt` or `a:b`, and 8.3 short-name shapes such as
`KEEL-P~1`), and every piece and status request rides the peer's one held connection.
Publishing refuses a name whose folder really (after links, junctions and short names)
lies outside the inbox or inside a staging folder. Pieces stage in
`<inbox>/.keel-partial-<key>/` (`<key>` from the device and drop id; hidden on Windows
too) with a `meta.json` written once on accept and a line per published file in
`published` (synced to disk as each file lands), so a restarted receiver resumes when
the sender re-offers the same files.
Each file is verified and moved into the inbox only when complete, with a no-replace
rename (`name (1).ext` when the name is taken, also when two drops land one name at
once), emitting `NetEvent::DropReceived` and logging `net.drop-received` (drop id,
path, name saved as, size, BLAKE3) through `Handler::log`; the staging folder goes
when the drop is complete or cancelled, or the device is forgotten. Staging that an
earlier session left untouched for `spacedrop::STALE` (7 days) is swept when
`on_drop` sets the inbox.

iroh uses an explicit ring CryptoProvider, without touching the process default.
This coexists with keel-vfs's graviola provider. The offline regression test
installs graviola first, then opens nodes and completes a pairing and QUIC ping.

`revoke` returns an error when no grant matches exactly (peer, source, subtree),
so a misspelled subtree is not silently ignored. If the store is unreadable
(not a database, corrupt, or undecodable contents) `Node::open` renames it (and
any rollback journal) to `net.sqlite3.corrupt-<unix time>`, logs a warning and
starts empty: peers must pair again. I/O and permission errors still fail `open`.

Run `cargo test -p keel-net` for offline tests. The ignored relay-only test must
also have `KEEL_NET_RELAY_TEST=1`; it uses short-code discovery and public relays.
