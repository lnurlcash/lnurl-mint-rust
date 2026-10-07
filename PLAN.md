# lnurl-mint-rust: plan

A standalone LNURLcash mint (LUD-25 notes, LUD-26 derivation and Lightning
Address) in one Rust binary that **is** its own Lightning node: LDK for
Lightning, BDK for the on-chain wallet (since 2026-10-07 through ldk-node,
see "ldk-node" under Phases). No lnd/cln/spark backends, no REST
credentials, no funding-source trait.

Sources it draws on:

| Source | What we take |
|---|---|
| `../lnurl-mint` (Python, ~5.8k LOC) | The behaviour spec: endpoints, error strings, fee/sunset/verify/registration/NIP-05/zap semantics, the melt "never guess" invariant, the test suite's attack scenarios (`tests/test_poc_*`). |
| `../cln-mint` (Rust, ~3k LOC) | The code base. It already ports lnurl-mint to Rust on `lnurlcash-core` + `lnurlcash-kernel`, with axum, rusqlite and lnurl-mint's schema. Only `node.rs`, `tasks.rs`, `rpc.rs` and the plugin parts of `main.rs` are CLN-specific. |
| `lnurlcash-core` | All note logic: `cp1`/`ck1`/`cw1`/`cs1`/`cx1` codecs, `decode_spend`, `derive_note_pubkey` (replaces `derivation.py`), `check_leaf`, `check_time_claim`, `spend_domain_of`, note/address-proof digests, mint-fee arithmetic, LNURL bech32. |
| `lnurlcash-kernel` | The authoritative spend check: `verify_key_path` / `verify_script_path` through Bitcoin Core's interpreter. |

The division of labour is the one both crates document: core decodes and
applies the leaf and time rules, kernel decides whether the witness opens `Q`.
The mint itself only adds custody: the notes table, the domains it answers
on, and its clock.

## Why it is faster than lnurl-mint

| lnurl-mint (Python) | lnurl-mint-rust |
|---|---|
| Every invoice, payment, status check and signature is an HTTPS RPC to lnd/cln/spark. | In-process calls on `ChannelManager`. No network hop, no TLS, no macaroon/rune. |
| Settlement is polled (`/verify` lookups, the zap poller every 5 s, `listpays` reconcile with backoff). | Pushed: LDK `Event::PaymentClaimable` / `PaymentClaimed` / `PaymentSent` / `PaymentFailed`. Zap receipts go out the moment a payment is claimed. |
| `cached_fetch_node_info` caches getinfo + graph RPCs for an hour. | Node info is read from local state (`list_channels`, `NetworkGraph`) per request. No cache needed. |
| Uvicorn + pydantic per request, one global lock over one sqlite connection. | axum on tokio. rusqlite in WAL mode with prepared statements: one writer connection behind a mutex (keeps the single-transaction swap/burn semantics) and a small read pool for `GET /w`, lnurlp and verify. |
| Kernel verification through a Python wheel. | `lnurlcash-kernel` linked statically. A merge naming up to `MAX_K1S` notes is verified inside `spawn_blocking`. |
| Separate process and RPC for the node, with its own DB. | One process, one data directory, one seed. |

## Architecture

```
            ┌──────────────── lnurl-mint-rust (one process) ───────────────┐
 HTTPS ───▶ │ axum: LNURL routes (public)    axum: admin routes (localhost)│
            │        │                                │                    │
            │        ▼                                ▼                    │
            │   mint::Mint ── NoteStore (mint.sqlite3, lnurl-mint schema)  │
            │        │  ▲                                                  │
            │        │  └── events: claimed / sent / failed                │
            │        ▼                                                     │
            │   ln::Ln  (LDK: ChannelManager, ChainMonitor, PeerManager,   │
            │            Router + ProbabilisticScorer, NetworkGraph,       │
            │            OutputSweeper, BackgroundProcessor)               │
            │        │                │                                    │
            │   wallet::Wallet (bdk_wallet, P2TR/P2WPKH descriptors)       │
            │        │                                                     │
            │   chain::Source  (bitcoind RPC or Esplora)                   │
            └──────────────────────────────────────────────────────────────┘
```

