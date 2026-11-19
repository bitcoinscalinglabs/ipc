// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

//! Tendermint RPC helper methods for the implementation of the APIs.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context};
use cid::Cid;
use ethers_core::types::{self as et};
use fendermint_rpc::client::{FendermintClient, TendermintClient};

const EOTS_QUERY_PATH: &str = "/eots";
use fendermint_rpc::query::QueryClient;
use fendermint_vm_actor_interface::{evm, system};
use fendermint_vm_message::query::{ActorState, FvmQueryHeight};
use fendermint_vm_message::signed::DomainHash;
use fendermint_vm_message::{chain::ChainMessage, conv::from_eth::to_fvm_address};
use fvm_ipld_encoding::{de::DeserializeOwned, RawBytes};
use fvm_shared::{chainid::ChainID, econ::TokenAmount, error::ExitCode, message::Message};
use ipc_eots::FinalityAnswer;
use rand::Rng;
use tendermint::block::Height;
use tendermint_rpc::query::Query;
use tendermint_rpc::{
    endpoint::{block, block_by_hash, block_results, commit, header, header_by_hash},
    Client,
};
use tendermint_rpc::{Order, Subscription, SubscriptionClient};
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::sync::RwLock;

use crate::cache::{AddressCache, Cache};
use crate::conv::from_tm;
use crate::filters::{
    run_subscription, BlockHash, FilterCommand, FilterDriver, FilterId, FilterKind, FilterMap,
    FilterRecords,
};
use crate::handlers::ws::MethodNotification;
use crate::mpool::{TransactionBuffer, TransactionCache};
use crate::GasOpt;
use crate::{
    conv::from_tm::{map_rpc_block_txs, to_chain_message, to_eth_block, to_eth_transaction},
    error, JsonRpcResult,
};

/// How long to keep transactions in the caches.
const TX_CACHE_TTL_SECS: u64 = 5 * 60;

pub type WebSocketId = usize;
pub type WebSocketSender = UnboundedSender<MethodNotification>;
pub type Nonce = u64;

// Made generic in the client type so we can mock it if we want to test API
// methods without having to spin up a server. In those tests the methods
// below would not be used, so those aren't generic; we'd directly invoke
// e.g. `fendermint_eth_api::apis::eth::accounts` with some mock client.
pub struct JsonRpcState<C> {
    pub client: FendermintClient<C>,
    pub addr_cache: AddressCache<C>,
    /// Cache submitted transactions until they are added to a block.
    pub tx_cache: TransactionCache,
    /// Buffer out-of-order transactions until they can be submitted.
    pub tx_buffer: TransactionBuffer,
    filter_timeout: Duration,
    filters: FilterMap,
    next_web_socket_id: AtomicUsize,
    web_sockets: RwLock<HashMap<WebSocketId, WebSocketSender>>,
    pub max_nonce_gap: Nonce,
    pub gas_opt: GasOpt,
}

impl<C> JsonRpcState<C>
where
    C: Client + Send + Sync + Clone,
{
    pub fn new(
        client: C,
        filter_timeout: Duration,
        cache_capacity: usize,
        max_nonce_gap: Nonce,
        gas_opt: GasOpt,
    ) -> Self {
        let client = FendermintClient::new(client);
        let addr_cache = AddressCache::new(client.clone(), cache_capacity);
        let tx_cache = Cache::new_with_ttl(cache_capacity, Duration::from_secs(TX_CACHE_TTL_SECS));
        let tx_buffer = TransactionBuffer(Cache::new_with_ttl(
            cache_capacity,
            Duration::from_secs(TX_CACHE_TTL_SECS),
        ));
        Self {
            client,
            addr_cache,
            tx_cache,
            tx_buffer,
            filter_timeout,
            filters: Default::default(),
            next_web_socket_id: Default::default(),
            web_sockets: Default::default(),
            gas_opt,
            max_nonce_gap,
        }
    }
}

