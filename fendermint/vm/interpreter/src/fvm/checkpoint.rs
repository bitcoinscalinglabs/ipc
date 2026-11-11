// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use super::observe::{
    CheckpointCreated, CheckpointFinalized, CheckpointSigned, CheckpointSignedRole,
};
use super::state::ipc::{decode_slash_output, decode_slash_pre_signatures, tokens_to_burn};
use super::{
    broadcast::Broadcaster,
    state::{ipc::GatewayCaller, FvmExecState},
    ValidatorContext,
};
use crate::fvm::activity::ValidatorActivityTracker;
use crate::fvm::exec::BlockEndEvents;
use anyhow::{anyhow, Context};
use ethers::abi::Tokenizable;
use fendermint_crypto::PublicKey;
use fendermint_crypto::SecretKey;
use fendermint_vm_actor_interface::eam::EthAddress;
use fendermint_vm_actor_interface::ipc::BottomUpCheckpoint;
use fendermint_vm_genesis::{Power, Validator, ValidatorKey};
use fvm_ipld_blockstore::Blockstore;
use fvm_shared::{address::Address, chainid::ChainID};
use ipc_actors_abis::checkpointing_facet as checkpoint;
use ipc_actors_abis::gateway_getter_facet as getter;
use ipc_api::staking::ConfigurationNumber;
use ipc_observability::{emit, serde::HexEncodableBlockHash};
use ipc_provider::manager::SubnetManager;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tendermint::block::Height;
use tendermint_rpc::endpoint::commit;
use tendermint_rpc::{endpoint::validators, Client, Paging};

/// Validator voting power snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerTable(pub Vec<Validator<Power>>);

/// Changes in the power table.
#[derive(Debug, Clone, Default)]
pub struct PowerUpdates(pub Vec<Validator<Power>>);