One BIP39 seed, from a file in the data dir (created on first run, mode 0600).
LDK's `KeysManager` gets `HMAC(seed, "ldk")`. BDK gets BIP86/BIP84 descriptors
from the same seed. The `cs1` certificate key is the LDK node secret, so
`mintPubkey` stays the node id, as in lnurl-mint and cln-mint.

### Crates

- `lightning`, `lightning-invoice`, `lightning-net-tokio`,
  `lightning-background-processor`, `lightning-persister` (or our own SQLite
  `KVStore`, see below), `lightning-rapid-gossip-sync`,
  `lightning-block-sync` (bitcoind) / `lightning-transaction-sync` (Esplora).
  Optional: `lightning-liquidity` for LSPS2 JIT inbound channels.
- `bdk_wallet` with `bdk_bitcoind_rpc` or `bdk_esplora`. Wallet state is
  persisted with BDK's rusqlite store.
- `lnurlcash-core`, `lnurlcash-kernel`.
- `axum`, `tokio`, `rusqlite` (bundled), `serde`, `qrcode` (svg), `tracing`.
  For the NIP-57 receipts: `tokio-tungstenite` plus `secp256k1` schnorr. No
  full nostr SDK.

Pin the current LDK release and the matching `bdk_wallet` in phase 0 and
check that they share one `bitcoin` version, because `ChannelManager` funding
needs BDK's PSBT and transaction types.

### Modules

| Module | Origin | Notes |
|---|---|---|
| `config.rs` | new (clap + env) | Same `.env` keys as lnurl-mint where they still apply. `FUNDINGSOURCE_*` keys go away. New keys: `DATA_DIR`, `NETWORK`, `CHAIN_SOURCE` (`bitcoind://…` or `esplora:https://…`), `LN_LISTEN`, `LN_ALIAS`, `ADMIN_LISTEN`, `ADMIN_TOKEN`, `RGS_URL`, optional `LSP_*`. |
| `db.rs` | cln-mint `db.rs` | lnurl-mint's schema, so an existing `mint.db` opens as is. Drop `pay_index` (LDK replays events itself). Add a `melts.payment_id` column (always `== payment_hash`). |
| `spend.rs` | cln-mint `spend.rs` | Unchanged: core decodes, kernel verifies against every domain. |
| `mint.rs` | cln-mint `mint.rs` | The protocol logic. Its `node: Node` field becomes `ln: Arc<Ln>`. |
| `lnurl.rs` | cln-mint `lnurl.rs` | axum routes and the front page. |
| `ln/mod.rs` | new | Builds and runs LDK. Exposes the small surface `mint.rs` needs (see the next section). |
| `ln/events.rs` | new | The event handler: the only place where Lightning outcomes reach `NoteStore`. |
| `ln/store.rs` | new | `KVStore` over SQLite (`ldk.sqlite3`), so backups stay at two SQLite files plus the seed. `FilesystemStore` would work as well. |
| `wallet.rs` | new | BDK wallet, sync loop, `FeeEstimator` and `BroadcasterInterface` implementations shared with LDK. |
| `admin.rs` | replaces cln-mint `rpc.rs` | Localhost HTTP with a bearer token. Same calls as cln-mint's RPC methods (`info`, `note`, `pending`, `reconcile`, `listusers`), plus node operations: `newaddress`, `balance`, `connect`, `openchannel`, `closechannel`, `listchannels`, `listpayments`, `send` (on-chain). |
| `nostr.rs` | lnurl-mint `nostr.py` | NIP-57 zaps. cln-mint left them out; we bring them back because event-driven settlement makes them cheap. |

## The Lightning surface `mint.rs` uses

Each cln-mint `Node` method maps onto in-process LDK:

