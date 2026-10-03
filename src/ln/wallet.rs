//! The on-chain wallet: BDK, fed the same blocks LDK sees from bitcoind, with
//! no second chain backend of its own. It funds channel opens, receives swept
//! channel outputs, and pays for anchor-channel fee bumps.

use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result, anyhow, bail};
use bdk_wallet::{
    ChangeSet, KeychainKind, PersistedWallet, SignOptions, Wallet,
    chain::{BlockId, CheckPoint},
    file_store::Store,
    template::Bip86,
};
use bitcoin::{
    Address, Amount, Block, FeeRate, ScriptBuf, Transaction,
    bip32::Xpriv,
    block::Header,
    constants::WITNESS_SCALE_FACTOR,
    psbt::Psbt,
    secp256k1::{All, Secp256k1},
};
use lightning::{
    chain::{BestBlock, Listen, transaction::TransactionData},
    events::bump_transaction::{Utxo, WalletSource},
    sign::ChangeDestinationSource,
    util::async_poll::AsyncResult,
};

const MAGIC: &[u8] = b"lnurl-mint-wallet";

/// Weight of a P2TR key-path input's witness: item count, length, signature.
const P2TR_KEY_SPEND_WEIGHT: u64 = 1 + 1 + 64;

struct Inner {
    wallet: PersistedWallet<Store<ChangeSet>>,
    db: Store<ChangeSet>,
}

impl Inner {
    fn persist(&mut self) {
        if let Err(e) = self.wallet.persist(&mut self.db) {
            log::error!("could not persist the on-chain wallet: {e}");
        }
    }
}

pub struct OnChainWallet {
    inner: Mutex<Inner>,
    network: bitcoin::Network,
    /// The wallet's master key: it signs its own inputs, BDK only builds.
    xprv: Xpriv,
    secp: Secp256k1<All>,
}

impl std::fmt::Debug for OnChainWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnChainWallet")
    }
}

/// What the wallet holds, in sat.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct WalletBalance {
    pub confirmed_sat: u64,
    pub unconfirmed_sat: u64,
}

