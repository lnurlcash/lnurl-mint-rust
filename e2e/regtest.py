#!/usr/bin/env python3
"""End-to-end on regtest: real bitcoind, real LDK nodes, real payments.

lnurl-mint processes share one bitcoind:

- A, the mint under test
- B, a counterparty: it pays A's mint invoices and receives A's melts. It is
  also an LSPS2 provider (TEST_LSP, a `--features test-lsp` build)
- C, a node no one has a channel with: melting into its invoice must fail
  before anything is sent
- F, a fresh mint with no funds and no channels: B opens its first inbound
  channel when A pays F's bootstrap invoice
- D, a mint on its own bitcoind, authenticating by cookie
- E (with ELECTRS set), a mint whose node knows the chain only through
  electrs: ELECTRS=/path/to/electrs, or ELECTRS=docker:<image>

Only the public LNURL endpoints and the admin API are used, the way a wallet
and an operator would. Needs bitcoind/bitcoin-cli (BITCOIN_BIN, a directory)
and a built lnurl-mint (MINT_BIN). Everything lives in a temporary directory.

Optional, with `npm ci --prefix e2e` first:

- CONFORM=1 grades A with lnurlcash-conformance's grader
- UI=1 drives the admin web UI in a headless browser (needs `npx --prefix e2e
  playwright install chromium`); UI_SCREENSHOTS=<dir> keeps its screenshots

    BITCOIN_BIN=/path/to/bitcoin/bin MINT_BIN=target/debug/lnurl-mint \\
        python3 e2e/regtest.py
"""

import hashlib
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

BITCOIN_BIN = os.environ["BITCOIN_BIN"]
MINT_BIN = os.path.abspath(os.environ.get("MINT_BIN", "target/debug/lnurl-mint"))
RPC_PORT = 18543
TOKEN = "e2e-token"

work = tempfile.mkdtemp(prefix="lnurl-mint-e2e-")
procs: dict[str, subprocess.Popen] = {}


def log(msg: str) -> None:
    print(f"[e2e] {msg}", flush=True)


def cli(*args: str) -> str:
    return subprocess.check_output(
        [
            f"{BITCOIN_BIN}/bitcoin-cli",
            "-regtest",
            f"-datadir={work}/bitcoind",
            f"-rpcport={RPC_PORT}",
            "-rpcuser=u",
            "-rpcpassword=p",
            *args,
        ],
        text=True,
    ).strip()


def mine(n: int) -> None:
    cli("generatetoaddress", str(n), cli("-rpcwallet=miner", "getnewaddress"))


def http(method: str, url: str, body: dict | None = None) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {TOKEN}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            return json.loads(res.read())
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{method} {url}: {e.code} {e.read().decode()}") from None


