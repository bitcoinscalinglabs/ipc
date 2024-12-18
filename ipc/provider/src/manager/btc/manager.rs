// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use ethers::providers::{Authorization, Http};
use http::HeaderValue;
use ipc_api::subnet::{Asset, AssetKind, PermissionMode};
use reqwest::Client;

use crate::config::subnet::SubnetConfig;
use crate::config::Subnet;
use crate::lotus::message::ipc::SubnetInfo;
use crate::manager::subnet::{
    BottomUpCheckpointRelayer, GetBlockHashResult, SubnetGenesisInfo, TopDownFinalityQuery,
    TopDownQueryPayload, ValidatorRewarder,
};

use crate::manager::SubnetManager;
use anyhow::Result;

use anyhow::anyhow;
use fvm_shared::clock::ChainEpoch;
use fvm_shared::{address::Address, econ::TokenAmount};
use ipc_actors_abis::subnet_actor_activity_facet::ValidatorClaim;
use ipc_api::checkpoint::{
    consensus::ValidatorData, BottomUpCheckpoint, BottomUpCheckpointBundle, QuorumReachedEvent,
    Signature,
};
use ipc_api::cross::IpcEnvelope;
use ipc_api::staking::{StakingChangeRequest, ValidatorInfo};
use ipc_api::subnet::ConstructParams;
use ipc_api::subnet_id::SubnetID;

pub struct BtcSubnetManager;
impl BtcSubnetManager {
    pub fn new(subnet: &Subnet) -> Result<Self> {
        let url = subnet.rpc_http().clone();
        let auth_token = subnet.auth_token();

        let config = match &subnet.config {
            SubnetConfig::Btc(config) => config,
            _ => return Err(anyhow!("Unsupported subnet configuration")),
        };

        let mut client = Client::builder();

        if let Some(auth_token) = auth_token {
            let auth = Authorization::Bearer(auth_token);
            let mut auth_value = HeaderValue::from_str(&auth.to_string())?;
            auth_value.set_sensitive(true);

            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth_value);

            client = client.default_headers(headers);
        }

        if let Some(timeout) = subnet.rpc_timeout() {
            client = client.timeout(timeout);
        }

        let client = client.build()?;

        // TODO: implement a Bitcoin IPC provider interface
        Ok(Self {})
    }
}
#[async_trait]
impl SubnetManager for BtcSubnetManager {
    async fn create_subnet(&self, _from: Address, params: ConstructParams) -> Result<Address> {
        tracing::info!("creating subnet on btc with params: {params:?}");
        todo!()
    }

