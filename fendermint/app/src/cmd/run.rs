// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT
use anyhow::{anyhow, bail, Context};
use async_stm::atomically_or_err;
use fendermint_abci::ApplicationService;
use fendermint_app::ipc::{AppParentFinalityQuery, AppVote};
use fendermint_app::{App, AppConfig, AppStore, BitswapBlockstore};
use fendermint_app_settings::AccountKind;
use fendermint_crypto::SecretKey;
use fendermint_rocksdb::{blockstore::NamespaceBlockstore, namespaces, RocksDb, RocksDbConfig};
use fendermint_vm_actor_interface::eam::EthAddress;
use fendermint_vm_interpreter::chain::ChainEnv;
use fendermint_vm_interpreter::fvm::observe::register_metrics as register_interpreter_metrics;
use fendermint_vm_interpreter::fvm::upgrades::UpgradeScheduler;
use fendermint_vm_interpreter::{
    bytes::{BytesMessageInterpreter, ProposalPrepareMode},
    chain::{ChainMessageInterpreter, CheckpointPool},
    fvm::{Broadcaster, FvmMessageInterpreter, ValidatorContext},
    signed::SignedMessageInterpreter,
};
use fendermint_vm_resolver::ipld::IpldResolver;
use fendermint_vm_snapshot::{SnapshotManager, SnapshotParams};
use fendermint_vm_topdown::eots::EotsTally;
use fendermint_vm_topdown::observe::register_metrics as register_topdown_metrics;
use fendermint_vm_topdown::proxy::{IPCProviderProxy, IPCProviderProxyWithLatency};
use fendermint_vm_topdown::sync::launch_polling_syncer;
use fendermint_vm_topdown::voting::{publish_vote_loop, Error as VoteError, VoteTally};
use fendermint_vm_topdown::{CachedFinalityProvider, IPCParentFinality, Toggle};
use fvm_shared::address::{current_network, Address, Network};
use ipc_api::subnet_id::{NetworkType, SubnetID};
use ipc_ipld_resolver::{Event as ResolverEvent, SignedVoteRecord};
use ipc_observability::observe::register_metrics as register_default_metrics;
use ipc_provider::config::subnet::{BTCSubnet, SubnetConfig};
use ipc_provider::IpcProvider;
use libp2p::identity::secp256k1;
use libp2p::identity::Keypair;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tower::ServiceBuilder;
use tracing::info;

use crate::cmd::key::read_secret_key;
use crate::{cmd, options::run::RunArgs, settings::Settings};
use fendermint_app::observe::register_metrics as register_consensus_metrics;

cmd! {
  RunArgs(self, settings) {
    run(settings).await
  }
}

// Database collection names.
namespaces! {
    Namespaces {
        app,
        state_hist,
        state_store,
        bit_store,
        eots
    }
}