class Mint:
    def __init__(
        self,
        name: str,
        n: int,
        rpc_port: int = RPC_PORT,
        cookie: str | None = None,
        http_admin: bool = True,
        env: dict | None = None,
    ):
        self.name = name
        self.extra_env = env or {}
        # without it the admin HTTP API is off, and only the CLI's socket is there
        self.http_admin = http_admin
        self.rpc_port = rpc_port
        # bitcoind's cookie file instead of the shared user and password
        self.cookie = cookie
        self.http = 18100 + n * 10
        self.admin = self.http + 1
        self.ln = 19700 + n
        self.dir = f"{work}/{name}"
        self.base = f"http://127.0.0.1:{self.http}"

    def start(self) -> None:
        env = dict(
            os.environ,
            BASE_URL=self.base,
            NETWORK="regtest",
            DATA_DIR=self.dir,
            LISTEN=f"127.0.0.1:{self.http}",
            ADMIN_LISTEN=f"127.0.0.1:{self.admin}",
            LN_LISTEN=f"127.0.0.1:{self.ln}",
            RUST_LOG=os.environ.get("E2E_RUST_LOG", "info,ldk_node=warn,ldk_node::chain::bitcoind=off"),
        )
        env.update(self.extra_env)
        if self.http_admin:
            env["ADMIN_TOKEN"] = TOKEN
        if "ELECTRUM_URL" in env:
            pass  # the chain comes from electrs alone
        elif self.cookie:
            env["BITCOIND_RPC_COOKIE"] = self.cookie
        else:
            env.update(BITCOIND_RPC_USER="u", BITCOIND_RPC_PASSWORD="p")
        if "ELECTRUM_URL" not in env:
            env["BITCOIND_RPC"] = f"127.0.0.1:{self.rpc_port}"
        out = open(f"{work}/{self.name}.log", "a")
        procs[self.name] = subprocess.Popen([MINT_BIN], env=env, stdout=out, stderr=out, cwd=work)
        wait_for(lambda: self.info()["lightning"] == "ready", f"{self.name} up")

    def cli(self, *args: str) -> dict:
        """lnurl-mint-cli over this mint's admin socket."""
        res = run_cli(self, *args)
        assert res.returncode == 0, f"lnurl-mint-cli {' '.join(args)}: {res.stderr}"
        return json.loads(res.stdout)

    def stop(self) -> None:
        proc = procs.pop(self.name)
        proc.send_signal(signal.SIGTERM)
        proc.wait(timeout=60)

    def get(self, path: str) -> dict:
        return http("GET", f"{self.base}{path}")

    def admin_get(self, path: str) -> dict:
        return http("GET", f"http://127.0.0.1:{self.admin}{path}")

    def admin_post(self, path: str, body: dict | None = None) -> dict:
        return http("POST", f"http://127.0.0.1:{self.admin}{path}", body or {})

    def info(self) -> dict:
        try:
            return self.admin_get("/info") if self.http_admin else self.cli("info")
        except (OSError, RuntimeError, AssertionError):
            return {"lightning": "down"}

    def node_id(self) -> str:
        return self.info()["mint_pubkey"]

    def peer(self) -> str:
        return f"{self.node_id()}@127.0.0.1:{self.ln}"

    def usable(self) -> int:
        return sum(c["usable"] for c in self.admin_get("/node/channels"))


def wait_for(check, what: str, timeout: float = 90) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if check():
                return
        except (OSError, RuntimeError, KeyError, subprocess.CalledProcessError):
            pass
        time.sleep(0.5)
    raise AssertionError(f"timed out waiting for: {what}")


def bearer_note(seed: int) -> tuple[str, str]:
    """A bearer note's spend (the preimage) and its reference (h)."""
    preimage = bytes([seed]) * 32
    return preimage.hex(), hashlib.sha256(preimage).hexdigest()


def fund(mint: Mint, btc: str) -> None:
    address = mint.admin_post("/node/address")["address"]
    cli("-rpcwallet=miner", "sendtoaddress", address, btc)


HERE = os.path.dirname(os.path.abspath(__file__))
# e2e/package.json pins the grader and the browser driver: `npm ci --prefix e2e`
NODE_BIN = os.path.join(HERE, "node_modules", ".bin")


def mint_note(a: "Mint", b: "Mint", seed: int, amount_msat: int) -> str:
    """A fresh bearer note on A, paid for by B: its note URL."""
    k1, h = bearer_note(seed)
    invoice = a.get(f"/p/cb?amount={amount_msat}&comment={h}")
    b.admin_post("/node/pay", {"bolt11": invoice["pr"]})
    wait_for(lambda: "maxWithdrawable" in a.get(f"/w?k1={k1}"), f"note {seed} credited")
    return f"lnurlw://127.0.0.1:{a.http}/w?k1={k1}"


def conformance(a: "Mint", b: "Mint") -> None:
    """lnurlcash-conformance's grader against A: read-only, the paid-value
    check on a fresh note, then the full mutating run on another."""
    pay_url = f"{a.base}/.well-known/lnurlp/mint"
    grader = [os.path.join(NODE_BIN, "lnurlcash-conform"), pay_url]
    fresh = mint_note(a, b, 101, 60_000)
    spendable = mint_note(a, b, 102, 60_000)
    for name, extra in [
        ("read-only + minted value", [f"--note={fresh}", "--paid=60000"]),
        ("spend", [f"--note={spendable}", "--spend"]),
    ]:
        run = subprocess.run(grader + extra, capture_output=True, text=True, timeout=600)
        with open(f"{work}/conformance-{name.split()[0]}.txt", "w") as out:
            out.write(run.stdout + run.stderr)
        print(run.stdout + run.stderr, flush=True)
        assert run.returncode == 0, f"conformance ({name}) failed"
        log(f"conformance ({name}): passed")


