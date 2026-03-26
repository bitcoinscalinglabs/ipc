// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use clap::Args;
use ipc_api::subnet_id::SubnetID;
use std::{fmt::Debug, str::FromStr};

use crate::{get_ipc_provider, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

/// Query the WrappedToken address for a given (homeSubnet, homeToken) pair on a subnet
pub(crate) struct QueryWrappedToken;

#[async_trait]
impl CommandLineHandler for QueryWrappedToken {
    type Arguments = QueryWrappedTokenArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("query-wrapped-token operation with args: {:?}", arguments);

        let provider = get_ipc_provider(global)?;

        let subnet = SubnetID::from_str(&arguments.subnet)?;
        let home_subnet = SubnetID::from_str(&arguments.home_subnet)?;
        let home_token = require_fil_addr_from_str(&arguments.home_token)?;

        let gateway_addr = match &arguments.gateway_address {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        let wrapped = provider
            .get_wrapped_token(&subnet, gateway_addr, home_subnet, home_token)
            .await?;

        println!("{wrapped}");
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(about = "Query the WrappedToken address for a (homeSubnet, homeToken) pair on a subnet")]
pub(crate) struct QueryWrappedTokenArgs {
    #[arg(long, help = "The subnet to query (where the wrapped token lives)")]
    pub subnet: String,
    #[arg(long, help = "The home subnet of the original token")]
    pub home_subnet: String,
    #[arg(long, help = "The token address on the home subnet")]
    pub home_token: String,
    #[arg(long, help = "The gateway address on the queried subnet (optional)")]
    pub gateway_address: Option<String>,
}
