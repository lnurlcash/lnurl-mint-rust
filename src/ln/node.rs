//! The LDK node: built and run in-process, following LDK's own sample node
//! (ldk-sample, LDK 0.2), with the BDK wallet in place of bitcoind's.
//!
//! Layout under `<DATA_DIR>`: `seed`, `ldk/` (LDK's own store: the channel
//! manager, channel monitors, network graph, scorer, output sweeper) and
//! `wallet.bdk` (the on-chain wallet).

use std::{
    collections::HashMap,
    fs,
    io::BufReader,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use bdk_wallet::chain::BlockId;
use bitcoin::{BlockHash, io, secp256k1::PublicKey};
use lightning::{
    chain::{self, BestBlock, ChannelMonitorUpdateStatus, Filter, chainmonitor},
    events::bump_transaction::{BumpTransactionEventHandler, Wallet},
    ln::{
        channelmanager::{self, ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager},
        msgs::SocketAddress,
        peer_handler::{IgnoringMessageHandler, MessageHandler, PeerManager as LdkPeerManager},
    },
    onion_message::messenger::{DefaultMessageRouter, OnionMessenger as LdkOnionMessenger},
    routing::{
        gossip::{self, NodeId, P2PGossipSync},
        router::DefaultRouter,
        scoring::{
            ProbabilisticScorer, ProbabilisticScoringDecayParameters,
            ProbabilisticScoringFeeParameters,
        },
    },
    sign::{InMemorySigner, KeysManager, NodeSigner},
    util::{
        config::UserConfig,
        persist::{
            KVStore, MonitorUpdatingPersisterAsync, OUTPUT_SWEEPER_PERSISTENCE_KEY,
            OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
            OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
        },
        ser::ReadableArgs,
        sweep::OutputSweeper,
    },
};
use lightning_background_processor::{GossipSync, NO_LIQUIDITY_MANAGER, process_events_async};
use lightning_block_sync::{SpvClient, UnboundedCache, gossip::TokioSpawner, init, poll};
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use tokio::{sync::watch, task::JoinHandle};

use super::{
    bitcoind::{Bitcoind, BitcoindConfig},
    events,
    logger::LdkLogger,
    seed::Seed,
    wallet::OnChainWallet,
};
use crate::db::NoteStore;

pub(super) type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<dyn Filter + Send + Sync>,
    Arc<Bitcoind>,
    Arc<Bitcoind>,
    Arc<LdkLogger>,
    chainmonitor::AsyncPersister<
        Arc<FilesystemStore>,
        TokioSpawner,
        Arc<LdkLogger>,
        Arc<KeysManager>,
        Arc<KeysManager>,
        Arc<Bitcoind>,
        Arc<Bitcoind>,
    >,
    Arc<KeysManager>,
>;

pub(super) type ChannelManager =
    SimpleArcChannelManager<ChainMonitor, Bitcoind, Bitcoind, LdkLogger>;

pub(super) type NetworkGraph = gossip::NetworkGraph<Arc<LdkLogger>>;

type GossipVerifier =
    lightning_block_sync::gossip::GossipVerifier<TokioSpawner, Arc<Bitcoind>, Arc<LdkLogger>>;

type P2pGossip = P2PGossipSync<Arc<NetworkGraph>, Arc<GossipVerifier>, Arc<LdkLogger>>;

type MessageRouter = DefaultMessageRouter<Arc<NetworkGraph>, Arc<LdkLogger>, Arc<KeysManager>>;

type OnionMessenger = LdkOnionMessenger<
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<LdkLogger>,
    Arc<ChannelManager>,
    Arc<MessageRouter>,
    Arc<ChannelManager>,
    Arc<ChannelManager>,
    IgnoringMessageHandler,
    IgnoringMessageHandler,
>;

pub(super) type PeerManager = LdkPeerManager<
    SocketDescriptor,
    Arc<ChannelManager>,
    Arc<P2pGossip>,
    Arc<OnionMessenger>,
    Arc<LdkLogger>,
    IgnoringMessageHandler,
    Arc<KeysManager>,
    Arc<ChainMonitor>,
>;

pub(super) type Sweeper = OutputSweeper<
    Arc<Bitcoind>,
    Arc<OnChainWallet>,
    Arc<Bitcoind>,
    Arc<dyn Filter + Send + Sync>,
    Arc<FilesystemStore>,
    Arc<LdkLogger>,
    Arc<KeysManager>,
>;

pub(super) type BumpHandler = BumpTransactionEventHandler<
    Arc<Bitcoind>,
    Arc<Wallet<Arc<OnChainWallet>, Arc<LdkLogger>>>,
    Arc<KeysManager>,
    Arc<LdkLogger>,
>;

/// Everything the node is started with.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub network: bitcoin::Network,
    pub bitcoind: BitcoindConfig,
    /// Where peers connect to this node.
    pub listen: SocketAddr,
    /// The alias announced with public channels.
    pub alias: String,
    /// Addresses announced with public channels.
    pub announce_addresses: Vec<SocketAddress>,
}

