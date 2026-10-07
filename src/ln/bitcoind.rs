//! bitcoind over RPC: the block source both LDK and the wallet sync from, the
//! fee estimator, and the broadcaster.

use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::Engine;
use bitcoin::{BlockHash, OutPoint, Transaction, consensus::encode};
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::{
    AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource, gossip::UtxoSource,
    http::HttpEndpoint, rpc::RpcClient,
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
    /// bitcoind's `.cookie` file, re-read when it changes (see `Rpc`).
    Cookie(std::path::PathBuf),
}

impl BitcoindConfig {
    /// `user:password`, read fresh from the cookie file when that is the auth.
    fn credentials(&self) -> Result<String> {
        Ok(match &self.auth {
            BitcoindAuth::UserPass(user, pass) => format!("{user}:{pass}"),
            BitcoindAuth::Cookie(path) => match std::fs::read_to_string(path) {
                Ok(cookie) => cookie.trim().to_string(),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
                    "could not read {}: permission denied{}. bitcoind writes its cookie 0600 \
                     inside a 0700 data directory: give bitcoind `rpccookieperms=group` \
                     (Bitcoin Core 28+) and make the directory searchable by that group \
                     (`chmod g+x <datadir>`), or move the cookie with `rpccookiefile=` to a \
                     directory this user can read",
                    path.display(),
                    crate::whoami::process()
                        .map(|w| format!(" for {w}"))
                        .unwrap_or_default(),
                ),
                Err(e) => {
                    return Err(e).with_context(|| format!("could not read {}", path.display()));
                }
            },
        })
    }

    fn client(&self, credentials: &str) -> RpcClient {
        let endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
        RpcClient::new(
            &base64::engine::general_purpose::STANDARD.encode(credentials),
            endpoint,
        )
    }
}

/// bitcoind's RPC client. bitcoind writes a new cookie each time it starts:
/// when a call fails and the cookie changed, the client is rebuilt with it and
/// the call tried once more, so a bitcoind restart needs no mint restart.
pub(super) struct Rpc {
    config: BitcoindConfig,
    /// The client, and the credentials it was built with.
    current: RwLock<(String, Arc<RpcClient>)>,
}

impl Rpc {
    fn new(config: BitcoindConfig) -> Result<Self> {
        let credentials = config.credentials()?;
        let client = Arc::new(config.client(&credentials));
        Ok(Rpc {
            config,
            current: RwLock::new((credentials, client)),
        })
    }

    fn client(&self) -> Arc<RpcClient> {
        Arc::clone(&self.current.read().unwrap_or_else(|e| e.into_inner()).1)
    }

    /// Re-read the cookie: whether it changed, and the client was rebuilt.
    fn refresh(&self) -> bool {
        if !matches!(self.config.auth, BitcoindAuth::Cookie(_)) {
            return false;
        }
        let Ok(credentials) = self.config.credentials() else {
            return false;
        };
        let mut current = self.current.write().unwrap_or_else(|e| e.into_inner());
        if current.0 == credentials {
            return false;
        }
        let client = Arc::new(self.config.client(&credentials));
        *current = (credentials, client);
        log::info!("bitcoind's RPC cookie changed: using the new one");
        true
    }

    /// After a call on `failed` failed: whether a different client is now in
    /// place, because the cookie changed or another call already rebuilt it.
    fn renewed(&self, failed: &Arc<RpcClient>) -> bool {
        self.refresh() || !Arc::ptr_eq(failed, &self.client())
    }

    pub(super) async fn call(&self, method: &str, params: &[Value]) -> std::io::Result<Value> {
        let client = self.client();
        match client.call_method(method, params).await {
            Err(_) if self.renewed(&client) => self.client().call_method(method, params).await,
            result => result,
        }
    }
}