impl<C> JsonRpcState<C> {
    /// The underlying Tendermint RPC client.
    pub fn tm(&self) -> &C {
        self.client.underlying()
    }

    /// Register the sender of a web socket.
    pub async fn add_web_socket(&self, tx: WebSocketSender) -> WebSocketId {
        let next_id = self.next_web_socket_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.web_sockets.write().await;
        guard.insert(next_id, tx);
        next_id
    }

    /// Remove the sender of a web socket.
    pub async fn remove_web_socket(&self, id: &WebSocketId) {
        let mut guard = self.web_sockets.write().await;
        guard.remove(id);
    }

    /// Get the sender of a web socket.
    pub async fn get_web_socket(&self, id: &WebSocketId) -> anyhow::Result<WebSocketSender> {
        let guard = self.web_sockets.read().await;
        guard
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("web socket not found"))
    }
}

/// Represents the actor type of a concrete actor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ActorType {
    /// The queried actor does not exist in the state tree.
    Inexistent,
    /// The queried actor exists, and it's one of the built-in actor types.
    Known(Cow<'static, str>),
    /// The queried actor exists, but it's not a built-in actor and therefore it cannot be identified.
    Unknown(Cid),
}

impl ActorType {
    pub const EVM: ActorType = ActorType::Known(Cow::Borrowed("evm"));
    pub const ETH_ACCOUNT: ActorType = ActorType::Known(Cow::Borrowed("ethaccount"));
}