/// Construct and store a checkpoint if this is the end of the checkpoint period.
/// Perform end-of-checkpoint-period transitions in the ledger.
///
/// If we are the boundary, return the validators eligible to sign and any updates
/// to the power table, along with the checkpoint that needs to be signed by validators.
pub fn maybe_create_checkpoint<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
    event_tracker: &mut BlockEndEvents,
) -> anyhow::Result<Option<(checkpoint::BottomUpCheckpoint, PowerUpdates)>>
where
    DB: Blockstore + Sync + Send + Clone + 'static,
{
    // Epoch transitions for checkpointing.
    let height: tendermint::block::Height = state
        .block_height()
        .try_into()
        .context("block height is not u64")?;

    let block_hash = state
        .block_hash()
        .ok_or_else(|| anyhow!("block hash not set"))?;

    let Some((mut msgs, subnet_id)) = should_create_checkpoint(gateway, state, height)? else {
        return Ok(None);
    };

    // Collect ERC20 supply adjustments (IPC:ETS).
    // We snapshot totalSupply() for each registered token once per checkpoint period,
    // compute the delta from the previous snapshot, and append the results to msgs.
    let supply_deltas = gateway
        .get_token_supply_deltas(state, height.value())
        .context("failed to get token supply deltas")?;
    let mut supply_adjustment_bytes = 0;
    if !supply_deltas.is_empty() {
        let delta_count = supply_deltas.len();
        gateway
            .update_supply_snapshots(state, height.value())
            .context("failed to update supply snapshots")?;

        let empty_subnet_id = checkpoint::SubnetID {
            root: 0,
            route: vec![],
        };
        // A supply delta has no real sender/recipient.
        let zero_addr = checkpoint::FvmAddress {
            addr_type: 4,
            payload: ethers::types::Bytes::from(ethers::abi::encode(&[ethers::abi::Token::Tuple(
                vec![
                    ethers::abi::Token::Uint(ethers::types::U256::from(10u64)), // EAM_ACTOR namespace
                    ethers::abi::Token::Uint(ethers::types::U256::from(20u64)), // length
                    ethers::abi::Token::Bytes(vec![0u8; 20]),                   // address(0)
                ],
            )])),
        };
        let empty_ipc_addr = checkpoint::Ipcaddress {
            subnet_id: empty_subnet_id,
            raw_address: zero_addr,
        };

        for (token, delta) in supply_deltas {
            // message = abi.encode(token, delta) — matches the Solidity ErcSupplyDelta format
            let message = ethers::abi::encode(&[
                ethers::abi::Token::Address(token),
                ethers::abi::Token::Int(delta.into_raw()),
            ]);
            supply_adjustment_bytes += message.len();
            msgs.push(checkpoint::IpcEnvelope {
                kind: 5, // IpcMsgKind::ErcSupplyDelta
                to: empty_ipc_addr.clone(),
                from: empty_ipc_addr.clone(),
                nonce: 0,
                value: ethers::types::U256::zero(),
                message: ethers::types::Bytes::from(message),
            });
        }
        tracing::info!(
            delta_count,
            "injected ERC supply delta messages into checkpoint"
        );
    }

    // Get the current power table from the ledger, not CometBFT.
    let (curr_configuration_number, curr_power_table) =
        ipc_power_table(gateway, state).context("failed to get the current power table")?;
    tracing::debug!(
        curr_configuration_number,
        curr_power_table_len = curr_power_table.0.len(),
        "loaded current power table before checkpoint creation"
    );

    // Apply any validator set transitions.
    let next_configuration_number = gateway
        .apply_validator_changes(state)
        .context("failed to apply validator changes")?;

    // The checkpoint carries the outputs with a slash pre-signature posted since their last carry.
    let slash_pre_signature_count = gateway
        .carry_slash_pre_signatures(state, height.value(), supply_adjustment_bytes)
        .context("failed to carry the slash pre-signatures")?;

    // Sum up the value leaving the subnet as part of the bottom-up messages.
    let burnt_tokens = tokens_to_burn(&msgs);

    // NOTE: Unlike when we minted tokens for the gateway by modifying its balance,
    // we don't have to burn them here, because it's already being done in
    // https://github.com/consensus-shipyard/ipc-solidity-actors/pull/263
    // by sending the funds to the BURNTFUNDS_ACTOR.
    // Ostensibly we could opt _not_ to decrease the circ supply here, but rather
    // look up the burnt funds balance at the beginning of each block and subtract
    // it from the monotonically increasing supply, in which case it could reflect
    // a wider range of burning activity than just IPC.
    // It might still be inconsistent if someone uses another address for burning tokens.
    // By decreasing here, at least `circ_supply` is consistent with IPC.
    state.update_circ_supply(|circ_supply| {
        *circ_supply -= burnt_tokens;
    });

    let num_msgs = msgs.len();

    let full_activity_rollup = state.activity_tracker().commit_activity()?;

    // Construct checkpoint.
    let checkpoint = BottomUpCheckpoint {
        subnet_id,
        block_height: ethers::types::U256::from(height.value()),
        block_hash,
        next_configuration_number,
        slash_pre_signature_count,
        msgs,
        activity: full_activity_rollup.compressed()?,
    };

    // Save the checkpoint in the ledger.
    // Pass in the current power table, because these are the validators who can sign this checkpoint.
    let ret = gateway
        .create_bottom_up_checkpoint(
            state,
            checkpoint.clone(),
            &curr_power_table.0,
            full_activity_rollup.into_inner(),
        )
        .context("failed to store checkpoint")?;
    event_tracker.push((ret.apply_ret.events, ret.emitters));

    // Figure out the power updates if there was some change in the configuration.
    let power_updates = if next_configuration_number == 0 {
        PowerUpdates(Vec::new())
    } else {
        let (next_power_configuration_number, next_power_table) =
            ipc_power_table(gateway, state).context("failed to get next power table")?;
        tracing::debug!(
            next_power_configuration_number,
            next_power_table_len = next_power_table.0.len(),
            "loaded next power table after validator updates"
        );

        debug_assert_eq!(next_power_configuration_number, next_configuration_number);

        power_diff(curr_power_table, next_power_table)
    };
    tracing::debug!(
        power_updates_len = power_updates.0.len(),
        "computed checkpoint validator power updates"
    );

    emit(CheckpointCreated {
        height: height.value(),
        hash: HexEncodableBlockHash(block_hash.to_vec()),
        msg_count: num_msgs,
        config_number: next_configuration_number,
    });
    tracing::debug!(
        height = height.value(),
        "created checkpoint at height, checkpoint: {:?}",
        checkpoint,
    );

    Ok(Some((checkpoint, power_updates)))
}

