use ethers::utils::hex::hex;
// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT
use fnv::FnvHasher;
use fvm_shared::address::Address;
use lazy_static::lazy_static;
use serde_tuple::{Deserialize_tuple, Serialize_tuple};
use std::fmt;
use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use crate::as_human_readable_str;

use crate::error::Error;

/// MaxChainID is the maximum chain ID value
/// possible. This is the MAX_CHAIN_ID currently
/// supported by Ethereum chains.
pub const MAX_CHAIN_ID: u64 = 4503599627370476;

#[derive(PartialEq, Eq, Hash, Clone, Debug, Serialize, Deserialize)]
pub enum NetworkType {
    Fevm, // For EIP-155 chains
    Btc,  // For Bitcoin networks
}

/// Bitcoin namespace for delegated addresses
// TODO explain why 20 + see if there's a better name
pub const BTC_NAMESPACE: u64 = 20;

/// SubnetID is a unique identifier for a subnet.
/// It is composed of the chainID of the root network, and the address of
/// all the subnet actors from the root to the corresponding level in the
/// hierarchy where the subnet is spawned.
#[derive(PartialEq, Eq, Hash, Clone, Debug, Serialize_tuple, Deserialize_tuple)]
pub struct SubnetID {
    root_network_type: NetworkType,
    root: u64, // For FEVM: chain_id, For BTC: 1=mainnet, 2=testnet, etc
    children: Vec<Address>,
}

as_human_readable_str!(SubnetID);

lazy_static! {
    pub static ref UNDEF: SubnetID = SubnetID {
        root_network_type: NetworkType::Fevm,
        root: 0,
        children: vec![],
    };
}

impl SubnetID {
    pub fn new(root_id: u64, children: Vec<Address>) -> Self {
        Self {
            root_network_type: NetworkType::Fevm,
            root: root_id,
            children,
        }
    }

    // New constructor for Bitcoin networks
    pub fn new_btc(network_id: u64, btc_id: &str) -> Result<Self, Error> {
        // Convert Bitcoin address to bytes
        let btc_bytes = hex::decode(&btc_id).map_err(|_| {
            Error::InvalidID(
                btc_id.to_string(),
                "Bitcoin subnet child is not a valid hex".to_string(),
            )
        })?;

        // Create delegated address with Bitcoin namespace
        let delegated_addr = Address::new_delegated(BTC_NAMESPACE, &btc_bytes)?;

        Ok(Self {
            root_network_type: NetworkType::Btc,
            root: network_id,
            children: vec![delegated_addr],
        })
    }

    pub fn new_from_parent(parent: &SubnetID, subnet_act: Address) -> Self {
        let mut children = parent.children();
        children.push(subnet_act);
        Self {
            root_network_type: parent.root_network_type(),
            root: parent.root_id(),
            children,
        }
    }

    pub fn root_network_type(&self) -> NetworkType {
        self.root_network_type.clone()
    }

    /// Returns the network type of the parent network
    /// If there is only one child, returns the root network type
    /// If there are more children, returns FEVM as all intermediate networks are FEVM
    pub fn parent_network_type(&self) -> Option<NetworkType> {
        match self.children.len() {
            0 => None,                           // No parent if we're at root
            1 => Some(self.root_network_type()), // We're on L2, so return the root network type
            _ => Some(NetworkType::Fevm),        // Anything deeper, parent is Fevm
        }
    }

    /// Returns the network type of current chain
    /// For the root, returns the root network type
    /// For anything deeper, returns FEVM
    pub fn network_type(&self) -> Option<NetworkType> {
        match self.children.len() {
            0 => Some(self.root_network_type()), // At root, return root network type
            _ => Some(NetworkType::Fevm),        // Anything L2+ is Fevm
        }
    }

    /// Ensures that the SubnetID only uses f0 addresses for the subnet actor
    /// hosted in the current network. The rest of the route is left
    /// as-is. We only have information to translate from f2 to f0 for the
    /// last subnet actor in the root.
    #[cfg(feature = "fil-actor")]
    pub fn f0_id(&self, rt: &impl fil_actors_runtime::runtime::Runtime) -> SubnetID {
        let mut children = self.children();

        // replace the resolved child (if any)
        if let Some(actor_addr) = children.last_mut() {
            if let Some(f0) = rt.resolve_address(actor_addr) {
                *actor_addr = f0;
            }
        }

        SubnetID::new(self.root_id(), children)
    }

