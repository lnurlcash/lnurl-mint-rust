//! This mint's own Lightning node: ldk-node (LDK, with a BDK on-chain wallet)
//! in this process. Without bitcoind configured there is no node, and minting
//! and melting answer "unavailable" while rotate, split and merge still work.
//!
//! Settlement is pushed by ldk-node's events (`events.rs`), never polled. Mint
//! invoices are claimed by hand: ldk-node raises `PaymentClaimable`, and the
//! mint claims only payments of unsettled mint invoices. A mint invoice's
//! preimage is derived from the seed and the note it credits, so nothing about
//! it is stored.

mod events;
mod seed;

use std::{net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use ldk_node::{
    Builder, Node, NodeError,
    bitcoin::hashes::{Hash, HashEngine, Hmac, HmacEngine, sha256},
    config::Config as LdkConfig,
    lightning::{
        ln::{channelmanager::PaymentId, msgs::SocketAddress, types::ChannelId},
        routing::router::RouteParametersConfig,
    },
    lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Currency, Description, Sha256},
    lightning_types::payment::PaymentHash,
    payment::PaymentStatus,
};
use serde_json::{Value, json};

use crate::db::NoteStore;

/// How long a mint (or operator) invoice can be paid.
const INVOICE_EXPIRY_SECS: u32 = 3600;

/// The chain this node runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Bitcoin,
    Testnet,
    Testnet4,
    Signet,
    Regtest,
}

impl FromStr for Network {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "bitcoin" | "mainnet" => Network::Bitcoin,
            "testnet" => Network::Testnet,
            "testnet4" => Network::Testnet4,
            "signet" => Network::Signet,
            "regtest" => Network::Regtest,
            other => bail!("unknown network {other:?}"),
        })
    }
}

impl Network {
    /// The BOLT-11 currency an invoice on this chain carries.
    fn currency(self) -> Currency {
        match self {
            Network::Bitcoin => Currency::Bitcoin,
            Network::Testnet | Network::Testnet4 => Currency::BitcoinTestnet,
            Network::Signet => Currency::Signet,
            Network::Regtest => Currency::Regtest,
        }
    }

    pub fn bitcoin(self) -> ldk_node::bitcoin::Network {
        use ldk_node::bitcoin::Network as B;
        match self {
            Network::Bitcoin => B::Bitcoin,
            Network::Testnet => B::Testnet,
            Network::Testnet4 => B::Testnet4,
            Network::Signet => B::Signet,
            Network::Regtest => B::Regtest,
        }
    }

    /// bitcoind's default RPC port on this chain.
    pub fn default_rpc_port(self) -> u16 {
        match self {
            Network::Bitcoin => 8332,
            Network::Testnet => 18332,
            Network::Testnet4 => 48332,
            Network::Signet => 38332,
            Network::Regtest => 18443,
        }
    }

    /// LDK's Rapid Gossip Sync server for this chain. Only mainnet's is
    /// current: its testnet one serves data older than LDK accepts.
    pub fn default_rgs_url(self) -> Option<&'static str> {
        match self {
            Network::Bitcoin => Some("https://rapidsync.lightningdevkit.org/snapshot"),
            _ => None,
        }
    }
}

/// How bitcoind is reached.
#[derive(Debug, Clone)]
pub struct BitcoindConfig {
    pub host: String,
    pub port: u16,
    pub auth: BitcoindAuth,
}

#[derive(Debug, Clone)]
pub enum BitcoindAuth {
    UserPass(String, String),
    /// bitcoind's `.cookie` file. ldk-node keeps the credentials it starts
    /// with, so the mint watches the file and restarts when it changes.
    Cookie(PathBuf),
}

impl BitcoindConfig {
    /// The user and password, read fresh from the cookie when that is the auth.
    pub fn credentials(&self) -> Result<(String, String)> {
        match &self.auth {
            BitcoindAuth::UserPass(user, pass) => Ok((user.clone(), pass.clone())),
            BitcoindAuth::Cookie(path) => {
                let cookie = read_cookie(path)?;
                let (user, pass) = cookie
                    .split_once(':')
                    .with_context(|| format!("{} is not a bitcoind cookie", path.display()))?;
                Ok((user.to_string(), pass.to_string()))
            }
        }
    }
}