/// Wait until CometBFT has reached a specific block height.
///
/// This is used so we can wait for the next block where the ledger changes
/// we have done durign execution has been committed.
async fn wait_for_commit<C>(
    client: &C,
    block_height: u64,
    retry_delay: Duration,
) -> anyhow::Result<()>
where
    C: Client + Clone + Send + Sync + 'static,
{
    loop {
        let res: commit::Response = client
            .latest_commit()
            .await
            .context("failed to fetch latest commit")?;

        if res.signed_header.header().height.value() >= block_height {
            return Ok(());
        }

        tokio::time::sleep(retry_delay).await;
    }
}

fn x_only(key: &PublicKey) -> [u8; 32] {
    key.serialize_compressed()[1..]
        .try_into()
        .expect("32 bytes")
}

/// Posts this validator's missing slash pre-signatures; a failed output does not stop the rest.
pub async fn post_slash_pre_signatures<C, DB>(
    broadcaster: &Broadcaster<C>,
    gateway: &GatewayCaller<DB>,
    subnet_id: &ipc_api::subnet_id::SubnetID,
    parent_manager: &ipc_provider::manager::BtcSubnetManager,
    public_key: PublicKey,
    chain_id: ChainID,
) -> anyhow::Result<()>
where
    C: Client + Clone + Send + Sync + 'static,
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let own = x_only(&public_key);
    let signatory = ethers::types::Address::from(EthAddress::from(public_key).0);
    let outputs = parent_manager.get_unslashable_outputs(subnet_id).await?;
    for output in outputs_to_presign(&own, outputs) {
        let posted = async {
            let calldata = gateway.slash_output_calldata(&output.txid, output.vout, signatory)?;
            let data = broadcaster
                .fevm_view(Address::from(gateway.addr()), calldata, chain_id)
                .await?;
            let posted = decode_slash_output(&data)?;
            if !should_post_slash_pre_signature(&posted) {
                return Ok(None);
            }
            let presig = parent_manager
                .sign_slash(subnet_id, &output.txid, output.vout)
                .await
                .context("failed to pre-sign the slash")?;
            let calldata = gateway.add_slash_pre_signature_calldata(&presig)?;
            broadcaster
                .fevm_invoke(Address::from(gateway.addr()), calldata, chain_id)
                .await
                .context("failed to post a slash pre-signature")
                .map(Some)
        }
        .await;
        match posted {
            Ok(Some(tx_hash)) => tracing::info!(
                tx_hash = tx_hash.to_string(),
                txid = output.txid,
                vout = output.vout,
                "posted a slash pre-signature"
            ),
            Ok(None) => {}
            Err(e) => tracing::error!(
                error = %format!("{e:#}"),
                txid = output.txid,
                vout = output.vout,
                "slash pre-signature not posted"
            ),
        }
    }
    Ok(())
}

/// The outputs whose slash leaf names a configuration with this validator's key.
pub fn outputs_to_presign(
    own: &[u8; 32],
    outputs: Vec<ipc_api::eots::UnslashableOutput>,
) -> Vec<ipc_api::eots::UnslashableOutput> {
    outputs
        .into_iter()
        .filter(|output| output.signer && output.keys.iter().any(|(k, _)| k == own))
        .collect()
}

/// Whether this validator posts its slash pre-signature for an output that is not slashable,
/// given what the subnet holds for it: it has not posted, as for Bitcoin signatures of a checkpoint.
pub fn should_post_slash_pre_signature(posted: &checkpoint::GetSlashOutputReturn) -> bool {
    !posted.posted
}

/// A checkpoint with the signatures this validator still has to submit for it.
#[derive(Debug, Clone)]
pub struct CheckpointToSign {
    pub checkpoint: getter::BottomUpCheckpoint,
    pub checkpoint_signature: bool,
    pub bitcoin_signature: bool,
}

/// Heights `H > 0` whose `H + 1` lies in `(prev_final, final_height]`.
pub fn newly_final_heights(prev_final: u64, final_height: u64) -> std::ops::Range<u64> {
    prev_final.max(1)..final_height
}

/// Heights `H` above `checkpointed` whose `H + 1` is at or below `final_height`.
pub fn final_heights_above(checkpointed: u64, final_height: u64) -> std::ops::Range<u64> {
    checkpointed + 1..final_height
}

/// The checkpoints whose `H + 1` became final in this subnet block, with both signatures to submit.
pub fn newly_final_checkpoints<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
    prev_final: u64,
    final_height: u64,
) -> anyhow::Result<Vec<CheckpointToSign>>
where
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let mut to_sign = Vec::new();
    for height in newly_final_heights(prev_final, final_height) {
        if let Some(checkpoint) = gateway.bottom_up_checkpoint(state, height)? {
            to_sign.push(CheckpointToSign {
                checkpoint,
                checkpoint_signature: true,
                bitcoin_signature: true,
            });
        }
    }
    Ok(to_sign)
}

