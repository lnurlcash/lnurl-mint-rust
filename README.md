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

Released images are on Docker Hub as
[`lnurlcash/lnurl-mint-rust`](https://hub.docker.com/r/lnurlcash/lnurl-mint-rust)
(linux/amd64):

```sh
docker run -d --name lnurl-mint-rust --network host --stop-timeout 60 \
  -v lnurl-mint-rust:/data --env-file .env \
  lnurlcash/lnurl-mint-rust
```

Or build it yourself with `make build` (`docker build -t lnurl-mint-rust .`).
`make run` starts it with `.env`; `make run IMAGE_NAME=lnurlcash/lnurl-mint-rust`
runs the released image instead. It runs the container as the user running
`make` (`--user`), with its state in `./data`, owned by that user
(`DATA=/path` to move it).

Run it as the user bitcoind runs as, and the mint reads bitcoind's RPC cookie
with bitcoind's own permissions; nothing changes on bitcoind's side:
* The default cookie is `~/.bitcoin/.cookie`; override it with
  `make run BITCOIN_COOKIE=/home/bitcoin/bitcoin/.cookie`.
* Its directory is mounted read-only at `/bitcoin`, and `BITCOIND_RPC_COOKIE`
  points there. The directory is mounted rather than the file because bitcoind
  writes a new cookie on each restart, and a single-file mount would keep the
  old one.
* `make run` warns when the container won't be able to read the cookie.
  Running as another user also works if bitcoind has `rpccookieperms=group`
  and the data directory is group-searchable (`chmod g+x`); the container gets
  the cookie's group.

bitcoind writes a new cookie each time it restarts. When an RPC call fails
and the cookie has changed, the mint picks up the new one and retries, so a
bitcoind restart needs no mint restart.

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

`lnurl-mint-cli` is on the system's PATH; the admin socket belongs to the
service's dynamic user, so run it as root:
`sudo lnurl-mint-cli --data-dir /var/lib/lnurl-mint info`.

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

### Admin CLI

`lnurl-mint-cli` runs every admin function from a shell, and works whether or
not the admin HTTP API is on. The mint always serves its admin functions on a
Unix socket, `<DATA_DIR>/admin.sock` (`ADMIN_SOCKET` to move it), with mode
0600: running as the mint's own user is the authentication, so no token is
needed. The CLI finds the socket from `DATA_DIR`, in the environment or `.env`,
just as the mint does:

```sh
lnurl-mint-cli info                      # next to the mint's .env
docker exec lnurl-mint-rust lnurl-mint-cli balance
lnurl-mint-cli channels
lnurl-mint-cli open 02abc…@host:9735 1000000 --public
lnurl-mint-cli invoice 50000 "inbound liquidity"
lnurl-mint-cli pay lnbc1… --max-fee-sat 50
lnurl-mint-cli send bc1q… --all
lnurl-mint-cli --help                    # every command
```

Replies are JSON; a refusal goes to stderr with a non-zero exit code. For a
mint elsewhere, `--admin host:port --token …` (or `LNURL_MINT_ADMIN` and
`ADMIN_TOKEN`) uses its admin HTTP API instead.

The socket also keeps a data directory to one mint: a second mint started on
the same `DATA_DIR` finds the first one's socket answering and refuses to
start, before it touches the database or the node.

### Admin UI

The admin HTTP API and web UI are optional: they are served only when
`ADMIN_TOKEN` is set. With it set, open `ADMIN_LISTEN` (default
`http://127.0.0.1:8112/`) in a browser and sign in with the token. The UI has
five tabs:

* **Overview:** node status, notes outstanding, and how much of them the
  node's outbound liquidity covers.
* **Channels:** open, close and force-close channels; connect peers.
* **Payments:** create invoices to receive liquidity, and pay invoices.
* **Wallet:** balances, receive addresses with QR codes, on-chain sends.
* **Notes:** pending melts and reconcile, note lookup, registered usernames.

Signing in trades the token for a session cookie (`HttpOnly`,
`SameSite=Strict`, 12 hours), so the page's script never holds a credential.
A change made with that cookie must come from the admin page's own origin.
Sessions live in memory: a restart signs everyone out. The page loads
nothing from outside the binary and is served under a strict
Content-Security-Policy.

The admin listens on loopback by default. To reach it from elsewhere, use an
SSH tunnel (`ssh -L 8112:127.0.0.1:8112 host`) or a TLS reverse proxy. Behind
a proxy that sets `X-Forwarded-Proto: https`, the cookie is also marked
`Secure`.

### Admin API

Served on `ADMIN_LISTEN` only when `ADMIN_TOKEN` is set. Every request carries
`Authorization: Bearer <ADMIN_TOKEN>`, or the UI's session cookie. The same
routes, without that check, are on the admin socket the CLI uses:

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
| `GET /qr?data=` | An SVG QR code, for the UI. |

## Releasing

The git tag is the version. `Cargo.toml` carries a `0.0.0` placeholder, as
lnurlcash-core does, and local builds report it. To release:

```sh
make release VERSION=0.1.0    # tags v0.1.0 and pushes the tag
```

`.github/workflows/release.yml` then:
* stamps `0.1.0` into `Cargo.toml` and `Cargo.lock`, refusing a tag that isn't
  `vMAJOR.MINOR.PATCH`;
* builds the image, whose `lnurl-mint --version`, admin `/info` and admin UI
  report that version;
* pushes `lnurlcash/lnurl-mint-rust` tagged `X.Y.Z`, `X.Y`, `X` and `latest`;
* creates a GitHub release with generated notes.

The repository needs the secrets `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN`
(a Docker Hub access token, not the password), as in lnurl-mint.

## Lint

```sh
make check    # rustfmt, clippy, ruff (e2e/*.py), biome (admin UI JS/CSS, e2e/*.mjs)
make format   # apply all three formatters
```

Ruff runs through `uvx`. Biome, the conformance grader and Playwright are
pinned in `e2e/package.json` (`npm ci --prefix e2e`).

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
make e2e BITCOIN_BIN=/path/to/bitcoin-31.1/bin
# which is: npm ci --prefix e2e; npx --prefix e2e playwright install chromium
#           CONFORM=1 UI=1 MINT_BIN=target/debug/lnurl-mint python3 e2e/regtest.py
```

`e2e/regtest.py` covers:
* channel opens funded from the BDK wallet;
* a real mint and its LUD-21 proof, a rotate, a real melt and its proof;
* a melt that can't be routed: refused, note kept;
* a restart with reconnection;
* a crash between reserving a melt's notes and sending it: the note is
  released on start;
* a SIGKILL mid-melt: the note burns exactly when the payee was paid, and a
  force-closed channel's funds are swept back;
* bitcoind restarting with a new cookie under a running mint that
  authenticates by cookie: the mint keeps syncing without a restart.

Two optional steps:

* **`CONFORM=1`:** grades A with
  [lnurlcash-conformance](https://www.npmjs.com/package/lnurlcash-conformance)'s
  grader. It mints two fresh notes with real payments from B. On the first it
  runs the read-only checks plus the minted-value check. The second gets the
  full `--spend` run: rotate, split and merge, retries, refusals that must be
  atomic, script-path spends and time claims, and domain-bound key-path
  spends. Current result: 49 checks pass. The one warning is the optional
  `/stats` endpoint, which is outside LUD-25. The grader never melts; the
  regtest test covers melts.
* **`UI=1`:** runs `e2e/admin_ui.mjs`, the admin UI in headless Chromium
  against A. It checks a wrong token and the login, every tab with live data,
  an invoice and an address with their QR codes, the session surviving a
  reload, every tab at phone width, and logout. Any console error other than
  the expected 401s fails it, CSP violations included.
  `UI_SCREENSHOTS=<dir>` keeps its screenshots.

CI runs all of it in four jobs: lint, test, e2e and docker, plus `nix flake
check`. When the e2e job fails it uploads the mints' logs and the screenshots.
