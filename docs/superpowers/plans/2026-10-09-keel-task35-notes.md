# Task 35 implementation record

Scope: implement the Phase 8 Task 35 public transport API in the existing worktree.
The supplied spec, plan, and explicit instruction to implement and commit authorize
execution without further design or approval gates.

Design decisions:
- iroh 1.3.0 (current stable from Cargo and docs.rs), explicit ring TLS provider;
  never change the process default, which cloud code may set to graviola.
- 128 random bits in a short base32 code derive a temporary rendezvous endpoint
  identity. Public iroh address lookup resolves it; full tickets include its
  addresses for offline use. HMAC proofs bind both persistent identities,
  labels, addresses and fresh nonces. Only one successful pairing per invitation,
  with a ten-minute monotonic deadline and endpoint cleanup.
- Paired connections use the persistent iroh TLS identities. Unknown peers are
  rejected on the main endpoint; pairing has a separate temporary endpoint.
- A single bundled SQLite database atomically persists peers, addresses, grants
  and the local label. No secret goes in this database.
- Every request is authorized against current grants. Component-aware relative
  paths reject traversal and platform-specific aliases. Revocation cancels
  handlers and closes that peer's connections, including active streams.
- Add `open_with_options` for offline and relay-only configuration, and
  `write_stream` because `request(Request::Write)` has no argument for its body.
  Preserve all requested method signatures; document additions in crate docs.
- Crossbeam event receivers are independent bounded subscriptions; slow consumers
  may lose events and should refresh peers/grants. No unbounded event queues.

Verification ledger:
- Worktree branch verified as `task35`, initially clean.
- Scope tests first failed on traversal and sibling-prefix leakage, then passed.
- Integration tests caught unnecessary session closure on grant expansion and an
  idle cached-session race after revoke. Grant expansion keeps existing access;
  client operations now own fresh connections, with reads retaining ownership
  through body consumption. Mutations are never automatically retried.
- Fresh-context review identified cancellation-unsafe fragmented write-response
  parsing and a joiner wall-clock comparison. Fixed by retaining one receive
  future and using the host monotonic deadline. Added regression tests for both,
  plus concurrent single-use pairing and host-side expiry.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test -p keel-net`: 18 passed, 1 ignored; offline execution 0.54 seconds.
- Explicitly enabled `KEEL_NET_RELAY_TEST=1` and ran the ignored relay-only test:
  short-code discovery, pairing and ping passed in 3.03 seconds.
- `cargo build --workspace`: passed. The existing app build script warns that
  optional runtime dependency assets have not been fetched in this worktree.
- First `cargo test --workspace` attempt exhausted Windows paging-file memory
  while compiling test binaries concurrently; no machine configuration changes.
- Finishing pass (after the first worker stopped on a usage limit): one cold run
  failed opening SQLite with SQLITE_IOERR_TRUNCATE on Windows (not reproducible
  in 50 stress runs). Dropped WAL mode (single connection, no benefit; removes the
  shared-memory file truncate) and kept synchronous=FULL. Declared rustls `ring`
  explicitly instead of relying on iroh's feature unification.
- Final: fmt, clippy -D warnings, `cargo build --workspace` passed;
  `cargo test -p keel-net` 18 passed, 1 ignored (under 1 s); the env-gated relay
  test passed with `KEEL_NET_RELAY_TEST=1` in about 2 s.

Rulings for consumers:
- The requested base32 NodeId display is retained; iroh 1.3 now displays hex.
  Parsing accepts both forms.
- Task 36's provider adapter must live outside keel-vfs (for example in keel-net
  or an integration crate), because the required SecretStore API already makes
  keel-net depend on keel-vfs. Register the adapter through the existing router.
- Cross-platform execution needs the existing macOS/Linux CI runners; this run
  can execute native tests only on Windows.
