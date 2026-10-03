# lnurl-mint (Rust)

An [LNURLcash](https://github.com/lnurl/luds/blob/luds/25.md) mint, bearer
notes on LNURL-withdraw links (LUD-25) with deterministic notes and Lightning
Address auto-mint (LUD-26), that is its own Lightning node: LDK for Lightning,
BDK for the on-chain wallet, in one binary. There are no lnd, cln or spark
backends to configure.

It is a rewrite of [lnurl-mint](../lnurl-mint) (Python) built on the code of
[cln-mint](../cln-mint), which already ported lnurl-mint's protocol logic to
Rust. Note handling comes from
[`lnurlcash-core`](https://github.com/lnurlcash/lnurlcash-core); whether a
spend opens its note is decided by Bitcoin Core's own interpreter, through
[`lnurlcash-kernel`](https://github.com/lnurlcash/kernel).

**Status: phases 0–2 of [PLAN.md](PLAN.md), and most of 3.** The node runs
in-process and the money paths work against real nodes on regtest: minting,
melting, LUD-21 verify, `cs1` certificates signed by the node key, restarts,
and crash recovery. It passes
[lnurlcash-conformance](https://www.npmjs.com/package/lnurlcash-conformance)'s
grader against a live regtest mint. It ships as a Docker image and a nix
flake with a NixOS module. Not yet: NIP-57 zaps, Rapid Gossip Sync, LSPS2
inbound liquidity.

## How it differs from lnurl-mint and cln-mint

* **One funding source: this process.** LDK runs the node, BDK the on-chain
  wallet, both syncing from your bitcoind. There are no RPC credentials to an
  external node, so the mint now needs its own channels and liquidity (see
  "Running a node").
* **Settlement is pushed by LDK events, never polled.** A payment arriving is
  claimed only if it pays an unsettled mint invoice (anything else is failed
  back), and its note is credited as it is claimed. A melt's notes burn on
  `PaymentSent` and are released on `PaymentFailed`, which LDK emits only once
  no HTLC of the payment is left. A store write that fails makes LDK replay
  the event, across restarts too.
* **A melt that provably cannot leave is answered with its reason** (for
  example "Could not find a route to pay this invoice."), its notes are
  released and its invoice stays usable. lnurl-mint answers `OK` first and
  reports nothing. Once a payment has left, the answer is `OK` as before.
* **No preimage is stored for a mint invoice.** LDK derives it from the node's
  keys, and LUD-21 recomputes it from the invoice's payment secret.
* **Mint invoices commit to the LUD-06 metadata by `description_hash`**, as in
  cln-mint. Legacy 65-byte `ck1`s are refused, and registration proofs bind
  the domain (LUD-26), as in cln-mint.

## Building

You need Rust 1.87 or newer, a C++20 compiler, CMake ≥ 3.22 and Boost ≥ 1.74
headers (`apt install cmake libboost-dev`). `lnurlcash-kernel` compiles Bitcoin
Core's kernel from source and links it statically, so the first build takes a
few minutes.

```sh
cargo build --release   # or --profile optimized for a stripped, LTO'd binary
```

## Running

```sh
cp .env.example .env    # set BASE_URL, BITCOIND_RPC and its credentials
./target/release/lnurl-mint
```

Every option is a flag (`lnurl-mint --help`) and an environment variable,
read from `.env` too, under lnurl-mint's names. Run one process per data
directory: note reservation and melt reconciliation are coordinated inside
the process.

### Running a node

On first start the mint creates `<DATA_DIR>/seed`, the one secret both the
node's keys and the wallet's are derived from. **Back up the whole data
directory**: the seed alone does not recover channel funds, and restoring an
old copy of `ldk/` while channels are open can lose them, as with any
Lightning node.

```
<DATA_DIR>/seed           32 bytes, mode 0600
<DATA_DIR>/ldk/           LDK: channel manager and monitors, graph, scorer, sweeper, peers.json
<DATA_DIR>/wallet.bdk     the on-chain wallet
<DATA_DIR>/mint.sqlite3   the notes
```

The node needs outbound liquidity to pay melts and inbound liquidity to be
paid for mints. With the admin API: fund the wallet (`POST /node/address`),
open channels (`POST /node/channels`), and let a peer open one to you (inbound
channels are accepted). Channels are anchor channels, so keep some confirmed
coins in the wallet: closing a channel pays its fee from them. Funds from a
closed channel are swept back to the wallet.

Stop the mint with SIGTERM/Ctrl-C: it writes the channel state on the way
out. A hard kill (SIGKILL, power loss) just after a payment can leave LDK's
channel manager older than a channel monitor. LDK then force-closes that
channel on the next start. No funds are lost beyond on-chain fees, and a melt
caught in it resolves on-chain: its note burns if the payee was paid, and is
released otherwise.

The note database keeps lnurl-mint's schema, and an lnurl-mint `mint.db`
opens as is (older column names are migrated on first open). See PLAN.md's
"Migration" for moving a live mint, whose sats sit on its old node.

## Docker

```sh
docker build -t lnurl-mint .
docker run -d --name lnurl-mint --network host \
  -v lnurl-mint:/data --env-file .env \
  lnurl-mint
```

The image runs as a non-root user (uid 1000) with `DATA_DIR=/data`: keep that
volume, it holds the seed and the channels. `--network host` lets
`BITCOIND_RPC=127.0.0.1:8332` reach a bitcoind on the host, and keeps the
admin API on the host's loopback. Without host networking, publish `8111`
(LNURL) and `9735` (peers), and point `BITCOIND_RPC` at bitcoind's address.

Stop it with `docker stop -t 60`: on SIGTERM the node writes its channel
state before exiting. Docker's default 10 seconds, then a SIGKILL, can
force-close a channel (see "Running a node").

The binary links Bitcoin Core's kernel statically. The runtime image holds
it, libc and the C++ runtime, nothing else. Base images are pinned by digest.

## Nix

```sh
nix build                 # the package; its build runs cargo test
nix run . -- --help
nix develop               # cargo, the kernel's C++ toolchain, bitcoind, python, node
nix flake check -L        # package, module evaluation, and a NixOS VM on regtest
```

On NixOS, run it as a hardened systemd service next to nixpkgs' bitcoind:

```nix
{
  inputs.lnurl-mint.url = "github:dni/lnurl-mint-rust";

  outputs = { nixpkgs, lnurl-mint, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        lnurl-mint.nixosModules.lnurl-mint
        {
          services.bitcoind.main = {
            enable = true;
            extraConfig = "rpccookieperms=group";
          };
          services.lnurl-mint = {
            enable = true;
            baseUrl = "https://mint.example.com";
            bitcoind = {
              rpc = "127.0.0.1:8332";
              unit = "bitcoind-main.service";
              cookieFile = "/var/lib/bitcoind-main/.cookie";
            };
            extraGroups = [ "bitcoind-main" ];
            lightning.openFirewall = true;
            # secrets stay out of the nix store: ADMIN_TOKEN=...
            environmentFiles = [ "/run/secrets/lnurl-mint" ];
          };
        }
      ];
    };
  };
}
```

The service runs as a dynamic user with a locked-down sandbox. Its state
(seed, channels, wallet, notes) lives in `/var/lib/lnurl-mint`, mode 0700,
and it gets 120 seconds to stop gracefully. Every other setting in
`.env.example` goes through `services.lnurl-mint.settings`. On test
networks bitcoind keeps its cookie in a 0700 subdirectory; there, use an RPC
user and password in an environment file.

## Endpoints

| | |
|---|---|
| `GET /.well-known/lnurlp/<username>` | LUD-16 payRequest with `withdrawLink`. `_` and `USERNAME` are the mint itself; a registered username also carries `text/cpub`. |
| `GET /p/cb?amount=&comment=` | Mint: `comment` is `cp1<Q>` or a bearer note's hex `h`. |
| `GET /p/<username>?amount=` | Auto-mint onto a registered branch (purpose 2). |
| `POST /p/<username>?cx1=&sig=[&npub=]` | Register a username, or overwrite one (proven by the branch on file). |
| `DELETE /p/<username>?sig=` | Unregister. |
| `GET /w?k1=<spend>` / `GET /w?p=<cp1 or h>` | Informational withdrawRequest; never burns. |
| `GET /w/cb` | Melt (`k1`, `pr`), rotate (`k1`, `p1`), split (`k1`…, `amount`, `p1`, `p2`), merge (`k1`…, `p1`). |
| `GET /verify/<payment_hash>` | LUD-21, for mint invoices and melts. |
| `GET /.well-known/lnurlw/<username>` | Informational: node, bounds, outstanding total. |
| `GET /.well-known/nostr.json?name=` | NIP-05. |
| `GET /` | The mint's Lightning Address and its QR code. |

Every LNURL endpoint answers HTTP 200, with `{"status": "ERROR", "reason"}` on
failure (LUD-01).

### Admin API

Served on `ADMIN_LISTEN` only when `ADMIN_TOKEN` is set, with every request
carrying `Authorization: Bearer <ADMIN_TOKEN>`:

| | |
|---|---|
| `GET /info` | URLs, fees, `mintPubkey`, Lightning status and totals. |
| `GET /note/<cp1 or h>` | A note's status and value. |
| `GET /pending` | Notes reserved by in-flight melts, by payment hash. |
| `POST /reconcile` | Resolve pending melts now. |
| `GET /users` | Registered usernames, their `cx1` and next index. |
| `GET /node/balance` | On-chain and Lightning balances. |
| `POST /node/address` | A fresh address to fund the wallet. |
| `GET /node/peers`, `POST /node/peers` `{"peer": "pubkey@host:port"}` | List or connect peers. |
| `GET /node/channels`, `POST /node/channels` `{"peer", "amount_sat", "public"}` | List or open channels (`peer` may be `pubkey@host:port`). |
| `POST /node/channels/close` `{"channel_id", "force"}` | Close a channel. |
| `POST /node/invoice` `{"amount_msat", "description"}` | An invoice paying the node itself, crediting no note. |
| `GET /node/invoice/<payment_hash>` | Whether such an invoice was paid. |
| `POST /node/pay` `{"bolt11", "max_fee_msat"}` | Pay from the node's liquidity. |
| `GET /node/payment/<payment_hash>` | `complete`, `pending` or `absent`. |
| `POST /node/send` `{"address", "amount_sat"}` | Send on-chain (everything, without `amount_sat`). |

## Tests

```sh
cargo test
```

This runs LUD-25's and LUD-26's test vectors against Bitcoin Core's
interpreter, the store's transactions and migrations, the protocol over HTTP
for everything that needs no Lightning, and checks that `cs1` signatures are
byte-identical to LDK's own `signmessage`.

The money paths run end to end on regtest, with a real bitcoind and three
mint processes (A under test, B paying and being paid, C unreachable):

```sh
BITCOIN_BIN=/path/to/bitcoin-31.1/bin MINT_BIN=target/debug/lnurl-mint \
    python3 scripts/regtest_e2e.py
```

The test covers channel opens from the BDK wallet, a real mint and its LUD-21
proof, rotate, a real melt and its proof, a melt that can't be routed (refused,
note kept), a restart with reconnection, a crash between reserving a melt's
notes and sending it (released on start), and a SIGKILL mid-melt (note burned
exactly when the payee was paid, funds swept back from the closed channel).
CI runs it too.

With `CONFORM=1` the same run also grades mint A with
[lnurlcash-conformance](https://www.npmjs.com/package/lnurlcash-conformance)'s
`lnurlcash-conform` (pinned to 0.15.0, fetched with `npx`, so it needs node).
It mints two fresh notes with real payments from B. On the first it runs the
read-only checks plus the minted-value check. The second gets the full
`--spend` run: rotate, split and merge, retries, refusals that must be atomic,
script-path spends and time claims, and domain-bound key-path spends. CI runs
this too. The grader never melts; the regtest test covers melts.

```sh
CONFORM=1 BITCOIN_BIN=... MINT_BIN=target/debug/lnurl-mint python3 scripts/regtest_e2e.py
```

Current result: 49 checks pass. The one warning is the optional `/stats`
endpoint, which is outside LUD-25.
