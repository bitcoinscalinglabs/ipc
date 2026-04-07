// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use ethers::abi::ethereum_types;
use ethers::providers::Authorization;
use ethers::types::{Address as EthAddress, H256};
use http::HeaderValue;
use ipc_api::address::IPCAddress;
use ipc_api::checkpoint::{
    BitcoinCheckpointSignature, BitcoinCheckpointSignatureQuorum, BitcoinHandoverSignature,
    BitcoinSignature, BitcoinTx, UnsignedPsbt,
};
use ipc_api::evm::payload_to_evm_address;
use ipc_api::subnet::{
    Asset, AssetKind, BtcConstructParams, BtcFundParams, BtcKillSubnetParams, ConstructParams,
    FundParams, KillSubnetParams, PermissionMode, PreFundParams, RewardParams,
};
use ipc_api::subnet::{BtcJoinParams, JoinParams};
use ipc_api::validator::Validator;
use ipc_api::{ethers_address_to_fil_address, token_amount_from_satoshi, token_amount_to_satoshi};
use ipc_wallet::{EthKeyAddress, EvmKeyStore, PersistentKeyStore};
use libsecp256k1::SecretKey;
use reqwest::Client;
use serde_json::{json, Value};

use crate::config::subnet::SubnetConfig;
use crate::config::Subnet;
use crate::lotus::message::ipc::SubnetInfo;
use crate::manager::subnet::{
    BottomUpCheckpointRelayer, GetBlockHashResult, GetRewardedCollateralsResponse,
    SubnetGenesisInfo, TopDownFinalityQuery, TopDownQueryPayload, ValidatorRewarder,
};

use crate::manager::SubnetManager;
use anyhow::Result;

use anyhow::anyhow;
use num_traits::Zero;
use fvm_shared::clock::ChainEpoch;
use fvm_shared::{address::Address, econ::TokenAmount};
use ipc_actors_abis::subnet_actor_activity_facet::ValidatorClaim;
use ipc_api::checkpoint::{
    consensus::ValidatorData, BottomUpCheckpoint, BottomUpCheckpointBundle, QuorumReachedEvent,
    Signature,
};
use ipc_api::cross::{IpcEnvelope, IpcMsgKind};
use ipc_api::staking::{
    StakingChange, StakingChangeRequest, StakingOperation, ValidatorInfo, ValidatorStakingInfo,
};
use ipc_api::subnet_id::{NetworkType, SubnetID, BTC_NAMESPACE};

