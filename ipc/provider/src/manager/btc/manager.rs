// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use async_trait::async_trait;
use ethers::providers::Authorization;
use ethers::types::H256;
use http::HeaderValue;
use ipc_api::address::IPCAddress;
use ipc_api::evm::payload_to_evm_address;
use ipc_api::subnet::{
    Asset, AssetKind, BtcConstructParams, BtcFundParams, BtcPreFundParams, ConstructParams,
    FundParams, PermissionMode, PreFundParams,
};
use ipc_api::subnet::{BtcJoinParams, JoinParams};
use ipc_api::validator::Validator;
use ipc_api::{ethers_address_to_fil_address, token_amount_from_satoshi};
use reqwest::Client;
use serde_json::{json, Value};

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
use ipc_api::cross::{IpcEnvelope, IpcMsgKind};
use ipc_api::staking::{StakingChangeRequest, ValidatorInfo};
use ipc_api::subnet_id::{SubnetID, BTC_NAMESPACE};

pub struct BtcSubnetManager {
    client: Client,
    rpc_url: String,
}

impl BtcSubnetManager {
    pub fn new(subnet: &Subnet) -> Result<Self> {
        let url = subnet.rpc_http().clone();
        let auth_token = subnet.auth_token();

        match &subnet.config {
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
        Ok(Self {
            client,
            rpc_url: url.to_string(),
        })
    }
}
#[async_trait]
impl SubnetManager for BtcSubnetManager {
    async fn create_subnet(
        &self,
        _from: Option<Address>,
        params: ConstructParams,
    ) -> Result<Address> {
        let params: BtcConstructParams = match params {
            ConstructParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            ConstructParams::Btc(params) => params,
        };
        tracing::info!("creating subnet on btc with params: {params:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "createsubnet",
            "id": 1,
            "params": {
                "min_validator_stake":     params.min_validator_stake,
                "min_validators":          params.min_validators,
                "bottomup_check_period":   params.bottomup_check_period,
                "active_validators_limit": params.active_validators_limit,
                "min_cross_msg_fee":       params.min_cross_msg_fee,
                "whitelist":               params.validator_whitelist,
            }
        });
        tracing::info!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Create Subnet request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        let subnet_id = data
            .get("result")
            .and_then(|r| r.get("subnet_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'result.subnet_id' in JSON-RPC response"))?;

        tracing::info!("New subnet created with ID: {subnet_id}");

        let subnet_id = SubnetID::from_str(subnet_id)?;
        let new_child = subnet_id
            .children_as_ref()
            .last()
            .ok_or_else(|| anyhow!("Newly created subnet must have a child in ID"))?;

        Ok(new_child.clone())
    }

