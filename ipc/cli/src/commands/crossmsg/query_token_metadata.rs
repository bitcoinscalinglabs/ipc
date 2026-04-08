// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use clap::Args;
use ipc_api::evm::payload_to_evm_address;
use ipc_api::subnet_id::SubnetID;
use std::{fmt::Debug, str::FromStr};

use crate::{get_ipc_provider, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

/// Query token metadata (and the WrappedToken address, if applicable) for a given
/// (homeSubnet, homeToken) pair on a subnet.
pub(crate) struct QueryTokenMetadata;

#[async_trait]
impl CommandLineHandler for QueryTokenMetadata {
    type Arguments = QueryTokenMetadataArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("query-token-metadata operation with args: {:?}", arguments);

        let provider = get_ipc_provider(global)?;

        let subnet = SubnetID::from_str(&arguments.subnet)?;
        let home_subnet = SubnetID::from_str(&arguments.home_subnet)?;
        let home_token = require_fil_addr_from_str(&arguments.home_token)?;

        let gateway_addr = match &arguments.gateway_address {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        let metadata = provider
            .get_token_metadata(&subnet, gateway_addr, home_subnet.clone(), home_token)
            .await?;

        println!(
            "Token metadata for (home_subnet={}, home_token={}) on {}:",
            home_subnet, arguments.home_token, subnet
        );
        println!("  name:     {}", metadata.name);
        println!("  symbol:   {}", metadata.symbol);
        println!("  decimals: {}", metadata.decimals);

        if subnet == home_subnet {
            // Home subnet: by design holds the original token, not a wrapped copy.
            println!("  (this subnet is the token's home subnet — no wrapped contract)");
        } else {
            // Non-home subnet: always print the wrapped: line.
            let wrapped = provider
                .get_wrapped_token(&subnet, gateway_addr, home_subnet, home_token)
                .await?;
            let wrapped_eth = payload_to_evm_address(wrapped.payload())?;
            if wrapped_eth == ethers::types::Address::zero() {
                println!(
                    "  wrapped:  0x0000000000000000000000000000000000000000  (not yet deployed — no transfer received)"
                );
            } else {
                println!("  wrapped:  {:?}", wrapped_eth);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(
    about = "Query stored token metadata (and WrappedToken address) for a (homeSubnet, homeToken) pair on a subnet"
)]
pub(crate) struct QueryTokenMetadataArgs {
    #[arg(long, help = "The subnet to query")]
    pub subnet: String,
    #[arg(long, help = "The home subnet of the original token")]
    pub home_subnet: String,
    #[arg(long, help = "The token address on the home subnet")]
    pub home_token: String,
    #[arg(long, help = "The gateway address on the queried subnet (optional)")]
    pub gateway_address: Option<String>,
}