pub struct Node {
    pub(super) network: bitcoin::Network,
    pub(super) channel_manager: Arc<ChannelManager>,
    pub(super) chain_monitor: Arc<ChainMonitor>,
    pub(super) peer_manager: Arc<PeerManager>,
    pub(super) keys_manager: Arc<KeysManager>,
    pub(super) network_graph: Arc<NetworkGraph>,
    pub(super) wallet: Arc<OnChainWallet>,
    pub(super) bitcoind: Arc<Bitcoind>,
    pub(super) sweeper: Arc<Sweeper>,
    pub(super) bump_handler: Arc<BumpHandler>,
    pub(super) store: Arc<NoteStore>,
    pub(super) alias: String,
    pub(super) announce_addresses: Vec<SocketAddress>,
    /// Where peers this node dialled were reached: channels are usually
    /// private, so the gossip graph knows no address to reconnect to.
    peers: Mutex<HashMap<PublicKey, SocketAddr>>,
    peers_path: PathBuf,
    stop: watch::Sender<()>,
    stopping: Arc<AtomicBool>,
    background: Mutex<Option<JoinHandle<Result<(), io::Error>>>>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("node_id", &self.channel_manager.get_our_node_id())
            .finish()
    }
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
}

fn read_graph(path: &std::path::Path, network: bitcoin::Network) -> NetworkGraph {
    if let Ok(file) = fs::File::open(path) {
        match NetworkGraph::read(&mut BufReader::new(file), Arc::new(LdkLogger)) {
            Ok(graph) => return graph,
            Err(e) => log::warn!("could not read the network graph, starting afresh: {e:?}"),
        }
    }
    NetworkGraph::new(network, Arc::new(LdkLogger))
}

fn read_scorer(
    path: &std::path::Path,
    graph: Arc<NetworkGraph>,
) -> ProbabilisticScorer<Arc<NetworkGraph>, Arc<LdkLogger>> {
    let params = ProbabilisticScoringDecayParameters::default();
    if let Ok(file) = fs::File::open(path) {
        let args = (params, Arc::clone(&graph), Arc::new(LdkLogger));
        if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
            return scorer;
        }
    }
    ProbabilisticScorer::new(params, graph, Arc::new(LdkLogger))
}

fn read_peers(path: &std::path::Path) -> HashMap<PublicKey, SocketAddr> {
    let Ok(raw) = fs::read(path) else {
        return HashMap::new();
    };
    let entries: HashMap<String, String> = serde_json::from_slice(&raw).unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|(id, addr)| Some((id.parse().ok()?, addr.parse().ok()?)))
        .collect()
}