    async fn join_subnet(&self, params: JoinParams) -> Result<ChainEpoch> {
        let params: BtcJoinParams = match params {
            JoinParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            JoinParams::Btc(params) => params,
        };

        tracing::info!("joining subnet on btc with params: {params:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "joinsubnet",
            "id": 1,
            "params": {
                "subnet_id":        params.subnet_id.to_string(),
                "pubkey":           params.sender_public_key,
                "collateral":       params.collateral,
                "ip":               params.ip,
                "backup_address":   params.backup_address,
            }
        });
        tracing::info!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Join Subnet request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        let tx_id = data
            .get("result")
            .and_then(|r| r.get("join_txid"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'result.join_txid' in JSON-RPC response"))?;

        tracing::info!("Joined subnet with txid: {tx_id}");

        // TODO(Orestis). Check what block number to return
        return Ok(0);
    }

    async fn pre_fund(&self, params: PreFundParams) -> Result<()> {
        let params: BtcPreFundParams = match params {
            PreFundParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            PreFundParams::Btc(params) => params,
        };
        tracing::info!("pre-fund subnet on btc with params: {params:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "prefundsubnet",
            "id": 1,
            "params": {
                "subnet_id":        params.subnet_id.to_string(),
                "amount":           params.amount,
                "address":          payload_to_evm_address(params.dst_address.payload())?,
            }
        });
        tracing::info!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Pre-fund request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        Ok(())
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

    async fn fund(&self, params: FundParams) -> Result<ChainEpoch> {
        let params: BtcFundParams = match params {
            FundParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            FundParams::Btc(params) => params,
        };
        tracing::info!("funding on btc with params: {params:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "fundsubnet",
            "id": 1,
            "params": {
                "subnet_id":        params.subnet_id.to_string(),
                "amount":           params.amount,
                "address":          payload_to_evm_address(params.dst_address.payload())?,
            }
        });
        tracing::info!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Fund request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        // TODO(Orestis). Check what block number to return
        Ok(0)
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

    async fn get_genesis_info(&self, subnet_id: &SubnetID) -> Result<SubnetGenesisInfo> {
        tracing::info!("getting genesis info on btc with params: {subnet_id:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "getgenesisinfo",
            "id": 1,
            "params": {
                "subnet_id": subnet_id.to_string(),
            }
        });
        println!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "btc getgenesisinfo request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        let result = data
            .get("result")
            .ok_or_else(|| anyhow!("No result found"))?;

        println!("btc manager get genesis info result: {result:#?}");

        // Check if subnet is bootstrapped
        if result
            .get("bootstrapped")
            .and_then(Value::as_bool)
            .unwrap_or_default()
            == false
        {
            return Err(anyhow!("Subnet not bootstrapped"));
        }

        // Extract create_subnet_msg parameters
        let create_subnet_msg = result
            .get("create_subnet_msg")
            .ok_or_else(|| anyhow!("No create_subnet_msg found"))?;

        let min_validator_stake = create_subnet_msg
            .get("min_validator_stake")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Invalid min_validator_stake"))?;

        let active_validators_limit = create_subnet_msg
            .get("active_validators_limit")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Invalid active_validators_limit"))?;

        // Ensure active_validators_limit fits in u16
        if active_validators_limit > u16::MAX as u64 {
            return Err(anyhow!("active_validators_limit exceeds maximum u16 value"));
        }

        let bottomup_check_period = create_subnet_msg
            .get("bottomup_check_period")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Invalid bottomup_check_period"))?;

        // Extract genesis validators
        let genesis_validators = result
            .get("genesis_validators")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("No genesis_validators found"))?;

        let validators = genesis_validators
            .iter()
            .filter_map(|v| {
                let subnet_address = v.get("subnet_address")?.as_str()?;
                let collateral = v.get("collateral")?.as_u64()?;
                let pubkey = v.get("pubkey")?.as_str()?;
                let pubkey = hex::decode(pubkey).ok()?;

                let addr = ethers::types::Address::from_str(subnet_address).ok()?;
                let addr = ethers_address_to_fil_address(&addr).ok()?;

                // Recreate a compressed pubkey (with even y-coordinate)
                let mut metadata: Vec<u8> = Vec::with_capacity(33);
                metadata.push(0x02);
                metadata.extend(pubkey);

                let weight = token_amount_from_satoshi(collateral);

                let v = Validator {
                    addr,
                    metadata,
                    weight,
                };

                Some(v)
            })
            .collect();

        let min_collateral = token_amount_from_satoshi(min_validator_stake);

        println!("validators = {validators:#?}");

