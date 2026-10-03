//! This mint's own Lightning node: LDK, with a BDK on-chain wallet, in this
//! process. Without bitcoind configured there is no node, and minting and
//! melting answer "unavailable" while rotate, split and merge still work.
//!
//! Settlement is pushed, not polled: `events.rs` credits a note when its mint
//! invoice is claimed and burns or releases a melt's notes when LDK reports
//! its payment's outcome.

mod bitcoind;
mod events;
mod logger;
mod node;
mod seed;
mod wallet;

use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin::hashes::{Hash, sha256, sha256d};
use lightning::{
    chain::chaininterface::ConfirmationTarget,
    ln::{
        channelmanager::{
            Bolt11InvoiceParameters, Bolt11PaymentError, PaymentId, RecentPaymentDetails, Retry,
            RetryableSendFailure,
        },
        types::ChannelId,
    },
    routing::router::RouteParametersConfig,
    types::payment::{PaymentHash, PaymentSecret},
};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Currency, Sha256};
use serde_json::{Value, json};

pub use bitcoind::{BitcoindAuth, BitcoindConfig};
use node::Node;
pub use node::NodeConfig;

use crate::db::NoteStore;

/// How long a mint invoice can be paid.
const INVOICE_EXPIRY_SECS: u32 = 3600;

/// How long LDK keeps retrying a melt's payment along other routes.
const PAYMENT_RETRY: Duration = Duration::from_secs(60);