/// The final checkpoints that lack a signature of this validator in the gateway.
pub fn checkpoints_to_resign<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
    validator_key: PublicKey,
    final_height: u64,
    checkpointed: Option<u64>,
) -> anyhow::Result<Vec<CheckpointToSign>>
where
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let validator_addr = EthAddress::from(validator_key);
    let unsigned = unsigned_checkpoints(gateway, state, validator_key)?;

    let mut bitcoin_missing = Vec::new();
    if let Some(checkpointed) = checkpointed {
        for height in final_heights_above(checkpointed, final_height) {
            let Some(checkpoint) = gateway.bottom_up_checkpoint(state, height)? else {
                continue;
            };
            if !gateway.has_bitcoin_checkpoint_signature(state, height, validator_addr)? {
                bitcoin_missing.push(checkpoint);
            }
        }
    }

    Ok(resign_plan(unsigned, final_height, bitcoin_missing))
}

/// The signatures to submit by height; a checkpoint at height `H` needs `H + 1` final.
pub fn resign_plan(
    unsigned: Vec<getter::BottomUpCheckpoint>,
    final_height: u64,
    bitcoin_missing: Vec<getter::BottomUpCheckpoint>,
) -> Vec<CheckpointToSign> {
    let mut to_sign: BTreeMap<u64, CheckpointToSign> = BTreeMap::new();
    for cp in unsigned {
        let height = cp.block_height.as_u64();
        if height < final_height {
            to_sign.insert(
                height,
                CheckpointToSign {
                    checkpoint: cp,
                    checkpoint_signature: true,
                    bitcoin_signature: false,
                },
            );
        }
    }
    for cp in bitcoin_missing {
        to_sign
            .entry(cp.block_height.as_u64())
            .or_insert_with(|| CheckpointToSign {
                checkpoint: cp,
                checkpoint_signature: false,
                bitcoin_signature: false,
            })
            .bitcoin_signature = true;
    }
    to_sign.into_values().collect()
}

/// Collect incomplete signatures from the ledger which this validator hasn't signed yet.
///
/// It doesn't check whether the validator should have signed it, that's done inside
/// [broadcast_signatures] at the moment. The goal is rather to avoid double
/// signing for those who have already done it.
pub fn unsigned_checkpoints<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
    validator_key: PublicKey,
) -> anyhow::Result<Vec<getter::BottomUpCheckpoint>>
where
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let mut unsigned_checkpoints = Vec::new();
    let validator_addr = EthAddress::from(validator_key);

    let incomplete_checkpoints = gateway
        .incomplete_checkpoints(state)
        .context("failed to fetch incomplete checkpoints")?;

    for cp in incomplete_checkpoints {
        let signatories = gateway
            .checkpoint_signatories(state, cp.block_height.as_u64())
            .context("failed to get checkpoint signatories")?;

        if !signatories.contains(&validator_addr) {
            unsigned_checkpoints.push(cp);
        }
    }

    Ok(unsigned_checkpoints)
}

/// Submits the signatures of `to_sign` once subnet block `block_height` is committed.
pub async fn broadcast_signatures<C, DB>(
    client: &C,
    validator_ctx: &ValidatorContext<C>,
    gateway: &GatewayCaller<DB>,
    chain_id: ChainID,
    to_sign: Vec<CheckpointToSign>,
    block_height: u64,
    subnet_id: Option<ipc_api::subnet_id::SubnetID>,
    parent_manager: Option<ipc_provider::manager::BtcSubnetManager>,
) -> anyhow::Result<()>
where
    C: Client + Clone + Send + Sync + 'static,
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    // Make sure that the state the checkpoints were read from is in the ledger.
    wait_for_commit(client, block_height, validator_ctx.broadcaster.retry_delay())
        .await
        .context("failed to wait for commit")?;

    for cp in to_sign {
        let height = cp.checkpoint.block_height.as_u64();
        // We mustn't do these in parallel because of how nonces are fetched.
        if let Err(e) = broadcast_checkpoint_signatures(
            client,
            validator_ctx,
            gateway,
            chain_id,
            cp,
            &subnet_id,
            &parent_manager,
        )
        .await
        {
            tracing::error!(error = %format!("{e:#}"), height, "checkpoint not signed");
        }
    }
    Ok(())
}