#[derive(Clone)]
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

    async fn get_block_hash_inner(&self, height: ChainEpoch) -> Result<H256> {
        tracing::info!("getting block hash for height: {height:}");
        let body = json!({
            "jsonrpc": "2.0",
            "method": "getblockhash",
            "id": 1,
            "params": {
                "height": height,
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
                "getblockhash request failed with status: {}",
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

        let block_hash = data
            .get("result")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Field result not found"))?;

        let block_hash = H256::from_str(block_hash)?;
        Ok(block_hash)
    }

    /// Parse reward params from a JSON value.
    /// Returns Ok(None) if "reward" is not found.
    /// Returns Ok(Some(...)) if "reward" is found and all required params parse.
    /// Returns Err if "reward" is found but any required param is missing or invalid.
    fn _parse_reward_params(value: &Value) -> Result<Option<RewardParams>, anyhow::Error> {
        let reward = match value.get("reward") {
            Some(r) => r,
            None => return Ok(None),
        };

        let activation_height = reward
            .get("activation_height")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("reward.activation_height missing or invalid"))?;
        let snapshot_length = reward
            .get("snapshot_length")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("reward.snapshot_length missing or invalid"))?;

        Ok(Some(RewardParams {
            activation_height,
            snapshot_length,
        }))
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
                "min_validator_stake":     token_amount_to_satoshi(params.min_validator_stake)?,
                "min_validators":          params.min_validators,
                "bottomup_check_period":   params.bottomup_check_period,
                "active_validators_limit": params.active_validators_limit,
                "min_cross_msg_fee":       token_amount_to_satoshi(params.min_cross_msg_fee)?,
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
                "pubkey":           params.public_key,
                "collateral":       token_amount_to_satoshi(params.collateral)?,
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

        let current_height = self.chain_head_height().await?;
        Ok(current_height)
    }

    async fn pre_fund(&self, params: PreFundParams) -> Result<()> {
        tracing::info!("pre-fund subnet on btc with params: {params:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "prefundsubnet",
            "id": 1,
            "params": {
                "subnet_id":        params.subnet_id.to_string(),
                "amount":           token_amount_to_satoshi(params.amount)?,
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

    async fn stake(&self, params: JoinParams) -> Result<()> {
        let params: BtcJoinParams = match params {
            JoinParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            JoinParams::Btc(params) => params,
        };

        tracing::info!("staking subnet on btc with params: {params:?}");
        let body = json!({
            "jsonrpc": "2.0",
            "method": "stakecollateral",
            "id": 1,
            "params": {
                "subnet_id":     params.subnet_id.to_string(),
                "amount":        token_amount_to_satoshi(params.collateral)?,
                "pubkey":        params.public_key,
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
                "stakecollateral request failed with status: {}",
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

        tracing::info!("stakecollateral request successful");
        Ok(())
    }

    async fn unstake(&self, params: JoinParams) -> Result<()> {
        let params: BtcJoinParams = match params {
            JoinParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            JoinParams::Btc(params) => params,
        };

        tracing::info!("unstaking subnet on btc with params: {params:?}");

        // We don't need to send the public key because the RPC method
        // will use the one from the wallet.
        let body = json!({
            "jsonrpc": "2.0",
            "method": "unstakecollateral",
            "id": 1,
            "params": {
                "subnet_id":     params.subnet_id.to_string(),
                "amount":        token_amount_to_satoshi(params.collateral)?,
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
                "unstakecollateral request failed with status: {}",
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

        let current_height = self.chain_head_height().await?;
        tracing::info!("unstakecollateral submitted at height {current_height}");

        Ok(())
    }

    async fn leave_subnet(&self, subnet: SubnetID, _from: Address) -> Result<()> {
        tracing::info!("leaving subnet on btc with params: {subnet:?}");
        todo!()
    }

    async fn kill_subnet(&self, params: KillSubnetParams) -> Result<()> {
        let params: BtcKillSubnetParams = match params {
            KillSubnetParams::Eth(_) => return Err(anyhow!("Unsupported subnet configuration")),
            KillSubnetParams::Btc(params) => params,
        };

        tracing::info!("killing subnet on btc with params: {:?}", params.subnet_id);

        // We don't need to send the public key because the RPC method
        // will use the one from the wallet, same as in unstakecollateral.
        let body = json!({
            "jsonrpc": "2.0",
            "method": "killsubnet",
            "id": 1,
            "params": {
                "subnet_id":     params.subnet_id.to_string(),
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
                "killsubnet request failed with status: {}",
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

        let current_height = self.chain_head_height().await?;
        tracing::info!("killsubnet submitted at height {current_height}");

        Ok(())
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
                "amount":           token_amount_to_satoshi(params.amount)?,
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

        let current_height = self.chain_head_height().await?;
        Ok(current_height)
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
        _gateway_addr: Option<Address>,
        _from: Address,
        _to: Address,
        _amount: TokenAmount,
    ) -> Result<ChainEpoch> {
        tracing::info!("releasing on btc");
        unimplemented!(
            "release on bitcoin is not supported, it is not meant to be used as a child subnet"
        )
    }

    async fn transfer(
        &self,
        _gateway_addr: Option<Address>,
        _from: Address,
        _to: Address,
        _amount: TokenAmount,
        _dst_subnet: SubnetID,
    ) -> Result<ChainEpoch> {
        unimplemented!(
            "transfer on bitcoin is not supported, it is not meant to be used as a child subnet"
        );
    }

    async fn transfer_erc_token(
        &self,
        _gateway_addr: Option<Address>,
        _from: Address,
        _to: Address,
        _local_token: Address,
        _amount: TokenAmount,
        _dst_subnet: SubnetID,
    ) -> Result<ChainEpoch> {
        unimplemented!(
            "transfer_erc_token on bitcoin is not supported, it is not meant to be used as a child subnet"
        )
    }

    async fn get_wrapped_token(
        &self,
        _gateway_addr: Option<Address>,
        _home_subnet: SubnetID,
        _home_token: Address,
    ) -> Result<Address> {
        Err(anyhow!(
            "get_wrapped_token is only supported on EVM child subnets, not the Bitcoin parent"
        ))
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
        unimplemented!("getting balances of addresses on bitcoin is not supported")
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

        tracing::debug!("btc manager get genesis info result: {result:#?}");

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

        // let reward = BtcSubnetManager::_parse_reward_params(result)?;

        // TODO: Reward config should be read from the genesis, but it is not written
        // on the parent yet. Hardcoded for now.
        // Enable reward params only when EMISSION_CHAIN_FEATURES=true
        let reward = env::var("EMISSION_CHAIN_FEATURES")
            .map(|v| v == "true")
            .unwrap_or(false)
            .then_some(RewardParams {
                activation_height: 10,
                snapshot_length: 10,
            });

        if reward.is_some() {
            tracing::info!("emission chain reward params enabled");
        } else {
            tracing::info!("emission chain reward params disabled (set EMISSION_CHAIN_FEATURES=true to enable)");
        };

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
            reward,
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

        let body = json!({
            "jsonrpc": "2.0",
            "method": "getsubnet",
            "id": 1,
            "params": {
                "subnet_id": subnet.to_string(),
            }
        });

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "btc getsubnet request failed with status: {}",
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

        let mut validators = Vec::new();

        // parse current committee from response
        let current_committee = result
            .get("committee")
            .and_then(|v| v.get("validators"))
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Field committee.validators not found in the response"))?;

        validators.extend(get_validators_from_response(current_committee, true)?);

        // parse waiting committee from response, if it exists
        match result
            .get("waiting_committee")
            .and_then(|v| v.get("validators"))
            .and_then(Value::as_array)
        {
            Some(waiting_committee) => {
                validators.extend(get_validators_from_response(waiting_committee, false)?);
            }
            None => {
                tracing::info!("no waiting committee found in response");
            }
        }

        Ok(validators)
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

    /// This function asks the parent subnet (bitcoin) to generate the required transaction for the given `checkpoint` and `subnet_id` and sign it.
    async fn get_checkpoint_transaction(
        &self,
        subnet_id: &SubnetID,
        checkpoint: BottomUpCheckpoint,
    ) -> Result<BitcoinCheckpointSignature> {
        tracing::debug!("Creating bitcoin signatures for checkpoint: {checkpoint:?}");

        // collect all withdrawals, transfers, and ERC token data from the checkpoint msgs
        let mut releases = Vec::new();
        let mut transfers = Vec::new();
        let mut token_registrations = Vec::new();
        let mut token_supply_adjustments = Vec::new();
        let mut token_transfers = Vec::new();

        for msg in checkpoint.msgs {
            match msg.kind {
                ipc_api::cross::IpcMsgKind::Transfer => {
                    let mut destination_subnet = msg.to.subnet()?;
                    if destination_subnet.is_root() {
                        // Release
                        releases.push(json!({
                            "amount": ipc_api::token_amount_to_satoshi(msg.value)?,
                            "address": ipc_api::address::bitcoin_address_from_fvm_address(&msg.to.raw_addr()?)?,
                        }));
                    } else {
                        //TODO(btc): The following is because the contracts do not return the correct network type
                        destination_subnet.root_network_type = NetworkType::Btc;
                        transfers.push(json!({
                            "amount": ipc_api::token_amount_to_satoshi(msg.value)?,
                            "destination_subnet_id": destination_subnet.to_string(),
                            "subnet_user_address": ipc_api::address::to_eth_address(&msg.to.raw_addr()?)?
                        }));
                    }
                }
                ipc_api::cross::IpcMsgKind::Call => {
                    tracing::info!("ignoring call messages: unsupported on bitcoin")
                }
                //TODO(btc): add receipt handling
                ipc_api::cross::IpcMsgKind::Receipt => {}
                ipc_api::cross::IpcMsgKind::ErcTransfer => {
                    let (home_subnet, home_token, amount) =
                        abi_decode_erc_transfer_msg(&msg.message)?;
                    let mut destination_subnet = msg.to.subnet()?;
                    destination_subnet.root_network_type = NetworkType::Btc;
                    let mut home_subnet_btc = home_subnet;
                    home_subnet_btc.root_network_type = NetworkType::Btc;
                    let recipient = ipc_api::address::to_eth_address(&msg.to.raw_addr()?)?
                        .ok_or_else(|| anyhow!("ErcTransfer recipient must be an eth address"))?;
                    token_transfers.push(json!({
                        "home_subnet_id": home_subnet_btc.to_string(),
                        "home_token_address": format!("{:?}", home_token),
                        "amount": amount.to_string(),
                        "destination_subnet_id": destination_subnet.to_string(),
                        "recipient": format!("{:?}", recipient),
                    }));
                }
                ipc_api::cross::IpcMsgKind::ErcRegistration => {
                    let (_home_subnet, home_token, name, symbol, decimals, initial_supply) =
                        abi_decode_erc_registration_msg(&msg.message)?;
                    // home_subnet_id is NOT in the bitcoin-ipc struct —
                    // derived from the checkpoint's OP_RETURN subnet ID.
                    token_registrations.push(json!({
                        "home_token_address": format!("{:?}", home_token),
                        "name": name,
                        "symbol": symbol,
                        "decimals": decimals,
                        "initial_supply": initial_supply.to_string(),
                    }));
                }
                ipc_api::cross::IpcMsgKind::ErcSupplyDelta => {
                    let (home_token, delta) = abi_decode_erc_supply_delta_msg(&msg.message)?;
                    token_supply_adjustments.push(json!({
                        "home_token_address": format!("{:?}", home_token),
                        "delta": delta.to_string(),
                    }));
                }
            }
        }

        let body = json!({
            "jsonrpc": "2.0",
            "method": "gencheckpointpsbt",
            "id": 1,
            "params": {
                // TODO(btc): should we get the subnet_id from the checkpoint?
                "subnet_id":            subnet_id.to_string(),
                "checkpoint_hash":      hex::encode(checkpoint.block_hash),
                "checkpoint_height":    checkpoint.block_height,
                "next_committee_configuration_number": checkpoint.next_configuration_number,
                "withdrawals":          releases,
                "transfers":            transfers,
                "token_registrations":  token_registrations,
                "token_supply_adjustments": token_supply_adjustments,
                "token_transfers":      token_transfers,
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
                "gencheckpointpsbt request failed with status: {}",
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

        let unsigned_psbt = data
            .get("result")
            .and_then(|r| r.get("unsigned_psbt_base64"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'result.unsigned_psbt_base64' in JSON-RPC response"))?
            .to_string();

        // The RPC call returns one signature for each input in the PSBT, hex encoded.
        // We decode each signature and flatten the result into a single vector of bytes,
        // which we then store in the `PsbtSignature` struct.
        // When these signatures are submitted to the `finalizecheckpointpsbt` RPC call,
        // they must be split again (see `split_signatures_and_zip_with_signatories`).
        let signature = data
            .get("result")
            .and_then(|r| r.get("psbt_inputs_signatures"))
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Missing 'result.psbt_inputs_signatures' in JSON-RPC response for checkpoint"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| {
                        anyhow!(
                            "Invalid entry in 'result.psbt_inputs_signatures' in JSON-RPC response for checkpoint"
                        )
                    })
                    .and_then(|s| {
                        hex::decode(s)
                            .map_err(|e| anyhow!("decoding bitcoin signature failed: {}", e))
                    })
            })
            .collect::<Result<Vec<Vec<u8>>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();

        let transfer_tx = match data
            .get("result")
            .and_then(|r| r.get("batch_transfer_tx_hex"))
        {
            Some(v) if v.is_null() => "".to_string(),
            Some(v) => v
                .as_str()
                .ok_or_else(|| anyhow!("'batch_transfer_tx_hex' is not a string"))?
                .to_string(),
            None => "".to_string(),
        };

        tracing::info!("BtcSubnetManager obtained checkpoint PSBT and signatures.");

        Ok(BitcoinCheckpointSignature {
            unsigned_psbt: UnsignedPsbt(unsigned_psbt),
            signature,
            transfer_tx: BitcoinTx(transfer_tx),
        })
    }

    /// This function asks the parent subnet (bitcoin) to generate the required transaction to perform the initial bootstrap handover.
    /// The reason why this bootstrap handover is related to the way a subnet is created when the parent is bitcoin.
    /// The RPC endpoint we use (`genbootstraphandover`) returns a PSBT and a number of signatures, same as the `gencheckpointpsbt` endpoint.
    async fn get_bootstrap_handover_transaction(
        &self,
        subnet_id: &SubnetID,
    ) -> Result<BitcoinHandoverSignature> {
        tracing::info!("Creating bitcoin signatures for bootstrap handover: {subnet_id:?}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "genbootstraphandover",
            "id": 1,
            "params": {
                "subnet_id": subnet_id.to_string(),
            }
        });

        tracing::debug!("Request body: {body:?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "genbootstraphandover request failed with status: {}",
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

        let unsigned_psbt = data
            .get("result")
            .and_then(|r| r.get("unsigned_psbt_base64"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'result.unsigned_psbt_base64' in JSON-RPC response"))?
            .to_string();

        // The RPC call returns one signature for each input in the PSBT, hex encoded.
        // We decode each signature and flatten the result into a single vector of bytes,
        // which we then store in the `HandoverPsbtSignature` struct.
        // When these signatures are submitted to the `finalizebootstraphandover` RPC call,
        // they must be split again (see `split_signatures_and_zip_with_signatories`).
        let signature = data
        .get("result")
        .and_then(|r| r.get("psbt_inputs_signatures"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Missing 'result.psbt_inputs_signatures' in JSON-RPC response for bootstrap handover"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| {
                    anyhow!(
                        "Invalid entry in 'result.psbt_inputs_signatures' in JSON-RPC response for bootstrap handover"
                    )
                })
                .and_then(|s| {
                    hex::decode(s)
                        .map_err(|e| anyhow!("decoding bitcoin signature failed: {}", e))
                })
        })
        .collect::<Result<Vec<Vec<u8>>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<u8>>();

        tracing::info!("BtcSubnetManager obtained bootstrap-handover PSBT and signatures.");

        Ok(BitcoinHandoverSignature {
            unsigned_psbt: UnsignedPsbt(unsigned_psbt),
            signature,
        })
    }

    async fn get_rewarded_collaterals(
        &self,
        snapshot_number: u64,
    ) -> Result<GetRewardedCollateralsResponse> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "getrewardedcollaterals",
            "id": 1,
            "params": {
                "snapshot": snapshot_number,
            }
        });

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "getrewardedcollaterals request failed with status: {}",
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
            return Err(anyhow!(
                "JSON-RPC error: code={}, message={}",
                code,
                message
            ));
        }

        let result = data
            .get("result")
            .ok_or_else(|| anyhow!("Field result not found"))?;

        let collaterals_arr = result
            .get("collaterals")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("collaterals array not found"))?;

        let collaterals = collaterals_arr
            .iter()
            .map(|v| {
                let arr = v
                    .as_array()
                    .ok_or_else(|| anyhow!("invalid collateral entry"))?;
                let addr_str = arr
                    .get(0)
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("address not found"))?;
                let amount = arr
                    .get(1)
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("amount not found"))?;
                let addr = EthAddress::from_str(addr_str)
                    .map_err(|e| anyhow!("invalid address: {}", e))?;
                Ok((addr, amount))
            })
            .collect::<Result<Vec<_>>>()?;

        let total_rewarded_collateral = result
            .get("total_rewarded_collateral")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("total_rewarded_collateral not found"))?;

        let snapshot = result
            .get("snapshot")
            .and_then(Value::as_u64)
            .unwrap_or(snapshot_number);

        Ok(GetRewardedCollateralsResponse {
            collaterals,
            total_rewarded_collateral,
            snapshot,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait]
impl BottomUpCheckpointRelayer for BtcSubnetManager {
    async fn submit_checkpoint(
        &self,
        keystore: Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
        _submitter: &Option<Address>,
        checkpoint: BottomUpCheckpoint,
        _signatures: Vec<Signature>,
        _signatories: Vec<Address>,
        bitcoin_signatures: Option<BitcoinCheckpointSignatureQuorum>,
    ) -> anyhow::Result<ChainEpoch> {
        tracing::trace!("submitting checkpoint on btc with params: {checkpoint:?}");
        let bitcoin_signatures = match bitcoin_signatures {
            Some(signatures) => signatures,
            None => {
                return Err(anyhow!(
                    "Submitting checkpoint on bitcoin requires bitcoin_signatures"
                ));
            }
        };

        let signatures_json = split_signatures_and_zip_with_signatories(
            bitcoin_signatures.signatures,
            bitcoin_signatures.signatories,
            &keystore,
        )?;

        let body = json!({
            "jsonrpc": "2.0",
            "method": "finalizecheckpointpsbt",
            "id": 1,
            "params": {
                "subnet_id":            checkpoint.subnet_id.to_string(),
                "unsigned_psbt_base64": bitcoin_signatures.unsigned_psbt.0,
                "signatures":           signatures_json,
                "batch_transfer_tx_hex":bitcoin_signatures.transfer_tx.0,
            }
        });

        tracing::debug!("Request body: {body:#?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "finalizecheckpointpsbt request failed with status: {}",
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

        let current_height = self.chain_head_height().await?;

        tracing::info!("checkpoint submitted on btc at height: {current_height:}");
        Ok(current_height)
    }

    async fn last_bottom_up_checkpoint_height(
        &self,
        subnet_id: &SubnetID,
    ) -> anyhow::Result<ChainEpoch> {
        tracing::info!(
            "getting last bottom up checkpoint height on btc with params: {subnet_id:?}"
        );

        let body = json!({
            "jsonrpc": "2.0",
            "method": "getsubnetcheckpoint",
            "id": 1,
            "params": {
                "subnet_id": subnet_id.to_string(),
            }
        });

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "btc getlastcheckpointheight request failed with status: {}",
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

        let height = match data.get("result") {
            Some(v) if v.is_null() => 0,
            Some(v) => v
                .get("checkpoint_height")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    anyhow!("No checkpoint_height found in getsubnetcheckpoint response")
                })?,
            None => return Err(anyhow!("No result found")),
        };

        Ok(height as ChainEpoch)
    }

    async fn checkpoint_period(&self, subnet_id: &SubnetID) -> anyhow::Result<ChainEpoch> {
        tracing::info!("getting checkpoint period on btc with params: {subnet_id:?}");
        let genesis_info = self.get_genesis_info(subnet_id).await?;
        Ok(genesis_info.bottom_up_checkpoint_period as ChainEpoch)
    }

    async fn checkpoint_bundle_at(
        &self,
        height: ChainEpoch,
    ) -> Result<Option<BottomUpCheckpointBundle>> {
        tracing::info!("getting checkpoint bundle on bitcoin at height: {height:}");
        anyhow::bail!("not supported on btc, it is not meant to be a child subnet")
    }
    /// Queries the signature quorum reached events at target height.
    async fn quorum_reached_events(&self, _height: ChainEpoch) -> Result<Vec<QuorumReachedEvent>> {
        tracing::info!("getting quorum reached events on bitcoin at height: {_height:}");
        anyhow::bail!("not supported on btc, it is not meant to be a child subnet")
    }
    /// Get the current epoch in the current subnet
    async fn current_epoch(&self) -> Result<ChainEpoch> {
        tracing::info!("getting current epoch on bitcoin");
        anyhow::bail!("not supported on btc, it is not meant to be a child subnet")
    }

    async fn submit_bootstrap_handover(
        &self,
        subnet_id: &SubnetID,
        keystore: Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
        handover_signatures: ipc_api::checkpoint::BitcoinHandoverSignatureQuorum,
    ) -> Result<ChainEpoch> {
        tracing::info!("submitting bootstrap handover transaction on btc");

        let signatures_json = split_signatures_and_zip_with_signatories(
            handover_signatures.signatures,
            handover_signatures.signatories,
            &keystore,
        )?;

        let body = json!({
            "jsonrpc": "2.0",
            "method": "finalizebootstraphandover",
            "id": 1,
            "params": {
                "subnet_id":            subnet_id.to_string(),
                "unsigned_psbt_base64": handover_signatures.unsigned_psbt.0,
                "signatures":           signatures_json,
            }
        });

        tracing::debug!("Request body: {body:#?}");

        let resp = self
            .client
            .post(self.rpc_url.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "finalizebootstraphandover request failed with status: {}",
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

        let current_height = self.chain_head_height().await?;

        tracing::info!("bootstrap handover submitted on btc at height: {current_height:}");
        Ok(current_height)
    }

    async fn get_bootstrap_handover_signatures(
        &self,
        _height: ChainEpoch,
    ) -> Result<ipc_api::checkpoint::BitcoinHandoverSignatureQuorum> {
        anyhow::bail!("not supported on btc, it is not meant to be a child subnet")
    }
}

// The `signatures` contains, for each signatory, multiple concatenated signatures from that signatory.
// The `signatories` contains the ethereum addresses of the signatories.
// (see the `get_checkpoint_transaction` for how this is created, we concatenate the signatures of each signatory).
//
// We need to split them again here, and then zip them with the XOnly public keys of the signatories,
// because that's how the RPC methods `finalizebootstraphandover` and `finalizecheckpointpsbt` expect them.
//
// Example of what this code produces:
// signatories_xonly_pubkey = vec![
//     "5f0dfed3a527ac740c7d4a594cd3aa1059a936187399fc49e3fc6ea6ae177268",
//     "67308c2f3915f4c36135f267ed709418c2880025d669e4ada7a206842d53c146",
// ];
//
// split_signatures = vec![
//     vec![
//         "f245679ccda14b190213d4115ba8c10d484d5f0d1e0a37a493bd88f9fce3f05b5514debb23e83c693a1fdeb0622970fc3691dbbdee87b7430af41acdca58f44c",
//         "ce02c09922cde3a671337baa86028a094d456a523286dccfcec015eff78fcf8b666db66c7368fe93f5d75fabf64451b2469931aab4386653194572261586e6dd",
//     ],
//     vec![
//         "41592da0f93d2483ca227a75e36c8898d7097c61f56f2770ca8efe260b3d38011353edd64833cd6b5cc1b6e7c2be0b3a55fc55d5aa9cf34bfd4fa57d4ea551bf",
//         "3e5f2635a43eab0560a038e300a5e1a4fb11cdfe0da4bf9842ca292db3538ff382d55ff05c2a32c412d558ff4333d0a0d16016b97b58971e16a93f43da01fe89",
//     ],
// ];
//
// "signatures_json": [
//     [
//         "5f0dfed3a527ac740c7d4a594cd3aa1059a936187399fc49e3fc6ea6ae177268",
//         [
//             "f245679ccda14b190213d4115ba8c10d484d5f0d1e0a37a493bd88f9fce3f05b5514debb23e83c693a1fdeb0622970fc3691dbbdee87b7430af41acdca58f44c",
//             "ce02c09922cde3a671337baa86028a094d456a523286dccfcec015eff78fcf8b666db66c7368fe93f5d75fabf64451b2469931aab4386653194572261586e6dd"
//         ]
//     ],
//     [
//         "67308c2f3915f4c36135f267ed709418c2880025d669e4ada7a206842d53c146",
//         [
//             "ce02c09922cde3a671337baa86028a094d456a523286dccfcec015eff78fcf8b666db66c7368fe93f5d75fabf64451b2469931aab4386653194572261586e6dd",
//             "3e5f2635a43eab0560a038e300a5e1a4fb11cdfe0da4bf9842ca292db3538ff382d55ff05c2a32c412d558ff4333d0a0d16016b97b58971e16a93f43da01fe89"
//         ]
//     ]
// ]
fn split_signatures_and_zip_with_signatories(
    signatures: Vec<BitcoinSignature>,
    signatories: Vec<ethers::types::Address>,
    keystore: &Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
) -> Result<Vec<serde_json::Value>> {
    // Split the signatures of each signatory into chunks of 64 bytes (see info above function for more details)
    let mut split_signatures = Vec::new();
    for concatenated_signatures_of_signatory in signatures.iter() {
        let split_signatures_of_signatory = concatenated_signatures_of_signatory
            .chunks(libsecp256k1::util::SIGNATURE_SIZE)
            .map(|chunk| hex::encode(chunk.to_vec()))
            .collect::<Vec<_>>();
        split_signatures.push(split_signatures_of_signatory);
    }
    // Replace the IPC addresses with the XOnlyPubKey, as the RPC expects the XOnlyPubKey
    let signatories_xonly_pubkey = signatories
        .iter()
        .map(|&s| -> Result<String> {
            let sk = keystore
                .read()
                .map_err(|e| anyhow!("failed to read evm wallet: {e}"))?
                .get(&s.into())
                .map_err(|e| anyhow!("failed to get key from evm wallet: {e}"))?
                .ok_or_else(|| anyhow!("key {} does not exist in evm wallet", s))?
                .private_key()
                .to_vec();
            let x_only_pub_key = hex::encode(
                ipc_wallet::get_xonly_public_key_serialized(&SecretKey::parse_slice(&sk)?)?
                    .to_vec(),
            );
            Ok(x_only_pub_key)
        })
        .collect::<Result<Vec<String>>>()?;

    // Construct the JSON array
    let mut signatures_json = Vec::new();
    for (signatory, signatures) in signatories_xonly_pubkey.iter().zip(split_signatures.iter()) {
        let json_entry = json!([signatory, signatures]);
        signatures_json.push(json_entry);
    }
    Ok(signatures_json)
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

        tracing::debug!("btc manager get genesis epoch result: {result:#?}");

        result
            .get("genesis_block_height")
            .and_then(Value::as_i64)
            .ok_or(anyhow!("Invalid bootstrap_block_height"))
    }
    /// Returns the chain head height
    async fn chain_head_height(&self) -> Result<ChainEpoch> {
        tracing::info!("getting chain head height on btc");
        let body = json!({
            "jsonrpc": "2.0",
            "method": "getconfirmedcount",
            "id": 1,
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
                "getconfirmedcount request failed with status: {}",
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

        let height = data
            .get("result")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("Field result not found"))?;

        Ok(height as ChainEpoch)
    }

    /// Returns the list of top down messages
    async fn get_top_down_msgs(
        &self,
        subnet_id: &SubnetID,
        epoch: ChainEpoch,
    ) -> Result<TopDownQueryPayload<Vec<IpcEnvelope>>> {
        tracing::info!("getting top down messages for subnet: {subnet_id:} at height: {epoch:}");

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
            // parse block_hash (common to all kinds)
            let block_hash = result
                .get("block_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Field block_hash not found in result"))?;
            let block_hash = H256::from_str(block_hash)?;
            if prev_block_hash.is_some() && prev_block_hash != Some(block_hash) {
                return Err(anyhow!("Block hash mismatch in result"));
            }
            prev_block_hash = Some(block_hash);

            // parse nonce (common to all kinds)
            let nonce = result
                .get("nonce")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("Field nonce not found in result"))?;

            let btc_root_addr =
                Address::new_delegated(BTC_NAMESPACE, &vec![0; 20])?;
            let from_root =
                IPCAddress::new(&SubnetID::new_root(subnet_id.root_id()), &btc_root_addr)?;

            let envelope = match result.get("kind").and_then(Value::as_str) {
                Some("fund") => {
                    let msg = result
                        .get("msg")
                        .ok_or_else(|| anyhow!("Field msg not found in fund result"))?;

                    let target_subnet_id = msg
                        .get("subnet_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field subnet_id not found in fund result"))?;
                    let target_subnet_id = SubnetID::from_str(target_subnet_id)?;

                    let value = msg
                        .get("amount")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| anyhow!("Field amount not found in fund result"))?;

                    let target_address = msg
                        .get("address")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field address not found in fund result"))?;
                    let eth_addr = ethers::types::Address::from_str(target_address)?;
                    let target_address = ethers_address_to_fil_address(&eth_addr)?;

                    IpcEnvelope {
                        kind: IpcMsgKind::Transfer,
                        to: IPCAddress::new(&target_subnet_id, &target_address)?,
                        value: token_amount_from_satoshi(value),
                        from: from_root,
                        message: vec![],
                        nonce,
                    }
                }
                Some("erc_registration") => {
                    // ErcRegistration JSON structure (from bitcoin-ipc RootnetMessage):
                    // { "kind": "erc_registration", "home_subnet_id": "...",
                    //   "registration": { "home_token_address": "0x...", "name": "...", "symbol": "...", "decimals": N },
                    //   "block_height": N, "block_hash": "...", "nonce": N, "txid": "..." }
                    // Note: no "msg" wrapper and no "subnet_id" — the destination is the subnet we queried for.

                    let home_subnet_id = result
                        .get("home_subnet_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field home_subnet_id not found in erc_registration result"))?;
                    let home_subnet_id = SubnetID::from_str(home_subnet_id)?;

                    let registration = result
                        .get("registration")
                        .ok_or_else(|| anyhow!("Field registration not found in erc_registration result"))?;

                    let home_token = registration
                        .get("home_token_address")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field home_token_address not found in registration"))?;
                    let home_token = ethers::types::Address::from_str(home_token)?;

                    let name = registration
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field name not found in registration"))?;

                    let symbol = registration
                        .get("symbol")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field symbol not found in registration"))?;

                    let decimals = registration
                        .get("decimals")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow!("Field decimals not found in registration"))?
                        as u8;

                    let initial_supply = registration
                        .get("initial_supply")
                        .and_then(Value::as_str)
                        .unwrap_or("0");
                    let initial_supply =
                        ethers::types::U256::from_dec_str(initial_supply).unwrap_or_default();

                    let message = abi_encode_erc_registration_msg(
                        &home_subnet_id,
                        home_token,
                        name,
                        symbol,
                        decimals,
                        initial_supply,
                    )?;

                    // Destination is the subnet we queried for (subnet_id parameter)
                    IpcEnvelope {
                        kind: IpcMsgKind::ErcRegistration,
                        to: IPCAddress::new(subnet_id, &btc_root_addr)?,
                        from: from_root,
                        value: TokenAmount::zero(),
                        message,
                        nonce,
                    }
                }
                Some("erc_transfer") => {
                    // ErcTransfer JSON structure (from bitcoin-ipc RootnetMessage):
                    // { "kind": "erc_transfer",
                    //   "msg": { "home_subnet_id": "...", "home_token_address": "0x...",
                    //            "amount": [0,0,...,3,232], "destination_subnet_id": "...", "recipient": "0x..." },
                    //   "block_height": N, "block_hash": "...", "nonce": N, "txid": "..." }
                    let msg = result
                        .get("msg")
                        .ok_or_else(|| anyhow!("Field msg not found in erc_transfer result"))?;

                    let home_subnet_id = msg
                        .get("home_subnet_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field home_subnet_id not found in erc_transfer msg"))?;
                    let home_subnet_id = SubnetID::from_str(home_subnet_id)?;

                    let home_token = msg
                        .get("home_token_address")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field home_token_address not found in erc_transfer msg"))?;
                    let home_token = ethers::types::Address::from_str(home_token)?;

                    // amount is serialized as a hex string by alloy_primitives::U256 (e.g. "0x3e8")
                    let amount_str = msg
                        .get("amount")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field amount not found or not string in erc_transfer msg"))?;
                    let amount = ethers::types::U256::from_str_radix(
                        amount_str.strip_prefix("0x").unwrap_or(amount_str),
                        16,
                    ).map_err(|e| anyhow!("Failed to parse amount '{}': {}", amount_str, e))?;

                    let recipient = msg
                        .get("recipient")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Field recipient not found in erc_transfer msg"))?;
                    let recipient_eth = ethers::types::Address::from_str(recipient)?;
                    let recipient_fil = ethers_address_to_fil_address(&recipient_eth)?;

                    let message =
                        abi_encode_erc_transfer_msg(&home_subnet_id, home_token, amount)?;

                    // Destination is the subnet we queried for
                    IpcEnvelope {
                        kind: IpcMsgKind::ErcTransfer,
                        to: IPCAddress::new(subnet_id, &recipient_fil)?,
                        from: from_root,
                        value: TokenAmount::zero(),
                        message,
                        nonce,
                    }
                }
                Some(unknown) => {
                    return Err(anyhow!("Unknown kind in getrootnetmessages result: {}", unknown))
                }
                None => return Err(anyhow!("Field kind not found in result")),
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
        let block_hash_current = self.get_block_hash_inner(height).await?;
        let block_hash_parent = self.get_block_hash_inner(height - 1).await?;

        Ok(GetBlockHashResult {
            block_hash: block_hash_current.0.to_vec(),
            parent_block_hash: block_hash_parent.0.to_vec(),
        })
    }

    /// Get the validator change set from start to end block.
    async fn get_validator_changeset(
        &self,
        subnet_id: &SubnetID,
        epoch: ChainEpoch,
    ) -> Result<TopDownQueryPayload<Vec<StakingChangeRequest>>> {
        tracing::info!("getting validator changeset for subnet: {subnet_id:} at height: {epoch:}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "getstakechanges",
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
                "getstakechanges request failed with status: {}",
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

        let mut changes: Vec<StakingChangeRequest> = vec![];
        let mut prev_block_hash: Option<H256> = None;

        let results = data
            .get("result")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Field result not found"))?;
        for result in results {
            // parse configuration_number
            let configuration_number =
                result
                    .get("configuration_number")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| anyhow!("Field configuration_number not found in result"))?;

            // parse validator address
            let validator_address = result
                .get("validator_subnet_address")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Field validator_subnet_address not found in result"))?;
            let validator_address = ethers::types::Address::from_str(validator_address)?;
            let validator_address = ethers_address_to_fil_address(&validator_address)?;

            let change_details = result
                .get("change")
                .ok_or_else(|| anyhow!("Field change not found in result"))?;

            let change = if change_details.get("join").is_some() {
                let pubkey = change_details
                    .get("join")
                    .and_then(|join_params| join_params.get("pubkey"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Field pubkey could not be found or parsed"))?;
                let pubkey_bytes =
                    hex::decode(pubkey).map_err(|_| anyhow!("Invalid hex in pubkey"))?;
                if pubkey_bytes.len() != 33 {
                    return Err(anyhow!(
                        "Invalid pubkey length, the RPC method should return 33 bytes"
                    ));
                }
                let secp_pubkey = libsecp256k1::PublicKey::parse_slice(
                    &pubkey_bytes,
                    Some(libsecp256k1::PublicKeyFormat::Compressed),
                )
                .map_err(|_| anyhow!("Invalid secp256k1 public key"))?;

                StakingChange {
                    op: StakingOperation::SetMetadata,
                    payload: secp_pubkey.serialize().to_vec(),
                    validator: validator_address,
                }
            } else if change_details.get("deposit").is_some() {
                let amount = change_details
                    .get("deposit")
                    .and_then(|deposit_params| deposit_params.get("amount"))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("Field amount could not be found or parsed"))?;

                // TODO check overflow
                let amount = amount * ipc_api::SATOSHI_TO_ATTO;
                // let amount = token_amount_from_satoshi(amount);

                StakingChange {
                    op: StakingOperation::Deposit,
                    payload: ethers::abi::encode(&[ethers::abi::Token::Uint(
                        ethereum_types::U256::from(amount),
                    )]),
                    validator: validator_address,
                }
            } else if change_details.get("withdraw").is_some() {
                let amount = change_details
                    .get("withdraw")
                    .and_then(|params| params.get("amount"))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("Field amount could not be found or parsed"))?;

                // TODO check overflow
                let amount = amount * ipc_api::SATOSHI_TO_ATTO;
                // let amount = token_amount_from_satoshi(amount);

                StakingChange {
                    op: StakingOperation::Withdraw,
                    payload: ethers::abi::encode(&[ethers::abi::Token::Uint(
                        ethereum_types::U256::from(amount),
                    )]),
                    validator: validator_address,
                }
            } else {
                return Err(anyhow!("Unknown operation in change"));
            };

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

            let change_request = StakingChangeRequest {
                configuration_number: configuration_number as u64,
                change,
            };
            tracing::debug!(
                "Received new change request. configuration_number: {configuration_number}, operation: {:?}, validator: {:?}, payload: {:?}",
                change_request.change.op,
                change_request.change.validator.to_string(),
                hex::encode(change_request.change.payload.clone()),
            );
            changes.push(change_request);
        }

        let block_hash = match prev_block_hash {
            Some(h) => h.0.to_vec(),
            None => self.get_block_hash(epoch).await?.block_hash,
        };

        Ok(TopDownQueryPayload {
            value: changes,
            block_hash,
        })
    }
    /// Returns the latest parent finality committed in a child subnet
    async fn latest_parent_finality(&self) -> Result<ChainEpoch> {
        tracing::info!("getting latest parent finality");
        unimplemented!("latest_parent_finality is not expected to be called on an L1 subnet")
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

fn get_validators_from_response(
    committee: &Vec<Value>,
    is_current_committee: bool,
) -> Result<Vec<(Address, ValidatorInfo)>> {
    let validators = committee
        .iter()
        .filter_map(|v| {
            let subnet_address = v.get("subnet_address")?.as_str()?;
            let addr = ethers::types::Address::from_str(subnet_address).ok()?;
            let addr = ethers_address_to_fil_address(&addr).ok()?;

            let collateral = v.get("collateral")?.as_u64()?;
            let weight = token_amount_from_satoshi(collateral);

            let v = ValidatorInfo {
                staking: ValidatorStakingInfo {
                    confirmed_collateral: weight.clone(),
                    total_collateral: weight,
                    metadata: Vec::new(),
                },
                is_active: is_current_committee,
                is_waiting: !is_current_committee,
            };

            Some((addr, v))
        })
        .collect();

    Ok(validators)
}

fn subnet_id_to_abi_token(subnet: &SubnetID) -> anyhow::Result<ethers::abi::Token> {
    use ipc_api::evm::subnet_id_to_evm_addresses;
    let route = subnet_id_to_evm_addresses(subnet)?
        .into_iter()
        .map(ethers::abi::Token::Address)
        .collect();
    Ok(ethers::abi::Token::Tuple(vec![
        ethers::abi::Token::Uint(ethers::types::U256::from(subnet.root_id())),
        ethers::abi::Token::Array(route),
    ]))
}

fn abi_encode_erc_transfer_msg(
    home_subnet: &SubnetID,
    home_token: ethers::types::Address,
    amount: ethers::types::U256,
) -> anyhow::Result<Vec<u8>> {
    let subnet_tok = subnet_id_to_abi_token(home_subnet)?;
    Ok(ethers::abi::encode(&[
        subnet_tok,
        ethers::abi::Token::Address(home_token),
        ethers::abi::Token::Uint(amount),
    ]))
}

fn abi_encode_erc_registration_msg(
    home_subnet: &SubnetID,
    home_token: ethers::types::Address,
    name: &str,
    symbol: &str,
    decimals: u8,
    initial_supply: ethers::types::U256,
) -> anyhow::Result<Vec<u8>> {
    let subnet_tok = subnet_id_to_abi_token(home_subnet)?;
    Ok(ethers::abi::encode(&[
        subnet_tok,
        ethers::abi::Token::Address(home_token),
        ethers::abi::Token::String(name.to_string()),
        ethers::abi::Token::String(symbol.to_string()),
        ethers::abi::Token::Uint(ethers::types::U256::from(decimals)),
        ethers::abi::Token::Uint(initial_supply),
    ]))
}

/// Inverse of `subnet_id_to_abi_token`: reconstructs a SubnetID from ABI-decoded tokens.
fn abi_token_to_subnet_id(
    root: ethers::types::U256,
    route: Vec<ethers::abi::Token>,
) -> anyhow::Result<SubnetID> {
    let children = route
        .into_iter()
        .map(|tok| {
            let addr = tok
                .into_address()
                .ok_or_else(|| anyhow!("expected address in subnet route"))?;
            ipc_api::ethers_address_to_fil_address(&addr)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(SubnetID::new(root.as_u64(), children))
}

/// Decode abi.encode(homeSubnet, homeToken, amount) — inverse of `abi_encode_erc_transfer_msg`.
fn abi_decode_erc_transfer_msg(
    data: &[u8],
) -> anyhow::Result<(SubnetID, ethers::types::Address, ethers::types::U256)> {
    use ethers::abi::{decode, ParamType};
    let tokens = decode(
        &[
            ParamType::Tuple(vec![
                ParamType::Uint(256),
                ParamType::Array(Box::new(ParamType::Address)),
            ]),
            ParamType::Address,
            ParamType::Uint(256),
        ],
        data,
    )?;
    let subnet_tuple = tokens[0]
        .clone()
        .into_tuple()
        .ok_or_else(|| anyhow!("expected tuple for subnet"))?;
    let root = subnet_tuple[0]
        .clone()
        .into_uint()
        .ok_or_else(|| anyhow!("expected uint for root"))?;
    let route = subnet_tuple[1]
        .clone()
        .into_array()
        .ok_or_else(|| anyhow!("expected array for route"))?;
    let subnet = abi_token_to_subnet_id(root, route)?;
    let home_token = tokens[1]
        .clone()
        .into_address()
        .ok_or_else(|| anyhow!("expected address for home_token"))?;
    let amount = tokens[2]
        .clone()
        .into_uint()
        .ok_or_else(|| anyhow!("expected uint for amount"))?;
    Ok((subnet, home_token, amount))
}

/// Decode abi.encode(homeSubnet, homeToken, name, symbol, decimals, initialSupply).
fn abi_decode_erc_registration_msg(
    data: &[u8],
) -> anyhow::Result<(
    SubnetID,
    ethers::types::Address,
    String,
    String,
    u8,
    ethers::types::U256,
)> {
    use ethers::abi::{decode, ParamType};
    let tokens = decode(
        &[
            ParamType::Tuple(vec![
                ParamType::Uint(256),
                ParamType::Array(Box::new(ParamType::Address)),
            ]),
            ParamType::Address,
            ParamType::String,
            ParamType::String,
            ParamType::Uint(8),
            ParamType::Uint(256),
        ],
        data,
    )?;
    let subnet_tuple = tokens[0]
        .clone()
        .into_tuple()
        .ok_or_else(|| anyhow!("expected tuple for subnet"))?;
    let root = subnet_tuple[0]
        .clone()
        .into_uint()
        .ok_or_else(|| anyhow!("expected uint for root"))?;
    let route = subnet_tuple[1]
        .clone()
        .into_array()
        .ok_or_else(|| anyhow!("expected array for route"))?;
    let subnet = abi_token_to_subnet_id(root, route)?;
    let home_token = tokens[1]
        .clone()
        .into_address()
        .ok_or_else(|| anyhow!("expected address"))?;
    let name = tokens[2]
        .clone()
        .into_string()
        .ok_or_else(|| anyhow!("expected string for name"))?;
    let symbol = tokens[3]
        .clone()
        .into_string()
        .ok_or_else(|| anyhow!("expected string for symbol"))?;
    let decimals = tokens[4]
        .clone()
        .into_uint()
        .ok_or_else(|| anyhow!("expected uint for decimals"))?
        .as_u32() as u8;
    let initial_supply = tokens[5]
        .clone()
        .into_uint()
        .ok_or_else(|| anyhow!("expected uint for initial_supply"))?;
    Ok((subnet, home_token, name, symbol, decimals, initial_supply))
}

/// Decode abi.encode(homeToken, delta) for ErcSupplyDelta messages.
fn abi_decode_erc_supply_delta_msg(
    data: &[u8],
) -> anyhow::Result<(ethers::types::Address, ethers::types::I256)> {
    use ethers::abi::{decode, ParamType};
    let tokens = decode(&[ParamType::Address, ParamType::Int(256)], data)?;
    let home_token = tokens[0]
        .clone()
        .into_address()
        .ok_or_else(|| anyhow!("expected address for home_token"))?;
    let delta = tokens[1]
        .clone()
        .into_int()
        .ok_or_else(|| anyhow!("expected int for delta"))?;
    Ok((home_token, ethers::types::I256::from_raw(delta)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_manager() {
        assert!(true);
    }

    fn test_subnet_id() -> SubnetID {
        let child =
            fvm_shared::address::Address::from(fvm_shared::address::current_network::ACCOUNT_ACTOR);
        SubnetID::new(4, vec![child])
    }

    #[test]
    fn test_abi_decode_erc_transfer_msg_roundtrip() {
        let subnet = test_subnet_id();
        let token = ethers::types::Address::random();
        let amount = ethers::types::U256::from(1_000_000u64);

        let encoded = abi_encode_erc_transfer_msg(&subnet, token, amount).unwrap();
        let (decoded_subnet, decoded_token, decoded_amount) =
            abi_decode_erc_transfer_msg(&encoded).unwrap();

        assert_eq!(decoded_subnet.root_id(), subnet.root_id());
        assert_eq!(decoded_subnet.children().len(), subnet.children().len());
        assert_eq!(decoded_token, token);
        assert_eq!(decoded_amount, amount);
    }

    #[test]
    fn test_abi_decode_erc_registration_msg_roundtrip() {
        let subnet = test_subnet_id();
        let token = ethers::types::Address::random();
        let name = "TestToken";
        let symbol = "TT";
        let decimals = 18u8;
        let initial_supply = ethers::types::U256::from(1_000_000_000u64);

        let encoded = abi_encode_erc_registration_msg(
            &subnet,
            token,
            name,
            symbol,
            decimals,
            initial_supply,
        )
        .unwrap();
        let (decoded_subnet, decoded_token, decoded_name, decoded_symbol, decoded_decimals, decoded_supply) =
            abi_decode_erc_registration_msg(&encoded).unwrap();

        assert_eq!(decoded_subnet.root_id(), subnet.root_id());
        assert_eq!(decoded_token, token);
        assert_eq!(decoded_name, name);
        assert_eq!(decoded_symbol, symbol);
        assert_eq!(decoded_decimals, decimals);
        assert_eq!(decoded_supply, initial_supply);
    }

    #[test]
    fn test_abi_decode_erc_supply_delta_msg_positive() {
        let token = ethers::types::Address::random();
        let delta = ethers::types::I256::from(500);
        let encoded = ethers::abi::encode(&[
            ethers::abi::Token::Address(token),
            ethers::abi::Token::Int(delta.into_raw()),
        ]);
        let (decoded_token, decoded_delta) = abi_decode_erc_supply_delta_msg(&encoded).unwrap();
        assert_eq!(decoded_token, token);
        assert_eq!(decoded_delta, delta);
    }

    #[test]
    fn test_abi_decode_erc_supply_delta_msg_negative() {
        let token = ethers::types::Address::random();
        let delta = ethers::types::I256::from(-300);
        let encoded = ethers::abi::encode(&[
            ethers::abi::Token::Address(token),
            ethers::abi::Token::Int(delta.into_raw()),
        ]);
        let (decoded_token, decoded_delta) = abi_decode_erc_supply_delta_msg(&encoded).unwrap();
        assert_eq!(decoded_token, token);
        assert_eq!(decoded_delta, delta);
    }

    #[test]
    fn test_abi_decode_erc_supply_delta_msg_zero() {
        let token = ethers::types::Address::random();
        let delta = ethers::types::I256::zero();
        let encoded = ethers::abi::encode(&[
            ethers::abi::Token::Address(token),
            ethers::abi::Token::Int(delta.into_raw()),
        ]);
        let (decoded_token, decoded_delta) = abi_decode_erc_supply_delta_msg(&encoded).unwrap();
        assert_eq!(decoded_token, token);
        assert_eq!(decoded_delta, delta);
    }

    #[test]
    fn test_abi_token_to_subnet_id_roundtrip() {
        let subnet = test_subnet_id();
        let token = subnet_id_to_abi_token(&subnet).unwrap();
        let tuple = token.into_tuple().unwrap();
        let root = tuple[0].clone().into_uint().unwrap();
        let route = tuple[1].clone().into_array().unwrap();
        let decoded = abi_token_to_subnet_id(root, route).unwrap();

        assert_eq!(decoded.root_id(), subnet.root_id());
        assert_eq!(decoded.children().len(), subnet.children().len());
    }
}