CLI_BIN = os.path.join(os.path.dirname(MINT_BIN), "lnurl-mint-cli")


def run_cli(mint: "Mint", *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
    """lnurl-mint-cli with `mint`'s DATA_DIR: its admin socket, unless `env` says otherwise.
    No admin setting leaks in from the calling environment."""
    clean = {"ADMIN_TOKEN", "LNURL_MINT_ADMIN", "ADMIN_SOCKET", "DATA_DIR"}
    full = {k: v for k, v in os.environ.items() if k not in clean}
    full["DATA_DIR"] = mint.dir
    full.update(env or {})
    return subprocess.run([CLI_BIN, *args], env=full, capture_output=True, text=True, timeout=60)


def mint_cli(a: "Mint") -> None:
    """lnurl-mint-cli against A: over the admin socket, and over HTTP with the token."""
    info = a.cli("info")
    assert info["lightning"] == "ready" and info["mint_pubkey"] == a.node_id(), info
    assert a.cli("balance")["onchain"]["confirmed_sat"] > 0
    assert len(a.cli("channels")) >= 1
    assert a.cli("address")["address"].startswith("bcrt1q")
    invoice = a.cli("invoice", "1234", "from the cli")
    assert invoice["bolt11"].startswith("lnbcrt12340n"), invoice
    assert a.cli("invoice-status", invoice["payment_hash"]) == {"paid": False}
    assert a.cli("pending") == {"pending_melts": {}}

    http = {"LNURL_MINT_ADMIN": f"127.0.0.1:{a.admin}", "ADMIN_TOKEN": TOKEN}
    res = run_cli(a, "info", env=http)
    assert res.returncode == 0 and json.loads(res.stdout)["mint_pubkey"] == a.node_id(), res
    refused = run_cli(a, "info", env={**http, "ADMIN_TOKEN": "wrong"})
    assert refused.returncode != 0 and "refused the admin token" in refused.stderr, refused
    nowhere = run_cli(a, "info", env={"DATA_DIR": f"{work}/nowhere"})
    assert nowhere.returncode != 0 and "no admin socket" in nowhere.stderr, nowhere
    bad = run_cli(a, "send", "bcrt1qnothing")
    assert bad.returncode != 0 and "--all" in bad.stderr, bad
    log("lnurl-mint-cli: socket and HTTP; wrong token, missing socket and bad arguments refused")


def admin_ui(a: "Mint") -> None:
    """The admin web UI in a headless browser, against A (e2e/admin_ui.mjs)."""
    shots = os.environ.get("UI_SCREENSHOTS") or None
    run = subprocess.run(
        ["node", os.path.join(HERE, "admin_ui.mjs"), f"http://127.0.0.1:{a.admin}/", TOKEN]
        + ([shots] if shots else []),
        capture_output=True,
        text=True,
        timeout=300,
    )
    print(run.stdout + run.stderr, flush=True)
    assert run.returncode == 0, "admin UI test failed"
    log("admin UI: passed")


def bootstrap(a: "Mint", b: "Mint") -> None:
    """LSPS2: F, a fresh mint with no funds and no channels, gets its first
    inbound channel from B when A - a wallet outside F - pays F's bootstrap
    invoice. F can then be paid: A mints a note on it, routed through B."""
    f = Mint("f", 6, http_admin=False, env={"LSP_NODE": b.peer()})
    f.start()
    assert f.cli("channels") == [] and f.cli("balance")["onchain"]["total_sat"] == 0
    assert f.info()["graph"]["lsp"] == b.peer()
    invoice = f.cli("bootstrap", "100000", "bootstrap", "--max-fee-sat", "2000")
    assert invoice["bolt11"].startswith("lnbcrt1m"), invoice
    a.admin_post("/node/pay", {"bolt11": invoice["bolt11"], "max_fee_msat": 10_000})
    wait_for(lambda: f.cli("invoice-status", invoice["payment_hash"])["paid"], "bootstrap paid", 120)
    [channel] = f.cli("channels")
    assert channel["peer"] == b.node_id() and channel["usable"], channel
    lightning = f.cli("balance")["lightning"]
    # 100000 sat paid, the LSP's 1% (1000 sat) kept, the channel twice the size
    assert lightning["total_sat"] == 99_000 and lightning["inbound_msat"] > 90_000_000, lightning
    log(
        f"LSPS2: A paid F's bootstrap invoice, B opened a {channel['value_sat']} sat channel"
        f" to F: 99000 sat on F's side, {lightning['inbound_msat'] // 1000} sat inbound"
    )

    k1, h = bearer_note(50)
    minted = f.get(f"/p/cb?amount=30000&comment={h}")
    a.admin_post("/node/pay", {"bolt11": minted["pr"], "max_fee_msat": 10_000})
    wait_for(lambda: f.get(f"/w?k1={k1}").get("maxWithdrawable") == 29_000, "note minted on F", 60)
    log("F minted its first note, paid by A over the LSP's channel")
    f.stop()


P2P_PORT = 18544
ELECTRUM_PORT = 18550


def electrum_height() -> int:
    """The chain tip electrs reports, over the Electrum protocol itself."""
    import socket

    with socket.create_connection(("127.0.0.1", ELECTRUM_PORT), timeout=5) as conn:
        conn.sendall(b'{"id": 0, "method": "blockchain.headers.subscribe", "params": []}\n')
        reply = conn.makefile().readline()
    return json.loads(reply)["result"]["height"]


def start_electrs() -> None:
    """electrs over the shared bitcoind: a binary, or docker:<image>."""
    spec = os.environ["ELECTRS"]
    os.makedirs(f"{work}/electrs")
    # electrs 0.10 authenticates by cookie file; ours holds the shared login
    with open(f"{work}/electrs/rpc.cookie", "w") as f:
        f.write("u:p")
    args = [
        "--network=regtest",
        f"--db-dir={work}/electrs/db",
        f"--cookie-file={work}/electrs/rpc.cookie",
        f"--daemon-rpc-addr=127.0.0.1:{RPC_PORT}",
        f"--daemon-p2p-addr=127.0.0.1:{P2P_PORT}",
        f"--electrum-rpc-addr=127.0.0.1:{ELECTRUM_PORT}",
        "--log-filters=WARN",
    ]
    if spec.startswith("docker:"):
        uid = f"{os.getuid()}:{os.getgid()}"
        cmd = ["docker", "run", "--rm", "--name", "lnurl-mint-e2e-electrs", "--network", "host",
               "--user", uid, "-v", f"{work}/electrs:{work}/electrs", "--entrypoint", "electrs",
               spec.removeprefix("docker:"), *args]  # fmt: skip
    else:
        cmd = [spec, *args]
    out = open(f"{work}/electrs.log", "a")
    procs["electrs"] = subprocess.Popen(cmd, stdout=out, stderr=out)
    tip = int(cli("getblockcount"))
    wait_for(lambda: electrum_height() >= tip, "electrs indexed the chain", 120)


def electrum(b: "Mint") -> None:
    """E's node learns about the chain only from electrs: its wallet's funds,
    its own channel's funding broadcast and confirmation, a channel opened to
    it, and then a mint and a melt over them."""
    start_electrs()
    e = Mint(
        "e",
        7,
        http_admin=False,
        env={"ELECTRUM_URL": f"tcp://127.0.0.1:{ELECTRUM_PORT}", "ELECTRUM_SYNC_SECS": "10"},
    )
    e.start()
    graph = e.info()
    assert graph["lightning"] == "ready", graph
    address = e.cli("address")["address"]
    cli("-rpcwallet=miner", "sendtoaddress", address, "0.5")
    mine(1)
    wait_for(lambda: e.cli("balance")["onchain"]["confirmed_sat"] == 50_000_000, "E funded via electrs", 120)
    log("E (Electrum only) sees its wallet funded")

    # E's own channel: funding broadcast through electrs; B's: seen confirming there
    e.cli("open", b.peer(), "300000")
    b.admin_post("/node/channels", {"peer": e.peer(), "amount_sat": 300_000})
    wait_for(lambda: len(e.cli("channels")) == 2, "E's channels negotiated", 60)
    time.sleep(2)
    mine(6)
    wait_for(lambda: sum(c["usable"] for c in e.cli("channels")) == 2, "E's channels usable", 180)
    log("E opened a channel to B and accepted one from B, confirmations seen via electrs")

    k1, h = bearer_note(70)
    invoice = e.get(f"/p/cb?amount=40000&comment={h}")
    b.admin_post("/node/pay", {"bolt11": invoice["pr"]})
    wait_for(lambda: e.get(f"/w?k1={k1}").get("maxWithdrawable") == 39_000, "note minted on E", 60)
    out = b.admin_post("/node/invoice", {"amount_msat": 39_000, "description": "e melt"})
    assert e.get(f"/w/cb?k1={k1}&pr={out['bolt11']}")["status"] == "OK"
    wait_for(lambda: e.get(f"/w?k1={k1}").get("reason") == "Note already spent.", "E's melt settled", 60)
    assert b.admin_get(f"/node/invoice/{out['payment_hash']}")["paid"] is True
    log("E minted a note paid by B and melted it back to B")
    e.stop()


COOKIE_RPC_PORT = 18643


def cookie_renewal() -> None:
    """bitcoind restarts, and writes a new cookie, under a running mint that
    authenticates by cookie: the mint shuts down cleanly with an error, for
    its supervisor to restart it, and syncs again with the new cookie."""
    datadir = f"{work}/bitcoind-cookie"
    os.makedirs(datadir)

    def start_bitcoind() -> None:
        procs["bitcoind-cookie"] = subprocess.Popen(
            [
                f"{BITCOIN_BIN}/bitcoind",
                "-regtest",
                f"-datadir={datadir}",
                f"-rpcport={COOKIE_RPC_PORT}",
                "-fallbackfee=0.0002",
                "-listen=0",
            ],
            stdout=subprocess.DEVNULL,
        )
        wait_for(lambda: cookie_cli("getblockcount") is not None, "cookie bitcoind")

    def cookie_cli(*args: str) -> str:
        return subprocess.check_output(
            [
                f"{BITCOIN_BIN}/bitcoin-cli",
                "-regtest",
                f"-datadir={datadir}",
                f"-rpcport={COOKIE_RPC_PORT}",
                *args,
            ],
            text=True,
        ).strip()

    def cookie_mine(n: int) -> None:
        cookie_cli("generatetoaddress", str(n), cookie_cli("-rpcwallet=miner", "getnewaddress"))

    start_bitcoind()
    cookie_cli("createwallet", "miner")
    cookie_mine(101)
    cookie = f"{datadir}/regtest/.cookie"
    # admin HTTP off: everything below goes through lnurl-mint-cli's socket
    d = Mint("d", 4, rpc_port=COOKIE_RPC_PORT, cookie=cookie, http_admin=False)
    d.start()
    try:
        d.admin_get("/info")
        raise AssertionError("the admin HTTP API answered without ADMIN_TOKEN")
    except OSError:
        pass
    address = d.cli("address")["address"]
    cookie_cli("-rpcwallet=miner", "sendtoaddress", address, "1")

    before = open(cookie).read()
    procs.pop("bitcoind-cookie").send_signal(signal.SIGTERM)
    wait_for(lambda: not os.path.exists(cookie), "cookie bitcoind stopped")
    start_bitcoind()
    assert open(cookie).read() != before, "bitcoind wrote the same cookie"
    wait_for(lambda: procs["d"].poll() is not None, "the mint stopping on the new cookie", 60)
    code = procs.pop("d").returncode
    assert code != 0, f"the mint exited {code} on a new cookie: a supervisor would not restart it"
    assert not os.path.exists(f"{d.dir}/admin.sock"), "the mint left its socket behind"
    # what systemd or docker's restart policy does
    d.start()
    cookie_cli("loadwallet", "miner")
    cookie_mine(1)
    wait_for(
        lambda: d.cli("balance")["onchain"]["confirmed_sat"] == 100_000_000,
        "the new block, synced with the new cookie",
    )
    log(
        f"bitcoind restarted with a new cookie: the mint stopped cleanly (exit {code}), and"
        " restarted, synced with the new one (admin HTTP off, driven by lnurl-mint-cli)"
    )
    d.stop()


def main() -> None:
    os.makedirs(f"{work}/bitcoind")
    procs["bitcoind"] = subprocess.Popen(
        [
            f"{BITCOIN_BIN}/bitcoind",
            "-regtest",
            f"-datadir={work}/bitcoind",
            f"-rpcport={RPC_PORT}",
            "-rpcuser=u",
            "-rpcpassword=p",
            "-fallbackfee=0.0002",
            "-txindex=0",
            # P2P on loopback only: electrs fetches blocks over it
            "-listen=1",
            f"-bind=127.0.0.1:{P2P_PORT}",
        ],
        stdout=subprocess.DEVNULL,
    )
    wait_for(lambda: cli("getblockcount") is not None, "bitcoind")
    cli("createwallet", "miner")
    mine(101)

    a, b, c = Mint("a", 1), Mint("b", 2, env={"TEST_LSP": "true"}), Mint("c", 3)
    for mint in (a, b, c):
        mint.start()
    log(f"nodes up: A={a.node_id()[:16]}.. B={b.node_id()[:16]}..")

    fund(a, "1")
    fund(b, "1")
    mine(1)
    wait_for(lambda: a.admin_get("/node/balance")["onchain"]["confirmed_sat"] == 100_000_000, "A funded")
    wait_for(lambda: b.admin_get("/node/balance")["onchain"]["confirmed_sat"] == 100_000_000, "B funded")
    log("wallets funded")

    # a channel each way: A can pay out (melts) and be paid (mints)
    a.admin_post("/node/channels", {"peer": b.peer(), "amount_sat": 1_000_000})
    b.admin_post("/node/channels", {"peer": a.peer(), "amount_sat": 1_000_000})
    wait_for(lambda: len(a.admin_get("/node/channels")) == 2, "both channels negotiated")
    time.sleep(2)
    mine(6)
    wait_for(lambda: a.usable() == 2 and b.usable() == 2, "channels usable", 120)
    log("channels usable")

    if os.environ.get("CONFORM"):
        conformance(a, b)

    # ---- mint: B pays A's invoice, A credits the note ----
    k1, h = bearer_note(1)
    pay_req = a.get("/.well-known/lnurlp/mint")
    assert pay_req["tag"] == "payRequest", pay_req
    invoice = a.get(f"/p/cb?amount=100000&comment={h}")
    assert "pr" in invoice, invoice
    b.admin_post("/node/pay", {"bolt11": invoice["pr"]})
    wait_for(lambda: a.get(f"/w?k1={k1}").get("maxWithdrawable") == 99_000, "note credited")
    verify = a.get(invoice["verify"].removeprefix(a.base))
    assert verify["settled"] is True and len(verify["preimage"]) == 64, verify
    note = a.get(f"/w?k1={k1}")
    assert note["mintPubkey"] == a.node_id() and note["c"].startswith("cs990n1"), note
    log("minted 99000 msat (100000 paid, 1000 fee), certificate signed by the node key")

    mint_cli(a)

    if os.environ.get("UI"):
        admin_ui(a)

    # ---- rotate into a fresh note, then melt it to B ----
    k1b, hb = bearer_note(2)
    assert a.get(f"/w/cb?k1={k1}&p1={hb}")["status"] == "OK"
    out = b.admin_post("/node/invoice", {"amount_msat": 99_000, "description": "melt"})
    melt = a.get(f"/w/cb?k1={k1b}&pr={out['bolt11']}")
    assert melt["status"] == "OK", melt
    wait_for(lambda: a.get(f"/w?k1={k1b}").get("reason") == "Note already spent.", "note burned")
    verify = a.get(melt["verify"].removeprefix(a.base))
    assert (
        verify["settled"] is True
        and hashlib.sha256(bytes.fromhex(verify["preimage"])).hexdigest() == out["payment_hash"]
    ), verify
    assert a.admin_get("/pending")["pending_melts"] == {}
    log("melted 99000 msat to B, burned once settled, preimage proves it")

    # ---- a melt that cannot leave: refused, note restored, invoice reusable ----
    k1c, hc = bearer_note(3)
    paid = a.get(f"/p/cb?amount=50000&comment={hc}")
    b.admin_post("/node/pay", {"bolt11": paid["pr"]})
    wait_for(lambda: a.get(f"/w?k1={k1c}").get("maxWithdrawable") == 49_000, "second note credited")
    nowhere = c.admin_post("/node/invoice", {"amount_msat": 49_000})
    refused = a.get(f"/w/cb?k1={k1c}&pr={nowhere['bolt11']}")
    assert refused.get("status") == "ERROR" and "route" in refused["reason"], refused
    assert a.get(f"/w?k1={k1c}")["maxWithdrawable"] == 49_000
    log(f"unroutable melt refused ({refused['reason']!r}), note still spendable")

    bootstrap(a, b)

    # ---- a crash between reserving a melt's notes and sending it ----
    # the notes are pending and the melt recorded, but the node never heard of
    # the payment: reconciliation at startup sends it, and B is paid once
    note_id = a.admin_get(f"/note/{hc}")["note_id"]
    unsent = b.admin_post("/node/invoice", {"amount_msat": 49_000, "description": "unsent"})
    a.stop()
    db = sqlite3.connect(f"{a.dir}/mint.sqlite3")
    with db:
        db.execute(
            "UPDATE notes SET pending = 1, pending_payment_hash = ? WHERE id = ?",
            (unsent["payment_hash"], note_id),
        )
        db.execute(
            "INSERT INTO melts (payment_hash, pr) VALUES (?, ?)",
            (unsent["payment_hash"], unsent["bolt11"]),
        )
    db.close()
    a.start()
    wait_for(lambda: a.admin_get("/pending")["pending_melts"] == {}, "unsent melt reconciled", 180)
    assert b.admin_get(f"/node/invoice/{unsent['payment_hash']}")["paid"] is True
    assert a.get(f"/w?k1={k1c}").get("reason") == "Note already spent."
    log("restarted A after a crash before sending: the melt was sent then, B paid, note burned")

    # a fresh note for the next melt
    k1c, hc = bearer_note(4)
    paid = a.get(f"/p/cb?amount=50000&comment={hc}")
    b.admin_post("/node/pay", {"bolt11": paid["pr"]})
    wait_for(lambda: a.get(f"/w?k1={k1c}").get("maxWithdrawable") == 49_000, "fourth note credited")

    # ---- killed outright mid-melt ----
    # The kill can land between LDK writing a channel monitor and writing its
    # channel manager; LDK then force-closes that channel on restart, and the
    # melt's HTLC resolves on-chain. Whichever way it resolves, the note must
    # burn exactly when B was paid, and be released otherwise.
    wait_for(lambda: a.usable() == 2, "channels usable after restart", 120)
    onchain_before = a.admin_get("/node/balance")["onchain"]["confirmed_sat"]
    spent_before = a.info()["stats"]["spent_notes"]
    out = b.admin_post("/node/invoice", {"amount_msat": 49_000})
    assert a.get(f"/w/cb?k1={k1c}&pr={out['bolt11']}")["status"] == "OK"
    procs.pop("a").kill()
    a.start()

    def resolved() -> bool:
        if a.admin_get("/pending")["pending_melts"]:
            mine(3)
            return False
        return True

    wait_for(resolved, "the killed melt resolved", 300)
    b_paid = b.admin_get(f"/node/invoice/{out['payment_hash']}")["paid"]
    spent = a.get(f"/w?k1={k1c}").get("reason") == "Note already spent."
    assert spent == b_paid, (spent, b_paid)
    stats = a.info()["stats"]
    log(
        f"SIGKILLed A mid-melt: B paid={b_paid}, note burned={spent} - consistent; "
        f"{a.usable()} channel(s) left usable"
    )

    # a force-closed channel's funds come back to the on-chain wallet
    if a.usable() < 2:

        def swept() -> bool:
            mine(10)
            return a.admin_get("/node/balance")["onchain"]["confirmed_sat"] > onchain_before

        wait_for(swept, "closed channel swept back to the wallet", 300)
        log("the closed channel's balance was swept back into A's on-chain wallet")
    assert stats["spent_notes"] == spent_before + spent, stats

    if os.environ.get("ELECTRS"):
        electrum(b)

    cookie_renewal()

    log("PASS")


def _exit_on_sigterm(*_) -> None:
    # SIGTERM would otherwise end the interpreter without the cleanup below,
    # leaving bitcoind and the mints running
    sys.exit(143)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, _exit_on_sigterm)
    ok = False
    try:
        main()
        ok = True
    finally:
        for proc in reversed(list(procs.values())):
            proc.send_signal(signal.SIGTERM)
        for proc in procs.values():
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                proc.kill()
        if ok and not os.environ.get("KEEP"):
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"[e2e] logs kept in {work}", file=sys.stderr)
    sys.exit(0 if ok else 1)