/// Submits the signatures of one checkpoint, if this validator was in its power table.
async fn broadcast_checkpoint_signatures<C, DB>(
    client: &C,
    validator_ctx: &ValidatorContext<C>,
    gateway: &GatewayCaller<DB>,
    chain_id: ChainID,
    cp: CheckpointToSign,
    subnet_id: &Option<ipc_api::subnet_id::SubnetID>,
    parent_manager: &Option<ipc_provider::manager::BtcSubnetManager>,
) -> anyhow::Result<()>
where
    C: Client + Clone + Send + Sync + 'static,
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let height = Height::try_from(cp.checkpoint.block_height.as_u64())?;
    // Getting the power table from CometBFT where the history is available.
    let power_table = bft_power_table(client, height)
        .await
        .context("failed to get power table")?;

    let Some(validator) = power_table
        .0
        .iter()
        .find(|v| v.public_key.0 == validator_ctx.public_key)
        .cloned()
    else {
        return Ok(());
    };

    let block_hash = cp.checkpoint.block_hash;
    let checkpoint = convert_checkpoint(cp.checkpoint)?;

    if cp.checkpoint_signature {
        broadcast_signature(
            &validator_ctx.broadcaster,
            gateway,
            checkpoint.clone(),
            &power_table,
            &validator,
            &validator_ctx.secret_key,
            chain_id,
        )
        .await
        .context("failed to broadcast checkpoint signature")?;
    }

    // The checkpoint's ABI subnet ID omits the parent network type: `subnet_id` gives it.
    if cp.bitcoin_signature && parent_is_bitcoin(subnet_id) {
        let subnet_id = subnet_id.as_ref().ok_or_else(|| {
            anyhow!("the Bitcoin signature needs the subnet_id of the current subnet")
        })?;
        let parent_manager = parent_manager
            .as_ref()
            .ok_or_else(|| anyhow!("the Bitcoin signature needs the parent manager"))?;
        let calldata = gateway.slash_pre_signatures_calldata(height.value())?;
        let data = validator_ctx
            .broadcaster
            .fevm_view(Address::from(gateway.addr()), calldata, chain_id)
            .await
            .context("failed to read the carried slash pre-signatures")?;
        broadcast_bitcoin_signature(
            &validator_ctx.broadcaster,
            gateway,
            subnet_id,
            parent_manager,
            checkpoint,
            chain_id,
            decode_slash_pre_signatures(&data)?,
        )
        .await
        .context("failed to broadcast bitcoin signature")?;
    }

    emit(CheckpointSigned {
        role: CheckpointSignedRole::Own,
        height: height.value(),
        hash: HexEncodableBlockHash(block_hash.to_vec()),
        validator: validator_ctx.addr,
    });

    tracing::debug!(?height, "submitted checkpoint signature");
    Ok(())
}

fn convert_checkpoint(
    cp: getter::BottomUpCheckpoint,
) -> anyhow::Result<checkpoint::BottomUpCheckpoint> {
    Ok(checkpoint::BottomUpCheckpoint::from_token(cp.into_token())?)
}

fn parent_is_bitcoin(subnet_id: &Option<ipc_api::subnet_id::SubnetID>) -> bool {
    if let Some(subnet_id) = subnet_id {
        return subnet_id.parent_network_type() == Some(ipc_api::subnet_id::NetworkType::Btc);
    }
    false
}

async fn broadcast_bitcoin_signature<C, DB>(
    broadcaster: &Broadcaster<C>,
    gateway: &GatewayCaller<DB>,
    subnet_id: &ipc_api::subnet_id::SubnetID,
    parent_manager: &ipc_provider::manager::BtcSubnetManager,
    checkpoint: checkpoint::BottomUpCheckpoint,
    chain_id: ChainID,
    slash_signatures: Vec<ipc_api::eots::SlashPreSignature>,
) -> anyhow::Result<()>
where
    C: Client + Clone + Send + Sync + 'static,
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let checkpoint_psbt = parent_manager
        .get_checkpoint_transaction(
            subnet_id,
            ipc_api::checkpoint::BottomUpCheckpoint::try_from(checkpoint.clone())?,
            slash_signatures,
        )
        .await?;
    tracing::trace!(
        "interpreter obtained checkpoint PSBT from bitcoin provider: {checkpoint_psbt:?}"
    );

    let calldata = gateway
        .add_bitcoin_checkpoint_signature_calldata(&checkpoint, checkpoint_psbt)
        .context("failed to produce bitcoin checkpoint signature calldata")?;

    let tx_hash = broadcaster
        .fevm_invoke(Address::from(gateway.addr()), calldata, chain_id)
        .await
        .context("failed to broadcast bitcoin checkpoint signature")?;

    tracing::info!(
        tx_hash = tx_hash.to_string(),
        "broadcasted bitcoin checkpoint signature"
    );

    Ok(())
}

