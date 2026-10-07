//! ldk-node's events, turned into the store's state. An event is acknowledged
//! only once the store took it: one the store failed on is delivered again,
//! here or after a restart.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use ldk_node::{
    Event, Node,
    bitcoin::hashes::{Hash, sha256},
    lightning_types::payment::{PaymentHash, PaymentPreimage},
};

use crate::db::NoteStore;

pub(super) async fn run(node: Arc<Node>, store: Arc<NoteStore>, preimage_key: [u8; 32]) {
    loop {
        let event = node.next_event_async().await;
        loop {
            match handle(&node, &store, &preimage_key, &event) {
                Ok(()) => break,
                Err(e) => {
                    log::error!("could not record {event:?}: {e:#}; retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
        if let Err(e) = node.event_handled() {
            log::error!("could not acknowledge an event: {e}");
        }
    }
}

fn handle(node: &Node, store: &NoteStore, preimage_key: &[u8; 32], event: &Event) -> Result<()> {
    match event {
        Event::PaymentClaimable {
            payment_hash,
            claimable_amount_msat,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            let preimage = store
                .claimable(&hash)?
                .map(|note_id| super::mint_preimage(preimage_key, &note_id))
                .filter(|p| sha256::Hash::hash(p).to_byte_array() == payment_hash.0);
            let result = match preimage {
                Some(preimage) => node.bolt11_payment().claim_for_hash(
                    *payment_hash,
                    *claimable_amount_msat,
                    PaymentPreimage(preimage),
                ),
                None => {
                    log::warn!("failing back a payment to {hash}: no unpaid mint invoice");
                    node.bolt11_payment().fail_for_hash(*payment_hash)
                }
            };
            if let Err(e) = result {
                log::error!("could not resolve the payment to {hash}: {e}");
            }
        }
        Event::PaymentReceived {
            payment_hash,
            amount_msat,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            match store.settle_mint(&hash)? {
                Some((note_id, value)) => {
                    log::info!("MINT {note_id} {value} msat (paid {amount_msat} msat, {hash})")
                }
                None => log::info!("received {amount_msat} msat to {hash}"),
            }
        }
        Event::PaymentSuccessful {
            payment_hash,
            payment_preimage,
            fee_paid_msat,
            ..
        } => {
            let hash = hex::encode(payment_hash.0);
            let preimage = payment_preimage.map(|p| hex::encode(p.0));
            let (burned, total) = store.finalize_melt(&hash, preimage.as_deref())?;
            if !burned.is_empty() {
                log::info!(
                    "MELT {hash}: {} note(s), {total} msat, fee {} msat",
                    burned.len(),
                    fee_paid_msat.unwrap_or(0)
                );
            }
        }
        Event::PaymentFailed {
            payment_hash: Some(PaymentHash(hash)),
            reason,
            ..
        } => {
            let hash = hex::encode(hash);
            let restored = store.restore_melt(&hash)?;
            if !restored.is_empty() {
                log::info!(
                    "melt {hash} failed ({reason:?}): {} note(s) released",
                    restored.len()
                );
            }
        }
        Event::ChannelPending {
            channel_id,
            counterparty_node_id,
            ..
        } => log::info!("channel {channel_id} with {counterparty_node_id} pending"),
        Event::ChannelReady {
            channel_id,
            counterparty_node_id,
            ..
        } => log::info!(
            "channel {channel_id} ready{}",
            counterparty_node_id
                .map(|p| format!(" with {p}"))
                .unwrap_or_default()
        ),
        Event::ChannelClosed {
            channel_id, reason, ..
        } => log::info!("channel {channel_id} closed: {reason:?}"),
        other => log::debug!("{other:?}"),
    }
    Ok(())
}