pub struct Bitcoind {
    rpc: Arc<Rpc>,
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
        let rpc = Arc::new(Rpc::new(config.clone())?);
        let info = rpc
            .call("getblockchaininfo", &[])
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
        let res = self
            .rpc
            .call("estimatesmartfee", &[json!(blocks), json!(mode)])
            .await
            .ok()?;
        // BTC per 1000 vbytes, to sat per 1000 weight units
        let btc_per_kvb = res["feerate"].as_f64()?;
        Some(((btc_per_kvb * 100_000_000.0 / 4.0).round() as u32).max(MIN_FEERATE))
    }

    async fn mempool_min(&self) -> Option<u32> {
        let res = self.rpc.call("getmempoolinfo", &[]).await.ok()?;
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

/// Runs a block-source call on the current client, and once more on a renewed
/// one if it failed because bitcoind's cookie changed (see `Rpc`).
macro_rules! with_renewal {
    ($rpc:expr, |$client:ident| $call:expr) => {{
        let $client = $rpc.client();
        match $call.await {
            Err(_) if $rpc.renewed(&$client) => {
                let $client = $rpc.client();
                $call.await
            }
            result => result,
        }
    }};
}

impl BlockSource for Bitcoind {
    fn get_header<'a>(
        &'a self,
        header_hash: &'a BlockHash,
        height_hint: Option<u32>,
    ) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
        Box::pin(async move {
            with_renewal!(self.rpc, |client| client
                .get_header(header_hash, height_hint))
        })
    }

    fn get_block<'a>(
        &'a self,
        header_hash: &'a BlockHash,
    ) -> AsyncBlockSourceResult<'a, BlockData> {
        Box::pin(async move { with_renewal!(self.rpc, |client| client.get_block(header_hash)) })
    }

    fn get_best_block(&self) -> AsyncBlockSourceResult<'_, (BlockHash, Option<u32>)> {
        Box::pin(async move { with_renewal!(self.rpc, |client| client.get_best_block()) })
    }
}

/// What LDK's gossip verifier asks: whether a channel's funding output exists.
impl UtxoSource for Bitcoind {
    fn get_block_hash_by_height(&self, block_height: u32) -> AsyncBlockSourceResult<'_, BlockHash> {
        Box::pin(async move {
            with_renewal!(self.rpc, |client| client
                .get_block_hash_by_height(block_height))
        })
    }

    fn is_output_unspent(&self, outpoint: OutPoint) -> AsyncBlockSourceResult<'_, bool> {
        Box::pin(
            async move { with_renewal!(self.rpc, |client| client.is_output_unspent(outpoint)) },
        )
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
                rpc.call("sendrawtransaction", &[json!(hex[0])]).await
            } else {
                rpc.call("submitpackage", &[json!(hex)]).await
            };
            // LDK rebroadcasts freely; "already in the chain/mempool" is expected
            if let Err(e) = res {
                log::debug!("broadcast: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(auth: BitcoindAuth) -> BitcoindConfig {
        BitcoindConfig {
            host: "127.0.0.1".into(),
            port: 1,
            auth,
        }
    }

    #[test]
    fn a_new_cookie_renews_the_client_once() {
        let dir = std::env::temp_dir().join(format!("lnurl-mint-cookie-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let cookie = dir.join(".cookie");
        std::fs::write(&cookie, "__cookie__:first\n").unwrap();
        let rpc = Rpc::new(config(BitcoindAuth::Cookie(cookie.clone()))).unwrap();
        let first = rpc.client();

        // unchanged: nothing to retry with
        assert!(!rpc.renewed(&first));
        assert!(Arc::ptr_eq(&first, &rpc.client()));

        // bitcoind restarted
        std::fs::write(&cookie, "__cookie__:second\n").unwrap();
        assert!(rpc.renewed(&first));
        let second = rpc.client();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(rpc.current.read().unwrap().0, "__cookie__:second");
        // a call that failed on the old client retries on the new one, even
        // though this call itself did not rebuild it
        assert!(rpc.renewed(&first));
        assert!(!rpc.renewed(&second));

        // a cookie that can't be read keeps the client as it is
        std::fs::remove_file(&cookie).unwrap();
        assert!(!rpc.renewed(&second));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unreadable_cookie_says_how_to_fix_it() {
        let dir = std::env::temp_dir().join(format!("lnurl-mint-cookie-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let cookie = dir.join(".cookie");
        std::fs::write(&cookie, "__cookie__:x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cookie, std::fs::Permissions::from_mode(0o000)).unwrap();
            // root reads anything: nothing to check then
            if std::fs::read(&cookie).is_err() {
                let err = config(BitcoindAuth::Cookie(cookie.clone()))
                    .credentials()
                    .unwrap_err();
                let msg = err.to_string();
                assert!(
                    msg.contains("rpccookieperms=group") && msg.contains("uid "),
                    "{msg}"
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_password_is_never_renewed() {
        let rpc = Rpc::new(config(BitcoindAuth::UserPass("u".into(), "p".into()))).unwrap();
        let client = rpc.client();
        assert!(!rpc.renewed(&client));
    }
}
