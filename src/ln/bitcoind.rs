//! bitcoind over RPC: the block source both LDK and the wallet sync from, the
//! fee estimator, and the broadcaster.

use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::Engine;
use bitcoin::{BlockHash, Transaction, consensus::encode};
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::{
    AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource, http::HttpEndpoint,
    rpc::RpcClient,
};
use serde_json::{Value, json};

use super::wallet::OnChainWallet;

/// The lowest feerate LDK may be given, in sat per 1000 weight.
const MIN_FEERATE: u32 = 253;

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
    /// bitcoind's `.cookie` file, read on every connect: it changes per restart.
    Cookie(std::path::PathBuf),
}

impl BitcoindConfig {
    fn rpc_client(&self) -> Result<RpcClient> {
        let credentials = match &self.auth {
            BitcoindAuth::UserPass(user, pass) => format!("{user}:{pass}"),
            BitcoindAuth::Cookie(path) => std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?
                .trim()
                .to_string(),
        };
        let endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
        Ok(RpcClient::new(
            &base64::engine::general_purpose::STANDARD.encode(credentials),
            endpoint,
        ))
    }
}

pub struct Bitcoind {
    pub(super) rpc: Arc<RpcClient>,
    fees: Arc<HashMap<ConfirmationTarget, AtomicU32>>,
    /// Every transaction LDK broadcasts is shown to the wallet too, so it
    /// knows its own unconfirmed spends and change before they confirm.
    wallet: OnceLock<Arc<OnChainWallet>>,
}

impl std::fmt::Debug for Bitcoind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bitcoind")
    }
}

impl Bitcoind {
    /// Connect, and refuse a bitcoind on another chain than `network`.
    pub async fn connect(config: &BitcoindConfig, network: bitcoin::Network) -> Result<Self> {
        let rpc = Arc::new(config.rpc_client()?);
        let info: Value = rpc
            .call_method("getblockchaininfo", &[])
            .await
            .context("could not reach bitcoind - check BITCOIND_RPC_* settings")?;
        let chain = info["chain"].as_str().unwrap_or_default();
        let expected = match network {
            bitcoin::Network::Bitcoin => "main",
            bitcoin::Network::Testnet => "test",
            bitcoin::Network::Testnet4 => "testnet4",
            bitcoin::Network::Signet => "signet",
            _ => "regtest",
        };
        if chain != expected {
            bail!("bitcoind is on {chain:?}, but NETWORK is {network}");
        }
        let fees = [
            (ConfirmationTarget::MaximumFeeEstimate, 50_000),
            (ConfirmationTarget::UrgentOnChainSweep, 5000),
            (
                ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
                MIN_FEERATE,
            ),
            (
                ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
                MIN_FEERATE,
            ),
            (ConfirmationTarget::AnchorChannelFee, MIN_FEERATE),
            (ConfirmationTarget::NonAnchorChannelFee, 2000),
            (ConfirmationTarget::ChannelCloseMinimum, MIN_FEERATE),
            (ConfirmationTarget::OutputSpendingFee, MIN_FEERATE),
        ]
        .into_iter()
        .map(|(target, rate)| (target, AtomicU32::new(rate)))
        .collect();
        let bitcoind = Bitcoind {
            rpc,
            fees: Arc::new(fees),
            wallet: OnceLock::new(),
        };
        bitcoind.refresh_fees().await;
        Ok(bitcoind)
    }

    pub(super) fn set_wallet(&self, wallet: Arc<OnChainWallet>) {
        let _ = self.wallet.set(wallet);
    }

    /// `estimatesmartfee` in sat per 1000 weight, or `None` without an estimate.
    async fn estimate(&self, blocks: u16, mode: &str) -> Option<u32> {
        let res: Value = self
            .rpc
            .call_method("estimatesmartfee", &[json!(blocks), json!(mode)])
            .await
            .ok()?;
        // BTC per 1000 vbytes, to sat per 1000 weight units
        let btc_per_kvb = res["feerate"].as_f64()?;
        Some(((btc_per_kvb * 100_000_000.0 / 4.0).round() as u32).max(MIN_FEERATE))
    }

    async fn mempool_min(&self) -> Option<u32> {
        let res: Value = self.rpc.call_method("getmempoolinfo", &[]).await.ok()?;
        let btc_per_kvb = res["mempoolminfee"].as_f64()?;
        Some(((btc_per_kvb * 100_000_000.0 / 4.0).round() as u32).max(MIN_FEERATE))
    }

    /// Fetch fresh estimates. A target without one keeps its last value.
    pub(super) async fn refresh_fees(&self) {
        let set = |target, rate: Option<u32>| {
            if let Some(rate) = rate {
                self.fees[&target].store(rate, Ordering::Release);
            }
        };
        let background = self.estimate(144, "ECONOMICAL").await;
        set(
            ConfirmationTarget::MaximumFeeEstimate,
            self.estimate(2, "CONSERVATIVE").await,
        );
        set(
            ConfirmationTarget::UrgentOnChainSweep,
            self.estimate(6, "CONSERVATIVE").await,
        );
        set(
            ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
            self.mempool_min().await,
        );
        set(
            ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
            background.map(|b| b.saturating_sub(250).max(MIN_FEERATE)),
        );
        set(ConfirmationTarget::AnchorChannelFee, background);
        set(
            ConfirmationTarget::NonAnchorChannelFee,
            self.estimate(18, "ECONOMICAL").await,
        );
        set(ConfirmationTarget::ChannelCloseMinimum, background);
        set(ConfirmationTarget::OutputSpendingFee, background);
    }

    pub(super) async fn fee_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            self.refresh_fees().await;
        }
    }
}

impl BlockSource for Bitcoind {
    fn get_header<'a>(
        &'a self,
        header_hash: &'a BlockHash,
        height_hint: Option<u32>,
    ) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
        Box::pin(async move { self.rpc.get_header(header_hash, height_hint).await })
    }

    fn get_block<'a>(
        &'a self,
        header_hash: &'a BlockHash,
    ) -> AsyncBlockSourceResult<'a, BlockData> {
        Box::pin(async move { self.rpc.get_block(header_hash).await })
    }

    fn get_best_block(&self) -> AsyncBlockSourceResult<'_, (BlockHash, Option<u32>)> {
        Box::pin(async move { self.rpc.get_best_block().await })
    }
}

impl FeeEstimator for Bitcoind {
    fn get_est_sat_per_1000_weight(&self, target: ConfirmationTarget) -> u32 {
        self.fees[&target].load(Ordering::Acquire)
    }
}

impl BroadcasterInterface for Bitcoind {
    fn broadcast_transactions(&self, txs: &[&Transaction]) {
        if let Some(wallet) = self.wallet.get() {
            wallet.saw_unconfirmed(txs.iter().map(|tx| (*tx).clone()));
        }
        let hex: Vec<String> = txs.iter().map(|tx| encode::serialize_hex(*tx)).collect();
        let rpc = Arc::clone(&self.rpc);
        tokio::spawn(async move {
            // a package goes in whole, so an anchor's child can carry its parent
            let res = if hex.len() == 1 {
                rpc.call_method::<Value>("sendrawtransaction", &[json!(hex[0])])
                    .await
            } else {
                rpc.call_method::<Value>("submitpackage", &[json!(hex)])
                    .await
            };
            // LDK rebroadcasts freely; "already in the chain/mempool" is expected
            if let Err(e) = res {
                log::debug!("broadcast: {e}");
            }
        });
    }
}