/// As a validator, sign the checkpoint and broadcast a transaction to add our signature to the ledger.
pub async fn broadcast_signature<C, DB>(
    broadcaster: &Broadcaster<C>,
    gateway: &GatewayCaller<DB>,
    checkpoint: checkpoint::BottomUpCheckpoint,
    power_table: &PowerTable,
    validator: &Validator<Power>,
    secret_key: &SecretKey,
    chain_id: ChainID,
) -> anyhow::Result<()>
where
    C: Client + Clone + Send + Sync + 'static,
    DB: Blockstore + Send + Sync + Clone + 'static,
{
    let calldata = gateway
        .add_checkpoint_signature_calldata(checkpoint, &power_table.0, validator, secret_key)
        .context("failed to produce checkpoint signature calldata")?;

    let tx_hash = broadcaster
        .fevm_invoke(Address::from(gateway.addr()), calldata, chain_id)
        .await
        .context("failed to broadcast signature")?;

    // The transaction should be in the mempool now.
    tracing::info!(tx_hash = tx_hash.to_string(), "broadcasted signature");

    Ok(())
}

// Emit a CheckpointFinalized trace event if a checkpoint has been finalized on the current block.
pub fn emit_trace_if_check_checkpoint_finalized<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
) -> anyhow::Result<()>
where
    DB: Blockstore + Clone,
{
    if !gateway.is_anchored(state)? {
        return Ok(());
    }

    let block_height = state.block_height();
    let block_hash = state
        .block_hash()
        .ok_or_else(|| anyhow!("block hash not set"))?;

    // Check if the checkpoint has been finalized.
    // If no checkpoint was emitted at this height, the QuorumInfo struct will carry zero values,
    // including reached=false.
    let checkpoint_quorum = gateway.checkpoint_info(state, block_height)?;

    if checkpoint_quorum.reached {
        emit(CheckpointFinalized {
            height: block_height,
            hash: HexEncodableBlockHash(block_hash.to_vec()),
        })
    }

    Ok(())
}

fn convert_tokenizables<Source: Tokenizable, Target: Tokenizable>(
    tokenizables: Vec<Source>,
) -> anyhow::Result<Vec<Target>> {
    Ok(tokenizables
        .into_iter()
        .map(|t| Target::from_token(t.into_token()))
        .collect::<Result<Vec<_>, _>>()?)
}

fn should_create_checkpoint<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
    height: Height,
) -> anyhow::Result<Option<(Vec<checkpoint::IpcEnvelope>, checkpoint::SubnetID)>>
where
    DB: Blockstore + Clone,
{
    let id = gateway.subnet_id(state)?;
    let is_root = id.route.is_empty();

    if is_root {
        return Ok(None);
    }

    let batch = gateway.bottom_up_msg_batch(state, height.into())?;

    if batch.block_height.as_u64() != 0 {
        tracing::debug!(
            height = height.value(),
            "bottom up msg batch exists at height",
        );
    } else if height.value() % gateway.bottom_up_check_period(state)? == 0 {
        tracing::debug!(
            height = height.value(),
            "bottom up checkpoint period reached height"
        );
    } else {
        return Ok(None);
    }

    let id = checkpoint::SubnetID {
        root: id.root,
        route: id.route,
    };
    let msgs = convert_tokenizables(batch.msgs)?;
    Ok(Some((msgs, id)))
}

/// Get the power table from CometBFT.
///
/// This is prone to failing, e.g. one theory is that CometBFT is trying to restart
/// the application, and while doing that it does not open up its HTTP services,
/// leading to a chicken-and-egg problem of failing to start.
async fn bft_power_table<C>(client: &C, height: Height) -> anyhow::Result<PowerTable>
where
    C: Client + Sync + Send + 'static,
{
    let mut power_table = Vec::new();
    let validators: validators::Response = client.validators(height, Paging::All).await?;

    for v in validators.validators {
        power_table.push(Validator {
            public_key: ValidatorKey::try_from(v.pub_key)?,
            power: Power(v.power()),
        });
    }

    Ok(PowerTable(power_table))
}

