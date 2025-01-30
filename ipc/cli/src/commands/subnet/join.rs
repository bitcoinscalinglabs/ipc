// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Join subnet cli command handler.

use async_trait::async_trait;
use clap::{Args, Subcommand};

use ipc_api::subnet_id::SubnetID;
use ipc_provider::{config::subnet::NetworkType, IpcProvider};

use num_traits::Zero;
use std::{fmt::Debug, str::FromStr};

use crate::{
    f64_to_token_amount, get_ipc_provider, require_fil_addr_from_str, CommandLineHandler,
    GlobalArguments,
};

/// The command to join a subnet
pub struct JoinSubnet;

impl JoinSubnet {
    async fn join(global: &GlobalArguments, arguments: &JoinSubnetArgs) -> anyhow::Result<()> {
        log::debug!("join subnet with args: {:?}", arguments);
        let subnet_id = SubnetID::from_str(&arguments.subnet)?;
        let mut provider = get_ipc_provider(global)?;

        let conn_to_parent = provider.get_connection(
            &subnet_id
                .parent()
                .ok_or(anyhow::anyhow!("subnet has no parent"))?,
        )?;
        let parent_subnet = conn_to_parent.subnet();

        match parent_subnet.network_type() {
            NetworkType::Fevm => match &arguments.network_specific {
                SpecifiedNetwork::Fevm(fevm_args) => {
                    Self::join_fevm(&mut provider, subnet_id, arguments, fevm_args).await
                }
                _ => Err(anyhow::anyhow!(
                    "FEVM-specific arguments are required for FEVM parent subnet"
                )),
            },
            NetworkType::Btc => match &arguments.network_specific {
                SpecifiedNetwork::Btc(btc_args) => {
                    Self::join_btc(&mut provider, subnet_id, arguments, btc_args).await
                }
                _ => Err(anyhow::anyhow!(
                    "BTC-specific arguments are required for BTC parent subnet"
                )),
            },
        }
    }

    async fn join_fevm(
        provider: &mut IpcProvider,
        subnet_id: SubnetID,
        arguments: &JoinSubnetArgs,
        fevm_args: &FevmJoinArgs,
    ) -> anyhow::Result<()> {
        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        if let Some(initial_balance) = arguments.initial_balance.filter(|x| !x.is_zero()) {
            log::info!("pre-funding address with {initial_balance}");
            provider
                .pre_fund(
                    subnet_id.clone(),
                    from,
                    f64_to_token_amount(initial_balance)?,
                )
                .await?;
        };

        let epoch = provider
            .join_subnet(subnet_id, from, fevm_args.collateral, None, None)
            .await?;
        println!("joined at epoch: {epoch}");

        Ok(())
    }

    async fn join_btc(
        provider: &mut IpcProvider,
        subnet_id: SubnetID,
        arguments: &JoinSubnetArgs,
        btc_args: &BtcJoinArgs,
    ) -> anyhow::Result<()> {
        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        if let Some(_) = arguments.initial_balance.filter(|x| !x.is_zero()) {
            unimplemented!("pre-funding not yet implemented for BTC");
        }

        let epoch = provider
            .join_subnet(
                subnet_id,
                from,
                btc_args.collateral as f64,
                Some(btc_args.ip.clone()),
                Some(btc_args.backup_address.clone()),
            )
            .await?;
        println!("joined at epoch: {epoch}");

        Ok(())
    }
}

#[async_trait]
impl CommandLineHandler for JoinSubnet {
    type Arguments = JoinSubnetArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("join subnet with args: {:?}", arguments);

        JoinSubnet::join(global, arguments).await
    }
}

#[derive(Debug, Args)]
#[command(name = "join", about = "Join a subnet")]
pub struct JoinSubnetArgs {
    #[arg(long, help = "The address that joins the subnet")]
    pub from: Option<String>,
    #[arg(long, help = "The subnet to join")]
    pub subnet: String,
    #[arg(
        long,
        help = "Optionally add an initial balance to the validator in genesis in the subnet"
    )]
    pub initial_balance: Option<f64>,

    #[command(subcommand)]
    pub network_specific: SpecifiedNetwork,
}

#[derive(Debug, Subcommand)]
pub enum SpecifiedNetwork {
    #[command(name = "fevm")]
    Fevm(FevmJoinArgs),
    #[command(name = "btc")]
    Btc(BtcJoinArgs),
}

#[derive(Debug, Args)]
pub struct FevmJoinArgs {
    #[arg(
        long,
        help = "The collateral to stake in the subnet (in whole FIL units)"
    )]
    pub collateral: f64,
}

#[derive(Debug, Args)]
pub struct BtcJoinArgs {
    #[arg(long, help = "The collateral to stake in the subnet (in sats)")]
    pub collateral: u64,
    #[arg(long, help = "The IP address of the validator")]
    pub ip: String,
    #[arg(long, help = "The backup address of the validator")]
    pub backup_address: String,
}

/// The command to stake in a subnet from validator
pub struct StakeSubnet;

#[async_trait]
impl CommandLineHandler for StakeSubnet {
    type Arguments = StakeSubnetArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("join subnet with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;
        let subnet = SubnetID::from_str(&arguments.subnet)?;
        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };
        provider
            .stake(subnet, from, f64_to_token_amount(arguments.collateral)?)
            .await
    }
}

#[derive(Debug, Args)]
#[command(name = "stake", about = "Add collateral to an already joined subnet")]
pub struct StakeSubnetArgs {
    #[arg(long, help = "The address that stakes in the subnet")]
    pub from: Option<String>,
    #[arg(long, help = "The subnet to add collateral to")]
    pub subnet: String,
    #[arg(
        long,
        help = "The collateral to stake in the subnet (in whole FIL units)"
    )]
    pub collateral: f64,
}

/// The command to unstake in a subnet from validator
pub struct UnstakeSubnet;

#[async_trait]
impl CommandLineHandler for UnstakeSubnet {
    type Arguments = UnstakeSubnetArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("join subnet with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;
        let subnet = SubnetID::from_str(&arguments.subnet)?;
        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };
        provider
            .unstake(subnet, from, f64_to_token_amount(arguments.collateral)?)
            .await
    }
}

#[derive(Debug, Args)]
#[command(
    name = "unstake",
    about = "Remove collateral to an already joined subnet"
)]
pub struct UnstakeSubnetArgs {
    #[arg(long, help = "The address that unstakes in the subnet")]
    pub from: Option<String>,
    #[arg(long, help = "The subnet to release collateral from")]
    pub subnet: String,
    #[arg(
        long,
        help = "The collateral to unstake from the subnet (in whole FIL units)"
    )]
    pub collateral: f64,
}