impl<C> JsonRpcState<C>
where
    C: Client + Sync + Send,
{
    /// `None` when the subnet has no EOTS finality.
    pub async fn final_height(&self) -> JsonRpcResult<Option<tendermint::block::Height>> {
        let res = self
            .tm()
            .abci_query(Some(EOTS_QUERY_PATH.to_string()), vec![], None, false)
            .await?;
        if res.code.is_err() {
            return Ok(None);
        }
        let answer: FinalityAnswer =
            serde_json::from_slice(&res.value).context("the finality answer does not parse")?;
        Ok(Some(
            Height::try_from(answer.final_height).context("the final height is not a height")?,
        ))
    }

    /// The height a `finalized` or `safe` tag resolves to: the subnet final height, or the latest
    /// committed height minus one without EOTS finality. Zero is genesis, absent in CometBFT.
    pub async fn finalized_height(&self) -> JsonRpcResult<tendermint::block::Height> {
        match self.final_height().await? {
            Some(h) => Ok(h),
            None => self.latest_height().await,
        }
    }

    /// Get the height of the latest commit.
    pub async fn latest_height(&self) -> JsonRpcResult<tendermint::block::Height> {
        let res: commit::Response = self.tm().latest_commit().await?;
        // Return -1 so we don't risk having no data to serve.
        let h = res.signed_header.header.height.value();
        let h = h.saturating_sub(1);
        Ok(Height::try_from(h).context("decrementing should be fine")?)
    }

    /// Get the Tendermint block at a specific height.
    pub async fn block_by_height(
        &self,
        block_number: et::BlockNumber,
    ) -> JsonRpcResult<tendermint::Block> {
        let block = match block_number {
            et::BlockNumber::Number(height) if height == et::U64::from(0) => {
                from_tm::BLOCK_ZERO.clone()
            }
            et::BlockNumber::Number(height) => {
                let height =
                    Height::try_from(height.as_u64()).context("failed to convert to height")?;
                let res: block::Response = self.tm().block(height).await?;
                res.block
            }
            et::BlockNumber::Finalized | et::BlockNumber::Safe => {
                let height = self.finalized_height().await?;
                if height.value() == 0 {
                    from_tm::BLOCK_ZERO.clone()
                } else {
                    let res: block::Response = self.tm().block(height).await?;
                    res.block
                }
            }
            et::BlockNumber::Latest | et::BlockNumber::Pending => {
                // Using 1 block less than latest so if this is followed up by `block_results` then we don't get an error.
                let commit: commit::Response = self.tm().latest_commit().await?;
                let height = commit.signed_header.header.height.value();
                let height = Height::try_from((height.saturating_sub(1)).max(1))
                    .context("failed to convert to height")?;
                let res: block::Response = self.tm().block(height).await?;
                res.block
            }
            et::BlockNumber::Earliest => {
                let res: block::Response = self.tm().block(Height::from(1u32)).await?;
                res.block
            }
        };
        Ok(block)
    }

    /// Get the Tendermint header at a specific height.
    pub async fn header_by_height(
        &self,
        block_number: et::BlockNumber,
    ) -> JsonRpcResult<tendermint::block::Header> {
        let header = match block_number {
            et::BlockNumber::Number(height) if height == et::U64::from(0) => {
                from_tm::BLOCK_ZERO.header.clone()
            }
            et::BlockNumber::Number(height) => {
                let height =
                    Height::try_from(height.as_u64()).context("failed to convert to height")?;
                let res: header::Response = self.tm().header(height).await?;
                res.header
            }
            et::BlockNumber::Finalized | et::BlockNumber::Safe => {
                let height = self.finalized_height().await?;
                if height.value() == 0 {
                    from_tm::BLOCK_ZERO.header.clone()
                } else {
                    let res: header::Response = self.tm().header(height).await?;
                    res.header
                }
            }
            et::BlockNumber::Latest | et::BlockNumber::Pending => {
                // `.latest_commit()` actually points at the block before the last one,
                // because the commit is attached to the next block.
                // Not using `.latest_block().header` because this is a lighter query.
                let res: commit::Response = self.tm().latest_commit().await?;
                res.signed_header.header
            }
            et::BlockNumber::Earliest => {
                let res: header::Response = self.tm().header(Height::from(1u32)).await?;
                res.header
            }
        };
        Ok(header)
    }

    /// Get the Tendermint header at a specificed height or hash.
    pub async fn header_by_id(
        &self,
        block_id: et::BlockId,
    ) -> JsonRpcResult<tendermint::block::Header> {
        match block_id {
            et::BlockId::Number(n) => self.header_by_height(n).await,
            et::BlockId::Hash(h) => self.header_by_hash(h).await,
        }
    }

    /// Return the height of a block which we should send with a query,
    /// or None if it's the latest, to let the node figure it out.
    ///
    /// Adjusts the height of the query to +1 so the effects of the block is visible.
    /// The node stores the results at height+1 to be consistent with how CometBFT works,
    /// ie. the way it publishes the state hash in the *next* block.
    ///
    /// The assumption here is that the client got the height from one of two sources:
    /// * by calling the `latest_height` method above, which adjusts it down,
    ///   so that the returned height is one which is surely executed
    /// * by getting a block (e.g. from a subscription) which was already executed
    ///
    /// In both cases we know that there should be state stored at height + 1.
    pub async fn query_height(&self, block_id: et::BlockId) -> JsonRpcResult<FvmQueryHeight> {
        match block_id {
            et::BlockId::Number(bn) => match bn {
                // The client might be asking by height of a block, expecting to see the results.
                et::BlockNumber::Number(height) => Ok(FvmQueryHeight::from(height.as_u64() + 1)),
                // The state after the final block.
                et::BlockNumber::Finalized | et::BlockNumber::Safe => {
                    match self.final_height().await? {
                        Some(h) => Ok(FvmQueryHeight::Height(h.value() + 1)),
                        None => Ok(FvmQueryHeight::Committed),
                    }
                }
                et::BlockNumber::Latest => Ok(FvmQueryHeight::Committed),
                et::BlockNumber::Pending => Ok(FvmQueryHeight::Pending),
                et::BlockNumber::Earliest => Ok(FvmQueryHeight::Height(1)),
            },
            et::BlockId::Hash(h) => {
                // The effects of this block are saved at the next height.
                let header = self.header_by_hash(h).await?;
                Ok(FvmQueryHeight::Height(header.height.value() + 1))
            }
        }
    }

    /// Get a Tendermint block by hash, if it exists.
    pub async fn block_by_hash_opt(
        &self,
        block_hash: et::H256,
    ) -> JsonRpcResult<Option<tendermint::block::Block>> {
        if block_hash.0 == *from_tm::BLOCK_ZERO_HASH {
            return Ok(Some(from_tm::BLOCK_ZERO.clone()));
        }
        let hash = tendermint::Hash::Sha256(*block_hash.as_fixed_bytes());
        let res: block_by_hash::Response = self.tm().block_by_hash(hash).await?;
        Ok(res.block)
    }

    /// Get a Tendermint height by hash, if it exists.
    pub async fn header_by_hash_opt(
        &self,
        block_hash: et::H256,
    ) -> JsonRpcResult<Option<tendermint::block::Header>> {
        if block_hash.0 == *from_tm::BLOCK_ZERO_HASH {
            return Ok(Some(from_tm::BLOCK_ZERO.header.clone()));
        }
        let hash = tendermint::Hash::Sha256(*block_hash.as_fixed_bytes());
        let res: header_by_hash::Response = self.tm().header_by_hash(hash).await?;
        Ok(res.header)
    }

    /// Get a Tendermint header by hash.
    pub async fn header_by_hash(
        &self,
        block_hash: et::H256,
    ) -> JsonRpcResult<tendermint::block::Header> {
        match self.header_by_hash_opt(block_hash).await? {
            Some(header) => Ok(header),
            None => error(
                ExitCode::USR_NOT_FOUND,
                format!("block {block_hash} not found"),
            ),
        }
    }

    /// Fetch transaction results to produce the full block.
    pub async fn enrich_block(
        &self,
        block: tendermint::Block,
        full_tx: bool,
    ) -> JsonRpcResult<et::Block<serde_json::Value>>
    where
        C: Client + Sync + Send,
    {
        let block = enrich_block(&self.client, &block).await?;

        let block = if full_tx {
            map_rpc_block_txs(block, serde_json::to_value).context("failed to convert to JSON")?
        } else {
            map_rpc_block_txs(block, |h| serde_json::to_value(h.hash))
                .context("failed to convert hash to JSON")?
        };

        Ok(block)
    }

    /// Get a transaction from a block by index.
    pub async fn transaction_by_index(
        &self,
        block: tendermint::Block,
        index: et::U64,
    ) -> JsonRpcResult<Option<et::Transaction>> {
        if let Some(msg) = block.data().get(index.as_usize()) {
            let msg = to_chain_message(msg)?;

            if let ChainMessage::Signed(msg) = msg {
                let sp = self
                    .client
                    .state_params(FvmQueryHeight::from(index.as_u64()))
                    .await?;

                let chain_id = ChainID::from(sp.value.chain_id);

                let hash = if let Ok(Some(DomainHash::Eth(h))) = msg.domain_hash(&chain_id) {
                    et::TxHash::from(h)
                } else {
                    return error(ExitCode::USR_ILLEGAL_ARGUMENT, "incompatible transaction");
                };

                let mut tx = to_eth_transaction(msg, chain_id, hash)
                    .context("failed to convert to eth transaction")?;
                tx.transaction_index = Some(index);
                tx.block_hash = Some(et::H256::from_slice(block.header.hash().as_bytes()));
                tx.block_number = Some(et::U64::from(block.header.height.value()));
                Ok(Some(tx))
            } else {
                error(ExitCode::USR_ILLEGAL_ARGUMENT, "incompatible transaction")
            }
        } else {
            Ok(None)
        }
    }

    /// Get the Tendermint transaction by hash.
    pub async fn tx_by_hash(
        &self,
        tx_hash: et::TxHash,
    ) -> JsonRpcResult<Option<tendermint_rpc::endpoint::tx::Response>> {
        // We cannot use `self.tm().tx()` because the ethers.js forces us to use Ethereum specific hashes.
        // For now we can try to retrieve the transaction using the `tx_search` mechanism, and relying on
        // CometBFT indexing capabilities.

        // Doesn't work with `Query::from(EventType::Tx).and_eq()`
        let query = Query::eq("eth.hash", hex::encode(tx_hash.as_bytes()));

        match self
            .tm()
            .tx_search(query, false, 1, 1, Order::Ascending)
            .await
        {
            Ok(res) => Ok(res.txs.into_iter().next()),
            Err(e) => error(ExitCode::USR_UNSPECIFIED, e),
        }
    }

    /// Send a message by the system actor to an EVM actor for a read-only query.
    ///
    /// If the actor doesn't exist then the FVM will create a placeholder actor,
    /// which will not respond to any queries. In that case `None` is returned.
    pub async fn read_evm_actor<T>(
        &self,
        address: et::H160,
        method: evm::Method,
        params: RawBytes,
        height: FvmQueryHeight,
    ) -> JsonRpcResult<Option<T>>
    where
        T: DeserializeOwned,
    {
        let method_num = method as u64;

        // We send off a read-only query to an EVM actor at the given address.
        let message = Message {
            version: Default::default(),
            from: system::SYSTEM_ACTOR_ADDR,
            to: to_fvm_address(address),
            sequence: 0,
            value: TokenAmount::from_atto(0),
            method_num,
            params,
            gas_limit: fvm_shared::BLOCK_GAS_LIMIT,
            gas_fee_cap: TokenAmount::from_atto(0),
            gas_premium: TokenAmount::from_atto(0),
        };

        let result = self
            .client
            .call(message, height)
            .await
            .context("failed to call contract")?;

        if result.value.code.is_err() {
            return match ExitCode::new(result.value.code.value()) {
                ExitCode::USR_UNHANDLED_MESSAGE => {
                    // If the account is an ETHACCOUNT then it doesn't handle certain methods like `GetCode`.
                    // Let's make it work the same way as a PLACEHOLDER and return nothing.
                    Ok(None)
                }
                other => error(other, result.value.info),
            };
        }

        tracing::debug!(addr = ?address, method_num, data = hex::encode(&result.value.data), "evm actor response");

        let data = fendermint_rpc::response::decode_bytes(&result.value)
            .context("failed to decode data as bytes")?;

        if data.is_empty() {
            Ok(None)
        } else {
            let data: T =
                fvm_ipld_encoding::from_slice(&data).context("failed to decode as IPLD")?;

            Ok(Some(data))
        }
    }

    pub async fn get_actor_type(
        &self,
        address: &et::H160,
        height: FvmQueryHeight,
    ) -> JsonRpcResult<ActorType> {
        let addr = to_fvm_address(*address);

        if let Some(actor_type) = self.addr_cache.get_actor_type_from_addr(&addr) {
            tracing::debug!(
                ?addr,
                ?actor_type,
                "addr cache hit, directly return the actor type"
            );
            return Ok(actor_type);
        }

        let Some((
            _,
            ActorState {
                code: actor_type_cid,
                ..
            },
        )) = self.client.actor_state(&addr, height).await?.value
        else {
            return Ok(ActorType::Inexistent);
        };

        if let Some(actor_type) = self.addr_cache.get_actor_type_from_cid(&actor_type_cid) {
            tracing::debug!(
                ?actor_type_cid,
                ?actor_type,
                "cid cache hit, directly return the actor type"
            );
            tracing::debug!(?addr, ?actor_type, "put result into addr cache");
            self.addr_cache
                .set_actor_type_for_addr(addr, actor_type.clone());
            return Ok(actor_type);
        }

        let registry = self.client.builtin_actors(height).await?.value.registry;
        let ret = match registry.into_iter().find(|(_, cid)| cid == &actor_type_cid) {
            Some((typ, _)) => ActorType::Known(Cow::Owned(typ)),
            None => ActorType::Unknown(actor_type_cid),
        };

        tracing::debug!(?actor_type_cid, ?ret, "put result into cid cache");
        self.addr_cache
            .set_actor_type_for_cid(actor_type_cid, ret.clone());
        tracing::debug!(?addr, ?ret, "put result into addr cache");
        self.addr_cache.set_actor_type_for_addr(addr, ret.clone());

        Ok(ret)
    }
}