impl Node {
    pub async fn start(config: NodeConfig, store: Arc<NoteStore>) -> Result<Arc<Node>> {
        let network = config.network;
        let logger = Arc::new(LdkLogger);
        let bitcoind = Arc::new(Bitcoind::connect(&config.bitcoind, network).await?);

        let seed = Seed::load_or_create(&config.data_dir.join("seed"))?;
        let started = now();
        let keys_manager = Arc::new(KeysManager::new(
            &seed.ldk(),
            started.as_secs(),
            started.subsec_nanos(),
            true,
        ));

        let ldk_dir = config.data_dir.join("ldk");
        fs::create_dir_all(&ldk_dir)?;
        let fs_store = Arc::new(FilesystemStore::new(ldk_dir.clone()));
        let persister = MonitorUpdatingPersisterAsync::new(
            Arc::clone(&fs_store),
            TokioSpawner,
            Arc::clone(&logger),
            1000,
            Arc::clone(&keys_manager),
            Arc::clone(&keys_manager),
            Arc::clone(&bitcoind),
            Arc::clone(&bitcoind),
        );
        let mut monitors = persister
            .read_all_channel_monitors_with_updates()
            .await
            .map_err(|e| anyhow!("could not read channel monitors: {e}"))?;

        let chain_monitor: Arc<ChainMonitor> =
            Arc::new(chainmonitor::ChainMonitor::new_async_beta(
                None,
                Arc::clone(&bitcoind),
                Arc::clone(&logger),
                Arc::clone(&bitcoind),
                persister,
                Arc::clone(&keys_manager),
                keys_manager.get_peer_storage_key(),
            ));

        let polled_tip = init::validate_best_block_header(bitcoind.as_ref())
            .await
            .map_err(|e| anyhow!("could not read bitcoind's best block: {e:?}"))?;

        let network_graph = Arc::new(read_graph(&ldk_dir.join("network_graph"), network));
        let scorer = Arc::new(RwLock::new(read_scorer(
            &ldk_dir.join("scorer"),
            Arc::clone(&network_graph),
        )));
        let router = Arc::new(DefaultRouter::new(
            Arc::clone(&network_graph),
            Arc::clone(&logger),
            Arc::clone(&keys_manager),
            Arc::clone(&scorer),
            ProbabilisticScoringFeeParameters::default(),
        ));
        let message_router = Arc::new(DefaultMessageRouter::new(
            Arc::clone(&network_graph),
            Arc::clone(&keys_manager),
        ));

        let mut user_config = UserConfig::default();
        user_config
            .channel_handshake_limits
            .force_announced_channel_preference = false;
        user_config
            .channel_handshake_config
            .negotiate_anchors_zero_fee_htlc_tx = true;
        // every inbound channel is accepted (events.rs), but anchor channels
        // can only be accepted by hand
        user_config.manually_accept_inbound_channels = true;

        let manager_path = ldk_dir.join("manager");
        let restarting = manager_path.exists();
        let (manager_block, channel_manager) = if restarting {
            let monitor_refs = monitors.iter().map(|(_, m)| m).collect();
            let read_args = ChannelManagerReadArgs::new(
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                Arc::clone(&bitcoind),
                Arc::clone(&chain_monitor),
                Arc::clone(&bitcoind),
                router,
                Arc::clone(&message_router),
                Arc::clone(&logger),
                user_config,
                monitor_refs,
            );
            let file = fs::File::open(&manager_path)?;
            <(BlockHash, ChannelManager)>::read(&mut BufReader::new(file), read_args)
                .map_err(|e| anyhow!("could not read the channel manager: {e:?}"))?
        } else {
            let best_block = polled_tip.to_best_block();
            let manager = channelmanager::ChannelManager::new(
                Arc::clone(&bitcoind),
                Arc::clone(&chain_monitor),
                Arc::clone(&bitcoind),
                router,
                Arc::clone(&message_router),
                Arc::clone(&logger),
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                user_config,
                ChainParameters {
                    network,
                    best_block,
                },
                started.as_secs() as u32,
            );
            (best_block.block_hash, manager)
        };

        let wallet = Arc::new(OnChainWallet::open(
            &config.data_dir.join("wallet.bdk"),
            seed.wallet(),
            network,
            BlockId {
                height: polled_tip.height,
                hash: polled_tip.to_best_block().block_hash,
            },
        )?);
        bitcoind.set_wallet(Arc::clone(&wallet));

        let (sweeper_block, sweeper) = match fs_store
            .read(
                OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
                OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
                OUTPUT_SWEEPER_PERSISTENCE_KEY,
            )
            .await
        {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let best = channel_manager.current_best_block();
                let sweeper = OutputSweeper::new(
                    best,
                    Arc::clone(&bitcoind),
                    Arc::clone(&bitcoind),
                    None,
                    Arc::clone(&keys_manager),
                    Arc::clone(&wallet),
                    Arc::clone(&fs_store),
                    Arc::clone(&logger),
                );
                (best, sweeper)
            }
            Ok(bytes) => {
                let args = (
                    Arc::clone(&bitcoind),
                    Arc::clone(&bitcoind),
                    None,
                    Arc::clone(&keys_manager),
                    Arc::clone(&wallet),
                    Arc::clone(&fs_store),
                    Arc::clone(&logger),
                );
                <(BestBlock, Sweeper)>::read(&mut io::Cursor::new(bytes), args)
                    .map_err(|e| anyhow!("could not read the output sweeper: {e:?}"))?
            }
            Err(e) => return Err(anyhow!("could not read the output sweeper: {e}")),
        };

