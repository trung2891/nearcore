// FIXME(nagisa): Is there a good reason we're triggering this? Luckily though this is just test
// code so we're in the clear.
#![allow(clippy::arc_with_non_send_sync)]

use super::block_stats::BlockStats;
use super::peer_manager_mock::PeerManagerMock;
use crate::adapter::{
    AnnounceAccountRequest, BlockApproval, BlockHeadersRequest, BlockHeadersResponse, BlockRequest,
    BlockResponse, SetNetworkInfo, StateRequestHeader, StateRequestPart,
};
use crate::{start_view_client, Client, ClientActor, SyncAdapter, SyncStatus, ViewClientActor};
use actix::{Actor, Addr, AsyncContext, Context};
use actix_rt::System;
use chrono::DateTime;
use chrono::Utc;
use futures::{future, FutureExt};
use near_async::actix::AddrWithAutoSpanContextExt;
use near_async::messaging::{CanSend, IntoSender, LateBoundSender, Sender};
use near_async::time;
use near_chain::state_snapshot_actor::SnapshotCallbacks;
use near_chain::test_utils::{KeyValueRuntime, MockEpochManager, ValidatorSchedule};
use near_chain::types::{ChainConfig, RuntimeAdapter};
use near_chain::{Chain, ChainGenesis, DoomslugThresholdMode};
use near_chain_configs::{ClientConfig, StateSplitConfig};
use near_chunks::adapter::ShardsManagerRequestFromClient;
use near_chunks::client::ShardsManagerResponse;
use near_chunks::shards_manager_actor::start_shards_manager;
use near_chunks::test_utils::SynchronousShardsManagerAdapter;
use near_chunks::ShardsManager;
use near_crypto::{KeyType, PublicKey};
use near_epoch_manager::shard_tracker::{ShardTracker, TrackedConfig};
use near_epoch_manager::EpochManagerAdapter;
use near_network::shards_manager::ShardsManagerRequestFromNetwork;
use near_network::types::{BlockInfo, PeerChainInfo};
use near_network::types::{
    ConnectedPeerInfo, FullPeerInfo, NetworkRequests, NetworkResponses, PeerManagerAdapter,
};
use near_network::types::{NetworkInfo, PeerManagerMessageRequest, PeerManagerMessageResponse};
use near_network::types::{PeerInfo, PeerType};
use near_o11y::WithSpanContextExt;
use near_primitives::block::{ApprovalInner, Block, GenesisId};
use near_primitives::epoch_manager::RngSeed;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::network::PeerId;
use near_primitives::static_clock::StaticClock;
use near_primitives::test_utils::create_test_signer;
use near_primitives::types::{AccountId, BlockHeightDelta, NumBlocks, NumSeats};
use near_primitives::validator_signer::ValidatorSigner;
use near_primitives::version::PROTOCOL_VERSION;
use near_store::test_utils::create_test_store;
use near_store::Store;
use near_telemetry::TelemetryActor;
use num_rational::Ratio;
use once_cell::sync::OnceCell;
use rand::{thread_rng, Rng};
use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::ops::DerefMut;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub const TEST_SEED: RngSeed = [3; 32];

/// min block production time in milliseconds
pub const MIN_BLOCK_PROD_TIME: Duration = Duration::from_millis(100);
/// max block production time in milliseconds
pub const MAX_BLOCK_PROD_TIME: Duration = Duration::from_millis(200);

