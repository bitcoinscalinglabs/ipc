// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Unilateral-exit join and exit cli command handlers (Bitcoin parents only).

use async_trait::async_trait;
use clap::Args;
use ipc_api::{subnet_id::SubnetID, token_amount_from_satoshi};
use num_traits::Zero;
use std::{fmt::Debug, str::FromStr};

use crate::{get_ipc_provider, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

/// Join a subnet with the collateral locked under the validator's own unilateral-exit script
pub struct JoinUe;

#[async_trait]
impl CommandLineHandler for JoinUe {
    type Arguments = JoinUeArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("join-ue with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;
        let subnet = SubnetID::from_str(&arguments.subnet)?;
        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        if let Some(initial_balance) = arguments.initial_balance.filter(|x| !x.is_zero()) {
            log::info!("pre-funding address with {initial_balance}");
            provider
                .pre_fund(
                    subnet.clone(),
                    from,
                    token_amount_from_satoshi(initial_balance),
                )
                .await?;
        };

        let epoch = provider
            .join_subnet(
                subnet,
                from,
                token_amount_from_satoshi(arguments.collateral),
                Some(arguments.ip.clone()),
                Some(arguments.backup_address.clone()),
                Some(arguments.lock_blocks),
            )
            .await?;
        println!("join-ue transaction submitted at height: {epoch}");
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(
    name = "join-ue",
    about = "Join a subnet with the collateral in the validator's own output, spendable by it alone from a block it chooses (Bitcoin parents only)"
)]
pub struct JoinUeArgs {
    #[arg(long, help = "The address that joins the subnet")]
    pub from: Option<String>,
    #[arg(long, help = "The subnet to join")]
    pub subnet: String,
    #[arg(long, help = "The collateral to lock (in sats)")]
    pub collateral: u64,
    #[arg(
        long,
        help = "Optionally add an initial balance to the validator in genesis in the subnet (in sats)"
    )]
    pub initial_balance: Option<u64>,
    #[arg(long, help = "The IP address of the validator")]
    pub ip: String,
    #[arg(long, help = "The backup address of the validator, paid by unstake-ue")]
    pub backup_address: String,
    #[arg(
        long,
        help = "The collateral matures this many blocks after the current Bitcoin tip (at least 3)"
    )]
    pub lock_blocks: u64,
}

/// Spend the validator's matured unilateral-exit collaterals to its backup address
pub struct UnstakeUe;

#[async_trait]
impl CommandLineHandler for UnstakeUe {
    type Arguments = UnstakeUeArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("unstake-ue with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;
        let subnet = SubnetID::from_str(&arguments.subnet)?;
        provider.unstake_ue(subnet).await
    }
}

#[derive(Debug, Args)]
#[command(
    name = "unstake-ue",
    about = "Spend the validator's matured unilateral-exit collaterals to its backup address (Bitcoin parents only)"
)]
pub struct UnstakeUeArgs {
    #[arg(long, help = "The subnet the validator joined with join-ue")]
    pub subnet: String,
}