/// Get the current power table from the Gateway actor.
fn ipc_power_table<DB>(
    gateway: &GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
) -> anyhow::Result<(ConfigurationNumber, PowerTable)>
where
    DB: Blockstore + Sync + Send + Clone + 'static,
{
    gateway
        .current_power_table(state)
        .context("failed to get current power table")
        .map(|(cn, pt)| (cn, PowerTable(pt)))
}

/// Calculate the difference between the current and the next power table, to return to CometBFT only what changed:
/// * include any new validator, or validators whose power has been updated
/// * include validators to be removed with a power of 0, as [expected](https://github.com/informalsystems/tendermint-rs/blob/bcc0b377812b8e53a02dff156988569c5b3c81a2/rpc/src/dialect/end_block.rs#L12-L14) by CometBFT
fn power_diff(current: PowerTable, next: PowerTable) -> PowerUpdates {
    let current = into_power_map(current);
    let next = into_power_map(next);

    let mut diff = Vec::new();

    // Validators in `current` but not in `next` should be removed.
    for (k, v) in current.iter() {
        if !next.contains_key(k) {
            let delete = Validator {
                public_key: v.public_key.clone(),
                power: Power(0),
            };
            diff.push(delete);
        }
    }

    // Validators in `next` that differ from `current` should be updated.
    for (k, v) in next.into_iter() {
        let insert = match current.get(&k) {
            Some(w) if *w == v => None,
            _ => Some(v),
        };
        if let Some(insert) = insert {
            diff.push(insert);
        }
    }

    PowerUpdates(diff)
}