/// Run the Fendermint ABCI Application.
///
/// This method acts as our composition root.
async fn run(settings: Settings) -> anyhow::Result<()> {
    let tendermint_rpc_url = settings.tendermint_rpc_url()?;
    tracing::info!("Connecting to Tendermint at {tendermint_rpc_url}");

    let tendermint_client: tendermint_rpc::HttpClient =
        tendermint_rpc::HttpClient::new(tendermint_rpc_url)
            .context("failed to create Tendermint client")?;

    // Prometheus metrics
    let metrics_registry = if settings.metrics.enabled {
        let registry = prometheus::Registry::new_custom(
            Some("ipc".to_string()),
            Some([("subnet_id".to_string(), settings.ipc.subnet_id.to_string())].into()),
        )
        .context("failed to create Prometheus registry")?;

        register_default_metrics(&registry).context("failed to register default metrics")?;
        register_topdown_metrics(&registry).context("failed to register topdown metrics")?;
        register_interpreter_metrics(&registry)
            .context("failed to register interpreter metrics")?;
        register_consensus_metrics(&registry).context("failed to register consensus metrics")?;

        Some(registry)
    } else {
        None
    };

    let validator = match settings.validator_key {
        Some(ref key) => {
            let sk = key.path(settings.home_dir());
            if sk.exists() && sk.is_file() {
                let sk = read_secret_key(&sk).context("failed to read validator key")?;
                let addr = to_address(&sk, &key.kind)?;
                tracing::info!("validator key address: {addr} detected");
                Some((sk, addr))
            } else {
                bail!("validator key does not exist: {}", sk.to_string_lossy());
            }
        }
        None => {
            tracing::debug!("validator key not configured");
            None
        }
    };

    let validator_keypair = validator.as_ref().map(|(sk, _)| {
        let mut bz = sk.serialize();
        let sk = libp2p::identity::secp256k1::SecretKey::try_from_bytes(&mut bz)
            .expect("secp256k1 secret key");
        let kp = libp2p::identity::secp256k1::Keypair::from(sk);
        libp2p::identity::Keypair::from(kp)
    });

    let validator_ctx = validator.map(|(sk, addr)| {
        // For now we are using the validator key for submitting transactions.
        // This allows us to identify transactions coming from empowered validators, to give priority to protocol related transactions.
        let broadcaster = Broadcaster::new(
            tendermint_client.clone(),
            addr,
            sk.clone(),
            settings.fvm.gas_fee_cap.clone(),
            settings.fvm.gas_premium.clone(),
            settings.fvm.gas_overestimation_rate,
        )
        .with_max_retries(settings.broadcast.max_retries)
        .with_retry_delay(settings.broadcast.retry_delay);

        ValidatorContext::new(sk, addr, broadcaster)
    });

    let testing_settings = match settings.testing.as_ref() {
        Some(_) if current_network() == Network::Mainnet => {
            bail!("testing settings are not allowed on Mainnet");
        }
        other => other,
    };

    let own_subnet_id = settings.ipc.subnet_id.clone();

    let ipc_provider = make_ipc_provider(&settings, own_subnet_id.root_network_type())?;
    let ipc_provider_proxy =
        IPCProviderProxy::new(ipc_provider.clone(), settings.ipc.subnet_id.clone())?;
    let ipc_provider_proxy_with_latency =
        Arc::new(IPCProviderProxyWithLatency::new(ipc_provider_proxy));

    let ns = Namespaces::default();
    let db = open_db(&settings, &ns).context("error opening DB")?;

    // Blockstore for actors.
    let state_store =
        NamespaceBlockstore::new(db.clone(), ns.state_store).context("error creating state DB")?;

    let checkpoint_pool = CheckpointPool::new();
    let parent_finality_votes = VoteTally::empty();

    tracing::info!("settings = {settings:#?}");

    let topdown_enabled = settings.topdown_enabled();
    tracing::info!("topdown_enabled = {topdown_enabled}");

    let btc_manager = match own_subnet_id.parent_network_type() {
        Some(ipc_api::subnet_id::NetworkType::Btc) => {
            let connection = ipc_provider
                .get_connection(
                    &own_subnet_id
                        .parent()
                        .ok_or_else(|| anyhow!("parent subnet not found"))?,
                )
                .context("failed to get connection to bitcoin parent")?;
            Some(
                connection
                    .manager()
                    .as_any()
                    .downcast_ref::<ipc_provider::manager::BtcSubnetManager>()
                    .context("a manager other than BtcSubnetManager was used for bitcoin parent")?
                    .clone(),
            )
        }
        _ => None,
    };

    // The EOTS tally exists only on subnets with a Bitcoin parent.
    let eots_votes = match (&btc_manager, topdown_enabled) {
        (Some(manager), true) => {
            let store = Arc::new(fendermint_app::eots::AppEotsStore::<_, AppStore>::new(
                db.clone(),
                ns.eots.clone(),
            ));
            Some(
                EotsTally::new(
                    store,
                    manager.clone(),
                    own_subnet_id.clone(),
                    Vec::new(),
                    fendermint_app::eots::envelope_check(own_subnet_id.clone()),
                )
                .context("failed to create the EOTS tally")?,
            )
        }
        _ => None,
    };

    let interpreter = match btc_manager.clone() {
        // If the parent subnet is a bitcoin, we need to give the interpreter
        // the manager to use for the bitcoin parent.
        Some(manager) => {
            let interpreter =
                FvmMessageInterpreter::<NamespaceBlockstore, _>::new_for_bitcoin_parent(
                    tendermint_client.clone(),
                    validator_ctx,
                    settings.fvm.gas_overestimation_rate,
                    settings.fvm.gas_search_step,
                    settings.fvm.exec_in_check,
                    UpgradeScheduler::new(),
                    own_subnet_id.clone(),
                    manager,
                )
                .with_eots_tally(eots_votes.clone())
                .with_push_chain_meta(testing_settings.map_or(true, |t| t.push_chain_meta));
            tracing::info!("created interpreter with subnet_id: {:?}", own_subnet_id);
            interpreter
        }
        // If the parent subnet is not bitcoin, we can use the standard interpreter.
        None => {
            let interpreter = FvmMessageInterpreter::<NamespaceBlockstore, _>::new(
                tendermint_client.clone(),
                validator_ctx,
                settings.fvm.gas_overestimation_rate,
                settings.fvm.gas_search_step,
                settings.fvm.exec_in_check,
                UpgradeScheduler::new(),
            )
            .with_push_chain_meta(testing_settings.map_or(true, |t| t.push_chain_meta));
            tracing::info!("created interpreter with subnet_id: {:?}", own_subnet_id);
            interpreter
        }
    };

    let interpreter = SignedMessageInterpreter::new(interpreter);
    let interpreter = ChainMessageInterpreter::<_, NamespaceBlockstore>::new(interpreter);
    let interpreter = BytesMessageInterpreter::new(
        interpreter,
        ProposalPrepareMode::PrependOnly,
        false,
        settings.abci.block_max_msgs,
    );


    // tracing::info!("temporarily disabling topdown finality");
    // let topdown_enabled = false;

    // If enabled, start a resolver that communicates with the application through the resolve pool.
    if settings.resolver_enabled() {
        let mut service =
            make_resolver_service(&settings, db.clone(), state_store.clone(), ns.bit_store)?;

        // Register all metrics from the IPLD resolver stack
        if let Some(ref registry) = metrics_registry {
            service
                .register_metrics(registry)
                .context("failed to register IPLD resolver metrics")?;
        }

        let client = service.client();

        client
            .add_provided_subnet(own_subnet_id.clone())
            .context("error adding own provided subnet.")?;

        if let (Some(tally), Some(key)) = (&eots_votes, &validator_keypair) {
            tracing::info!("starting the EOTS vote loop...");
            tokio::spawn(fendermint_app::eots::vote_loop(
                tally.clone(),
                key.clone(),
                own_subnet_id.clone(),
                client.clone(),
            ));
        }

        let resolver = IpldResolver::new(
            client.clone(),
            checkpoint_pool.queue(),
            settings.resolver.retry_delay,
            own_subnet_id.clone(),
        );

        if topdown_enabled {
            if let Some(key) = validator_keypair {
                let parent_finality_votes = parent_finality_votes.clone();

                let own_subnet_id = own_subnet_id.clone();
                tracing::info!("starting the parent finality vote gossip loop...");
                tokio::spawn(async move {
                    publish_vote_loop(
                        parent_finality_votes,
                        settings.ipc.vote_interval,
                        settings.ipc.vote_timeout,
                        key,
                        own_subnet_id,
                        client,
                        |height, block_hash| {
                            AppVote::ParentFinality(IPCParentFinality { height, block_hash })
                        },
                    )
                    .await
                });
            }
        } else {
            tracing::info!("parent finality vote gossip disabled");
        }

        tracing::info!("subscribing to gossip...");
        let rx = service.subscribe();
        let parent_finality_votes = parent_finality_votes.clone();
        let eots_votes = eots_votes.clone();
        tokio::spawn(async move {
            dispatch_resolver_events(
                rx,
                parent_finality_votes,
                eots_votes,
                topdown_enabled,
            )
            .await;
        });

        tracing::info!("starting the IPLD Resolver Service...");
        tokio::spawn(async move {
            if let Err(e) = service.run().await {
                tracing::error!("IPLD Resolver Service failed: {e:#}")
            }
        });

        tracing::info!("starting the IPLD Resolver...");
        tokio::spawn(async move { resolver.run().await });
    } else {
        tracing::info!("IPLD Resolver disabled.")
    }

    let (parent_finality_provider, ipc_tuple) = if topdown_enabled {
        info!("topdown finality enabled");
        let topdown_config = settings
            .ipc
            .topdown_config(own_subnet_id.root_network_type())?;
        let mut config = fendermint_vm_topdown::Config::new(
            topdown_config.chain_head_delay,
            topdown_config.polling_interval,
            topdown_config.exponential_back_off,
            topdown_config.exponential_retry_limit,
        )
        .with_proposal_delay(topdown_config.proposal_delay)
        .with_max_proposal_range(topdown_config.max_proposal_range);

        if let Some(v) = topdown_config.max_cache_blocks {
            info!(value = v, "setting max cache blocks");
            config = config.with_max_cache_blocks(v);
        }

        let finality_provider = CachedFinalityProvider::uninitialized(
            config.clone(),
            ipc_provider_proxy_with_latency.clone(),
        )
        .await?;

        let p = Arc::new(Toggle::enabled(finality_provider));
        (p, Some((ipc_provider_proxy_with_latency, config)))
    } else {
        info!("topdown finality disabled");
        (Arc::new(Toggle::disabled()), None)
    };

    // Start a snapshot manager in the background.
    let snapshots = if settings.snapshots.enabled {
        let (manager, client) = SnapshotManager::new(
            state_store.clone(),
            SnapshotParams {
                snapshots_dir: settings.snapshots_dir(),
                download_dir: settings.snapshots.download_dir(),
                block_interval: settings.snapshots.block_interval,
                chunk_size: settings.snapshots.chunk_size_bytes,
                hist_size: settings.snapshots.hist_size,
                last_access_hold: settings.snapshots.last_access_hold,
                sync_poll_interval: settings.snapshots.sync_poll_interval,
            },
        )
        .context("failed to create snapshot manager")?;

        tracing::info!("starting the SnapshotManager...");
        let tendermint_client = tendermint_client.clone();
        tokio::spawn(async move { manager.run(tendermint_client).await });

        Some(client)
    } else {
        info!("snapshots disabled");
        None
    };

    let app: App<_, _, AppStore, _> = App::new(
        AppConfig {
            app_namespace: ns.app,
            state_hist_namespace: ns.state_hist,
            state_hist_size: settings.db.state_hist_size,
            halt_height: settings.halt_height,
        },
        db,
        state_store,
        interpreter,
        ChainEnv {
            checkpoint_pool,
            parent_finality_provider: parent_finality_provider.clone(),
            parent_finality_votes: parent_finality_votes.clone(),
            eots_votes: eots_votes.clone(),
        },
        snapshots,
    )?;

    if let Some(tally) = &eots_votes {
        let seeded = fendermint_app::eots::seed_power_table(
            tally,
            &AppParentFinalityQuery::new(app.clone()),
        )
        .context("failed to seed the EOTS power table")?;
        if !seeded {
            info!("EOTS power table: no ledger state yet, seeded at genesis");
        }
    }

    if let Some((agent_proxy, config)) = ipc_tuple {
        let app_parent_finality_query = AppParentFinalityQuery::new(app.clone());
        tokio::spawn(async move {
            match launch_polling_syncer(
                app_parent_finality_query,
                config,
                parent_finality_provider,
                parent_finality_votes,
                agent_proxy,
                tendermint_client,
            )
            .await
            {
                Ok(_) => {}
                Err(e) => tracing::error!("cannot launch polling syncer: {e}"),
            }
        });
    }

    // Start the metrics on a background thread.
    if let Some(registry) = metrics_registry {
        info!(
            listen_addr = settings.metrics.listen.to_string(),
            "serving metrics"
        );
        let mut builder = prometheus_exporter::Builder::new(settings.metrics.listen.try_into()?);
        builder.with_registry(registry);
        let _ = builder.start().context("failed to start metrics server")?;
    } else {
        info!("metrics disabled");
    }

    let service = ApplicationService(app);

    // Split it into components.
    let (consensus, mempool, snapshot, info) =
        tower_abci::split::service(service, settings.abci.bound);

    // Hand those components to the ABCI server. This is where tower layers could be added.
    // TODO: Check out the examples about load shedding in `info` requests.
    let server = tower_abci::v037::Server::builder()
        .consensus(
            // Limiting the concurrency to 1 here because the `AplicationService::poll_ready` always
            // reports `Ready`, because it doesn't know which request it's going to get.
            // Not limiting the concurrency to 1 can lead to transactions being applied
            // in different order across nodes. The buffer size has to be large enough
            // to allow all in-flight requests to not block message handling in
            // `tower_abci::Connection::run`, which could lead to deadlocks.
            // With ABCI++ we need to be able to handle all block transactions plus the begin/end/commit
            // around it. With ABCI 2.0 we'll get the block as a whole, which makes this easier.
            ServiceBuilder::new()
                .buffer(settings.abci.block_max_msgs + 3)
                .concurrency_limit(1)
                .service(consensus),
        )
        .snapshot(snapshot)
        .mempool(mempool)
        .info(info)
        .finish()
        .context("error creating ABCI server")?;

    // Run the ABCI server.
    server
        .listen(settings.abci.listen.to_string())
        .await
        .map_err(|e| anyhow!("error listening: {e}"))?;

    Ok(())
}

