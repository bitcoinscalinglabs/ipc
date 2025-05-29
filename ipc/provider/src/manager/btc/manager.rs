// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use ethers::abi::ethereum_types;
use ethers::providers::Authorization;
use ethers::types::H256;
use http::HeaderValue;
use ipc_api::address::IPCAddress;
use ipc_api::checkpoint::{BitcoinTx, PsbtSignature, PsbtSignatureQuorum, UnsignedPsbt};
use ipc_api::evm::payload_to_evm_address;
use ipc_api::subnet::{
    Asset, AssetKind, BtcConstructParams, BtcFundParams, ConstructParams, FundParams,
    PermissionMode, PreFundParams,
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
use ipc_api::staking::{StakingChange, StakingChangeRequest, StakingOperation, ValidatorInfo};
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

        tracing::info!("unstakecollateral request successful");
        Ok(())
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

    /// This function asks the parent subnet (bitcoin) to generate the required transaction for the given `checkpoint` and `subnet_id` and sign it.
    async fn get_checkpoint_transaction(
        &self,
        subnet_id: &SubnetID,
        checkpoint: BottomUpCheckpoint,
    ) -> Result<PsbtSignature> {
        tracing::debug!("Creating bitcoin signatures for checkpoint: {checkpoint:?}");

        // collect all withdrawals and transfers from the checkpoint msgs
        let mut releases = Vec::new();
        let mut transfers = Vec::new();

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
        // When these signatures are submitted to the `finalize_checkpoint_psbt` RPC call,
        // they must be split again (see `submit_checkpoint` of `BtcSubnetManager`).
        let signature = data
            .get("result")
            .and_then(|r| r.get("psbt_inputs_signatures"))
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Missing 'result.psbt_inputs_signatures' in JSON-RPC response"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| {
                        anyhow!(
                            "Invalid entry in 'result.psbt_inputs_signatures' in JSON-RPC response"
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

        Ok(PsbtSignature {
            unsigned_psbt: UnsignedPsbt(unsigned_psbt),
            signature,
            transfer_tx: BitcoinTx(transfer_tx),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// In the `get_checkpoint_transaction` we concatenate the signatures of each signatory,
// so we need to split them again here.
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
// And the resulting json will be:
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
#[async_trait]
impl BottomUpCheckpointRelayer for BtcSubnetManager {
    async fn submit_checkpoint(
        &self,
        keystore: Arc<RwLock<PersistentKeyStore<EthKeyAddress>>>,
        _submitter: &Option<Address>,
        checkpoint: BottomUpCheckpoint,
        _signatures: Vec<Signature>,
        _signatories: Vec<Address>,
        bitcoin_signatures: Option<PsbtSignatureQuorum>,
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
        // Split the signatures of each signatory into chunks of 64 bytes (see info above function for more details)
        let mut split_signatures = Vec::new();
        for concatenated_signatures_of_signatory in bitcoin_signatures.signatures.iter() {
            let split_signatures_of_signatory = concatenated_signatures_of_signatory
                .chunks(libsecp256k1::util::SIGNATURE_SIZE)
                .map(|chunk| hex::encode(chunk.to_vec()))
                .collect::<Vec<_>>();
            split_signatures.push(split_signatures_of_signatory);
        }
        // Replace the IPC addresses with the XOnlyPubKey, as the RPC expects the XOnlyPubKey
        let signatories_xonly_pubkey = bitcoin_signatures
            .signatories
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
        for (signatory, signatures) in signatories_xonly_pubkey.iter().zip(split_signatures.iter())
        {
            let json_entry = json!([signatory, signatures]);
            signatures_json.push(json_entry);
        }

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
                value: token_amount_from_satoshi(value),
                // TODO(Orestis): The following should only work for fund/prefund messages.
                // Change when we implement transfers.
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

#[cfg(test)]
mod tests {

    #[test]
    fn test_create_manager() {
        // let _ = super::BtcSubnetManager::new();
        assert!(true);
    }
}