/// Convert the power list to a `HashMap` to support lookups by the public key.
///
/// Unfortunately in their raw format the [`PublicKey`] does not implement `Hash`,
/// so we have to use the serialized format.
fn into_power_map(value: PowerTable) -> HashMap<[u8; 65], Validator<Power>> {
    value
        .0
        .into_iter()
        .map(|v| {
            let k = v.public_key.0.serialize();
            (k, v)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use fendermint_vm_genesis::{Power, Validator};
    use quickcheck_macros::quickcheck;

    use crate::fvm::checkpoint::{
        final_heights_above, into_power_map, newly_final_heights, outputs_to_presign, power_diff,
        resign_plan, should_post_slash_pre_signature,
    };

    use super::{checkpoint::GetSlashOutputReturn, getter, PowerTable, PowerUpdates};

    fn checkpoint_at(height: u64) -> getter::BottomUpCheckpoint {
        getter::BottomUpCheckpoint {
            block_height: height.into(),
            ..Default::default()
        }
    }

    fn heights(plan: &[super::CheckpointToSign]) -> Vec<(u64, bool, bool)> {
        plan.iter()
            .map(|c| {
                (
                    c.checkpoint.block_height.as_u64(),
                    c.checkpoint_signature,
                    c.bitcoin_signature,
                )
            })
            .collect()
    }

    #[test]
    fn resigning_a_missing_bitcoin_signature_while_the_checkpoint_signature_is_in_the_gateway() {
        let plan = resign_plan(vec![], 301, vec![checkpoint_at(200)]);
        assert_eq!(heights(&plan), vec![(200, false, true)]);
        let plan = resign_plan(vec![checkpoint_at(100)], 301, vec![checkpoint_at(100)]);
        assert_eq!(heights(&plan), vec![(100, true, true)]);
        let plan = resign_plan(vec![checkpoint_at(100)], 301, vec![]);
        assert_eq!(heights(&plan), vec![(100, true, false)]);
    }

    #[test]
    fn no_signature_for_a_checkpoint_already_submitted_or_not_final() {
        assert!(resign_plan(vec![], 301, vec![]).is_empty());
        let plan = resign_plan(vec![checkpoint_at(300), checkpoint_at(200)], 300, vec![]);
        assert_eq!(heights(&plan), vec![(200, true, false)]);
        let plan = resign_plan(vec![checkpoint_at(300)], 301, vec![]);
        assert_eq!(heights(&plan), vec![(300, true, false)]);
    }

    fn unslashable(
        txid: &str,
        keys: &[[u8; 32]],
        signer: bool,
    ) -> ipc_api::eots::UnslashableOutput {
        ipc_api::eots::UnslashableOutput {
            txid: txid.to_string(),
            vout: 1,
            keys: keys.iter().map(|k| (*k, 1)).collect(),
            threshold: 2,
            signer,
        }
    }

    #[test]
    fn slash_pre_signatures_of_the_outputs_naming_this_validator() {
        let (a, own) = ([1u8; 32], [2u8; 32]);
        let outputs = vec![
            unslashable("t1", &[a, own], true),
            unslashable("t2", &[own, a], true),
            unslashable("t3", &[a], false),
        ];
        let to_sign: Vec<_> = outputs_to_presign(&own, outputs)
            .into_iter()
            .map(|o| o.txid)
            .collect();
        assert_eq!(to_sign, vec!["t1".to_string(), "t2".to_string()]);
    }

    #[test]
    fn slash_pre_signature_posted_once() {
        let output = |carried: u64, posted: bool| GetSlashOutputReturn {
            carried_height: carried.into(),
            posted,
        };
        // not posted, whether or not a checkpoint carried the output
        assert!(should_post_slash_pre_signature(&output(0, false)));
        assert!(should_post_slash_pre_signature(&output(350, false)));
        // already posted
        assert!(!should_post_slash_pre_signature(&output(0, true)));
        assert!(!should_post_slash_pre_signature(&output(350, true)));
    }

    fn power_update(current: PowerTable, updates: PowerUpdates) -> PowerTable {
        let mut current = into_power_map(current);

        for v in updates.0 {
            let k = v.public_key.0.serialize();
            if v.power.0 == 0 {
                current.remove(&k);
            } else {
                current.insert(k, v);
            }
        }

        PowerTable(current.into_values().collect())
    }

    #[derive(Debug, Clone)]
    struct TestPowerTables {
        current: PowerTable,
        next: PowerTable,
    }

    impl quickcheck::Arbitrary for TestPowerTables {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            let v = 1 + usize::arbitrary(g) % 10;
            let c = 1 + usize::arbitrary(g) % v;
            let n = 1 + usize::arbitrary(g) % v;

            let vs = (0..v).map(|_| Validator::arbitrary(g)).collect::<Vec<_>>();
            let cvs = vs.iter().take(c).cloned().collect();
            let nvs = vs
                .into_iter()
                .skip(v - n)
                .map(|mut v| {
                    v.power = Power::arbitrary(g);
                    v
                })
                .collect();

            TestPowerTables {
                current: PowerTable(cvs),
                next: PowerTable(nvs),
            }
        }
    }

    #[quickcheck]
    fn prop_power_diff_update(powers: TestPowerTables) {
        let diff = power_diff(powers.current.clone(), powers.next.clone());
        let next = power_update(powers.current, diff);

        // Order shouldn't matter.
        let next = into_power_map(next);
        let expected = into_power_map(powers.next);

        assert_eq!(next, expected)
    }

    #[quickcheck]
    fn prop_power_diff_nochange(v1: Validator<Power>, v2: Validator<Power>) {
        let current = PowerTable(vec![v1.clone(), v2.clone()]);
        let next = PowerTable(vec![v2, v1]);
        assert!(power_diff(current, next).0.is_empty());
    }

    #[test]
    fn first_signing_when_next_height_final() {
        assert!(!newly_final_heights(99, 100).contains(&100));
        assert!(newly_final_heights(100, 101).contains(&100));
        assert_eq!(newly_final_heights(101, 350), 101..350);
        assert!(newly_final_heights(0, 1).is_empty());
        assert_eq!(newly_final_heights(0, 201), 1..201);
    }

    #[quickcheck]
    fn prop_first_signing_once(steps: Vec<u8>) {
        let mut prev = 0;
        let mut seen = Vec::new();
        for step in steps {
            let next = prev + u64::from(step);
            seen.extend(newly_final_heights(prev, next));
            prev = next;
        }
        let expected: Vec<u64> = (1..prev).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn resigning_above_checkpointed() {
        assert_eq!(final_heights_above(0, 301), 1..301);
        assert_eq!(final_heights_above(100, 301), 101..301);
        assert!(final_heights_above(300, 301).is_empty());
        assert!(!final_heights_above(100, 300).contains(&300));
    }
}
