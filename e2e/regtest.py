#!/usr/bin/env python3
"""End-to-end on regtest: real bitcoind, real LDK nodes, real payments.

Three lnurl-mint processes share one bitcoind:

- A, the mint under test
- B, a counterparty: it pays A's mint invoices and receives A's melts
- C, a node no one has a channel with: melting into its invoice must fail
  before anything is sent

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
    def __init__(self, name: str, n: int, rpc_port: int = RPC_PORT, cookie: str | None = None):
        self.name = name
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
            ADMIN_TOKEN=TOKEN,
            BITCOIND_RPC=f"127.0.0.1:{self.rpc_port}",
            LN_LISTEN=f"127.0.0.1:{self.ln}",
            RUST_LOG=os.environ.get("E2E_RUST_LOG", "info,ldk=warn"),
        )
        if self.cookie:
            env["BITCOIND_RPC_COOKIE"] = self.cookie
        else:
            env.update(BITCOIND_RPC_USER="u", BITCOIND_RPC_PASSWORD="p")
        out = open(f"{work}/{self.name}.log", "a")
        procs[self.name] = subprocess.Popen([MINT_BIN], env=env, stdout=out, stderr=out, cwd=work)
        wait_for(lambda: self.info()["lightning"] == "ready", f"{self.name} up")

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
            return self.admin_get("/info")
        except (OSError, RuntimeError):
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


COOKIE_RPC_PORT = 18643


def cookie_renewal() -> None:
    """bitcoind restarts, and writes a new cookie, under a running mint that
    authenticates by cookie: the mint keeps syncing without a restart."""
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
    d = Mint("d", 4, rpc_port=COOKIE_RPC_PORT, cookie=cookie)
    d.start()
    address = d.admin_post("/node/address")["address"]
    cookie_cli("-rpcwallet=miner", "sendtoaddress", address, "1")

    before = open(cookie).read()
    procs.pop("bitcoind-cookie").send_signal(signal.SIGTERM)
    wait_for(lambda: not os.path.exists(cookie), "cookie bitcoind stopped")
    start_bitcoind()
    assert open(cookie).read() != before, "bitcoind wrote the same cookie"
    cookie_cli("loadwallet", "miner")
    cookie_mine(1)
    wait_for(
        lambda: d.admin_get("/node/balance")["onchain"]["confirmed_sat"] == 100_000_000,
        "the new block, synced with the new cookie",
    )
    assert "d" in procs and procs["d"].poll() is None, "the mint was restarted"
    log("bitcoind restarted with a new cookie: the running mint renewed it and kept syncing")
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
            "-listen=0",
        ],
        stdout=subprocess.DEVNULL,
    )
    wait_for(lambda: cli("getblockcount") is not None, "bitcoind")
    cli("createwallet", "miner")
    mine(101)

    a, b, c = Mint("a", 1), Mint("b", 2), Mint("c", 3)
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

    # ---- a crash between reserving a melt's notes and sending it ----
    # the notes are pending, but the node never heard of the payment:
    # reconciliation at startup must release them, not leave them frozen
    note_id = a.admin_get(f"/note/{hc}")["note_id"]
    a.stop()
    never_sent = "ee" * 32
    db = sqlite3.connect(f"{a.dir}/mint.sqlite3")
    with db:
        db.execute(
            "UPDATE notes SET pending = 1, pending_payment_hash = ? WHERE id = ?",
            (never_sent, note_id),
        )
        db.execute("INSERT INTO melts (payment_hash, pr) VALUES (?, 'lnbcrt1')", (never_sent,))
    db.close()
    a.start()
    wait_for(lambda: a.admin_get("/pending")["pending_melts"] == {}, "unsent melt reconciled")
    assert a.get(f"/w?k1={k1c}")["maxWithdrawable"] == 49_000
    log("restarted A after a crash before sending: the reserved note was released")

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
