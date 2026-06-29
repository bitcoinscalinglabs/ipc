// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use anyhow::Context;
use ethers::types as et;
use fendermint_vm_actor_interface::{init::builtin_actor_eth_addr, ipc};
use fvm_ipld_blockstore::Blockstore;
use ipc_actors_abis::reward_config::RewardConfig;
use ipc_actors_abis::reward_token::RewardToken;

use crate::fvm::state::fevm::{ContractCaller, MockProvider, NoRevert};
use crate::fvm::state::{FvmExecState, RewardState};
use ipc_provider::manager::{BtcSubnetManager, SubnetManager};

const REWARD_TOKEN_ACTOR_ID: fvm_shared::ActorID = ipc::REWARD_TOKEN_ACTOR_ID;
const REWARD_CONFIG_ACTOR_ID: fvm_shared::ActorID = ipc::REWARD_CONFIG_ACTOR_ID;

/// Try to mint rewards for the current snapshot. Updates reward state when minting occurs.
pub async fn maybe_mint_rewards<DB>(
    gateway: &crate::fvm::state::ipc::GatewayCaller<DB>,
    state: &mut FvmExecState<DB>,
    parent_manager: Option<&BtcSubnetManager>,
) -> anyhow::Result<()>
where
    DB: Blockstore + Clone + 'static,
{
    // Return early if we're not on the Emission Chain.
    let Some(reward_state) = state.reward_state().cloned() else {
        tracing::info!("skipping reward mint: not on emission chain");
        return Ok(());
    };
    // Err if the parent manager is not set.
    let Some(parent_manager) = parent_manager else {
        anyhow::bail!("parent manager not set");
    };

    let finality = gateway
        .get_latest_parent_finality(state)
        .context("failed to get latest parent finality")?;

    let parent_height = finality.height;

    let config_addr = builtin_actor_eth_addr(REWARD_CONFIG_ACTOR_ID);
    let config_caller: ContractCaller<DB, RewardConfig<MockProvider>, NoRevert> =
        ContractCaller::new(config_addr, RewardConfig::new);

    // activation_height and snapshot_length are read from the RewardConfig contract.
    // But the source of truth for being or not on the Emission Chain is reward_state.
    // We only reach here when reward_state is Some.
    let activation_height = config_caller.call(state, |c| c.activation_height())?;
    let snapshot_length = config_caller.call(state, |c| c.snapshot_length())?;

    if activation_height == 0 || snapshot_length == 0 {
        anyhow::bail!(
            "invalid reward config for emission chain: activation_height={}, snapshot_length={}",
            activation_height,
            snapshot_length
        );
    }

    if parent_height < activation_height {
        tracing::info!(
            parent_height = parent_height,
            activation_height = activation_height,
            "skipping reward mint: before activation height"
        );
        return Ok(());
    }

    // Mint the last completed snapshot; the in-progress one isn't finalized on every validator's monitor yet.
    let current_snapshot = (parent_height - activation_height) / snapshot_length;
    if current_snapshot == 0 {
        return Ok(());
    }
    let snapshot = current_snapshot - 1;

    if reward_state
        .last_minted_snapshot
        .map_or(false, |last| snapshot <= last)
    {
        tracing::info!(
            snapshot = snapshot,
            last_minted_snapshot = ?reward_state.last_minted_snapshot,
            "skipping reward mint: snapshot already minted"
        );
        return Ok(());
    }

    tracing::info!("processing reward mint for emission chain at height: {parent_height} for snapshot: {snapshot}");

    let tokens_per_snapshot = config_caller
        .call(state, |c| c.tokens_per_snapshot(et::U256::from(snapshot)))?
        .as_u128();

    let mut response = parent_manager
        .get_rewarded_collaterals(snapshot)
        .await
        .context("failed to get rewarded collaterals")?;

    // Provider returns HashMap order (per-process-random); sort so mints apply identically on every validator.
    response.collaterals.sort_by(|a, b| a.0.cmp(&b.0));

    tracing::info!(
        snapshot = snapshot,
        total_rewarded_collateral = response.total_rewarded_collateral,
        collaterals_count = response.collaterals.len(),
        "get_rewarded_collaterals result"
    );

    if response.collaterals.is_empty() || response.total_rewarded_collateral == 0 {
        tracing::info!(
            snapshot = snapshot,
            "reward mint completed with no collaterals to reward"
        );
    } else {
        let total = response.total_rewarded_collateral as u128;
        let token_addr = builtin_actor_eth_addr(REWARD_TOKEN_ACTOR_ID);
        let reward_token: ContractCaller<DB, RewardToken<MockProvider>, NoRevert> =
            ContractCaller::new(token_addr, RewardToken::new);

        // tokens_per_snapshot is in whole tokens; scale to wei for ERC20 mint (18 decimals).
        let unit = 10u128.pow(ipc::reward_token::DECIMALS as u32);

        for (addr, amount_sats) in &response.collaterals {
            let amount = *amount_sats as u128;
            let mint_tokens = (amount * tokens_per_snapshot) / total;
            if mint_tokens == 0 {
                continue;
            }
            let mint_amount = et::U256::from(mint_tokens * unit);
            // Keep `.from(...)` unset so ContractCaller defaults to system::SYSTEM_ACTOR_ADDR (t00).
            match reward_token.call_with_return(state, |c| c.mint(*addr, mint_amount)) {
                Ok(_) => {
                    tracing::info!(
                        addr = %addr,
                        mint_amount = mint_tokens,
                        "reward minted successfully"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        addr = %addr,
                        mint_amount = %mint_tokens,
                        error = %e,
                        "reward mint failed for address, skipping"
                    );
                }
            }
        }
        tracing::info!(snapshot = snapshot, "reward mint completed");
    }

    // TODO: Handle failures in minting rewards. Only advance `last_minted_snapshot` when all mints for this snapshot succeed.
    state.update_reward_state(|rs| {
        *rs = Some(RewardState {
            last_minted_snapshot: Some(snapshot),
        })
    });
    tracing::info!("reward state updated to {:?}", state.reward_state());
    Ok(())
}