        Ok(SubnetGenesisInfo {
            active_validators_limit: active_validators_limit as u16,
            bottom_up_checkpoint_period: bottomup_check_period,
            genesis_epoch: result
                .get("genesis_block_height")
                // TODO recheck parsing + casting
                .and_then(Value::as_i64)
                .unwrap_or(0),
            // TODO impl majority_percentage
            majority_percentage: 66, // Default value as per the original implementation
            min_collateral,
            validators,
            // TODO impl genesis_balances
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

    async fn list_validators(&self, subnet: &SubnetID) -> Result<Vec<(Address, ValidatorInfo)>> {
        tracing::info!("list validators on btc with params: {subnet:?}");
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
        tracing::info!("getting genesis epoch on btc for: {subnet_id}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "getgenesisinfo",
            "id": 1,
            "params": {
                "subnet_id": subnet_id.to_string(),
            }
        });
        tracing::info!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "btc getgenesisinfo request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        let result = data
            .get("result")
            .ok_or_else(|| anyhow!("No result found"))?;

        dbg!(result);

        result
            .get("genesis_block_height")
            .and_then(Value::as_i64)
            .ok_or(anyhow!("Invalid bootstrap_block_height"))
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
        epoch: ChainEpoch,
    ) -> Result<TopDownQueryPayload<Vec<IpcEnvelope>>> {
        tracing::info!("getting top down messages for subnet: {subnet_id:}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "getrootnetmessages",
            "id": 1,
            "params": {
                "subnet_id":        subnet_id.to_string(),
                "block_height":     epoch,
            }
        });
        tracing::info!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "getrootnetmessages request failed with status: {}",
                resp.status()
            ));
        }

        let data = resp.json::<Value>().await?;

        if let Some(err_obj) = data.get("error") {
            let code = err_obj
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            let error_data = err_obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}, details={}",
                code,
                message,
                error_data
            ));
        }

        let mut messages: Vec<IpcEnvelope> = vec![];
        let mut prev_block_hash: Option<H256> = None;

        let results = data
            .get("result")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Field result not found"))?;
        for result in results {
            // parse kind
            let kind = match result.get("kind").and_then(Value::as_str) {
                Some("fund") => IpcMsgKind::Transfer,
                Some(_) => return Err(anyhow!("Unknown kind in result")),
                None => return Err(anyhow!("Field kind not found in result")),
            };

            // parse subnet_id
            let target_subnet_id = result
                .get("msg")
                .and_then(|msg| msg.get("subnet_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Field subnet_id not found in result"))?;
            let target_subnet_id = SubnetID::from_str(target_subnet_id)?;

            // parse value
            let value = result
                .get("msg")
                .and_then(|msg| msg.get("amount"))
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow!("Field amount not found in result"))?;

            // parse address
            let target_address = result
                .get("msg")
                .and_then(|msg| msg.get("address"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Field address not found in result"))?;
            let address = ethers::types::Address::from_str(target_address)?;
            let target_address = ethers_address_to_fil_address(&address)?;

            // TODO(Orestis): add "from" argument to RPC
            // parse from
            // let from = result
            //     .get("from")
            //     .and_then(Value::as_str)
            //     .ok_or_else(|| anyhow!("No from address found in result"))?;
            // let from = ethers::types::Address::from_str(from)?;
            // let from = ethers_address_to_fil_address(&from)?;

            // parse block_hash
            let block_hash = result
                .get("block_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Field block_hash not found in result"))?;
            let block_hash = H256::from_str(block_hash)?;
            if prev_block_hash.is_some() && prev_block_hash != Some(block_hash) {
                return Err(anyhow!("Block hash mismatch in result"));
            }
            prev_block_hash = Some(block_hash);

            // parse nonce
            let nonce = result
                .get("nonce")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("Field nonce not found in result"))?;

            let envelope = IpcEnvelope {
                kind,
                to: IPCAddress::new(&target_subnet_id, &target_address)?,
                value: TokenAmount::from_whole(value),
                // TODO(Orestis): The following should only work for fund/prefund messages.
                // Change when we implement transders.
                from: IPCAddress::new(
                    &SubnetID::new_root(subnet_id.root_id()),
                    &Address::new_delegated(BTC_NAMESPACE, &vec![0; 20])?,
                )?,
                message: vec![],
                nonce,
            };
            messages.push(envelope);
        }

        let block_hash = match prev_block_hash {
            Some(h) => h.0.to_vec(),
            None => self.get_block_hash(epoch).await?.block_hash,
        };

        Ok(TopDownQueryPayload {
            value: messages,
            block_hash,
        })
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
        // let _ = super::BtcSubnetManager::new();
        assert!(true);
    }
}
