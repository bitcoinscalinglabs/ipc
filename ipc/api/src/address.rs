// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
use crate::error::Error;
use crate::subnet_id::SubnetID;
use crate::{deserialize_human_readable_str, HumanReadable};
use anyhow::{anyhow, bail, Context};
use ethers_core::types as et;
use fvm_shared::address::{Address, Payload, Protocol};
use ipc_types::{EthAddress, EAM_ACTOR_ID};
use serde::ser::Error as SerializeError;
use serde_tuple::{Deserialize_tuple, Serialize_tuple};
use std::{fmt, str::FromStr};

const IPC_SEPARATOR_ADDR: &str = ":";

#[derive(Clone, PartialEq, Eq, Debug, Hash, Serialize_tuple, Deserialize_tuple)]
pub struct IPCAddress {
    subnet_id: SubnetID,
    raw_address: Address,
}

impl IPCAddress {
    /// Generates new IPC address
    pub fn new(sn: &SubnetID, addr: &Address) -> Result<Self, Error> {
        Ok(Self {
            subnet_id: sn.clone(),
            raw_address: *addr,
        })
    }

    /// Returns subnets of a IPC address
    pub fn subnet(&self) -> Result<SubnetID, Error> {
        Ok(self.subnet_id.clone())
    }

    /// Returns the raw address of a IPC address (without subnet context)
    pub fn raw_addr(&self) -> Result<Address, Error> {
        Ok(self.raw_address)
    }

    /// Returns encoded bytes of Address
    #[cfg(feature = "fil-actor")]
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(fil_actors_runtime::cbor::serialize(self, "ipc-address")?.to_vec())
    }

    #[cfg(feature = "fil-actor")]
    pub fn from_bytes(bz: &[u8]) -> Result<Self, Error> {
        let i: Self = fil_actors_runtime::cbor::deserialize(
            &fvm_ipld_encoding::RawBytes::new(bz.to_vec()),
            "ipc-address",
        )?;
        Ok(i)
    }

    pub fn to_string(&self) -> Result<String, Error> {
        Ok(format!(
            "{}{}{}",
            self.subnet_id, IPC_SEPARATOR_ADDR, self.raw_address
        ))
    }

    /// Checks if a raw address has a valid Filecoin address protocol
    /// compatible with cross-net messages targetting a contract
    pub fn is_valid_contract_address(addr: &Address) -> bool {
        matches!(addr.protocol(), Protocol::Delegated | Protocol::Actor)
    }

    /// Checks if a raw address has a valid Filecoin address protocol
    /// compatible with cross-net messages targetting a user account
    pub fn is_valid_account_address(addr: &Address) -> bool {
        // we support `Delegated` as a type for a valid account address
        // so we can send funds to eth addresses using cross-net primitives.
        // this may require additional care when executing in FEVM so we don't
        // send funds to a smart contract.
        matches!(
            addr.protocol(),
            Protocol::Delegated | Protocol::BLS | Protocol::Secp256k1 | Protocol::ID
        )
    }
}

impl fmt::Display for IPCAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.subnet_id, IPC_SEPARATOR_ADDR)?;
        write!(f, "{}", self.raw_address)
    }
}

impl FromStr for IPCAddress {
    type Err = Error;

    fn from_str(addr: &str) -> Result<Self, Error> {
        let r: Vec<&str> = addr.split(IPC_SEPARATOR_ADDR).collect();
        if r.len() != 2 {
            Err(Error::InvalidIPCAddr)
        } else {
            Ok(Self {
                raw_address: Address::from_str(r[1])?,
                subnet_id: SubnetID::from_str(r[0])?,
            })
        }
    }
}

impl serde_with::SerializeAs<IPCAddress> for HumanReadable {
    fn serialize_as<S>(address: &IPCAddress, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            address
                .to_string()
                .map_err(|e| {
                    S::Error::custom(format!("cannot convert ipc address to string: {e}"))
                })?
                .serialize(serializer)
        } else {
            address.serialize(serializer)
        }
    }
}

