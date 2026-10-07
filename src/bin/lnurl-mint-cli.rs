//! lnurl-mint-cli: the mint's admin functions from a shell, like bitcoin-cli.
//!
//! It talks to a running mint over its admin socket, `<DATA_DIR>/admin.sock`,
//! which the mint always serves whether or not its admin HTTP API is on. The
//! socket is mode 0600: running as the mint's own user is the authentication,
//! and DATA_DIR / ADMIN_SOCKET are found in the environment or `.env` just as
//! the mint finds them. Inside the mint's container that means no flags:
//!
//! ```sh
//! docker exec lnurl-mint-rust lnurl-mint-cli info
//! ```
//!
//! `--admin host:port` with `--token` uses the admin HTTP API instead, for a
//! mint elsewhere. Every reply is printed as JSON; a refusal goes to stderr
//! with a non-zero exit code.

use std::{
    io::{ErrorKind, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(
    name = "lnurl-mint-cli",
    version,
    about = "Run a lnurl-mint's admin functions"
)]
struct Cli {
    /// The mint's admin socket (default: <DATA_DIR>/admin.sock).
    #[arg(long, env = "ADMIN_SOCKET")]
    socket: Option<PathBuf>,

    /// The mint's data directory, where its admin socket is.
    #[arg(long, env = "DATA_DIR", default_value = "data")]
    data_dir: PathBuf,

    /// Use the admin HTTP API at host:port instead of the socket.
    #[arg(long, env = "LNURL_MINT_ADMIN")]
    admin: Option<String>,

    /// The admin token, for --admin.
    #[arg(long, env = "ADMIN_TOKEN", hide_env_values = true)]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The mint: URLs, fees, node id, Lightning status, version and totals.
    Info,
    /// A note's status and value.
    Note {
        /// Its cp1, or a bearer note's hex hash.
        note: String,
    },
    /// Notes reserved by in-flight melts, by payment hash.
    Pending,
    /// Resolve pending melts now.
    Reconcile,
    /// Registered Lightning Address usernames.
    Users,
    /// On-chain and Lightning balances.
    Balance,
    /// A fresh address to fund the on-chain wallet.
    Address,
    /// Connected peers.
    Peers,
    /// Connect to a peer.
    Connect {
        /// pubkey@host:port
        peer: String,
    },
    /// Channels.
    Channels,
    /// Open a channel.
    Open {
        /// A connected peer's pubkey, or pubkey@host:port to connect first.
        peer: String,
        amount_sat: u64,
        /// Announce the channel to the network.
        #[arg(long)]
        public: bool,
    },
    /// Close a channel, cooperatively unless --force.
    Close {
        channel_id: String,
        /// Broadcast the latest commitment transaction instead.
        #[arg(long)]
        force: bool,
    },
    /// An invoice paying the node itself, crediting no note.
    Invoice {
        amount_sat: u64,
        #[arg(default_value = "")]
        description: String,
    },
    /// Whether an invoice from `invoice` was paid.
    InvoiceStatus { payment_hash: String },
    /// Pay a BOLT-11 invoice from the node's liquidity.
    Pay {
        bolt11: String,
        /// Most to spend on routing (default: the mint fee, at least 0.5% or 5 sat).
        #[arg(long)]
        max_fee_sat: Option<u64>,
    },
    /// Where an outgoing payment stands: complete, pending or absent.
    Payment { payment_hash: String },
    /// Send on-chain.
    Send {
        address: String,
        /// Amount in sat; omit with --all to send everything.
        amount_sat: Option<u64>,
        /// Send everything the wallet holds.
        #[arg(long, conflicts_with = "amount_sat")]
        all: bool,
    },
}

impl Command {
    /// The admin API request this command makes: method, path, JSON body.
    fn request(&self) -> Result<(&'static str, String, Option<Value>)> {
        let get = |path: String| Ok(("GET", path, None));
        let post = |path: &str, body: Value| Ok(("POST", path.to_string(), Some(body)));
        match self {
            Command::Info => get("/info".into()),
            Command::Note { note } => get(format!("/note/{}", encode(note))),
            Command::Pending => get("/pending".into()),
            Command::Reconcile => post("/reconcile", json!({})),
            Command::Users => get("/users".into()),
            Command::Balance => get("/node/balance".into()),
            Command::Address => post("/node/address", json!({})),
            Command::Peers => get("/node/peers".into()),
            Command::Connect { peer } => post("/node/peers", json!({"peer": peer})),
            Command::Channels => get("/node/channels".into()),
            Command::Open {
                peer,
                amount_sat,
                public,
            } => post(
                "/node/channels",
                json!({"peer": peer, "amount_sat": amount_sat, "public": public}),
            ),
            Command::Close { channel_id, force } => post(
                "/node/channels/close",
                json!({"channel_id": channel_id, "force": force}),
            ),
            Command::Invoice {
                amount_sat,
                description,
            } => post(
                "/node/invoice",
                json!({"amount_msat": amount_sat * 1000, "description": description}),
            ),
            Command::InvoiceStatus { payment_hash } => {
                get(format!("/node/invoice/{}", encode(payment_hash)))
            }
            Command::Pay {
                bolt11,
                max_fee_sat,
            } => {
                let mut body = json!({"bolt11": bolt11});
                if let Some(sat) = max_fee_sat {
                    body["max_fee_msat"] = json!(sat * 1000);
                }
                post("/node/pay", body)
            }
            Command::Payment { payment_hash } => {
                get(format!("/node/payment/{}", encode(payment_hash)))
            }
            Command::Send {
                address,
                amount_sat,
                all,
            } => match (amount_sat, all) {
                (Some(sat), false) => {
                    post("/node/send", json!({"address": address, "amount_sat": sat}))
                }
                (None, true) => post("/node/send", json!({"address": address})),
                _ => bail!("give an amount in sat, or --all to send everything"),
            },
        }
    }
}

/// Percent-encode a path segment.
fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// How to reach the mint.
#[derive(Debug, PartialEq, Eq)]
enum Transport {
    Socket(PathBuf),
    Http { addr: String, token: String },
}

impl Cli {
    fn transport(&self) -> Result<Transport> {
        match &self.admin {
            Some(addr) => {
                let token = self.token.clone().ok_or_else(|| {
                    anyhow!("--admin needs the admin token: --token or ADMIN_TOKEN")
                })?;
                Ok(Transport::Http {
                    addr: admin_address(addr),
                    token,
                })
            }
            None => Ok(Transport::Socket(
                self.socket
                    .clone()
                    .unwrap_or_else(|| self.data_dir.join("admin.sock")),
            )),
        }
    }
}

/// `host:port` to connect to: a listen address of 0.0.0.0 or [::] means this
/// machine's loopback.
fn admin_address(addr: &str) -> String {
    let addr = addr
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    if let Some(port) = addr.strip_prefix("0.0.0.0:") {
        format!("127.0.0.1:{port}")
    } else if let Some(port) = addr.strip_prefix("[::]:") {
        format!("[::1]:{port}")
    } else {
        addr
    }
}

/// One request over `transport`: the status and the body.
fn send(
    transport: &Transport,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, String)> {
    match transport {
        Transport::Socket(socket) => {
            if socket.as_os_str().len() > 107 {
                bail!(
                    "{} is longer than a Unix socket's path may be: the mint runs with a shorter \
                     ADMIN_SOCKET - pass the same with --socket",
                    socket.display()
                );
            }
            let stream = UnixStream::connect(socket).map_err(|e| match e.kind() {
                ErrorKind::NotFound => anyhow!(
                    "no admin socket at {} - is the mint running with this DATA_DIR? (or --socket)",
                    socket.display()
                ),
                ErrorKind::PermissionDenied => anyhow!(
                    "{} is the mint's own: run lnurl-mint-cli as the user the mint runs as",
                    socket.display()
                ),
                ErrorKind::ConnectionRefused => {
                    anyhow!("no mint answers at {}: it is not running", socket.display())
                }
                _ => anyhow!("could not connect to {}: {e}", socket.display()),
            })?;
            // a payment or a channel open answers at once; nothing takes minutes
            stream.set_read_timeout(Some(Duration::from_secs(120)))?;
            exchange(stream, "localhost", method, path, None, body)
        }
        Transport::Http { addr, token } => {
            let target = addr
                .to_socket_addrs()
                .with_context(|| format!("could not resolve {addr}"))?
                .next()
                .ok_or_else(|| anyhow!("{addr} resolves to no address"))?;
            let stream = TcpStream::connect_timeout(&target, Duration::from_secs(10)).with_context(|| {
                format!("could not reach the mint's admin HTTP API at {addr} - is ADMIN_TOKEN set on the mint?")
            })?;
            stream.set_read_timeout(Some(Duration::from_secs(120)))?;
            exchange(stream, addr, method, path, Some(token), body)
        }
    }
}

/// One HTTP/1.1 request, `Connection: close`, over `stream`.
fn exchange(
    mut stream: impl Read + Write,
    host: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Result<(u16, String)> {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if method == "POST" {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut body = &raw[split + 4..];
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line"))?;
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let decoded;
    if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        decoded = dechunk(body)?;
        body = &decoded;
    } else if let Some(len) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body = &body[..len.min(body.len())];
    }
    Ok((status, String::from_utf8_lossy(body).into_owned()))
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("malformed chunked body"))?;
        let size_hex = String::from_utf8_lossy(&raw[..line_end]);
        let size = usize::from_str_radix(size_hex.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| anyhow!("malformed chunk size"))?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < size {
            bail!("truncated chunked body");
        }
        out.extend_from_slice(&raw[..size]);
        raw = raw.get(size + 2..).unwrap_or_default();
    }
}