/// Sets up ClientActor and ViewClientActor viewing the same store/runtime.
pub fn setup(
    vs: ValidatorSchedule,
    epoch_length: BlockHeightDelta,
    account_id: AccountId,
    skip_sync_wait: bool,
    min_block_prod_time: u64,
    max_block_prod_time: u64,
    enable_doomslug: bool,
    archive: bool,
    epoch_sync_enabled: bool,
    state_sync_enabled: bool,
    network_adapter: PeerManagerAdapter,
    transaction_validity_period: NumBlocks,
    genesis_time: DateTime<Utc>,
    ctx: &Context<ClientActor>,
) -> (Block, ClientActor, Addr<ViewClientActor>, ShardsManagerAdapterForTest) {
    let store = create_test_store();
    let num_validator_seats = vs.all_block_producers().count() as NumSeats;
    let epoch_manager = MockEpochManager::new_with_validators(store.clone(), vs, epoch_length);
    let shard_tracker = ShardTracker::new(TrackedConfig::AllShards, epoch_manager.clone());
    let runtime = KeyValueRuntime::new_with_no_gc(store.clone(), epoch_manager.as_ref(), archive);
    let chain_genesis = ChainGenesis {
        time: genesis_time,
        height: 0,
        gas_limit: 1_000_000,
        min_gas_price: 100,
        max_gas_price: 1_000_000_000,
        total_supply: 3_000_000_000_000_000_000_000_000_000_000_000,
        gas_price_adjustment_rate: Ratio::from_integer(0),
        transaction_validity_period,
        epoch_length,
        protocol_version: PROTOCOL_VERSION,
    };
    let doomslug_threshold_mode = if enable_doomslug {
        DoomslugThresholdMode::TwoThirds
    } else {
        DoomslugThresholdMode::NoApprovals
    };
    let chain = Chain::new(
        epoch_manager.clone(),
        shard_tracker.clone(),
        runtime.clone(),
        &chain_genesis,
        doomslug_threshold_mode,
        ChainConfig {
            save_trie_changes: true,
            background_migration_threads: 1,
            state_split_config: StateSplitConfig::default(),
        },
        None,
    )
    .unwrap();
    let genesis_block = chain.get_block(&chain.genesis().hash().clone()).unwrap();

    let signer = Arc::new(create_test_signer(account_id.as_str()));
    let telemetry = TelemetryActor::default().start();
    let config = ClientConfig::test(
        skip_sync_wait,
        min_block_prod_time,
        max_block_prod_time,
        num_validator_seats,
        archive,
        true,
        epoch_sync_enabled,
        state_sync_enabled,
    );

    let adv = crate::adversarial::Controls::default();

    let view_client_addr = start_view_client(
        Some(signer.validator_id().clone()),
        chain_genesis.clone(),
        epoch_manager.clone(),
        shard_tracker.clone(),
        runtime.clone(),
        network_adapter.clone(),
        config.clone(),
        adv.clone(),
    );

    let (shards_manager_addr, _) = start_shards_manager(
        epoch_manager.clone(),
        shard_tracker.clone(),
        network_adapter.clone().into_sender(),
        ctx.address().with_auto_span_context().into_sender(),
        Some(account_id),
        store,
        config.chunk_request_retry_period,
    );
    let shards_manager_adapter = Arc::new(shards_manager_addr.with_auto_span_context());

    let state_sync_adapter =
        Arc::new(RwLock::new(SyncAdapter::new(Sender::noop(), Sender::noop())));
    let client = Client::new(
        config.clone(),
        chain_genesis,
        epoch_manager,
        shard_tracker,
        state_sync_adapter,
        runtime,
        network_adapter.clone(),
        shards_manager_adapter.as_sender(),
        Some(signer.clone()),
        enable_doomslug,
        TEST_SEED,
        None,
    )
    .unwrap();
    let client_actor = ClientActor::new(
        client,
        ctx.address(),
        config,
        PeerId::new(PublicKey::empty(KeyType::ED25519)),
        network_adapter,
        Some(signer),
        telemetry,
        ctx,
        None,
        adv,
        None,
    )
    .unwrap();
    (genesis_block, client_actor, view_client_addr, shards_manager_adapter.into())
}

pub fn setup_only_view(
    vs: ValidatorSchedule,
    epoch_length: BlockHeightDelta,
    account_id: AccountId,
    skip_sync_wait: bool,
    min_block_prod_time: u64,
    max_block_prod_time: u64,
    enable_doomslug: bool,
    archive: bool,
    epoch_sync_enabled: bool,
    state_sync_enabled: bool,
    network_adapter: PeerManagerAdapter,
    transaction_validity_period: NumBlocks,
    genesis_time: DateTime<Utc>,
) -> Addr<ViewClientActor> {
    let store = create_test_store();
    let num_validator_seats = vs.all_block_producers().count() as NumSeats;
    let epoch_manager = MockEpochManager::new_with_validators(store.clone(), vs, epoch_length);
    let shard_tracker = ShardTracker::new_empty(epoch_manager.clone());
    let runtime = KeyValueRuntime::new_with_no_gc(store, epoch_manager.as_ref(), archive);
    let chain_genesis = ChainGenesis {
        time: genesis_time,
        height: 0,
        gas_limit: 1_000_000,
        min_gas_price: 100,
        max_gas_price: 1_000_000_000,
        total_supply: 3_000_000_000_000_000_000_000_000_000_000_000,
        gas_price_adjustment_rate: Ratio::from_integer(0),
        transaction_validity_period,
        epoch_length,
        protocol_version: PROTOCOL_VERSION,
    };

    let doomslug_threshold_mode = if enable_doomslug {
        DoomslugThresholdMode::TwoThirds
    } else {
        DoomslugThresholdMode::NoApprovals
    };
    Chain::new(
        epoch_manager.clone(),
        shard_tracker.clone(),
        runtime.clone(),
        &chain_genesis,
        doomslug_threshold_mode,
        ChainConfig {
            save_trie_changes: true,
            background_migration_threads: 1,
            state_split_config: StateSplitConfig::default(),
        },
        None,
    )
    .unwrap();

    let signer = Arc::new(create_test_signer(account_id.as_str()));
    TelemetryActor::default().start();
    let config = ClientConfig::test(
        skip_sync_wait,
        min_block_prod_time,
        max_block_prod_time,
        num_validator_seats,
        archive,
        true,
        epoch_sync_enabled,
        state_sync_enabled,
    );

    let adv = crate::adversarial::Controls::default();

    start_view_client(
        Some(signer.validator_id().clone()),
        chain_genesis,
        epoch_manager,
        shard_tracker,
        runtime,
        network_adapter,
        config,
        adv,
    )
}