/// The cookie's contents, or why it can't be read and how to fix that.
pub fn read_cookie(path: &std::path::Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(cookie) => Ok(cookie.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "could not read {}: permission denied{}. bitcoind writes its cookie 0600 inside a 0700 \
             data directory: run the mint as bitcoind's user, or give bitcoind \
             `rpccookieperms=group` (Bitcoin Core 28+) and make the directory searchable by that \
             group (`chmod g+x <datadir>`)",
            path.display(),
            crate::whoami::process()
                .map(|w| format!(" for {w}"))
                .unwrap_or_default(),
        ),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

/// An LSPS2 Lightning Service Provider: who opens a channel to this node when
/// a bootstrap invoice is paid.
#[derive(Debug, Clone)]
pub struct LspConfig {
    pub node_id: ldk_node::bitcoin::secp256k1::PublicKey,
    pub address: SocketAddress,
    pub token: Option<String>,
}

/// Everything the node is started with.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub network: Network,
    pub bitcoind: BitcoindConfig,
    /// Where peers connect to this node.
    pub listen: SocketAddr,
    /// The alias announced with public channels.
    pub alias: String,
    /// Addresses announced with public channels.
    pub announce_addresses: Vec<SocketAddress>,
    /// A Rapid Gossip Sync server; `None` leaves the graph to peer gossip.
    pub rgs_url: Option<String>,
    pub lsp: Option<LspConfig>,
    /// Act as an LSPS2 provider: only for the regtest end-to-end test.
    pub test_lsp: bool,
}

/// A created invoice and its payment hash.
#[derive(Debug, Clone)]
pub struct Invoice {
    pub bolt11: String,
    pub payment_hash: String,
}

/// What a BOLT-11 invoice a WALLET handed in says.
#[derive(Debug, Clone)]
pub struct DecodedInvoice {
    pub amount_msat: Option<u64>,
    pub payment_hash: String,
    /// The node it pays, hex.
    pub payee: String,
}

/// Where an outgoing payment stands, as the node knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayStatus {
    Complete,
    /// Not terminal: an HTLC may still be held.
    Pending,
    /// Over, and nothing of it left in flight.
    Failed,
    /// Not in the node's payment store.
    Absent,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NodeInfo {
    pub id: String,
    pub alias: Option<String>,
    pub color: Option<String>,
    pub uris: Vec<String>,
    pub num_peers: Option<u64>,
    pub num_channels: Option<u64>,
    pub capacity_msat: Option<u64>,
}

#[derive(Debug)]
pub enum PayError {
    /// Refused before anything was sent: no HTLC exists for it.
    NotSent(String),
    /// A payment to this hash is already in flight, or went through.
    InFlight,
}

const NOT_RUNNING: &str = "the Lightning node is not running";

pub struct Ln {
    network: Network,
    node: Option<Arc<Node>>,
    /// Mint-invoice preimages are HMAC(preimage_key, note id).
    preimage_key: [u8; 32],
    lsp: Option<LspConfig>,
    rgs_url: Option<String>,
}

impl std::fmt::Debug for Ln {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ln")
            .field("network", &self.network)
            .field("node", &self.node.as_ref().map(|n| n.node_id()))
            .finish()
    }
}

fn hash32(hex_hash: &str) -> Result<[u8; 32]> {
    hex::decode(hex_hash)
        .ok()
        .and_then(|b| b.try_into().ok())
        .context("not a 32-byte hex hash")
}

impl Ln {
    /// No node: everything but invoice decoding answers "not running".
    pub fn new(network: Network) -> Self {
        Ln {
            network,
            node: None,
            preimage_key: [0; 32],
            lsp: None,
            rgs_url: None,
        }
    }