    /// Creates a new rootnet SubnetID
    pub fn new_root(root_id: u64) -> Self {
        Self {
            root_network_type: NetworkType::Fevm,
            root: root_id,
            children: vec![],
        }
    }

    /// Returns true if the current subnet is the root network
    pub fn is_root(&self) -> bool {
        self.children_as_ref().is_empty()
    }

    /// Returns the chainID of the root network.
    pub fn root_id(&self) -> u64 {
        self.root
    }

    /// Returns the chainID of the current subnet
    pub fn chain_id(&self) -> u64 {
        if self.is_root() {
            return self.root_id();
        }
        let mut hasher = FnvHasher::default();

        hasher.write(self.to_string().as_bytes());
        hasher.finish() % MAX_CHAIN_ID
    }

    /// Returns the route from the root to the current subnet
    pub fn children(&self) -> Vec<Address> {
        self.children.clone()
    }

    /// Returns the route from the root to the current subnet
    pub fn children_as_ref(&self) -> &Vec<Address> {
        &self.children
    }

    /// Returns the serialized version of the subnet id
    #[cfg(feature = "fil-actor")]
    pub fn to_bytes(&self) -> Vec<u8> {
        fil_actors_runtime::cbor::serialize(self, "subnetID")
            .unwrap()
            .into()
    }

    /// Returns the address of the actor governing the subnet in the parent
    /// If there is no subnet actor it returns the address ID=0
    pub fn subnet_actor(&self) -> Address {
        if let Some(addr) = self.children.last() {
            *addr
        } else {
            // protect against the case that the children slice is empty
            Address::new_id(0)
        }
    }

    /// Returns the parenet of the current subnet
    pub fn parent(&self) -> Option<SubnetID> {
        // if the subnet is the root, it has no parent
        if self.children_as_ref().is_empty() {
            return None;
        }

        let children = self.children();

        Some(SubnetID {
            root_network_type: self.root_network_type.clone(),
            root: self.root,
            children: children[..children.len() - 1].to_vec(),
        })
    }

    /// Computes the common parent of the current subnet and the one given
    /// as argument. It returns the number of common children and the subnet.
    pub fn common_parent(&self, other: &SubnetID) -> Option<(usize, SubnetID)> {
        // check if we have the same root first
        if self.root_id() != other.root_id() {
            return None;
        }

        let common = self
            .children_as_ref()
            .iter()
            .zip(other.children_as_ref())
            .take_while(|(a, b)| a == b)
            .count();
        let children = self.children()[..common].to_vec();

        Some((
            common,
            SubnetID {
                root_network_type: self.root_network_type.clone(),
                root: self.root,
                children,
            },
        ))
    }

    /// In the path determined by the current subnet id, it moves
    /// down in the path from the subnet id given as argument.
    pub fn down(&self, from: &SubnetID) -> Option<SubnetID> {
        // check if the current network's path is larger than
        // the one to be traversed.
        if self.children_as_ref().len() <= from.children_as_ref().len() {
            return None;
        }

        if let Some((i, _)) = self.common_parent(from) {
            let children = self.children()[..i + 1].to_vec();
            return Some(SubnetID {
                root_network_type: self.root_network_type.clone(),
                root: self.root,
                children,
            });
        }
        None
    }

    /// In the path determined by the current subnet id, it moves
    /// up in the path from the subnet id given as argument.
    pub fn up(&self, from: &SubnetID) -> Option<SubnetID> {
        // check if the current network's path is larger than
        // the one to be traversed.
        if self.children_as_ref().len() < from.children_as_ref().len() {
            return None;
        }

        if let Some((i, _)) = self.common_parent(from) {
            let children = self.children()[..i - 1].to_vec();
            return Some(SubnetID {
                root_network_type: self.root_network_type.clone(),
                root: self.root,
                children,
            });
        }
        None
    }
}

