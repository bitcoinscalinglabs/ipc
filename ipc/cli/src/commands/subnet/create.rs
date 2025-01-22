// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Create subnet cli command handler.

use std::fmt::Debug;
use std::str::FromStr;

use async_trait::async_trait;
use clap::{Args, Subcommand};
use fvm_shared::address::Address;
use fvm_shared::clock::ChainEpoch;

use ipc_api::subnet::{
    Asset, AssetKind, BtcConstructParams, ConsensusType, ConstructParams, EthConstructParams,
    PermissionMode,
};
use ipc_api::universal_subnet_id::UniversalSubnetId;
use ipc_provider::config::subnet::NetworkType;
use ipc_provider::config::Subnet;
use ipc_provider::IpcProvider;

use crate::commands::get_ipc_provider;
use crate::commands::subnet::ZERO_ADDRESS;
use crate::{f64_to_token_amount, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

const DEFAULT_ACTIVE_VALIDATORS: u16 = 100;

/// The command to create a new subnet actor.
pub struct CreateSubnet;

impl CreateSubnet {
    pub async fn create(
        global: &GlobalArguments,
        arguments: &CreateSubnetArgs,
    ) -> anyhow::Result<String> {
        let mut provider = get_ipc_provider(global)?;
        let parent = UniversalSubnetId::from_str(&arguments.parent)?;

        let conn_to_parent = provider.get_connection(&parent)?;
        let parent_subnet = conn_to_parent.subnet();

        match parent_subnet.network_type() {
            NetworkType::Fevm => match &arguments.network_specific {
                SpecifiedNetwork::Fevm(args) => {
                    Self::create_fevm(&mut provider, parent, parent_subnet, arguments, args).await
                }
                _ => Err(anyhow::anyhow!(
                    "FEVM-specific arguments are required for FEVM parent subnet"
                )),
            },
            NetworkType::Btc => match &arguments.network_specific {
                SpecifiedNetwork::Btc(args) => {
                    Self::create_btc(&mut provider, parent, arguments, args).await
                }
                _ => Err(anyhow::anyhow!(
                    "BTC-specific arguments are required for BTC parent subnet"
                )),
            },
        }
    }

    async fn create_fevm(
        provider: &mut IpcProvider,
        parent: UniversalSubnetId,
        parent_subnet: &Subnet,
        arguments: &CreateSubnetArgs,
        fevm_args: &FevmArgs,
    ) -> anyhow::Result<String> {
        let from = match &fevm_args.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        let supply_source = parse_supply_source(fevm_args)?;
        let collateral_source = parse_collateral_source(fevm_args)?;

        let raw_addr = fevm_args
            .validator_gater
            .clone()
            .unwrap_or(ZERO_ADDRESS.to_string());
        let validator_gater = require_fil_addr_from_str(&raw_addr)?;

        let raw_addr = fevm_args
            .validator_rewarder
            .clone()
            .unwrap_or(ZERO_ADDRESS.to_string());
        let validator_rewarder = require_fil_addr_from_str(&raw_addr)?;

        let construct_params = ConstructParams::Eth(EthConstructParams {
            parent: parent.clone(),
            ipc_gateway_addr: parent_subnet.gateway_addr(),
            consensus: ConsensusType::Fendermint,
            min_validators: arguments.min_validators,
            min_validator_stake: f64_to_token_amount(fevm_args.min_validator_stake)?,
            bottomup_check_period: arguments.bottomup_check_period,
            active_validators_limit: arguments
                .active_validators_limit
                .unwrap_or(DEFAULT_ACTIVE_VALIDATORS),
            min_cross_msg_fee: f64_to_token_amount(fevm_args.min_cross_msg_fee)?,
            permission_mode: fevm_args.permission_mode,
            supply_source,
            collateral_source,
            validator_gater,
            validator_rewarder,
        });

        let addr = provider
            .create_subnet(from, parent, construct_params)
            .await?;
        Ok(addr.to_string())
    }

    async fn create_btc(
        provider: &mut IpcProvider,
        parent: UniversalSubnetId,
        arguments: &CreateSubnetArgs,
        btc_args: &BtcArgs,
    ) -> anyhow::Result<String> {
        let whitelist = btc_args
            .validator_whitelist
            .split(',')
            .map(|str| str.to_string())
            .collect();

        let construct_params = ConstructParams::Btc(BtcConstructParams {
            parent: parent.clone(),
            min_validators: arguments.min_validators,
            min_validator_stake: btc_args.min_validator_stake,
            bottomup_check_period: arguments.bottomup_check_period,
            active_validators_limit: arguments
                .active_validators_limit
                .unwrap_or(DEFAULT_ACTIVE_VALIDATORS),
            min_cross_msg_fee: btc_args.min_cross_msg_fee,
            validator_whitelist: whitelist,
        });

        let addr = provider
            .create_subnet(None, parent, construct_params)
            .await?;
        Ok(addr.to_string())
    }
}

fn parse_supply_source(fevm_args: &FevmArgs) -> anyhow::Result<Asset> {
    let token_address = if let Some(addr) = &fevm_args.supply_source_address {
        Some(require_fil_addr_from_str(addr)?)
    } else {
        None
    };
    Ok(Asset {
        kind: fevm_args.supply_source_kind,
        token_address,
    })
}

fn parse_collateral_source(fevm_args: &FevmArgs) -> anyhow::Result<Asset> {
    let Some(ref kind) = fevm_args.collateral_source_kind else {
        return Ok(Asset::default());
    };

    let token_address = if let Some(addr) = &fevm_args.collateral_source_address {
        Some(require_fil_addr_from_str(addr)?)
    } else {
        None
    };

    Ok(Asset {
        kind: *kind,
        token_address,
    })
}

#[async_trait]
impl CommandLineHandler for CreateSubnet {
    type Arguments = CreateSubnetArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("create subnet with args: {:?}", arguments);

        let address = CreateSubnet::create(global, arguments).await?;

        log::info!(
            "created subnet actor with id: {}/{}",
            arguments.parent,
            address
        );

        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(name = "create", about = "Create a new subnet")]
pub struct CreateSubnetArgs {
    #[arg(long, help = "The parent subnet to create the new actor in")]
    pub parent: String,

    #[arg(
        long,
        help = "Minimum number of validators required to bootstrap the subnet"
    )]
    pub min_validators: u64,

    #[arg(long, help = "The bottom up checkpoint period in number of blocks")]
    pub bottomup_check_period: ChainEpoch,

    #[arg(long, help = "The max number of active validators in subnet")]
    pub active_validators_limit: Option<u16>,

    #[command(subcommand)]
    pub network_specific: SpecifiedNetwork,
}