/// Sets up ClientActor and ViewClientActor with mock PeerManager.
pub fn setup_mock(
    validators: Vec<AccountId>,
    account_id: AccountId,
    skip_sync_wait: bool,
    enable_doomslug: bool,
    peer_manager_mock: Box<
        dyn FnMut(
            &PeerManagerMessageRequest,
            &mut Context<PeerManagerMock>,
            Addr<ClientActor>,
        ) -> PeerManagerMessageResponse,
    >,
) -> ActorHandlesForTesting {
    setup_mock_with_validity_period_and_no_epoch_sync(
        validators,
        account_id,
        skip_sync_wait,
        enable_doomslug,
        peer_manager_mock,
        100,
    )
}

pub fn setup_mock_with_validity_period_and_no_epoch_sync(
    validators: Vec<AccountId>,
    account_id: AccountId,
    skip_sync_wait: bool,
    enable_doomslug: bool,
    mut peermanager_mock: Box<
        dyn FnMut(
            &PeerManagerMessageRequest,
            &mut Context<PeerManagerMock>,
            Addr<ClientActor>,
        ) -> PeerManagerMessageResponse,
    >,
    transaction_validity_period: NumBlocks,
) -> ActorHandlesForTesting {
    let network_adapter = Arc::new(LateBoundSender::default());
    let mut vca: Option<Addr<ViewClientActor>> = None;
    let mut sma: Option<ShardsManagerAdapterForTest> = None;
    let client_addr = ClientActor::create(|ctx: &mut Context<ClientActor>| {
        let vs = ValidatorSchedule::new().block_producers_per_epoch(vec![validators]);
        let (_, client, view_client_addr, shards_manager_adapter) = setup(
            vs,
            10,
            account_id,
            skip_sync_wait,
            MIN_BLOCK_PROD_TIME.as_millis() as u64,
            MAX_BLOCK_PROD_TIME.as_millis() as u64,
            enable_doomslug,
            false,
            false,
            true,
            network_adapter.clone().into(),
            transaction_validity_period,
            StaticClock::utc(),
            ctx,
        );
        vca = Some(view_client_addr);
        sma = Some(shards_manager_adapter);
        client
    });
    let client_addr1 = client_addr.clone();

    let network_actor =
        PeerManagerMock::new(move |msg, ctx| peermanager_mock(&msg, ctx, client_addr1.clone()))
            .start();

    network_adapter.bind(network_actor);

    ActorHandlesForTesting {
        client_actor: client_addr,
        view_client_actor: vca.unwrap(),
        shards_manager_adapter: sma.unwrap(),
    }
}

#[derive(Clone)]
pub struct ActorHandlesForTesting {
    pub client_actor: Addr<ClientActor>,
    pub view_client_actor: Addr<ViewClientActor>,
    pub shards_manager_adapter: ShardsManagerAdapterForTest,
}

fn send_chunks<T, I, F>(
    connectors: &[ActorHandlesForTesting],
    recipients: I,
    target: T,
    drop_chunks: bool,
    send_to: F,
) where
    T: Eq,
    I: Iterator<Item = (usize, T)>,
    F: Fn(&ShardsManagerAdapterForTest),
{
    for (i, name) in recipients {
        if name == target {
            if !drop_chunks || !thread_rng().gen_ratio(1, 5) {
                send_to(&connectors[i].shards_manager_adapter);
            }
        }
    }
}