impl fmt::Display for SubnetID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.root_network_type {
            NetworkType::Fevm => "/r",
            NetworkType::Btc => "/b",
        };

        let children_str = self
            .children_as_ref()
            .iter()
            .fold(String::new(), |mut output, s| {
                let _ = write!(output, "/{}", s);
                output
            });

        write!(f, "{}{}{}", prefix, self.root_id(), children_str)
    }
}

impl Default for SubnetID {
    fn default() -> Self {
        UNDEF.clone()
    }
}

impl FromStr for SubnetID {
    type Err = Error;
    fn from_str(id: &str) -> Result<Self, Error> {
        if !id.starts_with("/r") && !id.starts_with("/b") {
            return Err(Error::InvalidID(
                id.into(),
                "expected to start with '/r' or '/b'".into(),
            ));
        }

        let network_type = if id.starts_with("/r") {
            NetworkType::Fevm
        } else {
            NetworkType::Btc
        };

        let segments: Vec<&str> = id.split('/').skip(1).collect();

        let root = segments[0][1..]
            .parse::<u64>()
            .map_err(|_| Error::InvalidID(id.into(), "invalid root ID".into()))?;

        if matches!(network_type, NetworkType::Btc) && root == 0 {
            return Err(Error::InvalidID(
                id.into(),
                "invalid Bitcoin network ID".into(),
            ));
        }

        let mut children = Vec::new();
        for addr in segments[1..].iter() {
            let addr = Address::from_str(addr).map_err(|e| {
                Error::InvalidID(id.into(), format!("invalid child address {addr}: {e}"))
            })?;
            children.push(addr);
        }

        Ok(Self {
            root_network_type: network_type,
            root,
            children,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::subnet_id::{NetworkType, SubnetID};
    use fvm_shared::address::Address;
    use std::str::FromStr;

    #[test]
    fn test_parse_root_net() {
        let subnet_id = SubnetID::from_str("/r123").unwrap();
        assert_eq!(subnet_id.root_network_type, NetworkType::Fevm);
        assert_eq!(subnet_id.root, 123);
    }

    #[test]
    fn test_parse_bitcoin_root_net() {
        let subnet_id = SubnetID::from_str("/b1").unwrap();
        assert_eq!(subnet_id.root_network_type, NetworkType::Btc);
        assert_eq!(subnet_id.root, 1);
    }

    #[test]
    fn test_bitcoin_subnet() {
        // Test Bitcoin mainnet subnet creation
        let btc_id = "2e87774fe9e002d7afe7bf83158dbf7ab2797ba4bcab4c6561f8b5d335b8d161";
        let btc_subnet = SubnetID::new_btc(1, btc_id).unwrap();

        assert_eq!(btc_subnet.root_network_type(), NetworkType::Btc);
        assert_eq!(btc_subnet.root_id(), 1);
        assert_eq!(btc_subnet.children().len(), 1);

        // Test string representation
        let subnet_str = btc_subnet.to_string();
        assert!(subnet_str.starts_with("/b1")); // Bitcoin mainnet prefix

        // Test parsing back
        let parsed = SubnetID::from_str(&subnet_str).unwrap();
        assert_eq!(parsed, btc_subnet);
    }

    #[test]
    fn test_parse_subnet_id() {
        // NOTE: It would not work with `t` prefix addresses unless the current network is changed.
        let id = "/r31415926/f2xwzbdu7z5sam6hc57xxwkctciuaz7oe5omipwbq";
        let subnet_id = SubnetID::from_str(id).unwrap();
        assert_eq!(subnet_id.root, 31415926);
        assert!(!subnet_id.children.is_empty());
    }

    #[test]
    fn test_cannot_parse_subnet_id_with_wrong_prefix() {
        // NOTE: The default network prefix is `f`.
        let id = "/r31415926/t2xwzbdu7z5sam6hc57xxwkctciuaz7oe5omipwbq";
        match SubnetID::from_str(id) {
            Err(crate::error::Error::InvalidID(_, msg)) => {
                assert!(msg.contains("invalid child"));
                assert!(msg.contains("t2xwzbdu7z5sam6hc57xxwkctciuaz7oe5omipwbq"));
                assert!(msg.contains("network"));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn test_parse_empty_subnet_id() {
        assert!(SubnetID::from_str("").is_err())
    }

    #[test]
    fn test_subnet_id() {
        let act = Address::new_id(1001);
        let sub_id = SubnetID::new(123, vec![act]);
        let sub_id_str = sub_id.to_string();
        assert_eq!(sub_id_str, "/r123/f01001");

        let rtt_id = SubnetID::from_str(&sub_id_str).unwrap();
        assert_eq!(sub_id, rtt_id);

        let rootnet = SubnetID::new(123, vec![]);
        assert_eq!(rootnet.to_string(), "/r123");
        let root_sub = SubnetID::from_str(&rootnet.to_string()).unwrap();
        assert_eq!(root_sub, rootnet);
    }

    #[test]
    fn test_chain_id() {
        let act = Address::new_id(1001);
        let sub_id = SubnetID::new(123, vec![act]);
        let chain_id = sub_id.chain_id();
        assert_eq!(chain_id, 1011873294913613);

        let root = sub_id.parent().unwrap();
        let chain_id = root.chain_id();
        assert_eq!(chain_id, 123);
    }

    #[test]
    fn test_common_parent() {
        common_parent("/r123/f01", "/r123/f01/f02", "/r123/f01", 1);
        common_parent("/r123/f01/f02/f03", "/r123/f01/f02", "/r123/f01/f02", 2);
        common_parent("/r123/f01/f03/f04", "/r123/f02/f03/f04", "/r123", 0);
        common_parent(
            "/r123/f01/f03/f04",
            "/r123/f01/f03/f04/f05",
            "/r123/f01/f03/f04",
            3,
        );
        // The common parent of the same subnet is the current subnet
        common_parent(
            "/r123/f01/f03/f04",
            "/r123/f01/f03/f04",
            "/r123/f01/f03/f04",
            3,
        );
    }

    #[test]
    #[should_panic]
    fn test_panic_different_root() {
        common_parent("/r122/f01", "/r123/f01/f02", "/r123/f01", 1);
    }

    #[test]
    fn test_down() {
        down(
            "/r123/f01/f02/f03",
            "/r123/f01",
            Some(SubnetID::from_str("/r123/f01/f02").unwrap()),
        );
        down(
            "/r123/f01/f02/f03",
            "/r123/f01/f02",
            Some(SubnetID::from_str("/r123/f01/f02/f03").unwrap()),
        );
        down(
            "/r123/f01/f03/f04",
            "/r123/f01/f03",
            Some(SubnetID::from_str("/r123/f01/f03/f04").unwrap()),
        );
        down("/r123", "/r123/f01", None);
        down("/r123/f01", "/r123/f01", None);
        down("/r123/f02/f03", "/r123/f01/f03/f04", None);
        down("/r123", "/r123/f01", None);
    }

    #[test]
    fn test_up() {
        up(
            "/r123/f01/f02/f03",
            "/r123/f01",
            Some(SubnetID::from_str("/r123").unwrap()),
        );
        up(
            "/r123/f01/f02/f03",
            "/r123/f01/f02",
            Some(SubnetID::from_str("/r123/f01").unwrap()),
        );
        up("/r123", "/r123/f01", None);
        up("/r123/f02/f03", "/r123/f01/f03/f04", None);
        up(
            "/r123/f01/f02/f03",
            "/r123/f01/f02",
            Some(SubnetID::from_str("/r123/f01").unwrap()),
        );
        up(
            "/r123/f01/f02/f03",
            "/r123/f01/f02/f03",
            Some(SubnetID::from_str("/r123/f01/f02").unwrap()),
        );
    }

    fn common_parent(a: &str, b: &str, res: &str, index: usize) {
        let id = SubnetID::from_str(a).unwrap();
        assert_eq!(
            id.common_parent(&SubnetID::from_str(b).unwrap()).unwrap(),
            (index, SubnetID::from_str(res).unwrap()),
        );
    }

    fn down(a: &str, b: &str, res: Option<SubnetID>) {
        let id = SubnetID::from_str(a).unwrap();
        assert_eq!(id.down(&SubnetID::from_str(b).unwrap()), res);
    }

    fn up(a: &str, b: &str, res: Option<SubnetID>) {
        let id = SubnetID::from_str(a).unwrap();
        assert_eq!(id.up(&SubnetID::from_str(b).unwrap()), res);
    }
}