fn run(cli: Cli) -> Result<()> {
    let (method, path, body) = cli.command.request()?;
    let transport = cli.transport()?;
    let (status, text) = send(&transport, method, &path, body.as_ref())?;
    match status {
        200..=299 => {
            let pretty = serde_json::from_str::<Value>(&text)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or(text.clone()))
                .unwrap_or(text);
            println!("{pretty}");
            Ok(())
        }
        401 => bail!("the mint refused the admin token (ADMIN_TOKEN)"),
        _ => bail!(
            "{}",
            if text.trim().is_empty() {
                format!("HTTP {status}")
            } else {
                text
            }
        ),
    }
}

fn main() -> ExitCode {
    // the mint's own .env, if run next to it; a missing one is fine
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> Result<(&'static str, String, Option<Value>)> {
        let mut all = vec!["lnurl-mint-cli"];
        all.extend_from_slice(args);
        let cli = Cli::try_parse_from(all).map_err(|e| anyhow!("{e}"))?;
        let (method, path, body) = cli.command.request()?;
        Ok((method, path, body))
    }

    #[test]
    fn commands_map_to_the_admin_api() {
        assert_eq!(request(&["info"]).unwrap(), ("GET", "/info".into(), None));
        assert_eq!(
            request(&["address"]).unwrap(),
            ("POST", "/node/address".into(), Some(json!({})))
        );
        assert_eq!(
            request(&["open", "02ab@1.2.3.4:9735", "100000", "--public"]).unwrap(),
            (
                "POST",
                "/node/channels".into(),
                Some(json!({"peer": "02ab@1.2.3.4:9735", "amount_sat": 100000, "public": true}))
            )
        );
        assert_eq!(
            request(&["invoice", "2500", "liquidity"]).unwrap().2,
            Some(json!({"amount_msat": 2_500_000, "description": "liquidity"}))
        );
        assert_eq!(
            request(&["pay", "lnbc1", "--max-fee-sat", "10"]).unwrap().2,
            Some(json!({"bolt11": "lnbc1", "max_fee_msat": 10_000}))
        );
        assert_eq!(
            request(&["send", "bc1q", "--all"]).unwrap().2,
            Some(json!({"address": "bc1q"}))
        );
        assert!(request(&["send", "bc1q"]).is_err());
        assert!(request(&["send", "bc1q", "5", "--all"]).is_err());
        assert_eq!(request(&["note", "a/b c"]).unwrap().1, "/note/a%2Fb%20c");
    }

    fn transport(args: &[&str]) -> Result<Transport> {
        let mut all = vec!["lnurl-mint-cli"];
        all.extend_from_slice(args);
        all.push("info");
        Cli::try_parse_from(all)
            .map_err(|e| anyhow!("{e}"))?
            .transport()
    }

    #[test]
    fn the_socket_by_default_http_on_request() {
        assert_eq!(
            transport(&["--data-dir", "/srv/mint"]).unwrap(),
            Transport::Socket("/srv/mint/admin.sock".into())
        );
        assert_eq!(
            transport(&["--socket", "/run/mint.sock"]).unwrap(),
            Transport::Socket("/run/mint.sock".into())
        );
        assert_eq!(
            transport(&["--admin", "0.0.0.0:8112", "--token", "t"]).unwrap(),
            Transport::Http {
                addr: "127.0.0.1:8112".into(),
                token: "t".into()
            }
        );
        assert!(transport(&["--admin", "127.0.0.1:8112"]).is_err());
        assert_eq!(admin_address("[::]:8112"), "[::1]:8112");
        assert_eq!(admin_address("http://10.0.0.5:9000/"), "10.0.0.5:9000");
    }

    #[test]
    fn responses_parse_with_a_length_or_in_chunks() {
        let fixed = b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\n{\"a\":true}\n";
        assert_eq!(
            parse_response(fixed).unwrap(),
            (200, "{\"a\":true}\n".into())
        );
        let chunked = b"HTTP/1.1 400 Bad Request\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(
            parse_response(chunked).unwrap(),
            (400, "hello world".into())
        );
        assert!(parse_response(b"garbage").is_err());
    }
}