    pub async fn start(config: NodeConfig, store: Arc<NoteStore>) -> Result<Self> {
        let seed = seed::Seed::load_or_create(&config.data_dir.join("seed"))?;
        let (rpc_user, rpc_password) = config.bitcoind.credentials()?;

        let mut ldk_config = LdkConfig {
            network: config.network.bitcoin(),
            ..Default::default()
        };
        // the LSP opens its JIT channel zero-conf: it is trusted to broadcast
        // the funding transaction it forwards the payment over. And a fresh
        // mint has no on-chain funds to keep an anchor reserve with: the LSP
        // is trusted to bump the channel's commitment on a close instead.
        if let Some(lsp) = &config.lsp {
            ldk_config.trusted_peers_0conf.push(lsp.node_id);
            if let Some(anchors) = ldk_config.anchor_channels_config.as_mut() {
                anchors.trusted_peers_no_reserve.push(lsp.node_id);
            }
        }
        let mut builder = Builder::from_config(ldk_config);
        builder
            .set_runtime(tokio::runtime::Handle::current())
            .set_log_facade_logger()
            .set_entropy_seed_bytes(seed.node())
            .set_storage_dir_path(
                config
                    .data_dir
                    .join("ldk-node")
                    .to_string_lossy()
                    .into_owned(),
            )
            .set_chain_source_bitcoind_rpc(
                config.bitcoind.host.clone(),
                config.bitcoind.port,
                rpc_user,
                rpc_password,
            );
        builder
            .set_listening_addresses(vec![SocketAddress::from(config.listen)])
            .map_err(|e| anyhow!("LN_LISTEN: {e:?}"))?;
        if !config.announce_addresses.is_empty() {
            builder
                .set_announcement_addresses(config.announce_addresses.clone())
                .map_err(|e| anyhow!("LN_ANNOUNCE_ADDRESSES: {e:?}"))?;
            builder
                .set_node_alias(config.alias.clone())
                .map_err(|e| anyhow!("LN_ALIAS: {e:?}"))?;
        }
        match &config.rgs_url {
            Some(url) => builder.set_gossip_source_rgs(url.clone()),
            None => builder.set_gossip_source_p2p(),
        };
        if let Some(lsp) = &config.lsp {
            builder.set_liquidity_source_lsps2(lsp.node_id, lsp.address.clone(), lsp.token.clone());
        }
        if config.test_lsp {
            test_lsp(&mut builder)?;
        }

        let node = Arc::new(
            builder
                .build()
                .map_err(|e| anyhow!("could not build the node: {e}"))?,
        );
        // ldk-node blocks on the runtime it was given: off this worker
        tokio::task::block_in_place(|| node.start())
            .map_err(|e| anyhow!("could not start the node: {e}"))?;
        tokio::spawn(events::run(Arc::clone(&node), store, seed.preimages()));
        log::info!(
            "Lightning node {} on {:?}, listening on {}{}",
            node.node_id(),
            config.network,
            config.listen,
            config
                .lsp
                .as_ref()
                .map(|l| format!(", LSP {}", l.node_id))
                .unwrap_or_default()
        );
        Ok(Ln {
            network: config.network,
            node: Some(node),
            preimage_key: seed.preimages(),
            lsp: config.lsp,
            rgs_url: config.rgs_url,
        })
    }

    pub async fn stop(&self) {
        if let Some(node) = &self.node {
            match tokio::task::block_in_place(|| node.stop()) {
                Ok(()) => log::info!("Lightning node stopped"),
                Err(e) => log::error!("Lightning node stopped with an error: {e}"),
            }
        }
    }

    fn node(&self) -> Result<&Arc<Node>> {
        self.node.as_ref().ok_or_else(|| anyhow!(NOT_RUNNING))
    }

    /// Whether the node can take and make payments right now. Minting and
    /// melting are refused up front while it cannot, so no note is ever
    /// reserved for a payment that has no chance of leaving.
    pub fn ready(&self) -> Result<()> {
        let node = self.node()?;
        if !node.status().is_running {
            bail!(NOT_RUNNING);
        }
        Ok(())
    }

    /// Whether any channel can carry a payment now. Right after a start the
    /// node is still reconnecting to its peers, and none can.
    pub fn has_usable_channel(&self) -> bool {
        self.node
            .as_ref()
            .is_some_and(|n| n.list_channels().iter().any(|c| c.is_usable))
    }

    /// The node id: the `mintPubkey` certificates are signed with.
    pub fn node_id(&self) -> Option<String> {
        self.node.as_ref().map(|n| n.node_id().to_string())
    }

    /// The preimage of the mint invoice crediting `note_id`. A note id names
    /// one mint invoice ever (the store refuses a second), so the preimage is
    /// unique to it, and the mint can claim the payment without storing it.
    pub fn mint_preimage(&self, note_id: &str) -> [u8; 32] {
        mint_preimage(&self.preimage_key, note_id)
    }

