// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Create subnet cli command handler.

use std::fmt::Debug;
use std::str::FromStr;

use async_trait::async_trait;
use clap::Args;
use fvm_shared::clock::ChainEpoch;

use ipc_api::subnet::{BtcConstructParams, ConstructParams};
use ipc_api::subnet_id::SubnetID;
use ipc_provider::config::subnet::NetworkType;

use crate::commands::get_ipc_provider;
use crate::{require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

const ACTIVE_VALIDATORS_LIMIT_ON_BITCOIN: u16 = 100;

/// The command to create a new subnet actor.
pub struct CreateSubnetOnBitcoin;

impl CreateSubnetOnBitcoin {
    pub async fn create(
        global: &GlobalArguments,
        arguments: &CreateSubnetOnBitcoinArgs,
    ) -> anyhow::Result<String> {
        let mut provider = get_ipc_provider(global)?;
        let parent = SubnetID::from_str(&arguments.parent)?;

        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            // TODO Use a bitcoin address
            None => None,
        };

        let conn_to_parent = provider.get_connection(&parent)?;
        let parent_subnet = conn_to_parent.subnet();

        if parent_subnet.network_type() != NetworkType::Btc {
            return Err(anyhow::anyhow!(
                "The type of the parent subnet in the config is not set correctly."
            ));
        }

        let whitelist = arguments
            .whitelist
            .clone()
            .split(",")
            .map(|str| str.to_string())
            .collect();

        let construct_params = ConstructParams::Btc(BtcConstructParams {
            parent: parent.clone(),
            min_validators: arguments.min_validators,
            min_validator_stake: arguments.min_validator_stake,
            bottomup_check_period: arguments.bottomup_check_period,
            active_validators_limit: arguments
                .active_validators_limit
                .unwrap_or(ACTIVE_VALIDATORS_LIMIT_ON_BITCOIN),
            min_cross_msg_fee: arguments.min_cross_msg_fee,
            validator_whitelist: whitelist,
        });

        let addr = provider
            .create_subnet(from, parent, construct_params)
            .await?;
        Ok(addr.to_string())
    }
}

#[async_trait]
impl CommandLineHandler for CreateSubnetOnBitcoin {
    type Arguments = CreateSubnetOnBitcoinArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!(
            "create subnet with bitcoin as parentwith args: {:?}",
            arguments
        );

        let address = CreateSubnetOnBitcoin::create(global, arguments).await?;

        log::info!("created subnet with id: {}/{}", arguments.parent, address);

        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(name = "create", about = "Create a new subnet with bitcoin as parent")]
pub struct CreateSubnetOnBitcoinArgs {
    #[arg(long, help = "The address that creates the subnet")]
    pub from: Option<String>,
    #[arg(long, help = "The parent subnet to create the new actor in")]
    pub parent: String,
    #[arg(
        long,
        help = "The minimum number of collateral required for validators (in satoshis)"
    )]
    pub min_validator_stake: u64,
    #[arg(
        long,
        help = "Minimum number of validators required to bootstrap the subnet"
    )]
    pub min_validators: u64,
    #[arg(long, help = "The bottom up checkpoint period in number of blocks")]
    pub bottomup_check_period: ChainEpoch,
    #[arg(long, help = "The max number of active validators in subnet")]
    pub active_validators_limit: Option<u16>,
    #[arg(
        long,
        default_value = "1",
        help = "Minimum fee for cross-net messages in subnet (in satoshis, the minimum is 1 satoshi)"
    )]
    pub min_cross_msg_fee: u64,
    // #[arg(
    //     long,
    //     help = "The permission mode for the subnet: collateral, federated and static",
    //     value_parser = PermissionMode::from_str,
    // )]
    // pub permission_mode: PermissionMode,
    // #[arg(
    //     long,
    //     help = "The kind of supply source of a subnet on its parent subnet: native or erc20",
    //     value_parser = AssetKind::from_str,
    // )]
    // pub supply_source_kind: AssetKind,
    // #[arg(
    //     long,
    //     help = "The address of supply source of a subnet on its parent subnet. None if kind is native"
    // )]
    // pub supply_source_address: Option<String>,
    // #[arg(
    //     long,
    //     help = "The address of validator gating contract. None if validator gating is disabled"
    // )]
    // pub validator_gater: Option<String>,
    // #[arg(long, help = "The address of validator rewarder contract.")]
    // pub validator_rewarder: Option<String>,
    #[arg(
        long,
        help = "A comma-separated list of bitcoin x-only public keys that can join the subnet before it is bootstrapped"
    )]
    pub whitelist: String,
    // #[arg(
    //     long,
    //     help = "The kind of collateral source of a subnet on its parent subnet: native or erc20",
    //     value_parser = AssetKind::from_str,
    // )]
    // pub collateral_source_kind: Option<AssetKind>,
    // #[arg(
    //     long,
    //     help = "The address of collateral source of a subnet on its parent subnet. None if kind is native"
    // )]
    // pub collateral_source_address: Option<String>,
}
