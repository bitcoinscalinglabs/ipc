use async_trait::async_trait;
use clap::Args;
use ipc_api::{subnet_id::SubnetID, token_amount_from_satoshi};
use std::{fmt::Debug, str::FromStr};

use crate::{get_ipc_provider, require_fil_addr_from_str, CommandLineHandler, GlobalArguments};

/// The command to send funds to a subnet from parent
pub(crate) struct Transfer;

#[async_trait]
impl CommandLineHandler for Transfer {
    type Arguments = TransferArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        log::debug!("transfer operation with args: {:?}", arguments);

        let mut provider = get_ipc_provider(global)?;

        let source_subnet = SubnetID::from_str(&arguments.source_subnet)?;

        let destination_subnet = SubnetID::from_str(&arguments.destination_subnet)?;

        let source_address = match &arguments.source_address {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        let destination_address = require_fil_addr_from_str(&arguments.destination_address)?;

        let source_gateway_addr = match &arguments.source_gateway_address {
            Some(address) => Some(require_fil_addr_from_str(address)?),
            None => None,
        };

        let amount = token_amount_from_satoshi(arguments.amount);
        println!(
            "fund performed in epoch: {:?}",
            provider
                .transfer(
                    source_gateway_addr,
                    source_subnet,
                    destination_subnet,
                    source_address,
                    destination_address,
                    amount,
                )
                .await?,
        );
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(about = "Transfer funds between subnets")]
pub(crate) struct TransferArgs {
    #[arg(long, help = "The gateway address of the source subnet")]
    pub source_gateway_address: Option<String>,
    #[arg(long, help = "The source subnet")]
    pub source_subnet: String,
    #[arg(long, help = "The address (in the source subnet) to send funds from")]
    pub source_address: Option<String>,
    #[arg(long, help = "The destination subnet")]
    pub destination_subnet: String,
    #[arg(
        long,
        help = "The address (in the destination subnet) to send funds to"
    )]
    pub destination_address: String,
    #[arg(help = "The amount to transfer (in sats)")]
    pub amount: u64,
}