    /// An invoice for `amount_msat` crediting `note_id`, committing to
    /// `description` by hash only (LUD-06's `description_hash`). Its payment
    /// is claimed by hand (`events.rs`).
    pub fn create_invoice(
        &self,
        amount_msat: u64,
        description: &str,
        note_id: &str,
    ) -> Result<Invoice> {
        let node = self.node()?;
        let hash = sha256::Hash::hash(&self.mint_preimage(note_id));
        let description =
            Bolt11InvoiceDescription::Hash(Sha256(sha256::Hash::hash(description.as_bytes())));
        let invoice = node
            .bolt11_payment()
            .receive_for_hash(
                amount_msat,
                &description,
                INVOICE_EXPIRY_SECS,
                PaymentHash(hash.to_byte_array()),
            )
            .map_err(|e| anyhow!("could not create an invoice: {e}"))?;
        Ok(Invoice {
            payment_hash: hex::encode(hash.to_byte_array()),
            bolt11: invoice.to_string(),
        })
    }

    /// Parse a BOLT-11 invoice for this node's chain. Expired invoices are
    /// refused: paying one can only fail, after its note was reserved.
    pub fn decode_invoice(&self, bolt11: &str) -> Result<DecodedInvoice> {
        let invoice = Bolt11Invoice::from_str(bolt11.trim())
            .map_err(|e| anyhow!("not a BOLT-11 invoice: {e}"))?;
        if invoice.currency() != self.network.currency() {
            bail!("invoice is for another network");
        }
        if invoice.is_expired() {
            bail!("invoice has expired");
        }
        Ok(DecodedInvoice {
            amount_msat: invoice.amount_milli_satoshis(),
            payment_hash: hex::encode(invoice.payment_hash().as_byte_array()),
            payee: invoice.get_payee_pub_key().to_string(),
        })
    }

    /// Start paying `bolt11`, spending at most `maxfee_msat` on routing. Its
    /// outcome arrives as an event (`events.rs`). The payment id is the
    /// payment hash, so a second send of one invoice is refused.
    pub fn pay(&self, bolt11: &str, maxfee_msat: u64) -> Result<(), PayError> {
        let node = self.node().map_err(|e| PayError::NotSent(e.to_string()))?;
        let invoice = Bolt11Invoice::from_str(bolt11.trim())
            .map_err(|e| PayError::NotSent(format!("not a BOLT-11 invoice: {e}")))?;
        let params = RouteParametersConfig {
            max_total_routing_fee_msat: Some(maxfee_msat),
            ..Default::default()
        };
        match node.bolt11_payment().send(&invoice, Some(params)) {
            Ok(_) => Ok(()),
            Err(NodeError::DuplicatePayment) => Err(PayError::InFlight),
            // raised by LDK before it registers the payment: no route, an
            // expired invoice, an onion too large - nothing was sent
            Err(NodeError::PaymentSendingFailed) => Err(PayError::NotSent(
                "Could not find a route to pay this invoice.".into(),
            )),
            Err(NodeError::InvalidInvoice) => Err(PayError::NotSent("Invalid invoice.".into())),
            Err(e) => Err(PayError::NotSent(format!(
                "Could not pay this invoice: {e}"
            ))),
        }
    }

    /// Where the outgoing payment to `payment_hash` stands.
    pub fn payment_status(&self, payment_hash: &str) -> Result<PayStatus> {
        let node = self.node()?;
        let id = PaymentId(hash32(payment_hash)?);
        Ok(match node.payment(&id).map(|p| p.status) {
            Some(PaymentStatus::Succeeded) => PayStatus::Complete,
            Some(PaymentStatus::Pending) => PayStatus::Pending,
            Some(PaymentStatus::Failed) => PayStatus::Failed,
            None => PayStatus::Absent,
        })
    }

    /// A signature by the node key over `message`, made the way every
    /// Lightning signmessage is (sha256d of the prefixed message): r || s ||
    /// recovery id, as LUD-25's `cs1` wants.
    pub fn sign_message(&self, message: &str) -> Result<[u8; 65]> {
        let node = self.node()?;
        signature_from_zbase32(&node.sign_message(message.as_bytes()))
    }