impl<C> JsonRpcState<C>
where
    C: Client + SubscriptionClient + Clone + Sync + Send + 'static,
{
    /// Create a new filter with the next available ID and insert it into the filters collection.
    async fn insert_filter_driver(
        &self,
        kind: FilterKind,
        ws_sender: Option<WebSocketSender>,
    ) -> (FilterDriver, Sender<FilterCommand>) {
        let mut filters = self.filters.write().await;

        // Choose an unpredictable filter, so it's not so easy to clear out someone else's logs.
        let mut id: et::U256;
        loop {
            id = FilterId::from(rand::thread_rng().gen::<u64>());
            if !filters.contains_key(&id) {
                break;
            }
        }

        let (driver, tx) = FilterDriver::new(id, self.filter_timeout, kind, ws_sender);

        // Inserting happens here, while removal will be handled by the `FilterState` itself.
        filters.insert(id, tx.clone());

        (driver, tx)
    }

    /// Create a new filter driver, subscribe with Tendermint and start handlers in the background.
    async fn new_filter_driver(
        &self,
        kind: FilterKind,
        ws_sender: Option<WebSocketSender>,
    ) -> anyhow::Result<FilterId> {
        let queries = kind.to_queries();

        let mut subs = Vec::new();

        for query in queries {
            let sub: Subscription = self
                .tm()
                .subscribe(query)
                .await
                .context("failed to subscribe to query")?;

            subs.push(sub);
        }

        let (state, tx) = self.insert_filter_driver(kind, ws_sender).await;
        let id = state.id();
        let filters = self.filters.clone();
        let client = self.client.clone();

        tokio::spawn(async move { state.run(filters, client).await });

        for sub in subs {
            let tx = tx.clone();
            tokio::spawn(async move { run_subscription(id, sub, tx).await });
        }

        Ok(id)
    }

    /// Create a new filter, subscribe with Tendermint and start handlers in the background.
    pub async fn new_filter(&self, kind: FilterKind) -> anyhow::Result<FilterId> {
        self.new_filter_driver(kind, None).await
    }

    /// Create a new subscription, subscribe with Tendermint and start handlers in the background.
    pub async fn new_subscription(
        &self,
        kind: FilterKind,
        ws_sender: WebSocketSender,
    ) -> anyhow::Result<FilterId> {
        self.new_filter_driver(kind, Some(ws_sender)).await
    }
}