| cln-mint `Node` | LDK implementation |
|---|---|
| `create_invoice(amount, description_hash)` | `ChannelManager::create_bolt11_invoice` with `Bolt11InvoiceDescription::Hash`, so the invoice commits to the LUD-06 metadata the way cln-mint's invoices do. The preimage is derived statelessly from LDK's inbound key, and the mint stores only `payment_hash` and `pr`, matching lnurl-mint's "store hashes, not secrets" rule. |
| `wait_any_invoice` / `invoice_paid` | Gone. `Event::PaymentClaimable` and `Event::PaymentClaimed` replace them (see below). |
| LUD-21 preimage for a mint invoice | Recomputed on demand from the stored `pr`'s payment secret with `ChannelManager::get_payment_preimage`, so it is never persisted and still looked up live. |
| `pay(bolt11, maxfee)` | `ChannelManager::pay_for_bolt11_invoice` with `PaymentId(payment_hash)`, `Retry::Timeout(…)` and `max_total_routing_fee_msat` set to the fee cap. Fee cap: cln-mint's rule, the note's mint fee with a floor of 0.5% or 5 sat. |
| `pay_status` | `list_recent_payments` plus our `melts` table. Only reconcile uses it. |
| `sign_message` | `secp256k1` recoverable signature by the node secret over core's `note_signature_digest`. The message bytes and the "Lightning Signed Message:" wrapping stay identical, so existing `cs1` certificates still verify. |
| `info` | `node_id`, the configured alias and color, `list_channels`, and our own announced capacity from `NetworkGraph`. That is still only public data, as lnurl-mint's README requires. |
| `decode_invoice` | `lightning_invoice::Bolt11Invoice::from_str`, no RPC. |

## Money-critical flows

These carry over lnurl-mint's invariants unchanged. Only the mechanism moves.

### Mint (invoice → note)

1. `/p/cb` or `/p/<username>`: validate amount, fee and sunset, resolve the
   note id (comment, or `claim_next_index` on purpose 2), create the invoice
   and run `create_mint` in one DB transaction. If `create_mint` fails, no
   invoice is handed out.
2. `Event::PaymentClaimable { payment_hash, amount_msat, purpose }`: look up the
   `mints` row. Claim only if the row exists, is unsettled and
   `amount_msat` is at least the invoiced amount. Otherwise call
   `fail_htlc_backwards`. This replaces CLN's "only known invoices get paid".
3. `Event::PaymentClaimed`: `settle_mint(payment_hash)`, then any zap receipt.
   `settle_mint` must be idempotent. If the DB write fails, the handler returns
   `Err(ReplayEvent)` so LDK delivers the event again after a restart. A crash
   between claim and settle therefore cannot lose a note.

### Melt (note → payment)

1. `/w/cb` with `pr`: decode the invoice and require amount = note value.
   Refuse it if it is one of our own mint invoices (`mints.payment_hash`), was
   already used by a melt, or is payable to our own node id. Then run
   `mark_pending` and `record_melt` in one transaction **before** sending,
   reply `OK`, and spawn the send.
2. `pay_for_bolt11_invoice(PaymentId = payment_hash)`. LDK refuses a
   duplicate `PaymentId`, which closes the duplicate-melt race at the node
   layer as well as in the DB.
3. `Event::PaymentSent` → `finalize_melt` + `mark_melt_settled`. The actual
   routing fee comes from the event and goes to `mint_log`.
4. `Event::PaymentFailed` → `restore`. LDK emits this only after every HTLC of
   the payment has resolved and retries have stopped, so it is the positive
   "confirmed not paid" lnurl-mint's `_melt_pay` waits for. That includes the
   hodl-invoice case its `PaymentFailed` docstring worries about. An immediate
   `Err` from `pay_for_bolt11_invoice` (route not found before any HTLC left,
   or an expired invoice) is also proof that nothing was sent, so it restores
   too.
5. Startup reconcile replaces lnurl-mint's polling loop. After `ChannelManager`
   has been reloaded and chain sync has caught up, LDK has rebuilt pending
   outbound payments from the `ChannelMonitor`s, which are the source of truth
   for in-flight HTLCs. For each `pending_melts()` hash:
   - pending in `list_recent_payments`: leave it, an event will resolve it;
   - fulfilled: finalize;
   - abandoned or failed: restore;
   - absent: restore, but only if the crash window (between `mark_pending` and
     `pay_for_bolt11_invoice`) is the only way to get there. A test must cover
     this. If that cannot be proven, leave the note pending and log it, as
     lnurl-mint does. Never guess.

   The admin `reconcile` call runs the same pass on demand.

### Rotate, split, merge, register

These are pure DB plus core/kernel and contain no Lightning. They are ported
from cln-mint as is: the burns table answers retries with the original result,
split fees work as in cln-mint, and LUD-26 BIP-340 proofs include the domain.
Diff these against the Python behaviour in the conformance step, because
cln-mint is newer here: it refuses legacy 65-byte `ck1`s and puts the domain
into the registration proof.