/// Setup multiple clients talking to each other via a mock network.
///
/// # Arguments
///
/// `vs` - the set of validators and how they are assigned to shards in different epochs.
///
/// `key_pairs` - keys for `validators`
///
/// `skip_sync_wait`
///
/// `block_prod_time` - Minimum block production time, assuming there is enough approvals. The
///                 maximum block production time depends on the value of `tamper_with_fg`, and is
///                 equal to `block_prod_time` if `tamper_with_fg` is `true`, otherwise it is
///                 `block_prod_time * 2`
///
/// `drop_chunks` - if set to true, 10% of all the chunk messages / requests will be dropped
///
/// `tamper_with_fg` - if set to true, will split the heights into groups of 100. For some groups
///                 all the approvals will be dropped (thus completely disabling the finality gadget
///                 and introducing severe forkfulness if `block_prod_time` is sufficiently small),
///                 for some groups will keep all the approvals (and test the fg invariants), and
///                 for some will drop 50% of the approvals.
///                 This was designed to tamper with the finality gadget when we
///                 had it, unclear if has much effect today. Must be disabled if doomslug is
///                 enabled (see below), because doomslug will stall if approvals are not delivered.
///
/// `epoch_length` - approximate length of the epoch as measured
///                 by the block heights difference of it's last and first block.
///
/// `enable_doomslug` - If false, blocks will be created when at least one approval is present, without
///                   waiting for 2/3. This allows for more forkfulness. `cross_shard_tx` has modes
///                   both with enabled doomslug (to test "production" setting) and with disabled
///                   doomslug (to test higher forkfullness)
///
/// `network_mock` - the callback that is called for each message sent. The `mock` is called before
///                 the default processing. `mock` returns `(response, perform_default)`. If
///                 `perform_default` is false, then the message is not processed or broadcasted
///                 further and `response` is returned to the requester immediately. Otherwise
///                 the default action is performed, that might (and likely will) overwrite the
///                 `response` before it is sent back to the requester.
pub fn setup_mock_all_validators(
    vs: ValidatorSchedule,
    key_pairs: Vec<PeerInfo>,
    skip_sync_wait: bool,
    block_prod_time: u64,
    drop_chunks: bool,
    tamper_with_fg: bool,
    epoch_length: BlockHeightDelta,
    enable_doomslug: bool,
    archive: Vec<bool>,
    epoch_sync_enabled: Vec<bool>,
    check_block_stats: bool,
    peer_manager_mock: Box<
        dyn FnMut(
            // Peer validators
            &[ActorHandlesForTesting],
            // Validator that sends the message
            AccountId,
            // The message itself
            &PeerManagerMessageRequest,
        ) -> (PeerManagerMessageResponse, /* perform default */ bool),
    >,
) -> (Block, Vec<ActorHandlesForTesting>, Arc<RwLock<BlockStats>>) {
    let peer_manager_mock = Arc::new(RwLock::new(peer_manager_mock));
    let validators = vs.all_validators().cloned().collect::<Vec<_>>();
    let key_pairs = key_pairs;

    let addresses: Vec<_> = (0..key_pairs.len()).map(|i| hash(vec![i as u8].as_ref())).collect();
    let genesis_time = StaticClock::utc();
    let mut ret = vec![];

    let connectors: Arc<OnceCell<Vec<ActorHandlesForTesting>>> = Default::default();

    let announced_accounts = Arc::new(RwLock::new(HashSet::new()));
    let genesis_block = Arc::new(RwLock::new(None));

    let last_height = Arc::new(RwLock::new(vec![0; key_pairs.len()]));
    let largest_endorsed_height = Arc::new(RwLock::new(vec![0u64; key_pairs.len()]));
    let largest_skipped_height = Arc::new(RwLock::new(vec![0u64; key_pairs.len()]));
    let hash_to_height = Arc::new(RwLock::new(HashMap::new()));
    let block_stats = Arc::new(RwLock::new(BlockStats::new()));

    for (index, account_id) in validators.clone().into_iter().enumerate() {
        let vs = vs.clone();
        let block_stats1 = block_stats.clone();
        let mut view_client_addr_slot = None;
        let mut shards_manager_adapter_slot = None;
        let validators_clone2 = validators.clone();
        let genesis_block1 = genesis_block.clone();
        let key_pairs = key_pairs.clone();
        let key_pairs1 = key_pairs.clone();
        let addresses = addresses.clone();
        let connectors1 = connectors.clone();
        let network_mock1 = peer_manager_mock.clone();
        let announced_accounts1 = announced_accounts.clone();
        let last_height1 = last_height.clone();
        let last_height2 = last_height.clone();
        let largest_endorsed_height1 = largest_endorsed_height.clone();
        let largest_skipped_height1 = largest_skipped_height.clone();
        let hash_to_height1 = hash_to_height.clone();
        let archive1 = archive.clone();
        let epoch_sync_enabled1 = epoch_sync_enabled.clone();
        let client_addr = ClientActor::create(|ctx| {
            let client_addr = ctx.address();
            let _account_id = account_id.clone();
            let pm = PeerManagerMock::new(move |msg, _ctx| {
                // Note: this `.wait` will block until all `ClientActors` are created.
                let connectors1 = connectors1.wait();
                let mut guard = network_mock1.write().unwrap();
                let (resp, perform_default) =
                    guard.deref_mut()(connectors1.as_slice(), account_id.clone(), &msg);
                drop(guard);

                if perform_default {
                    let my_ord = validators_clone2.iter().position(|it| it == &account_id).unwrap();
                    let my_key_pair = key_pairs[my_ord].clone();
                    let my_address = addresses[my_ord];

                    {
                        let last_height2 = last_height2.read().unwrap();
                        let peers: Vec<_> = key_pairs1
                            .iter()
                            .take(connectors1.len())
                            .enumerate()
                            .map(|(i, peer_info)| ConnectedPeerInfo {
                                full_peer_info: FullPeerInfo {
                                    peer_info: peer_info.clone(),
                                    chain_info: PeerChainInfo {
                                        genesis_id: GenesisId {
                                            chain_id: "unittest".to_string(),
                                            hash: Default::default(),
                                        },
                                        // TODO: add the correct hash here
                                        last_block: Some(BlockInfo {
                                            height: last_height2[i],
                                            hash: CryptoHash::default(),
                                        }),
                                        tracked_shards: vec![0, 1, 2, 3],
                                        archival: true,
                                    },
                                },
                                received_bytes_per_sec: 0,
                                sent_bytes_per_sec: 0,
                                last_time_peer_requested: near_async::time::Instant::now(),
                                last_time_received_message: near_async::time::Instant::now(),
                                connection_established_time: near_async::time::Instant::now(),
                                peer_type: PeerType::Outbound,
                                nonce: 3,
                            })
                            .collect();
                        let peers2 = peers
                            .iter()
                            .filter_map(|it| it.full_peer_info.clone().into())
                            .collect();
                        let info = NetworkInfo {
                            connected_peers: peers,
                            tier1_connections: vec![],
                            num_connected_peers: key_pairs1.len(),
                            peer_max_count: key_pairs1.len() as u32,
                            highest_height_peers: peers2,
                            sent_bytes_per_sec: 0,
                            received_bytes_per_sec: 0,
                            known_producers: vec![],
                            tier1_accounts_keys: vec![],
                            tier1_accounts_data: vec![],
                        };
                        client_addr.do_send(SetNetworkInfo(info).with_span_context());
                    }

                    match msg.as_network_requests_ref() {
                        NetworkRequests::Block { block } => {
                            if check_block_stats {
                                let block_stats2 = &mut *block_stats1.write().unwrap();
                                block_stats2.add_block(block);
                                block_stats2.check_stats(false);
                            }

                            for actor_handles in connectors1 {
                                actor_handles.client_actor.do_send(
                                    BlockResponse {
                                        block: block.clone(),
                                        peer_id: PeerInfo::random().id,
                                        was_requested: false,
                                    }
                                    .with_span_context(),
                                );
                            }

                            let mut last_height1 = last_height1.write().unwrap();

                            let my_height = &mut last_height1[my_ord];

                            *my_height = max(*my_height, block.header().height());

                            hash_to_height1
                                .write()
                                .unwrap()
                                .insert(*block.header().hash(), block.header().height());
                        }
                        NetworkRequests::PartialEncodedChunkRequest { target, request, .. } => {
                            send_chunks(
                                connectors1,
                                validators_clone2.iter().map(|s| Some(s.clone())).enumerate(),
                                target.account_id.as_ref().map(|s| s.clone()),
                                drop_chunks,
                                |c| {
                                    c.send(ShardsManagerRequestFromNetwork::ProcessPartialEncodedChunkRequest { partial_encoded_chunk_request: request.clone(), route_back: my_address });
                                },
                            );
                        }
                        NetworkRequests::PartialEncodedChunkResponse { route_back, response } => {
                            send_chunks(
                                connectors1,
                                addresses.iter().enumerate(),
                                route_back,
                                drop_chunks,
                                |c| {
                                    c.send(ShardsManagerRequestFromNetwork::ProcessPartialEncodedChunkResponse { partial_encoded_chunk_response: response.clone(), received_time: Instant::now() });
                                },
                            );
                        }
                        NetworkRequests::PartialEncodedChunkMessage {
                            account_id,
                            partial_encoded_chunk,
                        } => {
                            send_chunks(
                                connectors1,
                                validators_clone2.iter().cloned().enumerate(),
                                account_id.clone(),
                                drop_chunks,
                                |c| {
                                    c.send(ShardsManagerRequestFromNetwork::ProcessPartialEncodedChunk(partial_encoded_chunk.clone().into()));
                                },
                            );
                        }
                        NetworkRequests::PartialEncodedChunkForward { account_id, forward } => {
                            send_chunks(
                                connectors1,
                                validators_clone2.iter().cloned().enumerate(),
                                account_id.clone(),
                                drop_chunks,
                                |c| {
                                    c.send(ShardsManagerRequestFromNetwork::ProcessPartialEncodedChunkForward(forward.clone()));
                                }
                            );
                        }
                        NetworkRequests::BlockRequest { hash, peer_id } => {
                            for (i, peer_info) in key_pairs.iter().enumerate() {
                                let peer_id = peer_id.clone();
                                if peer_info.id == peer_id {
                                    let me = connectors1[my_ord].client_actor.clone();
                                    actix::spawn(
                                        connectors1[i]
                                            .view_client_actor
                                            .send(BlockRequest(*hash).with_span_context())
                                            .then(move |response| {
                                                let response = response.unwrap();
                                                match response {
                                                    Some(block) => {
                                                        me.do_send(
                                                            BlockResponse {
                                                                block: *block,
                                                                peer_id,
                                                                was_requested: true,
                                                            }
                                                            .with_span_context(),
                                                        );
                                                    }
                                                    None => {}
                                                }
                                                future::ready(())
                                            }),
                                    );
                                }
                            }
                        }
                        NetworkRequests::BlockHeadersRequest { hashes, peer_id } => {
                            for (i, peer_info) in key_pairs.iter().enumerate() {
                                let peer_id = peer_id.clone();
                                if peer_info.id == peer_id {
                                    let me = connectors1[my_ord].client_actor.clone();
                                    actix::spawn(
                                        connectors1[i]
                                            .view_client_actor
                                            .send(
                                                BlockHeadersRequest(hashes.clone())
                                                    .with_span_context(),
                                            )
                                            .then(move |response| {
                                                let response = response.unwrap();
                                                match response {
                                                    Some(headers) => {
                                                        me.do_send(
                                                            BlockHeadersResponse(headers, peer_id)
                                                                .with_span_context(),
                                                        );
                                                    }
                                                    None => {}
                                                }
                                                future::ready(())
                                            }),
                                    );
                                }
                            }
                        }
                        NetworkRequests::StateRequestHeader {
                            shard_id,
                            sync_hash, ..
                        } => {
                            for (i, _) in validators_clone2.iter().enumerate() {
                                let me = connectors1[my_ord].client_actor.clone();
                                actix::spawn(
                                    connectors1[i]
                                        .view_client_actor
                                        .send(
                                            StateRequestHeader {
                                                shard_id: *shard_id,
                                                sync_hash: *sync_hash,
                                            }
                                                .with_span_context(),
                                        )
                                        .then(move |response| {
                                            let response = response.unwrap();
                                            match response {
                                                Some(response) => {
                                                    me.do_send(response.with_span_context());
                                                }
                                                None => {}
                                            }
                                            future::ready(())
                                        }),
                                );
                            }
                        }
                        NetworkRequests::StateRequestPart {
                            shard_id,
                            sync_hash,
                            part_id, ..
                        } => {
                            for (i, _) in validators_clone2.iter().enumerate() {
                                let me = connectors1[my_ord].client_actor.clone();
                                actix::spawn(
                                    connectors1[i]
                                        .view_client_actor
                                        .send(
                                            StateRequestPart {
                                                shard_id: *shard_id,
                                                sync_hash: *sync_hash,
                                                part_id: *part_id,
                                            }
                                                .with_span_context(),
                                        )
                                        .then(move |response| {
                                            let response = response.unwrap();
                                            match response {
                                                Some(response) => {
                                                    me.do_send(response.with_span_context());
                                                }
                                                None => {}
                                            }
                                            future::ready(())
                                        }),
                                );
                            }
                        }
                        NetworkRequests::AnnounceAccount(announce_account) => {
                            let mut aa = announced_accounts1.write().unwrap();
                            let key = (
                                announce_account.account_id.clone(),
                                announce_account.epoch_id.clone(),
                            );
                            if aa.get(&key).is_none() {
                                aa.insert(key);
                                for actor_handles in connectors1 {
                                    actor_handles.view_client_actor.do_send(
                                        AnnounceAccountRequest(vec![(
                                            announce_account.clone(),
                                            None,
                                        )])
                                            .with_span_context(),
                                    )
                                }
                            }
                        }
                        NetworkRequests::Approval { approval_message } => {
                            let height_mod = approval_message.approval.target_height % 300;

                            let do_propagate = if tamper_with_fg {
                                if height_mod < 100 {
                                    false
                                } else if height_mod < 200 {
                                    let mut rng = rand::thread_rng();
                                    rng.gen()
                                } else {
                                    true
                                }
                            } else {
                                true
                            };

                            let approval = approval_message.approval.clone();

                            if do_propagate {
                                for (i, name) in validators_clone2.iter().enumerate() {
                                    if name == &approval_message.target {
                                        connectors1[i].client_actor.do_send(
                                            BlockApproval(approval.clone(), my_key_pair.id.clone())
                                                .with_span_context(),
                                        );
                                    }
                                }
                            }

                            // Verify doomslug invariant
                            match approval.inner {
                                ApprovalInner::Endorsement(parent_hash) => {
                                    assert!(
                                        approval.target_height
                                            > largest_skipped_height1.read().unwrap()[my_ord]
                                    );
                                    largest_endorsed_height1.write().unwrap()[my_ord] =
                                        approval.target_height;

                                    if let Some(prev_height) =
                                        hash_to_height1.read().unwrap().get(&parent_hash)
                                    {
                                        assert_eq!(prev_height + 1, approval.target_height);
                                    }
                                }
                                ApprovalInner::Skip(prev_height) => {
                                    largest_skipped_height1.write().unwrap()[my_ord] =
                                        approval.target_height;
                                    let e = largest_endorsed_height1.read().unwrap()[my_ord];
                                    // `e` is the *target* height of the last endorsement. `prev_height`
                                    // is allowed to be anything >= to the source height, which is e-1.
                                    assert!(
                                        prev_height + 1 >= e,
                                        "New: {}->{}, Old: {}->{}",
                                        prev_height,
                                        approval.target_height,
                                        e - 1,
                                        e
                                    );
                                }
                            };
                        }
                        NetworkRequests::ForwardTx(_, _)
                        | NetworkRequests::BanPeer { .. }
                        | NetworkRequests::TxStatus(_, _, _)
                        | NetworkRequests::SnapshotHostInfo { .. }
                        | NetworkRequests::Challenge(_) => {}
                    };
                }
                resp
            })
                .start();
            let (block, client, view_client_addr, shards_manager_adapter) = setup(
                vs,
                epoch_length,
                _account_id,
                skip_sync_wait,
                block_prod_time,
                block_prod_time * 3,
                enable_doomslug,
                archive1[index],
                epoch_sync_enabled1[index],
                false,
                Arc::new(pm).into(),
                10000,
                genesis_time,
                ctx,
            );
            view_client_addr_slot = Some(view_client_addr);
            shards_manager_adapter_slot = Some(shards_manager_adapter);
            *genesis_block1.write().unwrap() = Some(block);
            client
        });
        ret.push(ActorHandlesForTesting {
            client_actor: client_addr,
            view_client_actor: view_client_addr_slot.unwrap(),
            shards_manager_adapter: shards_manager_adapter_slot.unwrap(),
        });
    }
    hash_to_height.write().unwrap().insert(CryptoHash::default(), 0);
    hash_to_height
        .write()
        .unwrap()
        .insert(*genesis_block.read().unwrap().as_ref().unwrap().header().clone().hash(), 0);
    connectors.set(ret.clone()).ok().unwrap();
    let value = genesis_block.read().unwrap();
    (value.clone().unwrap(), ret, block_stats)
}