/// Open database with all
fn open_db(settings: &Settings, ns: &Namespaces) -> anyhow::Result<RocksDb> {
    let path = settings.data_dir().join("rocksdb");
    info!(
        path = path.to_string_lossy().into_owned(),
        "opening database"
    );
    let config = RocksDbConfig {
        compaction_style: settings.db.compaction_style.to_string(),
        ..Default::default()
    };
    let db = RocksDb::open_cf(path, &config, ns.values().iter())?;
    Ok(db)
}

fn make_resolver_service(
    settings: &Settings,
    db: RocksDb,
    state_store: NamespaceBlockstore,
    bit_store_ns: String,
) -> anyhow::Result<ipc_ipld_resolver::Service<libipld::DefaultParams, AppVote>> {
    // Blockstore for Bitswap.
    let bit_store = NamespaceBlockstore::new(db, bit_store_ns).context("error creating bit DB")?;

    // Blockstore for Bitswap with a fallback on the actor store for reads.
    let bitswap_store = BitswapBlockstore::new(state_store, bit_store);

    let config = to_resolver_config(settings).context("error creating resolver config")?;

    let service = ipc_ipld_resolver::Service::new(config, bitswap_store)
        .context("error creating IPLD Resolver Service")?;

    Ok(service)
}

fn make_ipc_provider(
    settings: &Settings,
    root_network_type: NetworkType,
) -> anyhow::Result<IpcProvider> {
    let topdown_config = settings.ipc.topdown_config(root_network_type)?;

    info!("topdown config {topdown_config:#?}");

    let subnet_id = settings
        .ipc
        .subnet_id
        .parent()
        .ok_or_else(|| anyhow!("subnet has no parent"))?;

    info!("subnet_id {subnet_id:#?}");

    let subnet = ipc_provider::config::Subnet {
        id: subnet_id,
        // TODO(btc) btc or fevm config
        config: SubnetConfig::Btc(BTCSubnet {
            provider_http: topdown_config
                .parent_http_endpoint
                .to_string()
                .parse()
                .unwrap(),
            provider_timeout: topdown_config.parent_http_timeout,
            auth_token: topdown_config.parent_http_auth_token.as_ref().cloned(),
        }),
    };
    info!("init ipc provider with subnet: {}", subnet.id);

    let ipc_provider = IpcProvider::new_with_subnet(None, subnet)?;
    Ok(ipc_provider)
}

