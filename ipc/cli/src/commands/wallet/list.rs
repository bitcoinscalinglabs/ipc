// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
//! Wallet list cli handler

use async_trait::async_trait;
use clap::Args;
use ipc_api::ethers_address_to_fil_address;
use ipc_wallet::{EthKeyAddress, EvmKeyStore, WalletType};
use std::fmt::Debug;
use std::str::FromStr;

use crate::{get_ipc_provider, CommandLineHandler, GlobalArguments};

pub(crate) struct WalletList;

#[async_trait]
impl CommandLineHandler for WalletList {
    type Arguments = WalletListArgs;

    async fn handle(global: &GlobalArguments, arguments: &Self::Arguments) -> anyhow::Result<()> {
        let provider = get_ipc_provider(global)?;
        let wallet_type = WalletType::from_str(&arguments.wallet_type)?;
        match wallet_type {
            WalletType::Evm => {
                let wallet = provider.evm_wallet()?;
                let addresses = wallet.read().unwrap().list()?;
                for address in addresses.iter() {
                    if *address == EthKeyAddress::default() {
                        continue;
                    }
                    print!("Address: {}", address);

                    let key_info = wallet.read().unwrap().get(address)?.unwrap();
                    let sk = libsecp256k1::SecretKey::parse_slice(key_info.private_key())?;
                    let pub_key =
                        hex::encode(libsecp256k1::PublicKey::from_secret_key(&sk).serialize())
                            .to_string();
                    println!("\tPubKey: {}", pub_key);
                }
                Ok(())
            }
            WalletType::Fvm => {
                let wallet = provider.fvm_wallet()?;
                let addresses = wallet.read().unwrap().list_addrs()?;
                for address in addresses.iter() {
                    print!("Address: {}", address);

                    let key_info = wallet.write().unwrap().export(address)?;
                    let sk = libsecp256k1::SecretKey::parse_slice(key_info.private_key())?;
                    let pub_key =
                        hex::encode(libsecp256k1::PublicKey::from_secret_key(&sk).serialize())
                            .to_string();
                    print!("\tPubKey: {}", pub_key);
                    println!("\tKeyType: {:?}", key_info.key_type());
                }
                Ok(())
            }
            WalletType::Btc => {
                let wallet = provider.evm_wallet()?;
                let addresses = wallet.read().unwrap().list()?;
                for address in addresses.iter() {
                    let key_info = wallet.read().unwrap().get(address)?.unwrap();
                    let sk =
                        match ipc_wallet::parse_and_validate_secret_key(&key_info.private_key()) {
                            Ok(sk) => sk,
                            Err(_) => {
                                log::trace!("Ingored non-bitcoin-compatible key");
                                continue;
                            }
                        };

                    let pub_key =
                        hex::encode(libsecp256k1::PublicKey::from_secret_key(&sk).serialize())
                            .to_string();
                    let x_only_pub_key =
                        hex::encode(ipc_wallet::get_xonly_public_key_serialized(&sk)?.to_vec());

                    println!("Address: {}", address);
                    println!("\tSecret key: {}", hex::encode(sk.serialize()));
                    println!("\tPubKey: {}", pub_key);
                    println!("\tXonlyPubKey: {}", x_only_pub_key);

                    let eth_addr: ethers::types::Address = address.clone().into();
                    // let fvm_address = Address::new_delegated(BTC_NAMESPACE, eth_addr.as_bytes())?;
                    let fvm_address = ethers_address_to_fil_address(&eth_addr)?;
                    println!("\tFilAddress: {}", fvm_address);
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Args)]
#[command(about = "List addresses and pubkeys in the wallet")]
pub(crate) struct WalletListArgs {
    #[arg(long, help = "The type of the wallet, i.e. fvm, evm, btc")]
    pub wallet_type: String,
}