## Liquidity and operations

lnd/cln/spark operators never had to think about this. Running our own node
means the mint has to provide it.

- **Outbound** (melts): the operator funds the BDK wallet (`admin newaddress`)
  and opens channels (`admin openchannel <node>@<addr> <sats>`). LDK's
  `OutputSweeper` sweeps closed-channel outputs back into the BDK wallet.
- **Inbound** (mints): at first, operators buy or lease inbound liquidity, or
  dual-fund with a peer. Optional phase 6: LSPS2 through `lightning-liquidity`,
  where the mint's invoices carry an LSP route hint and the LSP opens a JIT
  channel on the first payment. This is the closest thing to spark's
  "no channel management".
- **Gossip**: Rapid Gossip Sync from `RGS_URL` at startup and hourly. P2P
  gossip is optional.
- **Watchtower / backups**: `ldk.sqlite3`, `mint.sqlite3` and the seed. Document
  that a stale `ldk.sqlite3` restore is dangerous (justice transactions), as
  with any Lightning node. An optional static channel backup export comes
  later.
- **Health**: replaces the funding-source monitor. Checks chain tip age,
  connected peer count and usable outbound/inbound capacity. Exposed on
  `admin info` and logged on every change.

## Phases

Progress: **phases 0 and 1 are done** (2026-10-01). Pinned: LDK 0.2.x
(lightning-invoice 0.34), BDK 3.2 with `bdk_file_store`, lnurlcash-core 0.2.3,
lnurlcash-kernel 0.2.3. Phase 1 also fixed a migration bug inherited from
cln-mint: the `mints_note_id` index was created before `note_id` existed, so
an older lnurl-mint database failed to open, and lnurl-mint's column renames
were missing.

**Phase 2 is done, and phase 3 mostly** (2026-10-02), proven end to end on
regtest by `e2e/regtest.py` (also run in CI). Done: the node (LDK 0.2.6,
following ldk-sample), the BDK wallet fed from the same block stream, anchor
fee bumps, the sweeper, the event handler, event-driven mint and melt,
startup/periodic reconcile, `cs1` from the node key, node info, and the admin
node operations. Decisions taken on the way:

- LDK state is in LDK's own `FilesystemStore` under `<DATA_DIR>/ldk`, not a
  SQLite `KVStore`: less code, LDK's audited one. Revisit if backups want one
  file.
- One chain source: bitcoind RPC. Esplora is not supported yet.
- P2P gossip, plus Rapid Gossip Sync on mainnet (added 2026-10-07, see below).
- Peers this node dialled are remembered in `ldk/peers.json`: private
  channels have no address in the gossip graph to reconnect to.
- A melt that LDK refuses before registering the payment (no route, expired,
  onion too large) is aborted on the spot: its notes are released, its melt
  row dropped, and the reason returned. LDK finds the route before it
  registers the payment, so nothing can have been sent.
- Reconcile releases a pending melt whose payment LDK does not list, per
  `list_recent_payments`' contract. The regtest test covers the crash
  window between `mark_pending` and `pay_for_bolt11_invoice`.
- Hard kills: LDK force-closes a channel whose manager on disk is older than
  its monitor ("no funds will be lost"). The regtest test kills the mint
  mid-melt: the channel force-closes, the HTLC resolves on-chain, and the note
  burns exactly when the payee was paid. This is a property of LDK, not a bug
  to fix here. Graceful stops don't hit it.

**Packaging and conformance are done** (2026-10-03):

- **Docker.** A two-stage image on digest-pinned Debian trixie, 152 MB, non-root,
  `DATA_DIR=/data`. Smoke-tested without bitcoind, and against a regtest
  bitcoind: the node starts, funds its wallet and issues mint invoices, and a
  graceful restart keeps its identity.
- **Nix.** A flake with a package (built with `rustPlatform`; its check runs
  cargo test), a NixOS module, and three checks: the package, module
  evaluation, and a NixOS VM test on regtest. Built here in a `nixos/nix`
  container, where all 47 tests passed. The VM test only evaluates here (no
  KVM); CI runs it on GitHub's runners.
- **Conformance.** `lnurlcash-conformance` 0.15.0's grader runs against a live
  regtest mint (`CONFORM=1` in the regtest test, and in CI): 49 checks pass,
  with one warning for the optional `/stats` endpoint.