fn to_resolver_config(settings: &Settings) -> anyhow::Result<ipc_ipld_resolver::Config> {
    use ipc_ipld_resolver::{
        Config, ConnectionConfig, ContentConfig, DiscoveryConfig, MembershipConfig, NetworkConfig,
    };

    let r = &settings.resolver;

    let local_key: Keypair = {
        let path = r.network.local_key(settings.home_dir());
        let sk = read_secret_key(&path)?;
        let sk = secp256k1::SecretKey::try_from_bytes(sk.serialize())?;
        secp256k1::Keypair::from(sk).into()
    };

    let network_name = format!(
        "ipld-resolver-{}-{}",
        settings.ipc.subnet_id.root_id(),
        r.network.network_name
    );

    let config = Config {
        connection: ConnectionConfig {
            listen_addr: r.connection.listen_addr.clone(),
            external_addresses: r.connection.external_addresses.clone(),
            expected_peer_count: r.connection.expected_peer_count,
            max_incoming: r.connection.max_incoming,
            max_peers_per_query: r.connection.max_peers_per_query,
            event_buffer_capacity: r.connection.event_buffer_capacity,
        },
        network: NetworkConfig {
            local_key,
            network_name,
        },
        discovery: DiscoveryConfig {
            static_addresses: r.discovery.static_addresses.clone(),
            target_connections: r.discovery.target_connections,
            enable_kademlia: r.discovery.enable_kademlia,
        },
        membership: MembershipConfig {
            static_subnets: r.membership.static_subnets.clone(),
            max_subnets: r.membership.max_subnets,
            publish_interval: r.membership.publish_interval,
            min_time_between_publish: r.membership.min_time_between_publish,
            max_provider_age: r.membership.max_provider_age,
        },
        content: ContentConfig {
            rate_limit_bytes: r.content.rate_limit_bytes,
            rate_limit_period: r.content.rate_limit_period,
        },
    };

    Ok(config)
}

