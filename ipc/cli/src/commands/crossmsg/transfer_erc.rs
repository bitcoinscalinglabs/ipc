// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use clap::Args;
use fvm_shared::econ::TokenAmount;
use ipc_api::subnet_id::SubnetID;
use std::{fmt::Debug, str::FromStr};

use crate::{get_ipc_provider, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

/// The command to perform a cross-subnet ERC20 token transfer
pub(crate) struct TransferErc;

#[async_trait]
impl CommandLineHandler for TransferErc {
    type Arguments = TransferErcArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("transfer-erc operation with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;

        let source_subnet = SubnetID::from_str(&arguments.source_subnet)?;
        let destination_subnet = SubnetID::from_str(&arguments.destination_subnet)?;

        let source_address = match &arguments.source_address {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        let destination_address = require_fil_addr_from_str(&arguments.destination_address)?;
        let local_token = require_fil_addr_from_str(&arguments.token)?;

        let source_gateway_addr = match &arguments.source_gateway_address {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        // ERC20 amounts are in the token's smallest unit — no satoshi-to-atto conversion.
        let amount = TokenAmount::from_atto(arguments.amount);

        println!(
            "transfer-erc performed in epoch: {:?}",
            provider
                .transfer_erc_token(
                    source_gateway_addr,
                    source_subnet,
                    destination_subnet,
                    source_address,
                    destination_address,
                    local_token,
                    amount,
                )
                .await?,
        );
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(about = "Cross-subnet ERC20 token transfer")]
pub(crate) struct TransferErcArgs {
    #[arg(long, help = "The gateway address of the source subnet")]
    pub source_gateway_address: Option<String>,
    #[arg(long, help = "The source subnet")]
    pub source_subnet: String,
    #[arg(long, help = "The address (in the source subnet) to send tokens from")]
    pub source_address: Option<String>,
    #[arg(long, help = "The destination subnet")]
    pub destination_subnet: String,
    #[arg(long, help = "The address (in the destination subnet) to send tokens to")]
    pub destination_address: String,
    #[arg(long, help = "The local ERC20 or WrappedToken address on the source subnet")]
    pub token: String,
    #[arg(help = "The token amount to transfer (in the token's smallest unit, as integer)")]
    pub amount: u64,
}