    async fn join_subnet(
        &self,
        subnet: SubnetID,
        _from: Address,
        _collateral: TokenAmount,
        _pub_key: Vec<u8>,
    ) -> Result<ChainEpoch> {
        tracing::info!("joining subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn pre_fund(
        &self,
        subnet: SubnetID,
        _from: Address,
        _balancee: TokenAmount,
    ) -> Result<()> {
        tracing::info!("pre-fund subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn pre_release(
        &self,
        subnet: SubnetID,
        _from: Address,
        _amount: TokenAmount,
    ) -> Result<()> {
        tracing::info!("pre-release subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn stake(
        &self,
        subnet: SubnetID,
        _from: Address,
        _collaterall: TokenAmount,
    ) -> Result<()> {
        tracing::info!("staking subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn unstake(
        &self,
        subnet: SubnetID,
        _from: Address,
        _collateral: TokenAmount,
    ) -> Result<()> {
        tracing::info!("unstaking subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn leave_subnet(&self, subnet: SubnetID, _from: Address) -> Result<()> {
        tracing::info!("leaving subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn kill_subnet(&self, subnet: SubnetID, _from: Address) -> Result<()> {
        tracing::info!("killing subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn list_child_subnets(
        &self,
        gateway_addr: Address,
    ) -> Result<HashMap<SubnetID, SubnetInfo>> {
        tracing::info!("listing child subnets on btc with params: {gateway_addr:?}");
        todo!()
    }

    async fn claim_collateral(&self, subnet: SubnetID, _from: Address) -> Result<()> {
        tracing::info!("claiming collateral on btc with params: {subnet:?}");
        todo!()
    }

    async fn fund(
        &self,
        subnet: SubnetID,
        _gateway_addr: Address,
        _from: Address,
        _to: Address,
        _amount: TokenAmount,
    ) -> Result<ChainEpoch> {
        tracing::info!("funding on btc with params: {subnet:?}");
        todo!()
    }

    async fn approve_token(
        &self,
        subnet: SubnetID,
        _from: Address,
        _amount: TokenAmount,
    ) -> Result<ChainEpoch> {
        tracing::info!("approving token on btc with params: {subnet:?}");
        todo!()
    }

    async fn fund_with_token(
        &self,
        subnet: SubnetID,
        _from: Address,
        _to: Address,
        _amount: TokenAmount,
    ) -> Result<ChainEpoch> {
        tracing::info!("funding with token on btc with params: {subnet:?}");
        todo!()
    }

    async fn release(
        &self,
        _gateway_addr: Address,
        _from: Address,
        _to: Address,
        _amount: TokenAmount,
    ) -> Result<ChainEpoch> {
        tracing::info!("releasing on btc");
        todo!()
    }

    async fn propagate(
        &self,
        subnet: SubnetID,
        _gateway_addr: Address,
        _from: Address,
        _postbox_msg_key: Vec<u8>,
    ) -> Result<()> {
        tracing::info!("propagating on btc with params: {subnet:?}");
        todo!()
    }

    async fn send_value(&self, _from: Address, _too: Address, _amount: TokenAmount) -> Result<()> {
        tracing::info!("sending value on btc with params");
        todo!()
    }

    async fn wallet_balance(&self, address: &Address) -> Result<TokenAmount> {
        tracing::info!("getting wallet balance on btc with params: {address:?}");
        todo!()
    }

    async fn get_chain_id(&self) -> Result<String> {
        tracing::info!("getting chain id");
        todo!()
    }

    async fn get_commit_sha(&self) -> Result<[u8; 32]> {
        tracing::info!("getting commit sha");
        todo!()
    }

    async fn get_subnet_supply_source(&self, subnet: &SubnetID) -> Result<Asset> {
        tracing::info!("getting subnet supply source on btc with params: {subnet:?}");
        todo!()
    }

    async fn get_genesis_info(&self, subnet: &SubnetID) -> Result<SubnetGenesisInfo> {
        tracing::info!("getting genesis info on btc with params: {subnet:?}");
        Ok(SubnetGenesisInfo {
            active_validators_limit: 10,
            bottom_up_checkpoint_period: 1000,
            genesis_epoch: 100,
            majority_percentage: 66,
            min_collateral: TokenAmount::from_whole(10000),
            validators: vec![],
            genesis_balances: BTreeMap::new(),
            permission_mode: PermissionMode::Collateral,
            supply_source: Asset {
                kind: AssetKind::Native,
                token_address: None,
            },
        })
    }

    async fn add_bootstrap(
        &self,
        subnet: &SubnetID,
        _from: &Address,
        _endpoint: String,
    ) -> Result<()> {
        tracing::info!("adding bootstrap on btc with params: {subnet:?}");
        todo!()
    }

    async fn list_bootstrap_nodes(&self, subnet: &SubnetID) -> Result<Vec<String>> {
        tracing::info!("listing bootstrap nodes on btc with params: {subnet:?}");
        todo!()
    }

    async fn get_validator_info(
        &self,
        subnet: &SubnetID,
        _validator: &Address,
    ) -> Result<ValidatorInfo> {
        tracing::info!("getting validator info on btc with params: {subnet:?}");
        todo!()
    }

    async fn set_federated_power(
        &self,
        _from: &Address,
        subnet: &SubnetID,
        _validators: &[Address],
        _public_keys: &[Vec<u8>],
        _federated_power: &[u128],
    ) -> Result<ChainEpoch> {
        tracing::info!("setting federated power on btc with params: {subnet:?}");
        todo!()
    }

    async fn get_subnet_collateral_source(&self, subnet: &SubnetID) -> Result<Asset> {
        tracing::info!("setting subnet collateral source on btc with params: {subnet:?}");
        todo!()
    }
}

#[async_trait]
impl BottomUpCheckpointRelayer for BtcSubnetManager {
    async fn submit_checkpoint(
        &self,
        _submitter: &Address,
        checkpoint: BottomUpCheckpoint,
        _signatures: Vec<Signature>,
        _signatories: Vec<Address>,
    ) -> anyhow::Result<ChainEpoch> {
        tracing::info!("submitting checkpoint on btc with params: {checkpoint:?}");
        todo!()
    }

    async fn last_bottom_up_checkpoint_height(
        &self,
        subnet_id: &SubnetID,
    ) -> anyhow::Result<ChainEpoch> {
        tracing::info!(
            "getting last bottom up checkpoint height on btc with params: {subnet_id:?}"
        );
        todo!()
    }

    async fn checkpoint_period(&self, subnet_id: &SubnetID) -> anyhow::Result<ChainEpoch> {
        tracing::info!("getting checkpoint period on btc with params: {subnet_id:?}");
        todo!()
    }

    async fn checkpoint_bundle_at(
        &self,
        height: ChainEpoch,
    ) -> Result<Option<BottomUpCheckpointBundle>> {
        tracing::info!("getting checkpoint bundle at height: {height:}");
        todo!()
    }
    /// Queries the signature quorum reached events at target height.
    async fn quorum_reached_events(&self, height: ChainEpoch) -> Result<Vec<QuorumReachedEvent>> {
        tracing::info!("getting quorum reached events at height: {height:}");
        todo!()
    }
    /// Get the current epoch in the current subnet
    async fn current_epoch(&self) -> Result<ChainEpoch> {
        tracing::info!("getting current epoch");
        todo!()
    }
}

#[async_trait]
impl TopDownFinalityQuery for BtcSubnetManager {
    /// Returns the genesis epoch that the subnet is created in parent network
    async fn genesis_epoch(&self, subnet_id: &SubnetID) -> Result<ChainEpoch> {
        tracing::info!("getting genesis epoch for subnet: {subnet_id:}");
        Ok(0)
    }
    /// Returns the chain head height
    async fn chain_head_height(&self) -> Result<ChainEpoch> {
        tracing::info!("getting chain head height");
        todo!()
    }
    /// Returns the list of top down messages
    async fn get_top_down_msgs(
        &self,
        subnet_id: &SubnetID,
        _epoch: ChainEpoch,
    ) -> Result<TopDownQueryPayload<Vec<IpcEnvelope>>> {
        tracing::info!("getting top down messages for subnet: {subnet_id:}");
        todo!()
    }
    /// Get the block hash
    async fn get_block_hash(&self, height: ChainEpoch) -> Result<GetBlockHashResult> {
        tracing::info!("getting block hash for height: {height:}");
        todo!()
    }
    /// Get the validator change set from start to end block.
    async fn get_validator_changeset(
        &self,
        subnet_id: &SubnetID,
        _epoch: ChainEpoch,
    ) -> Result<TopDownQueryPayload<Vec<StakingChangeRequest>>> {
        tracing::info!("getting validator changeset for subnet: {subnet_id:}");
        todo!()
    }
    /// Returns the latest parent finality committed in a child subnet
    async fn latest_parent_finality(&self) -> Result<ChainEpoch> {
        tracing::info!("getting latest parent finality");
        todo!()
    }
}

#[async_trait]
impl ValidatorRewarder for BtcSubnetManager {
    /// Query validator claims, indexed by checkpoint height, to batch claim rewards.
    async fn query_reward_claims(
        &self,
        validator_addr: &Address,
        from_checkpoint: ChainEpoch,
        to_checkpoint: ChainEpoch,
    ) -> Result<Vec<(u64, ValidatorClaim)>> {
        tracing::info!("querying reward claims for={validator_addr:?} from={from_checkpoint:?} to={to_checkpoint:?}");
        todo!()
    }

    /// Query validator rewards in the current subnet, without obtaining proofs.
    async fn query_validator_rewards(
        &self,
        validator_addr: &Address,
        from_checkpoint: ChainEpoch,
        to_checkpoint: ChainEpoch,
    ) -> Result<Vec<(u64, ValidatorData)>> {
        tracing::info!("querying validator rewards for={validator_addr:?} from={from_checkpoint:?} to={to_checkpoint:?}");
        todo!()
    }

    /// Claim validator rewards in a batch for the specified subnet.
    async fn batch_subnet_claim(
        &self,
        _submitter: &Address,
        reward_claim_subnet: &SubnetID,
        reward_origin_subnet: &SubnetID,
        _claims: Vec<(u64, ValidatorClaim)>,
    ) -> Result<()> {
        tracing::info!(
            "batch claim rewards for={reward_claim_subnet:?} from={reward_origin_subnet:?}"
        );
        todo!()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_create_manager() {
        let _ = super::BtcSubnetManager::new();
        assert!(true);
    }
}