fn to_address(sk: &SecretKey, kind: &AccountKind) -> anyhow::Result<Address> {
    let pk = sk.public_key().serialize();
    match kind {
        AccountKind::Regular => Ok(Address::new_secp256k1(&pk)?),
        AccountKind::Ethereum => Ok(Address::from(EthAddress::new_secp256k1(&pk)?)),
    }
}

async fn dispatch_resolver_events(
    mut rx: tokio::sync::broadcast::Receiver<ResolverEvent<AppVote>>,
    parent_finality_votes: VoteTally,
    eots_votes: Option<EotsTally>,
    topdown_enabled: bool,
) {
    loop {
        match rx.recv().await {
            Ok(event) => match event {
                ResolverEvent::ReceivedPreemptive(_, _) => {}
                ResolverEvent::ReceivedVote(vote) => {
                    dispatch_vote(
                        *vote,
                        &parent_finality_votes,
                        eots_votes.as_ref(),
                        topdown_enabled,
                    )
                    .await;
                }
            },
            Err(RecvError::Lagged(n)) => {
                tracing::warn!("the resolver service skipped {n} gossip events")
            }
            Err(RecvError::Closed) => {
                tracing::error!("the resolver service stopped receiving gossip");
                return;
            }
        }
    }
}

/// Logs a vote that can never be valid at error level.
fn log_dropped_vote(e: &fendermint_vm_topdown::eots::VoteError, what: &'static str) {
    if e.never_valid() {
        tracing::error!(error = %e, what);
    } else {
        tracing::debug!(error = %e, what);
    }
}