impl OnChainWallet {
    /// Open the wallet at `path`, or create it there. A new wallet starts at
    /// `birthday`: its keys are fresh, so no earlier block can pay them.
    pub fn open(
        path: &Path,
        seed: [u8; 32],
        network: bitcoin::Network,
        birthday: BlockId,
    ) -> Result<Self> {
        let xprv = Xpriv::new_master(network, &seed)?;
        let external = Bip86(xprv, KeychainKind::External);
        let internal = Bip86(xprv, KeychainKind::Internal);
        let (mut db, changeset) = Store::<ChangeSet>::load_or_create(MAGIC, path)
            .map_err(|e| anyhow!("could not open {}: {e}", path.display()))?;
        let wallet = match changeset {
            Some(_) => Wallet::load()
                .descriptor(KeychainKind::External, Some(external.clone()))
                .descriptor(KeychainKind::Internal, Some(internal.clone()))
                .check_network(network)
                .load_wallet(&mut db)
                .map_err(|e| anyhow!("could not load the wallet: {e}"))?
                .context("the wallet file is empty")?,
            None => {
                let mut wallet = Wallet::create(external, internal)
                    .network(network)
                    .create_wallet(&mut db)
                    .map_err(|e| anyhow!("could not create the wallet: {e}"))?;
                let tip = wallet.latest_checkpoint().insert(birthday);
                wallet
                    .apply_update(bdk_wallet::Update {
                        chain: Some(tip),
                        ..Default::default()
                    })
                    .map_err(|e| anyhow!("could not set the wallet's birthday: {e}"))?;
                wallet.persist(&mut db)?;
                wallet
            }
        };
        Ok(OnChainWallet {
            inner: Mutex::new(Inner { wallet, db }),
            network,
            xprv,
            secp: Secp256k1::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The block the wallet has synced to, where its chain sync resumes.
    pub fn best_block(&self) -> CheckPoint {
        self.lock().wallet.latest_checkpoint()
    }

    pub fn new_address(&self) -> Address {
        let mut inner = self.lock();
        let address = inner
            .wallet
            .reveal_next_address(KeychainKind::External)
            .address;
        inner.persist();
        address
    }

    fn change_script(&self) -> ScriptBuf {
        let mut inner = self.lock();
        let script = inner
            .wallet
            .reveal_next_address(KeychainKind::Internal)
            .address
            .script_pubkey();
        inner.persist();
        script
    }

    pub fn balance(&self) -> WalletBalance {
        let balance = self.lock().wallet.balance();
        WalletBalance {
            confirmed_sat: balance.confirmed.to_sat(),
            unconfirmed_sat: (balance.trusted_pending + balance.untrusted_pending).to_sat(),
        }
    }

    /// Transactions this node broadcast: their inputs are spent and their
    /// change is ours before any block says so.
    pub fn saw_unconfirmed(&self, txs: impl IntoIterator<Item = Transaction>) {
        let now = crate::db::now();
        let mut inner = self.lock();
        inner
            .wallet
            .apply_unconfirmed_txs(txs.into_iter().map(|tx| (tx, now)));
        inner.persist();
    }

    /// Sign every input of `psbt` this wallet holds the key of, and finalize
    /// them. Whether every input ended up final.
    fn sign_and_finalize(&self, inner: &Inner, psbt: &mut Psbt) -> Result<bool> {
        psbt.sign(&self.xprv, &self.secp)
            .map_err(|(_, errors)| anyhow!("could not sign: {errors:?}"))?;
        let options = SignOptions {
            // inputs this wallet does not hold stay as they are, for LDK
            try_finalize: true,
            ..Default::default()
        };
        Ok(inner.wallet.finalize_psbt(psbt, options)?)
    }

    /// Sign a PSBT built by this wallet, and hand back the transaction.
    fn sign(&self, inner: &Inner, mut psbt: Psbt) -> Result<Transaction> {
        if !self.sign_and_finalize(inner, &mut psbt)? {
            bail!("the wallet could not sign every input");
        }
        Ok(psbt.extract_tx()?)
    }

    /// A transaction paying `value` to `script`, at `sat_per_kw`: a channel's
    /// funding output, or an operator's withdrawal.
    pub fn pay_to(&self, script: ScriptBuf, value: Amount, sat_per_kw: u32) -> Result<Transaction> {
        let mut inner = self.lock();
        let mut builder = inner.wallet.build_tx();
        builder
            .add_recipient(script, value)
            .fee_rate(FeeRate::from_sat_per_kwu(sat_per_kw.into()));
        let psbt = builder.finish().context("could not fund the transaction")?;
        let tx = self.sign(&inner, psbt)?;
        // its inputs are taken from now on, broadcast or not: no second
        // transaction may pick them while this one waits (see `forget`)
        inner
            .wallet
            .apply_unconfirmed_txs([(tx.clone(), crate::db::now())]);
        inner.persist();
        Ok(tx)
    }

    /// A transaction this wallet built that will never be broadcast: its
    /// inputs are free again.
    pub fn forget(&self, txid: bitcoin::Txid) {
        let mut inner = self.lock();
        inner.wallet.apply_evicted_txs([(txid, crate::db::now())]);
        inner.persist();
    }

    /// Everything the wallet holds, to `address`, at `sat_per_kw`.
    pub fn drain_to(&self, address: &Address, sat_per_kw: u32) -> Result<Transaction> {
        let mut inner = self.lock();
        let mut builder = inner.wallet.build_tx();
        builder
            .drain_wallet()
            .drain_to(address.script_pubkey())
            .fee_rate(FeeRate::from_sat_per_kwu(sat_per_kw.into()));
        let psbt = builder
            .finish()
            .context("could not build the transaction")?;
        let tx = self.sign(&inner, psbt)?;
        inner.persist();
        Ok(tx)
    }

    pub fn parse_address(&self, address: &str) -> Result<Address> {
        Ok(address
            .parse::<Address<_>>()?
            .require_network(self.network)?)
    }
}

impl Listen for OnChainWallet {
    fn filtered_block_connected(&self, header: &Header, txdata: &TransactionData, height: u32) {
        // block-sync hands over whole blocks; only the wallet's own
        // transactions are kept from them
        let block = Block {
            header: *header,
            txdata: txdata.iter().map(|(_, tx)| (*tx).clone()).collect(),
        };
        let connected_to = BlockId {
            height: height.saturating_sub(1),
            hash: header.prev_blockhash,
        };
        let mut inner = self.lock();
        if let Err(e) = inner
            .wallet
            .apply_block_connected_to(&block, height, connected_to)
        {
            log::error!("wallet: could not apply block {height}: {e}");
            return;
        }
        inner.persist();
    }

    fn blocks_disconnected(&self, _fork_point: BestBlock) {
        // the next block connected on the new branch replaces everything
        // above the fork in the wallet's chain
    }
}

impl ChangeDestinationSource for OnChainWallet {
    fn get_change_destination_script<'a>(&'a self) -> AsyncResult<'a, ScriptBuf, ()> {
        let script = self.change_script();
        Box::pin(async move { Ok(script) })
    }
}

/// Coins for anchor-channel fee bumps, and their signatures.
impl WalletSource for OnChainWallet {
    fn list_confirmed_utxos<'a>(&'a self) -> AsyncResult<'a, Vec<Utxo>, ()> {
        let utxos = self
            .lock()
            .wallet
            .list_unspent()
            .filter(|utxo| utxo.chain_position.is_confirmed())
            .map(|utxo| Utxo {
                outpoint: utxo.outpoint,
                output: utxo.txout,
                // an empty script_sig, then the key-path witness
                satisfaction_weight: WITNESS_SCALE_FACTOR as u64 + P2TR_KEY_SPEND_WEIGHT,
            })
            .collect();
        Box::pin(async move { Ok(utxos) })
    }

    fn get_change_script<'a>(&'a self) -> AsyncResult<'a, ScriptBuf, ()> {
        let script = self.change_script();
        Box::pin(async move { Ok(script) })
    }

    fn sign_psbt<'a>(&'a self, mut psbt: Psbt) -> AsyncResult<'a, Transaction, ()> {
        let result = (|| {
            let inner = self.lock();
            // LDK builds this PSBT: give the wallet's inputs what its signer
            // needs to recognise them (key origins, the spent output)
            for (txin, input) in psbt.unsigned_tx.input.iter().zip(psbt.inputs.iter_mut()) {
                if let Some(utxo) = inner.wallet.get_utxo(txin.previous_output) {
                    let ours = inner.wallet.get_psbt_input(utxo, None, true).map_err(|e| {
                        log::error!(
                            "wallet: could not describe input {}: {e}",
                            txin.previous_output
                        );
                    })?;
                    *input = ours;
                }
            }
            // the anchor input is LDK's to sign, after this
            self.sign_and_finalize(&inner, &mut psbt).map_err(|e| {
                log::error!("wallet: could not sign a fee bump: {e:#}");
            })?;
            Ok(psbt.extract_tx_unchecked_fee_rate())
        })();
        Box::pin(async move { result })
    }
}