deserialize_human_readable_str!(IPCAddress);

/// Receives a bitcoin address (as a string) as an input and returns the corresponding
/// filecoin delegated address
pub fn fvm_address_from_bitcoin_address(s: &str) -> anyhow::Result<fvm_shared::address::Address> {
    let addr =
        fvm_shared::address::Address::new_delegated(crate::subnet_id::BTC_NAMESPACE, s.as_bytes())?;
    Ok(addr)
}

/// Receives a filecoin delegated address as an input and returns the corresponding
/// bitcoin address (as a string)
pub fn bitcoin_address_from_fvm_address(address: &Address) -> anyhow::Result<String> {
    match address.payload() {
        Payload::Delegated(d) if d.namespace() == crate::subnet_id::BTC_NAMESPACE => {
            let subaddr = d.subaddress().to_vec();
            let subaddr_str =
                String::from_utf8(subaddr).context("failed to parse subaddress as string")?;
            Ok(subaddr_str)
        }
        _ => Err(anyhow!("address is not a bitcoin delegated address")),
    }
}

pub fn to_eth_address(addr: &Address) -> anyhow::Result<Option<et::H160>> {
    match addr.payload() {
        Payload::Delegated(d) if d.namespace() == EAM_ACTOR_ID && d.subaddress().len() == 20 => {
            Ok(Some(et::H160::from_slice(d.subaddress())))
        }
        // Deployments should be sent with an empty `to`.
        Payload::ID(EAM_ACTOR_ID) => Ok(None),
        // It should be possible to send to an ethereum account by ID.
        Payload::ID(id) => Ok(Some(et::H160::from_slice(&EthAddress::from_id(*id).0))),
        // The following fit into the type but are not valid ethereum addresses.
        // Return an error so we can prevent tampering with the address when we convert ethereum transactions to FVM messages.
        _ => bail!("not an Ethereum address: {addr}"), // f1, f2, f3 or an invalid delegated address.
    }
}

#[cfg(test)]
mod tests {
    use crate::address::IPCAddress;
    use crate::subnet_id::SubnetID;
    use fvm_shared::address::Address;
    use std::str::FromStr;
    use std::vec;

    #[test]
    fn test_ipc_address() {
        let act = Address::new_id(1001);
        let sub_id = SubnetID::new(123, vec![act]);
        let bls = Address::from_str("f3vvmn62lofvhjd2ugzca6sof2j2ubwok6cj4xxbfzz4yuxfkgobpihhd2thlanmsh3w2ptld2gqkn2jvlss4a").unwrap();
        let haddr = IPCAddress::new(&sub_id, &bls).unwrap();

        let str = haddr.to_string().unwrap();

        let blss = IPCAddress::from_str(&str).unwrap();
        assert_eq!(haddr.raw_addr().unwrap(), bls);
        assert_eq!(haddr.subnet().unwrap(), sub_id);
        assert_eq!(haddr, blss);
    }

    #[test]
    fn test_ipc_from_str() {
        let sub_id = SubnetID::new(123, vec![Address::new_id(100)]);
        let addr = IPCAddress::new(&sub_id, &Address::new_id(101)).unwrap();
        let st = addr.to_string().unwrap();
        let addr_out = IPCAddress::from_str(&st).unwrap();
        assert_eq!(addr, addr_out);
        let addr_out = IPCAddress::from_str(&format!("{}", addr)).unwrap();
        assert_eq!(addr, addr_out);
    }

    #[cfg(feature = "fil-actor")]
    #[test]
    fn test_ipc_serialization() {
        let sub_id = SubnetID::new(123, vec![Address::new_id(100)]);
        let addr = IPCAddress::new(&sub_id, &Address::new_id(101)).unwrap();
        let st = addr.to_bytes().unwrap();
        let addr_out = IPCAddress::from_bytes(&st).unwrap();
        assert_eq!(addr, addr_out);
    }
}