#[derive(Debug, Subcommand)]
pub enum SpecifiedNetwork {
    #[command(name = "fevm")]
    Fevm(FevmArgs),
    #[command(name = "btc")]
    Btc(BtcArgs),
}

#[derive(Debug, Args)]
pub struct FevmArgs {
    #[arg(long, help = "The address that creates the subnet")]
    pub from: Option<String>,

    #[arg(
        long,
        help = "The minimum number of collateral required for validators in (in whole FIL; the minimum is 1 nanoFIL)"
    )]
    pub min_validator_stake: f64,

    #[arg(
        long,
        default_value = "0.000001",
        help = "Minimum fee for cross-net messages in subnet (in whole FIL; the minimum is 1 nanoFIL)"
    )]
    pub min_cross_msg_fee: f64,

    #[arg(
        long,
        help = "The permission mode for the subnet: collateral, federated and static",
        value_parser = PermissionMode::from_str,
    )]
    pub permission_mode: PermissionMode,

    #[arg(
        long,
        help = "The kind of supply source of a subnet on its parent subnet: native or erc20",
        value_parser = AssetKind::from_str,
    )]
    pub supply_source_kind: AssetKind,

    #[arg(
        long,
        help = "The address of supply source of a subnet on its parent subnet. None if kind is native"
    )]
    pub supply_source_address: Option<String>,
    #[arg(
        long,
        help = "The address of validator gating contract. None if validator gating is disabled"
    )]
    pub validator_gater: Option<String>,
    #[arg(long, help = "The address of validator rewarder contract.")]
    pub validator_rewarder: Option<String>,
    #[arg(
        long,
        help = "The kind of collateral source of a subnet on its parent subnet: native or erc20",
        value_parser = AssetKind::from_str,
    )]
    pub collateral_source_kind: Option<AssetKind>,
    #[arg(
        long,
        help = "The address of collateral source of a subnet on its parent subnet. None if kind is native"
    )]
    pub collateral_source_address: Option<String>,
}

#[derive(Debug, Args)]
pub struct BtcArgs {
    #[arg(
        long,
        help = "The minimum number of collateral required for validators (in satoshis)"
    )]
    pub min_validator_stake: u64,

    #[arg(
        long,
        default_value = "1",
        help = "Minimum fee for cross-net messages in subnet (in satoshis, the minimum is 1 satoshi)"
    )]
    pub min_cross_msg_fee: u64,

    #[arg(
        long,
        help = "A comma-separated list of bitcoin x-only public keys that can join the subnet before it is bootstrapped"
    )]
    pub validator_whitelist: String,
}
