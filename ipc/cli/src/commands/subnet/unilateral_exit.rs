// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Unilateral-exit join and exit cli command handlers (Bitcoin parents only).

use async_trait::async_trait;
use clap::Args;
use ipc_api::{subnet_id::SubnetID, token_amount_from_satoshi};
use num_traits::Zero;
use std::{fmt::Debug, str::FromStr};

use crate::{get_ipc_provider, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

/// Joins a subnet with the collateral locked under the validator's own unilateral-exit script
pub struct JoinUe;

#[async_trait]
impl CommandLineHandler for JoinUe {
    type Arguments = JoinUeArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("join-ue with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;
        let subnet = SubnetID::from_str(&arguments.subnet)?;
        // Before any key is selected or any pre-funding is sent
        provider.require_bitcoin_parent(&subnet)?;
        let from = match &arguments.from {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        // The provider sends the prefund transaction after the join's checks, before the join
        let initial_balance = arguments.initial_balance.filter(|x| !x.is_zero());
        if let Some(initial_balance) = initial_balance {
            log::info!("pre-funding address with {initial_balance}");
        }

        let epoch = provider
            .join_subnet(
                subnet,
                from,
                token_amount_from_satoshi(arguments.collateral),
                Some(arguments.ip.clone()),
                Some(arguments.backup_address.clone()),
                Some(arguments.epochs),
                Some((arguments.eots_nonce_start, arguments.eots_nonce_count)),
                initial_balance.map(token_amount_from_satoshi),
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
        help = "The number of epochs for which the collateral seats the validator (between the subnet's min and max timeout)"
    )]
    pub epochs: u32,
    #[arg(
        long,
        default_value_t = 1,
        help = "The first subnet height the EOTS nonce commitment covers"
    )]
    pub eots_nonce_start: u64,
    #[arg(
        long,
        default_value_t = 100_000,
        help = "The number of subnet heights the EOTS nonce commitment covers"
    )]
    pub eots_nonce_count: u32,
}

/// Spends the validator's matured unilateral-exit collaterals to its backup address
pub struct UnstakeUe;

#[async_trait]
impl CommandLineHandler for UnstakeUe {
    type Arguments = UnstakeUeArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("unstake-ue with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;
        let subnet = SubnetID::from_str(&arguments.subnet)?;
        let tx = provider.unstake_ue(subnet).await?;
        println!(
            "unstake-ue transaction submitted: txid {}; {} sats of collateral spent, paid minus the fee to {}",
            tx.txid, tx.amount, tx.address
        );
        Ok(())
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
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Command {
        #[command(flatten)]
        args: JoinUeArgs,
    }

    fn arguments() -> Vec<String> {
        [
            "join-ue",
            "--subnet",
            "/b4/test",
            "--collateral",
            "100000",
            "--ip",
            "192.0.2.1:8080",
            "--backup-address",
            "bcrt1pbackup",
            "--epochs",
            "128",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn join_ue_requires_and_preserves_the_epoch_count() {
        let parsed = Command::try_parse_from(arguments()).unwrap();
        assert_eq!(parsed.args.epochs, 128);
        let mut missing = arguments();
        missing.truncate(missing.len() - 2);
        assert!(Command::try_parse_from(missing).is_err());
    }

    #[test]
    fn join_ue_rejects_negative_and_overwide_epoch_counts() {
        for bad in ["-1", "4294967296"] {
            let mut args = arguments();
            *args.last_mut().unwrap() = bad.into();
            assert!(Command::try_parse_from(args).is_err());
        }
    }
}