impl<C> JsonRpcState<C> {
    pub async fn uninstall_filter(&self, filter_id: FilterId) -> anyhow::Result<bool> {
        let filters = self.filters.read().await;

        if let Some(tx) = filters.get(&filter_id) {
            // Signal to the background tasks that they can unsubscribe.
            tx.send(FilterCommand::Uninstall)
                .await
                .map_err(|e| anyhow!("failed to send command: {e}"))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Take the currently accumulated changes.
    pub async fn take_filter_changes(
        &self,
        filter_id: FilterId,
    ) -> anyhow::Result<Option<FilterRecords<BlockHash>>> {
        let filters = self.filters.read().await;

        match filters.get(&filter_id) {
            None => Ok(None),
            Some(tx) => {
                let (tx_res, rx_res) = tokio::sync::oneshot::channel();

                tx.send(FilterCommand::Take(tx_res))
                    .await
                    .map_err(|e| anyhow!("failed to send command: {e}"))?;

                rx_res.await.context("failed to receive response")?
            }
        }
    }
}

pub async fn enrich_block<C>(
    client: &FendermintClient<C>,
    block: &tendermint::Block,
) -> JsonRpcResult<et::Block<et::Transaction>>
where
    C: Client + Sync + Send,
{
    let height = block.header().height;

    let state_params = client
        .state_params(FvmQueryHeight::Height(height.value()))
        .await?;

    let base_fee = state_params.value.base_fee;
    let chain_id = ChainID::from(state_params.value.chain_id);

    let block_results: block_results::Response = client.underlying().block_results(height).await?;

    let block = to_eth_block(block, block_results, base_fee, chain_id)
        .context("failed to convert to eth block")?;

    Ok(block)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use ethers_core::types::{self as et};
    use fendermint_vm_message::query::FvmQueryHeight;
    use fvm_shared::econ::TokenAmount;
    use tendermint::block::{Commit, Height};
    use tendermint_rpc::endpoint::{abci_query, block, header};
    use tendermint_rpc::{Client, Error, Response, SimpleRequest};

    use crate::conv::from_tm;
    use crate::state::JsonRpcState;
    use crate::GasOpt;

    /// A CometBFT node that has committed the blocks up to `committed` and whose finality query
    /// answers `final_height`, and `conflict_height` with a recorded subnet final height above it.
    #[derive(Clone)]
    struct Node {
        final_height: Option<u64>,
        committed: u64,
        conflict_height: Option<u64>,
    }

    impl Node {
        fn finality(&self) -> abci_query::Response {
            let response = match self.final_height {
                Some(h) => abci_query::AbciQuery {
                    code: tendermint::abci::Code::Ok,
                    value: serde_json::to_vec(&ipc_eots::FinalityAnswer {
                        final_height: h,
                        recorded_final_height: self.conflict_height.map_or(h, |c| c + 1),
                        checkpointed_height: 0,
                        conflict_height: self.conflict_height,
                    })
                    .unwrap(),
                    height: Height::try_from(h).unwrap(),
                    ..Default::default()
                },
                None => abci_query::AbciQuery {
                    code: tendermint::abci::Code::from(1),
                    info: "no EOTS finality on this subnet".to_string(),
                    ..Default::default()
                },
            };
            abci_query::Response { response }
        }

        /// The committed block at the requested height, or the latest one.
        fn block(&self, height: &serde_json::Value) -> tendermint::Block {
            let height = height
                .as_str()
                .map_or(self.committed, |h| h.parse().unwrap());
            assert!(
                (1..=self.committed).contains(&height),
                "asked for uncommitted block {height}"
            );
            let mut block = from_tm::BLOCK_ZERO.clone();
            block.header.height = Height::try_from(height).unwrap();
            // The RPC encoding drops a Unix-epoch time, which then fails to decode.
            block.header.time = tendermint::Time::from_unix_timestamp(height as i64, 0).unwrap();
            block.last_commit = Some(Commit {
                height: Height::try_from(height - 1).unwrap(),
                ..Commit::default()
            });
            block
        }
    }

    #[async_trait]
    impl Client for Node {
        async fn perform<R>(&self, request: R) -> Result<R::Output, Error>
        where
            R: SimpleRequest,
        {
            let request: serde_json::Value = serde_json::from_str(&request.into_json()).unwrap();
            let height = &request["params"]["height"];
            let result = match request["method"].as_str().unwrap() {
                "abci_query" => serde_json::to_value(self.finality()),
                "block" => serde_json::to_value(block::Response {
                    block_id: Default::default(),
                    block: self.block(height),
                }),
                "header" => serde_json::to_value(header::Response {
                    header: self.block(height).header,
                }),
                method => panic!("unexpected request {method}"),
            }
            .unwrap();
            let response = serde_json::json!({ "jsonrpc": "2.0", "id": "", "result": result });
            Ok(R::Response::from_string(response.to_string())?.into())
        }
    }

    fn node(final_height: Option<u64>, committed: u64) -> JsonRpcState<Node> {
        state_of(Node {
            final_height,
            committed,
            conflict_height: None,
        })
    }

    fn state_of(node: Node) -> JsonRpcState<Node> {
        JsonRpcState::new(
            node,
            Duration::from_secs(1),
            10,
            0,
            GasOpt {
                min_gas_premium: TokenAmount::from_atto(0),
                num_blocks_max_prio_fee: 1,
                max_fee_hist_size: 1,
            },
        )
    }

    async fn query_height(state: &JsonRpcState<Node>, tag: et::BlockNumber) -> FvmQueryHeight {
        state.query_height(et::BlockId::Number(tag)).await.unwrap()
    }

    #[tokio::test]
    async fn finality_zero_is_the_genesis_block_and_initial_state() {
        let state = node(Some(0), 5);
        for tag in [et::BlockNumber::Finalized, et::BlockNumber::Safe] {
            let final_height = state.final_height().await.unwrap().unwrap();
            assert_eq!(final_height.value(), 0, "finality 0 stays 0");
            assert_eq!(state.finalized_height().await.unwrap().value(), 0);

            let block = state.block_by_height(tag).await.unwrap();
            assert!(
                from_tm::is_block_zero(&block),
                "{tag:?} serves the genesis block"
            );
            let header = state.header_by_height(tag).await.unwrap();
            assert_eq!(
                header.height.value(),
                0,
                "{tag:?} serves the genesis header"
            );

            assert_eq!(
                state.query_height(et::BlockId::Number(tag)).await.unwrap(),
                FvmQueryHeight::Height(1),
                "{tag:?} serves the initial state"
            );
        }
    }

    #[tokio::test]
    async fn finality_above_zero_is_the_final_block_and_its_state() {
        let state = node(Some(3), 5);
        for tag in [et::BlockNumber::Finalized, et::BlockNumber::Safe] {
            assert_eq!(state.finalized_height().await.unwrap().value(), 3);

            let block = state.block_by_height(tag).await.unwrap();
            assert_eq!(
                block.header.height.value(),
                3,
                "{tag:?} serves the final block"
            );
            assert!(!from_tm::is_block_zero(&block));
            let header = state.header_by_height(tag).await.unwrap();
            assert_eq!(header.height.value(), 3);

            assert_eq!(
                state.query_height(et::BlockId::Number(tag)).await.unwrap(),
                FvmQueryHeight::Height(4),
                "{tag:?} serves the state after the final block"
            );
        }
    }

    #[tokio::test]
    async fn finality_below_a_conflict_is_the_answers_final_height() {
        let state = state_of(Node {
            final_height: Some(3),
            committed: 6,
            conflict_height: Some(4),
        });
        for tag in [et::BlockNumber::Finalized, et::BlockNumber::Safe] {
            assert_eq!(state.finalized_height().await.unwrap().value(), 3);
            let block = state.block_by_height(tag).await.unwrap();
            assert_eq!(block.header.height.value(), 3, "{tag:?}");
        }
    }

    #[tokio::test]
    async fn genesis_state_height_matches_block_number_zero() {
        let state = node(Some(0), 5);
        assert_eq!(
            query_height(&state, et::BlockNumber::Finalized).await,
            query_height(&state, et::BlockNumber::Number(et::U64::zero())).await
        );
    }

    #[tokio::test]
    async fn no_eots_finality_falls_back_to_the_latest_committed_state() {
        let state = node(None, 5);
        assert!(state.final_height().await.unwrap().is_none());
        assert_eq!(
            state
                .query_height(et::BlockId::Number(et::BlockNumber::Finalized))
                .await
                .unwrap(),
            FvmQueryHeight::Committed
        );
    }
}