- **Admin UI and release** (2026-10-03). A web UI on the admin port, with a
  token login that issues an in-memory session cookie, tested in headless
  Chromium by `e2e/admin_ui.mjs` (`UI=1`, and in CI). A release workflow
  pushes `lnurlcash/lnurl-mint-rust` to Docker Hub on a `v*` tag, as
  lnurl-mint does. Lint covers all three languages (rustfmt/clippy, Ruff,
  Biome).
- **Open: the 10% in-flight limit.** LDK accepts at most 10% of an inbound
  channel's capacity in flight by default
  (`max_inbound_htlc_value_in_flight_percent_of_channel`). A mint therefore
  can't receive a single payment above 10% of its largest inbound channel,
  which caps the notes it can mint. Raising it means more value at risk per
  HTLC; decide and make it a setting.

**Where the phases stand** (2026-10-07):

- Phases 0 to 3 are done. Phase 3's milestone, the full round trip
  against a second node, is met: mint, rotate and melt in the default regtest
  run, and split and merge in the conformance grader's spend run
  (`CONFORM=1`, in CI).
- Phase 2 is done. **Rapid Gossip Sync** (2026-10-07): on mainnet the graph
  comes from LDK's snapshot server at startup and hourly (`src/ln/rgs.rs`,
  snapshots capped at 32 MB, applied off the async workers), and peer gossip
  keeps updating it. LDK's testnet server serves data past LDK's two-week
  limit, so only mainnet has a default; `RGS_URL` sets one anywhere. Tested
  against the real mainnet snapshot (an ignored network test). The SQLite
  `KVStore` became LDK's `FilesystemStore` by choice (above).
- Phase 4 is partly covered by the store and HTTP tests and the regtest test;
  the `test_poc_*` ports are still to do one by one.
- Phases 5 and 6 haven't started; NIP-05 is in, from cln-mint.

**ldk-node** (2026-10-07). The hand-wired LDK + BDK node (phases 2 and 3:
`bitcoind.rs`, `node.rs`, `wallet.rs`, `events.rs`, `rgs.rs`, `peers.json`)
is replaced by ldk-node 0.7, which assembles the same LDK 0.2 and BDK pieces,
and adds LSPS2. What changed:

- State lives in `<DATA_DIR>/ldk-node` (ldk-node's SQLite store: channel
  manager and monitors, wallet, payments, peers, graph). The 64-byte entropy
  is derived from the existing `seed`; the mint-preimage key from the seed
  too, under another label. Not compatible with a data directory of the
  hand-wired node: there were no deployments to migrate.
- Mint invoices are manual claims (`receive_for_hash`): the preimage is
  `HMAC(seed key, note id)`, and the mint claims on `PaymentClaimable` only
  for an unsettled mint invoice. Events are acknowledged (`event_handled`)
  only after the store took them.
- Operator invoices are ldk-node's own (`receive`): the `operator_invoices`
  table is no longer written.
- Reconcile follows ldk-node's payment store: Succeeded burns, Failed
  releases, Pending waits, and a payment it never heard of (a crash between
  `mark_pending` and `send`) is sent again, its original invoice, once a
  channel is usable. ldk-node records a send refused before anything left as
  Failed, so such a melt is released on the next round.
- bitcoind cookie: ldk-node keeps the credentials it was built with, so the
  mint watches the cookie and exits non-zero when it changes, for its
  supervisor to restart it.
- **LSPS2 inbound liquidity**: `LSP_NODE`/`LSP_TOKEN`, and a bootstrap
  invoice (`receive_via_jit_channel`) paid from outside the mint has the LSP
  open the first inbound channel. The LSP is a zero-conf and no-anchor-reserve
  trusted peer. A test LSP (`--features test-lsp`, `TEST_LSP=true`) runs in
  the regtest test.

0. **Skeleton.** Cargo workspace with one binary. Pin LDK, BDK, core and
   kernel. CI runs fmt, clippy and `cargo test` (the kernel build needs
   cmake and Boost headers). Nix flake and Dockerfile: done, see above.