        // bring every listener up to bitcoind's tip from wherever it stopped
        let wallet_block = wallet.best_block();
        let mut monitor_listeners = Vec::new();
        for (block, monitor) in monitors.drain(..) {
            monitor_listeners.push((
                block,
                (
                    monitor,
                    Arc::clone(&bitcoind),
                    Arc::clone(&bitcoind),
                    Arc::clone(&logger),
                ),
            ));
        }
        let mut cache = UnboundedCache::new();
        let chain_tip = {
            let mut listeners: Vec<(BlockHash, &(dyn chain::Listen + Send + Sync))> =
                vec![(wallet_block.hash(), wallet.as_ref())];
            if restarting {
                listeners.push((manager_block, &channel_manager));
                listeners.push((sweeper_block.block_hash, &sweeper));
                for (block, listener) in monitor_listeners.iter() {
                    listeners.push((*block, listener));
                }
            }
            init::synchronize_listeners(bitcoind.as_ref(), network, &mut cache, listeners)
                .await
                .map_err(|e| anyhow!("could not sync to the chain tip: {e:?}"))?
        };
        for (_, (monitor, ..)) in monitor_listeners {
            let channel_id = monitor.channel_id();
            if chain_monitor.load_existing_monitor(channel_id, monitor)
                != Ok(ChannelMonitorUpdateStatus::Completed)
            {
                return Err(anyhow!(
                    "could not load the monitor of channel {channel_id}"
                ));
            }
        }

        let channel_manager = Arc::new(channel_manager);
        let sweeper = Arc::new(sweeper);
        let gossip_sync = Arc::new(P2PGossipSync::new(
            Arc::clone(&network_graph),
            None,
            Arc::clone(&logger),
        ));
        let onion_messenger: Arc<OnionMessenger> = Arc::new(LdkOnionMessenger::new(
            Arc::clone(&keys_manager),
            Arc::clone(&keys_manager),
            Arc::clone(&logger),
            Arc::clone(&channel_manager),
            Arc::clone(&message_router),
            Arc::clone(&channel_manager),
            Arc::clone(&channel_manager),
            IgnoringMessageHandler {},
            IgnoringMessageHandler {},
        ));
        let peer_manager: Arc<PeerManager> = Arc::new(LdkPeerManager::new(
            MessageHandler {
                chan_handler: Arc::clone(&channel_manager),
                route_handler: Arc::clone(&gossip_sync),
                onion_message_handler: Arc::clone(&onion_messenger),
                custom_message_handler: IgnoringMessageHandler {},
                send_only_message_handler: Arc::clone(&chain_monitor),
            },
            started.as_secs() as u32,
            &rand::random::<[u8; 32]>(),
            Arc::clone(&logger),
            Arc::clone(&keys_manager),
        ));
        gossip_sync.add_utxo_lookup(Some(Arc::new(GossipVerifier::new(
            Arc::clone(&bitcoind),
            TokioSpawner,
            Arc::clone(&gossip_sync),
            Arc::clone(&peer_manager),
        ))));
        let bump_handler = Arc::new(BumpTransactionEventHandler::new(
            Arc::clone(&bitcoind),
            Arc::new(Wallet::new(Arc::clone(&wallet), Arc::clone(&logger))),
            Arc::clone(&keys_manager),
            Arc::clone(&logger),
        ));

