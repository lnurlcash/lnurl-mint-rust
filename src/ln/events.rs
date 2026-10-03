//! LDK's events: the only place a Lightning outcome reaches the note store.
//!
//! - A payment arriving is claimed only if it pays an unsettled mint (or
//!   operator) invoice; anything else is failed back.
//! - Claimed, it credits its note.
//! - An outgoing payment that succeeded burns the notes its melt reserved;
//!   one that failed, once LDK says no HTLC of it is left, releases them.
//!
//! A store write that fails asks LDK to replay the event, which it does,
//! across restarts too: no outcome is lost to a crash or a database error.

use std::sync::Arc;

use lightning::{
    chain::chaininterface::ConfirmationTarget,
    events::{Event, FundingInfo, PaymentPurpose, ReplayEvent},
};

use super::node::Node;

fn replay(what: &str, err: impl std::fmt::Display) -> ReplayEvent {
    log::error!("{what}: {err} - will retry");
    ReplayEvent()
}

pub(super) async fn handle(node: &Arc<Node>, event: Event) -> Result<(), ReplayEvent> {
    match event {
        Event::PaymentClaimable {
            payment_hash,
            purpose,
            amount_msat,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            let claimable = node
                .store
                .claimable(&hash)
                .map_err(|e| replay("checking an incoming payment", e))?;
            let preimage = match purpose {
                PaymentPurpose::Bolt11InvoicePayment {
                    payment_preimage, ..
                } => payment_preimage,
                _ => None,
            };
            match (claimable, preimage) {
                (true, Some(preimage)) => node.channel_manager.claim_funds(preimage),
                _ => {
                    log::info!("failing back an unexpected payment {hash} of {amount_msat} msat");
                    node.channel_manager.fail_htlc_backwards(&payment_hash);
                }
            }
        }
        Event::PaymentClaimed {
            payment_hash,
            amount_msat,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            match node
                .store
                .settle_mint(&hash)
                .map_err(|e| replay("crediting a mint", e))?
            {
                Some((note_id, net)) => log::info!(
                    "MINT payment_hash={hash} note={note_id} net_msat={net} received_msat={amount_msat}"
                ),
                None => {
                    if node
                        .store
                        .settle_operator_invoice(&hash)
                        .map_err(|e| replay("recording an operator payment", e))?
                    {
                        log::info!("received {amount_msat} msat on operator invoice {hash}");
                    }
                }
            }
        }
        Event::PaymentSent {
            payment_hash,
            payment_preimage,
            fee_paid_msat,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            let preimage = hex::encode(payment_preimage.0);
            let (burned, amount) = node
                .store
                .finalize_melt(&hash, Some(&preimage))
                .map_err(|e| replay("burning melted notes", e))?;
            if burned.is_empty() {
                log::info!("payment {hash} sent, routing fee {fee_paid_msat:?} msat");
            } else {
                log::info!(
                    "MELT payment_hash={hash} notes={burned:?} amount_msat={amount} routing_fee_msat={fee_paid_msat:?}"
                );
            }
        }
        Event::PaymentFailed {
            payment_hash: Some(payment_hash),
            reason,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            let restored = node
                .store
                .restore_melt(&hash)
                .map_err(|e| replay("restoring melted notes", e))?;
            log::info!("payment {hash} failed ({reason:?}), restored {restored:?}");
        }
        Event::PaymentFailed { .. } => {}
        Event::FundingGenerationReady {
            temporary_channel_id,
            counterparty_node_id,
            channel_value_satoshis,
            output_script,
            ..
        } => {
            let fee = node.sat_per_kw(ConfirmationTarget::NonAnchorChannelFee);
            let funded = node.wallet.pay_to(
                output_script,
                bitcoin::Amount::from_sat(channel_value_satoshis),
                fee,
            );
            match funded {
                Ok(tx) => {
                    let txid = tx.compute_txid();
                    if let Err(e) = node.channel_manager.funding_transaction_generated(
                        temporary_channel_id,
                        counterparty_node_id,
                        tx,
                    ) {
                        log::warn!(
                            "channel {temporary_channel_id} went away before funding: {e:?}"
                        );
                        node.wallet.forget(txid);
                    }
                }
                Err(e) => {
                    log::error!("could not fund channel {temporary_channel_id}: {e:#}");
                    let _ = node.channel_manager.force_close_broadcasting_latest_txn(
                        &temporary_channel_id,
                        &counterparty_node_id,
                        "could not fund the channel".into(),
                    );
                }
            }
        }
        Event::DiscardFunding {
            funding_info: FundingInfo::Tx { transaction },
            ..
        } => node.wallet.forget(transaction.compute_txid()),
        Event::OpenChannelRequest {
            temporary_channel_id,
            counterparty_node_id,
            ..
        } => {
            let user_channel_id = u128::from_be_bytes(rand::random());
            if let Err(e) = node.channel_manager.accept_inbound_channel(
                &temporary_channel_id,
                &counterparty_node_id,
                user_channel_id,
                None,
            ) {
                log::warn!("could not accept a channel from {counterparty_node_id}: {e:?}");
            }
        }
        Event::SpendableOutputs {
            outputs,
            channel_id,
        } => {
            node.sweeper
                .track_spendable_outputs(outputs, channel_id, false, None)
                .await
                .map_err(|()| replay("tracking spendable outputs", "sweeper refused"))?;
        }
        Event::BumpTransaction(bump) => node.bump_handler.handle_event(&bump).await,
        Event::ChannelPending {
            channel_id,
            counterparty_node_id,
            ..
        } => log::info!("channel {channel_id} with {counterparty_node_id} is pending"),
        Event::ChannelReady {
            channel_id,
            counterparty_node_id,
            ..
        } => log::info!("channel {channel_id} with {counterparty_node_id} is ready"),
        Event::ChannelClosed {
            channel_id,
            reason,
            counterparty_node_id,
            ..
        } => log::info!(
            "channel {channel_id} with {} closed: {reason}",
            counterparty_node_id
                .map(|id| id.to_string())
                .unwrap_or_default()
        ),
        Event::ConnectionNeeded { node_id, addresses } => {
            let node = Arc::clone(node);
            tokio::spawn(async move {
                for address in addresses {
                    let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&address) else {
                        continue;
                    };
                    for addr in addrs {
                        if node.connect(node_id, addr).await.is_ok() {
                            return;
                        }
                    }
                }
            });
        }
        _ => {}
    }
    Ok(())
}