/// What every Lightning signed message is prefixed with before hashing.
const SIGNED_MESSAGE_PREFIX: &[u8] = b"Lightning Signed Message:";

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

    pub fn bitcoin(self) -> bitcoin::Network {
        match self {
            Network::Bitcoin => bitcoin::Network::Bitcoin,
            Network::Testnet => bitcoin::Network::Testnet,
            Network::Testnet4 => bitcoin::Network::Testnet4,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
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
    /// Not among the node's payments: never sent, or resolved long ago.
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
    /// A payment to this hash is already in flight.
    InFlight,
}

const NOT_RUNNING: &str = "the Lightning node is not running";

#[derive(Debug)]
pub struct Ln {
    network: Network,
    node: Option<Arc<Node>>,
}

impl Ln {
    /// No node: everything but invoice decoding answers "not running".
    pub fn new(network: Network) -> Self {
        Ln {
            network,
            node: None,
        }
    }

    pub async fn start(
        network: Network,
        config: NodeConfig,
        store: Arc<NoteStore>,
    ) -> Result<Self> {
        let node = Node::start(config, store).await?;
        Ok(Ln {
            network,
            node: Some(node),
        })
    }

    pub async fn stop(&self) {
        if let Some(node) = &self.node {
            node.stop().await;
        }
    }

    fn node(&self) -> Result<&Arc<Node>> {
        self.node.as_ref().ok_or_else(|| anyhow!(NOT_RUNNING))
    }

    /// Whether the node can take and make payments right now. Minting and
    /// melting are refused up front while it cannot, so no note is ever
    /// reserved for a payment that has no chance of leaving.
    pub fn ready(&self) -> Result<()> {
        self.node().map(|_| ())
    }

    /// The node id: the `mintPubkey` certificates are signed with.
    pub fn node_id(&self) -> Option<String> {
        self.node.as_ref().map(|n| n.node_id())
    }

    /// An invoice for `amount_msat` committing to `description` by hash only
    /// (LUD-06's `description_hash`). Its preimage is derived from the
    /// node's keys and stored nowhere.
    pub fn create_invoice(&self, amount_msat: u64, description: &str) -> Result<Invoice> {
        let node = self.node()?;
        let hash = sha256::Hash::hash(description.as_bytes());
        let params = Bolt11InvoiceParameters {
            amount_msats: Some(amount_msat),
            description: Bolt11InvoiceDescription::Hash(Sha256(hash)),
            invoice_expiry_delta_secs: Some(INVOICE_EXPIRY_SECS),
            ..Default::default()
        };
        let invoice = node
            .channel_manager
            .create_bolt11_invoice(params)
            .map_err(|e| anyhow!("could not create an invoice: {e:?}"))?;
        Ok(Invoice {
            payment_hash: hex::encode(invoice.payment_hash().as_byte_array()),
            bolt11: invoice.to_string(),
        })
    }

    /// The preimage of an invoice this node issued, recomputed from its
    /// payment secret: LUD-21 hands it out once the invoice is paid.
    pub fn invoice_preimage(&self, bolt11: &str) -> Option<String> {
        let node = self.node.as_ref()?;
        let invoice = Bolt11Invoice::from_str(bolt11).ok()?;
        let preimage = node
            .channel_manager
            .get_payment_preimage(
                PaymentHash(invoice.payment_hash().to_byte_array()),
                PaymentSecret(invoice.payment_secret().0),
            )
            .ok()?;
        Some(hex::encode(preimage.0))
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
    /// payment hash, so a second send of one invoice is refused while the
    /// first is in flight.
    pub fn pay(&self, bolt11: &str, maxfee_msat: u64) -> Result<(), PayError> {
        let node = self.node().map_err(|e| PayError::NotSent(e.to_string()))?;
        let invoice = Bolt11Invoice::from_str(bolt11.trim())
            .map_err(|e| PayError::NotSent(format!("not a BOLT-11 invoice: {e}")))?;
        let payment_id = PaymentId(invoice.payment_hash().to_byte_array());
        let route_config = RouteParametersConfig {
            max_total_routing_fee_msat: Some(maxfee_msat),
            ..Default::default()
        };
        let res = node.channel_manager.pay_for_bolt11_invoice(
            &invoice,
            payment_id,
            None,
            route_config,
            Retry::Timeout(PAYMENT_RETRY),
        );
        // every error but a duplicate is raised before LDK registers the
        // payment: nothing was sent
        match res {
            Ok(()) => Ok(()),
            Err(Bolt11PaymentError::SendingFailed(RetryableSendFailure::DuplicatePayment)) => {
                Err(PayError::InFlight)
            }
            Err(Bolt11PaymentError::SendingFailed(RetryableSendFailure::RouteNotFound)) => Err(
                PayError::NotSent("Could not find a route to pay this invoice.".into()),
            ),
            Err(Bolt11PaymentError::SendingFailed(RetryableSendFailure::PaymentExpired)) => {
                Err(PayError::NotSent("This invoice has expired.".into()))
            }
            Err(e) => Err(PayError::NotSent(format!(
                "Could not pay this invoice: {e:?}"
            ))),
        }
    }

    /// Where the outgoing payment to `payment_hash` stands.
    pub fn payment_status(&self, payment_hash: &str) -> Result<PayStatus> {
        let node = self.node()?;
        let id: [u8; 32] = hex::decode(payment_hash)
            .ok()
            .and_then(|b| b.try_into().ok())
            .context("not a payment hash")?;
        let status = node
            .channel_manager
            .list_recent_payments()
            .into_iter()
            .find_map(|p| match p {
                RecentPaymentDetails::Fulfilled { payment_id, .. } if payment_id.0 == id => {
                    Some(PayStatus::Complete)
                }
                RecentPaymentDetails::Pending { payment_id, .. }
                | RecentPaymentDetails::Abandoned { payment_id, .. }
                | RecentPaymentDetails::AwaitingInvoice { payment_id }
                    if payment_id.0 == id =>
                {
                    Some(PayStatus::Pending)
                }
                _ => None,
            });
        Ok(status.unwrap_or(PayStatus::Absent))
    }

    /// A signature by the node key over `message`, made the way every
    /// Lightning signmessage is (sha256d of the prefixed message): r || s ||
    /// recovery id, as LUD-25's `cs1` wants.
    pub fn sign_message(&self, message: &str) -> Result<[u8; 65]> {
        let node = self.node()?;
        sign_message(&node.node_secret(), message)
    }

    pub fn info(&self) -> Result<NodeInfo> {
        let node = self.node()?;
        let id = node.node_id();
        let channels = node.channel_manager.list_channels();
        Ok(NodeInfo {
            uris: node
                .announce_addresses
                .iter()
                .map(|a| format!("{id}@{a}"))
                .collect(),
            alias: Some(node.alias.clone()).filter(|a| !a.is_empty()),
            color: None,
            num_peers: Some(node.peer_manager.list_peers().len() as u64),
            num_channels: Some(channels.iter().filter(|c| c.is_usable).count() as u64),
            // only what the network already sees: announced channels
            capacity_msat: Some(
                channels
                    .iter()
                    .filter(|c| c.is_announced)
                    .map(|c| c.channel_value_satoshis * 1000)
                    .sum(),
            ),
            id,
        })
    }

    // ---- the operator's node operations (admin API) ----

    pub fn new_address(&self) -> Result<String> {
        Ok(self.node()?.wallet.new_address().to_string())
    }

    pub fn balance(&self) -> Result<Value> {
        let node = self.node()?;
        let channels = node.channel_manager.list_channels();
        let usable = channels.iter().filter(|c| c.is_usable);
        Ok(json!({
            "onchain": node.wallet.balance(),
            "lightning": {
                "outbound_msat": usable.clone().map(|c| c.outbound_capacity_msat).sum::<u64>(),
                "inbound_msat": usable.map(|c| c.inbound_capacity_msat).sum::<u64>(),
                "claimable_on_close_sat": node
                    .chain_monitor
                    .get_claimable_balances(&[])
                    .iter()
                    .map(|b| b.claimable_amount_satoshis())
                    .sum::<u64>(),
            },
        }))
    }

    /// Connect to a peer given as `pubkey@host:port`.
    pub async fn connect(&self, peer: &str) -> Result<String> {
        let node = self.node()?;
        let (pubkey, addr) = parse_peer(peer)?;
        node.connect(pubkey, addr).await?;
        Ok(pubkey.to_string())
    }

    /// Open a channel of `amount_sat` to a connected peer (`pubkey`, or
    /// `pubkey@host:port` to connect first).
    pub async fn open_channel(&self, peer: &str, amount_sat: u64, public: bool) -> Result<String> {
        let node = self.node()?;
        let pubkey = if peer.contains('@') {
            let (pubkey, addr) = parse_peer(peer)?;
            node.connect(pubkey, addr).await?;
            pubkey
        } else {
            parse_pubkey(peer)?
        };
        let mut config = lightning::util::config::UserConfig::default();
        config.channel_handshake_config.announce_for_forwarding = public;
        config
            .channel_handshake_config
            .negotiate_anchors_zero_fee_htlc_tx = true;
        // lnd's maximum, so lnd peers accept it
        config.channel_handshake_limits.their_to_self_delay = 2016;
        let channel_id = node
            .channel_manager
            .create_channel(pubkey, amount_sat, 0, 0, None, Some(config))
            .map_err(|e| anyhow!("could not open a channel: {e:?}"))?;
        Ok(channel_id.to_string())
    }

    pub fn close_channel(&self, channel_id: &str, force: bool) -> Result<()> {
        let node = self.node()?;
        let id: [u8; 32] = hex::decode(channel_id)
            .ok()
            .and_then(|b| b.try_into().ok())
            .context("not a channel id")?;
        let channel_id = ChannelId(id);
        let channel = node
            .channel_manager
            .list_channels()
            .into_iter()
            .find(|c| c.channel_id == channel_id)
            .context("no such channel")?;
        let peer = channel.counterparty.node_id;
        if force {
            node.channel_manager.force_close_broadcasting_latest_txn(
                &channel_id,
                &peer,
                "closed by the operator".into(),
            )
        } else {
            node.channel_manager.close_channel(&channel_id, &peer)
        }
        .map_err(|e| anyhow!("could not close the channel: {e:?}"))
    }

    pub fn channels(&self) -> Result<Value> {
        let node = self.node()?;
        Ok(Value::Array(
            node.channel_manager
                .list_channels()
                .into_iter()
                .map(|c| {
                    json!({
                        "channel_id": c.channel_id.to_string(),
                        "peer": c.counterparty.node_id.to_string(),
                        "funding_txo": c.funding_txo.map(|o| o.to_string()),
                        "short_channel_id": c.short_channel_id,
                        "value_sat": c.channel_value_satoshis,
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
            node.peer_manager
                .list_peers()
                .into_iter()
                .map(|p| json!(p.counterparty_node_id.to_string()))
                .collect(),
        ))
    }

    /// An invoice the operator receives on, outside any mint: to take in
    /// liquidity. Payments to it are claimed and credit no note.
    pub fn operator_invoice(&self, amount_msat: u64, description: &str) -> Result<Invoice> {
        let invoice = self.create_invoice(amount_msat, description)?;
        self.node()?
            .store
            .create_operator_invoice(&invoice.payment_hash, amount_msat)
            .map_err(|e| anyhow!("{e}"))?;
        Ok(invoice)
    }

    /// Send on-chain: `amount_sat` to `address`, or everything with `None`.
    pub fn send_onchain(&self, address: &str, amount_sat: Option<u64>) -> Result<String> {
        let node = self.node()?;
        let address = node.wallet.parse_address(address)?;
        let fee = node.sat_per_kw(ConfirmationTarget::NonAnchorChannelFee);
        let tx = match amount_sat {
            Some(sat) => {
                node.wallet
                    .pay_to(address.script_pubkey(), bitcoin::Amount::from_sat(sat), fee)?
            }
            None => node.wallet.drain_to(&address, fee)?,
        };
        let txid = tx.compute_txid();
        lightning::chain::chaininterface::BroadcasterInterface::broadcast_transactions(
            node.bitcoind.as_ref(),
            &[&tx],
        );
        Ok(txid.to_string())
    }
}

fn parse_pubkey(value: &str) -> Result<bitcoin::secp256k1::PublicKey> {
    value.trim().parse().context("not a node id")
}

fn parse_peer(value: &str) -> Result<(bitcoin::secp256k1::PublicKey, std::net::SocketAddr)> {
    let (pubkey, addr) = value
        .split_once('@')
        .context("a peer is pubkey@host:port")?;
    let addr = std::net::ToSocketAddrs::to_socket_addrs(addr)
        .ok()
        .and_then(|mut a| a.next())
        .with_context(|| format!("could not resolve {addr}"))?;
    Ok((parse_pubkey(pubkey)?, addr))
}

/// Lightning's signmessage over `message` with `secret`: r || s || recovery id.
fn sign_message(secret: &[u8; 32], message: &str) -> Result<[u8; 65]> {
    let digest = sha256d::Hash::hash(&[SIGNED_MESSAGE_PREFIX, message.as_bytes()].concat());
    let key = secp256k1::SecretKey::from_secret_bytes(*secret)?;
    let signature = secp256k1::ecdsa::RecoverableSignature::sign_ecdsa_recoverable(
        secp256k1::Message::from_digest(digest.to_byte_array()),
        &key,
    );
    let (recid, rs) = signature.serialize_compact();
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&rs);
    out[64] = u8::from(recid);
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

    /// The same signature LDK's own signmessage makes, in cln's shape.
    #[test]
    fn signatures_match_ldks_signmessage() {
        let secret = [7u8; 32];
        let ours = sign_message(&secret, "LNURLcash:5000:00ff").unwrap();
        let key = bitcoin::secp256k1::SecretKey::from_slice(&secret).unwrap();
        let ldks = lightning::util::message_signing::sign(b"LNURLcash:5000:00ff", &key);
        // LDK's wire form: zbase32 of (31 + recovery id) || r || s
        let mut wire = vec![31 + ours[64]];
        wire.extend_from_slice(&ours[..64]);
        assert_eq!(zbase32(&wire), ldks);
    }

    fn zbase32(data: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
        let mut out = String::new();
        let (mut buffer, mut bits) = (0u32, 0);
        for byte in data {
            buffer = (buffer << 8) | u32::from(*byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
        }
        out
    }

    #[test]
    fn peers_parse() {
        let id = "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619";
        let (pubkey, addr) = parse_peer(&format!("{id}@127.0.0.1:9735")).unwrap();
        assert_eq!(pubkey.to_string(), id);
        assert_eq!(addr.port(), 9735);
        assert!(parse_peer(id).is_err());
    }
}