/// Sets up ClientActor and ViewClientActor without network.
pub fn setup_no_network(
    validators: Vec<AccountId>,
    account_id: AccountId,
    skip_sync_wait: bool,
    enable_doomslug: bool,
) -> ActorHandlesForTesting {
    setup_no_network_with_validity_period_and_no_epoch_sync(
        validators,
        account_id,
        skip_sync_wait,
        100,
        enable_doomslug,
    )
}

pub fn setup_no_network_with_validity_period_and_no_epoch_sync(
    validators: Vec<AccountId>,
    account_id: AccountId,
    skip_sync_wait: bool,
    transaction_validity_period: NumBlocks,
    enable_doomslug: bool,
) -> ActorHandlesForTesting {
    setup_mock_with_validity_period_and_no_epoch_sync(
        validators,
        account_id,
        skip_sync_wait,
        enable_doomslug,
        Box::new(|_, _, _| {
            PeerManagerMessageResponse::NetworkResponses(NetworkResponses::NoResponse)
        }),
        transaction_validity_period,
    )
}

pub fn setup_client_with_runtime(
    num_validator_seats: NumSeats,
    account_id: Option<AccountId>,
    enable_doomslug: bool,
    network_adapter: PeerManagerAdapter,
    shards_manager_adapter: ShardsManagerAdapterForTest,
    chain_genesis: ChainGenesis,
    epoch_manager: Arc<dyn EpochManagerAdapter>,
    shard_tracker: ShardTracker,
    runtime: Arc<dyn RuntimeAdapter>,
    rng_seed: RngSeed,
    archive: bool,
    save_trie_changes: bool,
    snapshot_callbacks: Option<SnapshotCallbacks>,
) -> Client {
    let validator_signer =
        account_id.map(|x| Arc::new(create_test_signer(x.as_str())) as Arc<dyn ValidatorSigner>);
    let mut config = ClientConfig::test(
        true,
        10,
        20,
        num_validator_seats,
        archive,
        save_trie_changes,
        true,
        true,
    );
    config.epoch_length = chain_genesis.epoch_length;
    let state_sync_adapter =
        Arc::new(RwLock::new(SyncAdapter::new(Sender::noop(), Sender::noop())));
    let mut client = Client::new(
        config,
        chain_genesis,
        epoch_manager,
        shard_tracker,
        state_sync_adapter,
        runtime,
        network_adapter,
        shards_manager_adapter.client.into(),
        validator_signer,
        enable_doomslug,
        rng_seed,
        snapshot_callbacks,
    )
    .unwrap();
    client.sync_status = SyncStatus::NoSync;
    client
}