1. **Lift cln-mint.** Copy `db.rs`, `spend.rs`, `mint.rs`, `lnurl.rs`,
   `structs.rs`, `parse.rs` and `vectors.rs`. Put a temporary `Ln` stub behind
   the same method names, with an in-memory fake for tests. All non-Lightning
   endpoints and the LUD-25/26 vectors pass.
2. **LDK node.** Chain source, BDK wallet, `KeysManager`, `ChannelManager`,
   `ChainMonitor`, SQLite `KVStore`, `PeerManager` on `LN_LISTEN`,
   `BackgroundProcessor` and RGS. Admin API for node operations. Milestone:
   open a channel and pay to and from the node on regtest.
3. **Wire the mint.** Invoice creation with description hash, the event
   handler, melt send, startup reconcile, `cs1` signing with the node key,
   and node info. Milestone: the full mint → rotate → split → merge → melt
   round trip on regtest against a second node (LDK or CLN).
4. **Parity with lnurl-mint.** Port the attack scenarios from
   `lnurl-mint/tests/test_poc_*` as Rust integration tests: duplicate melt,
   settle race, mark_pending race, reconcile in-flight race, collision
   griefing, fee conservation, fee loop, verify race, pending info leak, and
   melt restore double payout. Add crash-injection tests for the two crash
   windows described above. Then run lnurlcash-core's `client` and the
   lnurlcash-kit conformance grading against a running regtest mint.
5. **Zaps, NIP-05, frontend polish.** NIP-57 receipts fire from
   `PaymentClaimed` with no poller. The front page shows node id, channels,
   capacity and mempool/amboss links.
6. **Optional.** LSPS2 inbound, BOLT12 offers alongside LUD-06, and internal
   melts (a melt whose invoice is another note's mint invoice on this same
   mint gets settled in the DB without touching Lightning).

## Migration from a running lnurl-mint

The notes table is a liability ledger. The sats behind it live on the old
node, so cutover is a funds move plus a DB copy:

1. Stand up lnurl-mint-rust and open enough channels to cover
   `outstanding_msat` plus a buffer.
2. Stop the old mint once `pending_melts()` is empty and every unpaid mint
   invoice has expired (or set `SUNSET_MINT` for a day first to drain new
   mints).
3. Copy `mint.db` into `DATA_DIR/mint.sqlite3`. The schema is the same.
   `cs1` certificates issued by the old node no longer match the new
   `mintPubkey`, so wallets see an "unverified issuer" until they rotate.
   Document this, or offer an endpoint that re-certifies on rotate.
4. Move the old node's funds over by closing its channels or looping out.

## Risks and open questions

- **Two `secp256k1` versions** (confirmed in phase 0). lnurlcash-core uses
  0.33, while LDK 0.2 and BDK 3.2 share `bitcoin` 0.32 with secp256k1 0.29.
  Both link, but their types do not mix, so keep `[u8; 32]`/`[u8; 64]` at the
  boundary or bump core.
- **One SQLite binding** (found in phase 0). BDK's `rusqlite` feature pins
  rusqlite 0.31, and two `libsqlite3-sys` versions cannot link into one
  binary. The BDK wallet persists through `bdk_file_store` instead, and the
  mint and the LDK `KVStore` use rusqlite 0.40.
- **Kernel build cost.** libbitcoinkernel takes minutes to build from source.
  Cache it in CI. Release binaries come from the optimized profile, as in
  cln-mint.
- **Absent-payment proof** in the reconcile step above: LDK documents it
  (`list_recent_payments`), and the regtest test exercises it.
- **One coin, one fee bump.** LDK's anchor bumps spend confirmed wallet coins.
  With a single UTXO, a second bump before the first confirms finds nothing
  to spend. Operators should keep a few confirmed UTXOs; automating that
  reserve is open.
- **No spark equivalent.** A mint now needs channels. LSPS2 (phase 6) is the
  mitigation.
- **ldk-node instead of raw LDK?** ldk-node would cut phase 2 to a few days,
  but it is itself a node abstraction and owns the event loop and payment
  store. Raw LDK gives exact control over claim decisions and payment ids,
  which is the reason for this rewrite. Recommendation: raw LDK.
- **Which spec wins** where lnurl-mint and cln-mint differ (legacy `ck1`,
  registration proof message, zaps): follow lnurlcash-core and the current
  LUD-25/26 text, and record each difference in the README as cln-mint does.