    pub fn info(&self) -> Result<NodeInfo> {
        let node = self.node()?;
        let id = node.node_id().to_string();
        let channels = node.list_channels();
        Ok(NodeInfo {
            uris: node
                .announcement_addresses()
                .unwrap_or_default()
                .iter()
                .map(|a| format!("{id}@{a}"))
                .collect(),
            alias: node.node_alias().map(|a| a.to_string()),
            color: None,
            num_peers: Some(node.list_peers().iter().filter(|p| p.is_connected).count() as u64),
            num_channels: Some(channels.iter().filter(|c| c.is_usable).count() as u64),
            // only what the network already sees: announced channels
            capacity_msat: Some(
                channels
                    .iter()
                    .filter(|c| c.is_announced)
                    .map(|c| c.channel_value_sats * 1000)
                    .sum(),
            ),
            id,
        })
    }

    /// What the node knows of the network, and its gossip and LSP sources.
    pub fn graph(&self) -> Result<Value> {
        let node = self.node()?;
        let graph = node.network_graph();
        Ok(json!({
            "channels": graph.list_channels().len(),
            "nodes": graph.list_nodes().len(),
            "rapid_gossip_sync": self.rgs_url,
            "last_rapid_sync": node.status().latest_rgs_snapshot_timestamp,
            "lsp": self.lsp.as_ref().map(|l| format!("{}@{}", l.node_id, l.address)),
        }))
    }

    // ---- the operator's node operations (admin API) ----

    pub fn new_address(&self) -> Result<String> {
        let node = self.node()?;
        Ok(node.onchain_payment().new_address()?.to_string())
    }

    pub fn balance(&self) -> Result<Value> {
        let node = self.node()?;
        let balances = node.list_balances();
        let usable: Vec<_> = node
            .list_channels()
            .into_iter()
            .filter(|c| c.is_usable)
            .collect();
        Ok(json!({
            "onchain": {
                "confirmed_sat": balances.spendable_onchain_balance_sats,
                "total_sat": balances.total_onchain_balance_sats,
                "anchor_reserve_sat": balances.total_anchor_channels_reserve_sats,
            },
            "lightning": {
                "outbound_msat": usable.iter().map(|c| c.outbound_capacity_msat).sum::<u64>(),
                "inbound_msat": usable.iter().map(|c| c.inbound_capacity_msat).sum::<u64>(),
                "total_sat": balances.total_lightning_balance_sats,
            },
        }))
    }

    /// Connect to a peer given as `pubkey@host:port`, remembering it.
    pub async fn connect(&self, peer: &str) -> Result<String> {
        let node = self.node()?;
        let (pubkey, address) = parse_peer(peer)?;
        node.connect(pubkey, address, true)?;
        Ok(pubkey.to_string())
    }

    /// Open a channel of `amount_sat` to a peer: `pubkey@host:port`, or a
    /// pubkey this node already knows the address of.
    pub async fn open_channel(&self, peer: &str, amount_sat: u64, public: bool) -> Result<String> {
        let node = self.node()?;
        let (pubkey, address) = if peer.contains('@') {
            parse_peer(peer)?
        } else {
            let pubkey = parse_pubkey(peer)?;
            let known = node
                .list_peers()
                .into_iter()
                .find(|p| p.node_id == pubkey)
                .context("unknown peer: give it as pubkey@host:port")?;
            (pubkey, known.address)
        };
        let user_channel_id = if public {
            node.open_announced_channel(pubkey, address, amount_sat, None, None)?
        } else {
            node.open_channel(pubkey, address, amount_sat, None, None)?
        };
        Ok(user_channel_id.0.to_string())
    }

    pub fn close_channel(&self, channel_id: &str, force: bool) -> Result<()> {
        let node = self.node()?;
        let id = ChannelId(hash32(channel_id).context("not a channel id")?);
        let channel = node
            .list_channels()
            .into_iter()
            .find(|c| c.channel_id == id)
            .context("no such channel")?;
        if force {
            node.force_close_channel(
                &channel.user_channel_id,
                channel.counterparty_node_id,
                Some("closed by the operator".into()),
            )?;
        } else {
            node.close_channel(&channel.user_channel_id, channel.counterparty_node_id)?;
        }
        Ok(())
    }

    pub fn channels(&self) -> Result<Value> {
        let node = self.node()?;
        Ok(Value::Array(
            node.list_channels()
                .into_iter()
                .map(|c| {
                    json!({
                        "channel_id": c.channel_id.to_string(),
                        "peer": c.counterparty_node_id.to_string(),
                        "funding_txo": c.funding_txo.map(|o| o.to_string()),
                        "short_channel_id": c.short_channel_id,
                        "value_sat": c.channel_value_sats,
                        "outbound_msat": c.outbound_capacity_msat,
                        "inbound_msat": c.inbound_capacity_msat,
                        "ready": c.is_channel_ready,
                        "usable": c.is_usable,
                        "public": c.is_announced,
                    })
                })
                .collect(),
        ))
    }