pub fn setup_client(
    store: Store,
    vs: ValidatorSchedule,
    account_id: Option<AccountId>,
    enable_doomslug: bool,
    network_adapter: PeerManagerAdapter,
    shards_manager_adapter: ShardsManagerAdapterForTest,
    chain_genesis: ChainGenesis,
    rng_seed: RngSeed,
    archive: bool,
    save_trie_changes: bool,
) -> Client {
    let num_validator_seats = vs.all_block_producers().count() as NumSeats;
    let epoch_manager =
        MockEpochManager::new_with_validators(store.clone(), vs, chain_genesis.epoch_length);
    let shard_tracker = ShardTracker::new_empty(epoch_manager.clone());
    let runtime = KeyValueRuntime::new(store, epoch_manager.as_ref());
    setup_client_with_runtime(
        num_validator_seats,
        account_id,
        enable_doomslug,
        network_adapter,
        shards_manager_adapter,
        chain_genesis,
        epoch_manager,
        shard_tracker,
        runtime,
        rng_seed,
        archive,
        save_trie_changes,
        None,
    )
}

pub fn setup_synchronous_shards_manager(
    account_id: Option<AccountId>,
    client_adapter: Sender<ShardsManagerResponse>,
    network_adapter: PeerManagerAdapter,
    epoch_manager: Arc<dyn EpochManagerAdapter>,
    shard_tracker: ShardTracker,
    runtime: Arc<dyn RuntimeAdapter>,
    chain_genesis: &ChainGenesis,
) -> ShardsManagerAdapterForTest {
    // Initialize the chain, to make sure that if the store is empty, we write the genesis
    // into the store, and as a short cut to get the parameters needed to instantiate
    // ShardsManager. This way we don't have to wait to construct the Client first.
    // TODO(#8324): This should just be refactored so that we can construct Chain first
    // before anything else.
    let chain = Chain::new(
        epoch_manager.clone(),
        shard_tracker.clone(),
        runtime,
        chain_genesis,
        DoomslugThresholdMode::TwoThirds, // irrelevant
        ChainConfig {
            save_trie_changes: true,
            background_migration_threads: 1,
            state_split_config: StateSplitConfig::default(),
        }, // irrelevant
        None,
    )
    .unwrap();
    let chain_head = chain.head().unwrap();
    let chain_header_head = chain.header_head().unwrap();
    let shards_manager = ShardsManager::new(
        time::Clock::real(),
        account_id,
        epoch_manager,
        shard_tracker,
        network_adapter.request_sender,
        client_adapter,
        chain.store().new_read_only_chunks_store(),
        chain_head,
        chain_header_head,
    );
    Arc::new(SynchronousShardsManagerAdapter::new(shards_manager)).into()
}