async fn dispatch_vote(
    signed: SignedVoteRecord<AppVote>,
    parent_finality_votes: &VoteTally,
    eots_votes: Option<&EotsTally>,
    topdown_enabled: bool,
) {
    let signature = signed.signature();
    let vote = signed.into_record();
    match vote.content {
        AppVote::Eots(eots_vote) => {
            let Some(tally) = eots_votes else { return };
            let Some(validator) = fendermint_vm_topdown::eots::try_key_bytes(&vote.public_key)
            else {
                tracing::debug!("EOTS vote dropped: the sender key is not secp256k1");
                return;
            };
            let validator = validator.to_vec();
            let vote = ipc_eots::ValidatorVote {
                validator,
                vote: eots_vote,
                timestamp: vote.timestamp.into(),
                signature,
            };
            match tally.add_vote(vote, None) {
                Ok(outcome) => tracing::debug!(?outcome, "EOTS vote handled"),
                Err(e) => log_dropped_vote(&e, "EOTS vote dropped"),
            }
        }
        AppVote::ParentFinality(f) => {
            if !topdown_enabled {
                tracing::debug!("ignoring vote; topdown disabled");
                return;
            }
            let res = atomically_or_err(|| {
                parent_finality_votes.add_vote(
                    vote.public_key.clone(),
                    f.height,
                    f.block_hash.clone(),
                )
            })
            .await;

            match res {
                Err(e @ VoteError::Equivocation(_, _, _, _)) => {
                    tracing::warn!(error = e.to_string(), "failed to handle vote");
                }
                Err(e @ (
                      VoteError::Uninitialized // early vote, we're not ready yet
                    | VoteError::UnpoweredValidator(_) // maybe arrived too early or too late, or spam
                    | VoteError::UnexpectedBlock(_, _) // won't happen here
                )) => {
                    tracing::debug!(error = e.to_string(), "failed to handle vote");
                }
                _ => {
                    tracing::debug!("vote handled");
                }
            };
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{dispatch_vote, AppVote, EotsTally, Keypair, SubnetID, VoteTally};
    use fendermint_vm_topdown::eots::{AgreedState, EotsStore, KeyBytes, StoredState};
    use fendermint_vm_topdown::voting::Weight;
    use ipc_eots::{testing::TestSigner, ValidatorVote};
    use ipc_ipld_resolver::{SignedVoteRecord, VoteRecord};
    use ipc_provider::{config::Config, manager::BtcSubnetManager};
    use libp2p::core::SignedEnvelope;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Store(Mutex<Vec<ValidatorVote>>);

    impl EotsStore for Store {
        fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(vote.clone());
            Ok(())
        }
        fn put_block(&self, _: u64, _: [u8; 32]) -> anyhow::Result<()> {
            Ok(())
        }
        fn put_recorded_final(&self, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn put_checkpointed(&self, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn put_evidence_carried(&self, _: [u8; 32]) -> anyhow::Result<()> {
            Ok(())
        }
        fn put_power_table(&self, _: u64, _: &[(KeyBytes, Weight)]) -> anyhow::Result<()> {
            Ok(())
        }
        fn put_last_cast(&self, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn delete_up_to(&self, _: u64, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn load(&self) -> anyhow::Result<StoredState> {
            Ok(StoredState::default())
        }
    }

    fn subnet() -> SubnetID {
        SubnetID::new_btc(4, &hex::encode([42; 20])).unwrap()
    }

    fn eots_vote() -> AppVote {
        AppVote::Eots(TestSigner::new(7, 1, 2).vote(&[42; 20], 1, [1; 32]))
    }

    #[tokio::test]
    async fn an_ed25519_gossip_sender_cannot_crash_the_vote_dispatcher() {
        let subnet = subnet();
        let config = Config::from_toml_str(
            "[[subnets]]\nid = \"/b4\"\n[subnets.config]\nnetwork_type = \"btc\"\nprovider_http = \"http://127.0.0.1:1/api\"\n"
        ).unwrap();
        let provider = BtcSubnetManager::new(config.subnets.values().next().unwrap()).unwrap();
        let store = Arc::new(Store::default());
        let tally = EotsTally::new(
            store.clone(),
            provider,
            subnet.clone(),
            vec![],
            fendermint_app::eots::envelope_check(subnet.clone()),
        )
        .unwrap();
        tally.add_block(1, [1; 32]).unwrap();
        let agreed = AgreedState {
            checkpointed: 0,
            killed: false,
            bitcoin_height: 100,
        };
        let parent: VoteTally = VoteTally::new(vec![], (0, Default::default()));
        for key in [Keypair::generate_ed25519(), Keypair::generate_secp256k1()] {
            let signed = VoteRecord::signed(&key, subnet.clone(), eots_vote()).unwrap();
            let wire = signed.into_envelope().into_protobuf_encoding();
            let received = SignedVoteRecord::<AppVote>::from_bytes(&wire).unwrap();
            dispatch_vote(received, &parent, Some(&tally), false).await;
            assert!(store.0.lock().unwrap().is_empty());
            assert!(tally.proposal(&agreed, 2).is_none());
        }
    }

    /// `AppVote::Eots` with fields of any width.
    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    enum Content {
        Eots(Fields),
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Fields {
        pk_eots: Vec<u8>,
        height: i64,
        block_hash: Vec<u8>,
        signature: Vec<Vec<u8>>,
        start: u64,
        proof: Vec<Vec<u8>>,
    }

    #[test]
    fn signed_votes_with_malformed_content_fail_to_decode() {
        let key = Keypair::generate_secp256k1();
        let wire = |fields: Fields| {
            VoteRecord::signed(&key, subnet(), Content::Eots(fields))
                .unwrap()
                .into_envelope()
                .into_protobuf_encoding()
        };
        let AppVote::Eots(vote) = eots_vote() else {
            unreachable!()
        };
        let valid = Fields {
            pk_eots: vote.pk_eots.to_vec(),
            height: vote.height as i64,
            block_hash: vote.block_hash.to_vec(),
            signature: vote.signature.iter().map(|s| s.to_vec()).collect(),
            start: vote.start,
            proof: vote.proof.iter().map(|p| p.to_vec()).collect(),
        };
        let received = SignedVoteRecord::<AppVote>::from_bytes(&wire(valid.clone())).unwrap();
        assert_eq!(received.record().content, eots_vote());

        let changes: [(&str, fn(&mut Fields, usize)); 5] = [
            ("EOTS key", |f, n| f.pk_eots = vec![0; n]),
            ("block hash", |f, n| f.block_hash = vec![0; n]),
            ("public nonce", |f, n| f.signature[0] = vec![0; n]),
            ("signature scalar", |f, n| f.signature[1] = vec![0; n]),
            ("proof node", |f, n| f.proof[0] = vec![0; n]),
        ];
        for (field, change) in changes {
            for length in [0, 31, 33] {
                let mut fields = valid.clone();
                change(&mut fields, length);
                assert!(
                    SignedVoteRecord::<AppVote>::from_bytes(&wire(fields)).is_err(),
                    "accepted a {field} of {length} bytes"
                );
            }
        }
        let negative = Fields {
            height: -1,
            ..valid
        };
        assert!(SignedVoteRecord::<AppVote>::from_bytes(&wire(negative)).is_err());
    }

    #[test]
    fn truncated_signed_votes_fail_to_decode() {
        let key = Keypair::generate_secp256k1();
        let record = VoteRecord::signed(&key, subnet(), eots_vote()).unwrap();
        let payload = fvm_ipld_encoding::to_vec(record.record()).unwrap();
        let wire = |payload: &[u8]| {
            SignedEnvelope::new(
                &key,
                "/ipc/ipld/resolver".to_owned(),
                b"/ipc/vote-record".to_vec(),
                payload.to_vec(),
            )
            .unwrap()
            .into_protobuf_encoding()
        };
        assert!(SignedVoteRecord::<AppVote>::from_bytes(&wire(&payload)).is_ok());
        for end in 0..payload.len() {
            assert!(
                SignedVoteRecord::<AppVote>::from_bytes(&wire(&payload[..end])).is_err(),
                "accepted a payload truncated at byte {end}"
            );
        }
        let full = record.into_envelope().into_protobuf_encoding();
        for end in 0..full.len() {
            assert!(
                SignedVoteRecord::<AppVote>::from_bytes(&full[..end]).is_err(),
                "accepted an envelope truncated at byte {end}"
            );
        }
    }
}