    pub fn peers(&self) -> Result<Value> {
        let node = self.node()?;
        Ok(Value::Array(
            node.list_peers()
                .into_iter()
                .filter(|p| p.is_connected)
                .map(|p| json!(p.node_id.to_string()))
                .collect(),
        ))
    }

    /// An invoice the operator receives on, outside any mint: to take in
    /// liquidity. ldk-node claims it; it credits no note.
    pub fn operator_invoice(&self, amount_msat: u64, description: &str) -> Result<Invoice> {
        let node = self.node()?;
        let invoice = node
            .bolt11_payment()
            .receive(amount_msat, &direct(description)?, INVOICE_EXPIRY_SECS)
            .map_err(|e| anyhow!("could not create an invoice: {e}"))?;
        Ok(Invoice {
            payment_hash: hex::encode(invoice.payment_hash().as_byte_array()),
            bolt11: invoice.to_string(),
        })
    }

    /// A bootstrap invoice: paid from outside the mint, it makes the LSP open
    /// a channel to this node, and the LSP keeps its opening fee from it - at
    /// most `max_fee_msat`. Credits no note. The node's first inbound liquidity.
    pub fn bootstrap_invoice(
        &self,
        amount_msat: u64,
        description: &str,
        max_fee_msat: Option<u64>,
    ) -> Result<Invoice> {
        let node = self.node()?;
        if self.lsp.is_none() {
            bail!("no LSP configured: set LSP_NODE=pubkey@host:port");
        }
        let invoice = node
            .bolt11_payment()
            .receive_via_jit_channel(
                amount_msat,
                &direct(description)?,
                INVOICE_EXPIRY_SECS,
                max_fee_msat,
            )
            .map_err(|e| match e {
                NodeError::LiquidityFeeTooHigh => {
                    anyhow!("the LSP's opening fee is above the limit you set")
                }
                e => anyhow!("the LSP could not offer a channel: {e}"),
            })?;
        Ok(Invoice {
            payment_hash: hex::encode(invoice.payment_hash().as_byte_array()),
            bolt11: invoice.to_string(),
        })
    }

    /// Whether an invoice of `operator_invoice` or `bootstrap_invoice` was
    /// paid; `None` if the node never issued it.
    pub fn invoice_paid(&self, payment_hash: &str) -> Result<Option<bool>> {
        let node = self.node()?;
        let id = PaymentId(hash32(payment_hash)?);
        Ok(node
            .payment(&id)
            .map(|p| p.status == PaymentStatus::Succeeded))
    }

    /// Send on-chain: `amount_sat` to `address`, or everything with `None`.
    pub fn send_onchain(&self, address: &str, amount_sat: Option<u64>) -> Result<String> {
        let node = self.node()?;
        let address = ldk_node::bitcoin::Address::from_str(address)?
            .require_network(self.network.bitcoin())?;
        let txid = match amount_sat {
            Some(sat) => node
                .onchain_payment()
                .send_to_address(&address, sat, None)?,
            // keep what anchor channels need to close safely
            None => node
                .onchain_payment()
                .send_all_to_address(&address, true, None)?,
        };
        Ok(txid.to_string())
    }
}

fn direct(description: &str) -> Result<Bolt11InvoiceDescription> {
    Ok(Bolt11InvoiceDescription::Direct(
        Description::new(description.to_string()).map_err(|e| anyhow!("description: {e}"))?,
    ))
}

fn mint_preimage(key: &[u8; 32], note_id: &str) -> [u8; 32] {
    let mut engine = HmacEngine::<sha256::Hash>::new(key);
    engine.input(note_id.as_bytes());
    Hmac::<sha256::Hash>::from_engine(engine).to_byte_array()
}