pub fn setup_client_with_synchronous_shards_manager(
    store: Store,
    vs: ValidatorSchedule,
    account_id: Option<AccountId>,
    enable_doomslug: bool,
    network_adapter: PeerManagerAdapter,
    client_adapter: Sender<ShardsManagerResponse>,
    chain_genesis: ChainGenesis,
    rng_seed: RngSeed,
    archive: bool,
    save_trie_changes: bool,
) -> Client {
    if let None = System::try_current() {
        let _ = System::new();
    }
    let num_validator_seats = vs.all_block_producers().count() as NumSeats;
    let epoch_manager =
        MockEpochManager::new_with_validators(store.clone(), vs, chain_genesis.epoch_length);
    let shard_tracker = ShardTracker::new_empty(epoch_manager.clone());
    let runtime = KeyValueRuntime::new(store, epoch_manager.as_ref());
    let shards_manager_adapter = setup_synchronous_shards_manager(
        account_id.clone(),
        client_adapter,
        network_adapter.clone(),
        epoch_manager.clone(),
        shard_tracker.clone(),
        runtime.clone(),
        &chain_genesis,
    );
    setup_client_with_runtime(
        num_validator_seats,
        account_id,
        enable_doomslug,
        network_adapter,
        shards_manager_adapter,
        chain_genesis,
        epoch_manager,
        shard_tracker,
        runtime,
        rng_seed,
        archive,
        save_trie_changes,
        None,
    )
}

/// A combined trait bound for both the client side and network side of the ShardsManager API.
#[derive(Clone, derive_more::AsRef)]
pub struct ShardsManagerAdapterForTest {
    pub client: Sender<ShardsManagerRequestFromClient>,
    pub network: Sender<ShardsManagerRequestFromNetwork>,
}

impl<A: CanSend<ShardsManagerRequestFromClient> + CanSend<ShardsManagerRequestFromNetwork>>
    From<Arc<A>> for ShardsManagerAdapterForTest
{
    fn from(arc: Arc<A>) -> Self {
        Self { client: arc.as_sender(), network: arc.as_sender() }
    }
}