        let (stop, stop_check) = watch::channel(());
        let node = Arc::new(Node {
            network,
            channel_manager,
            chain_monitor,
            peer_manager,
            keys_manager,
            network_graph,
            wallet,
            bitcoind,
            sweeper,
            bump_handler,
            store,
            alias: config.alias.clone(),
            announce_addresses: config.announce_addresses.clone(),
            peers: Mutex::new(read_peers(&ldk_dir.join("peers.json"))),
            peers_path: ldk_dir.join("peers.json"),
            stop,
            stopping: Arc::new(AtomicBool::new(false)),
            background: Mutex::new(None),
        });

        node.listen(config.listen).await?;
        node.follow_chain(chain_tip, cache);
        tokio::spawn(Arc::clone(&node.bitcoind).fee_loop());

        let event_node = Arc::clone(&node);
        let event_handler = move |event| {
            let node = Arc::clone(&event_node);
            async move { events::handle(&node, event).await }
        };
        let background = tokio::spawn(process_events_async(
            fs_store,
            event_handler,
            Arc::clone(&node.chain_monitor),
            Arc::clone(&node.channel_manager),
            Some(onion_messenger),
            GossipSync::p2p(gossip_sync),
            Arc::clone(&node.peer_manager),
            NO_LIQUIDITY_MANAGER,
            Some(Arc::clone(&node.sweeper)),
            Arc::clone(&logger),
            Some(scorer),
            move |t| {
                let mut stop_check = stop_check.clone();
                Box::pin(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(t) => false,
                        _ = stop_check.changed() => true,
                    }
                })
            },
            false,
            || Some(now()),
        ));
        *node.background.lock().unwrap_or_else(|e| e.into_inner()) = Some(background);

        node.reconnect_channel_peers();
        node.announce_periodically();

        log::info!(
            "Lightning node {} on {network}, listening on {}",
            node.channel_manager.get_our_node_id(),
            config.listen
        );
        Ok(node)
    }

    async fn listen(self: &Arc<Self>, addr: SocketAddr) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("could not listen for peers on {addr}"))?;
        let node = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        log::warn!("peer listener: {e}");
                        continue;
                    }
                };
                if node.stopping.load(Ordering::Acquire) {
                    return;
                }
                let Ok(stream) = stream.into_std() else {
                    continue;
                };
                let peer_manager = Arc::clone(&node.peer_manager);
                tokio::spawn(lightning_net_tokio::setup_inbound(peer_manager, stream));
            }
        });
        Ok(())
    }

    /// Poll bitcoind for new blocks and hand them to every listener.
    fn follow_chain(
        self: &Arc<Self>,
        tip: lightning_block_sync::poll::ValidatedBlockHeader,
        mut cache: UnboundedCache,
    ) {
        let node = Arc::clone(self);
        tokio::spawn(async move {
            let poller = poll::ChainPoller::new(node.bitcoind.as_ref(), node.network);
            let listeners = (
                Arc::clone(&node.chain_monitor),
                &(
                    Arc::clone(&node.channel_manager),
                    &(Arc::clone(&node.sweeper), Arc::clone(&node.wallet)),
                ),
            );
            let mut client = SpvClient::new(tip, poller, &mut cache, &listeners);
            loop {
                if node.stopping.load(Ordering::Acquire) {
                    return;
                }
                if let Err(e) = client.poll_best_tip().await {
                    log::warn!("chain sync: {e:?}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    /// Keep a connection to every channel peer whose address is known.
    fn reconnect_channel_peers(self: &Arc<Self>) {
        let node = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if node.stopping.load(Ordering::Acquire) {
                    return;
                }
                for peer in node
                    .channel_manager
                    .list_channels()
                    .iter()
                    .map(|c| c.counterparty.node_id)
                    .filter(|id| node.peer_manager.peer_by_node_id(id).is_none())
                {
                    for addr in node.addresses_of(&peer) {
                        if node.connect(peer, addr).await.is_ok() {
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Where `peer` can be reached: where this node last dialled it, then
    /// what the network graph announces for it, onions aside.
    pub(super) fn addresses_of(&self, peer: &PublicKey) -> Vec<SocketAddr> {
        let known = self.lock_peers().get(peer).copied();
        let graph = self.network_graph.read_only();
        let announced = graph
            .node(&NodeId::from_pubkey(peer))
            .and_then(|n| n.announcement_info.as_ref())
            .map(|a| a.addresses().to_vec())
            .unwrap_or_default();
        known
            .into_iter()
            .chain(announced_socket_addrs(&announced))
            .collect()
    }

    fn lock_peers(&self) -> std::sync::MutexGuard<'_, HashMap<PublicKey, SocketAddr>> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember where `peer` was reached, for reconnecting after a restart.
    fn remember(&self, peer: PublicKey, addr: SocketAddr) {
        let entries: HashMap<String, String> = {
            let mut peers = self.lock_peers();
            if peers.get(&peer) == Some(&addr) {
                return;
            }
            peers.insert(peer, addr);
            peers
                .iter()
                .map(|(id, addr)| (id.to_string(), addr.to_string()))
                .collect()
        };
        let tmp = self.peers_path.with_extension("json.tmp");
        let written = serde_json::to_vec(&entries)
            .map_err(std::io::Error::other)
            .and_then(|raw| fs::write(&tmp, raw))
            .and_then(|()| fs::rename(&tmp, &self.peers_path));
        if let Err(e) = written {
            log::warn!("could not save peer addresses: {e}");
        }
    }
}

fn announced_socket_addrs(addresses: &[SocketAddress]) -> Vec<SocketAddr> {
    addresses
        .iter()
        .filter(|a| !matches!(a, SocketAddress::OnionV2(_) | SocketAddress::OnionV3 { .. }))
        .filter_map(|a| std::net::ToSocketAddrs::to_socket_addrs(a).ok())
        .flatten()
        .collect()
}

impl Node {
    /// Connect to `peer` at `addr`, unless already connected.
    pub(super) async fn connect(&self, peer: PublicKey, addr: SocketAddr) -> Result<()> {
        if self.peer_manager.peer_by_node_id(&peer).is_some() {
            return Ok(());
        }
        let Some(closed) =
            lightning_net_tokio::connect_outbound(Arc::clone(&self.peer_manager), peer, addr).await
        else {
            return Err(anyhow!("could not connect to {peer}@{addr}"));
        };
        let mut closed = Box::pin(closed);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                _ = &mut closed => return Err(anyhow!("{peer} closed the connection")),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            if self.peer_manager.peer_by_node_id(&peer).is_some() {
                self.remember(peer, addr);
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!("handshake with {peer} timed out"));
            }
        }
    }

    /// Announce this node hourly, once it has a public channel to announce.
    fn announce_periodically(self: &Arc<Self>) {
        let node = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                if node.stopping.load(Ordering::Acquire) {
                    return;
                }
                if node
                    .channel_manager
                    .list_channels()
                    .iter()
                    .any(|c| c.is_announced)
                {
                    let mut alias = [0u8; 32];
                    let bytes = node.alias.as_bytes();
                    alias[..bytes.len().min(32)].copy_from_slice(&bytes[..bytes.len().min(32)]);
                    node.peer_manager.broadcast_node_announcement(
                        [0; 3],
                        alias,
                        node.announce_addresses.clone(),
                    );
                }
            }
        });
    }

    /// Stop processing and persist: the channel manager is written by the
    /// background processor as it exits.
    pub async fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.peer_manager.disconnect_all_peers();
        let _ = self.stop.send(());
        let background = self
            .background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(background) = background {
            match background.await {
                Ok(Ok(())) => log::info!("Lightning node stopped"),
                Ok(Err(e)) => log::error!("Lightning node stopped with an error: {e}"),
                Err(e) => log::error!("Lightning node's background task failed: {e}"),
            }
        }
    }

    /// This node's id, hex.
    pub fn node_id(&self) -> String {
        self.channel_manager.get_our_node_id().to_string()
    }

    /// The node's secret key, as lnurlcash-core's secp256k1 sees it.
    pub(super) fn node_secret(&self) -> [u8; 32] {
        self.keys_manager.get_node_secret_key().secret_bytes()
    }

    pub(super) fn sat_per_kw(
        &self,
        target: lightning::chain::chaininterface::ConfirmationTarget,
    ) -> u32 {
        use lightning::chain::chaininterface::FeeEstimator;
        self.bitcoind.get_est_sat_per_1000_weight(target)
    }
}