/// The regtest end-to-end test's LSP: every bootstrap invoice opens a channel
/// twice its size, for a 1% fee, at least 2 sat. Never in a release build.
#[cfg(feature = "test-lsp")]
fn test_lsp(builder: &mut Builder) -> Result<()> {
    builder.set_liquidity_provider_lsps2(ldk_node::liquidity::LSPS2ServiceConfig {
        require_token: None,
        advertise_service: false,
        channel_opening_fee_ppm: 10_000,
        channel_over_provisioning_ppm: 1_000_000,
        min_channel_opening_fee_msat: 2_000,
        min_channel_lifetime: 100,
        max_client_to_self_delay: 2016,
        min_payment_size_msat: 1_000,
        max_payment_size_msat: 1_000_000_000,
        client_trusts_lsp: false,
    });
    Ok(())
}

#[cfg(not(feature = "test-lsp"))]
fn test_lsp(_builder: &mut Builder) -> Result<()> {
    bail!("TEST_LSP needs a build with --features test-lsp")
}

fn parse_pubkey(value: &str) -> Result<ldk_node::bitcoin::secp256k1::PublicKey> {
    value.trim().parse().context("not a node id")
}

/// `pubkey@host:port`.
pub fn parse_peer(value: &str) -> Result<(ldk_node::bitcoin::secp256k1::PublicKey, SocketAddress)> {
    let (pubkey, addr) = value
        .split_once('@')
        .context("a peer is pubkey@host:port")?;
    let address = SocketAddress::from_str(addr).map_err(|_| anyhow!("not an address: {addr}"))?;
    Ok((parse_pubkey(pubkey)?, address))
}

/// LDK's signmessage wire form, zbase32 of (31 + recovery id) || r || s, in
/// cln's shape: r || s || recovery id.
fn signature_from_zbase32(text: &str) -> Result<[u8; 65]> {
    const ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut wire = Vec::with_capacity(65);
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let value = ALPHABET
            .iter()
            .position(|&a| a == c)
            .context("malformed signature from the node")? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            wire.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if wire.len() != 65 || !(31..=34).contains(&wire[0]) {
        bail!("malformed signature from the node");
    }
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&wire[1..]);
    out[64] = wire[0] - 31;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks_parse() {
        assert_eq!("mainnet".parse::<Network>().unwrap(), Network::Bitcoin);
        assert_eq!("Regtest".parse::<Network>().unwrap(), Network::Regtest);
        assert!("litecoin".parse::<Network>().is_err());
    }

    #[test]
    fn garbage_is_no_invoice() {
        let ln = Ln::new(Network::Bitcoin);
        assert!(ln.decode_invoice("lnbc1garbage").is_err());
        assert!(ln.decode_invoice("").is_err());
    }

    /// LDK's own signmessage, in cln's shape, is the signature lnurlcash-core
    /// and lnurl-mint's certificates expect: same r || s, same recovery id.
    #[test]
    fn signatures_match_secp256k1s_own() {
        use ldk_node::bitcoin::hashes::sha256d;
        let secret = [7u8; 32];
        let key = ldk_node::bitcoin::secp256k1::SecretKey::from_slice(&secret).unwrap();
        let message = "LNURLcash:5000:00ff";
        let ours = signature_from_zbase32(&ldk_node::lightning::util::message_signing::sign(
            message.as_bytes(),
            &key,
        ))
        .unwrap();
        let digest = sha256d::Hash::hash(
            &[b"Lightning Signed Message:".as_slice(), message.as_bytes()].concat(),
        );
        let direct = secp256k1::ecdsa::RecoverableSignature::sign_ecdsa_recoverable(
            secp256k1::Message::from_digest(digest.to_byte_array()),
            &secp256k1::SecretKey::from_secret_bytes(secret).unwrap(),
        );
        let (recid, rs) = direct.serialize_compact();
        assert_eq!(&ours[..64], &rs);
        assert_eq!(ours[64], u8::from(recid));
    }

    #[test]
    fn preimages_are_per_note_and_per_seed() {
        let a = mint_preimage(&[1; 32], "aa");
        assert_eq!(a, mint_preimage(&[1; 32], "aa"));
        assert_ne!(a, mint_preimage(&[1; 32], "ab"));
        assert_ne!(a, mint_preimage(&[2; 32], "aa"));
    }

    #[test]
    fn peers_parse() {
        let id = "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619";
        let (pubkey, addr) = parse_peer(&format!("{id}@127.0.0.1:9735")).unwrap();
        assert_eq!(pubkey.to_string(), id);
        assert_eq!(addr.to_string(), "127.0.0.1:9735");
        assert!(parse_peer(id).is_err());
    }
}
