use fvm_shared::address::Address;
use lazy_static::lazy_static;
use serde_tuple::{Deserialize_tuple, Serialize_tuple};
use ssi_caips::caip2::ChainId;
use std::fmt;
use std::str::FromStr;

use crate::as_human_readable_str;
use crate::error::Error;
use crate::subnet_id::SubnetID;

/// A helper type to determine the type of root chain
/// based of the universal subnet id
pub enum ChainType {
    Fevm,
    Btc,
}

/// UniversalSubnetId represents a subnet identifier that can work across different
/// blockchain ecosystems by using CAIP-2 chain IDs for the root network and allowing
/// arbitrary string identifiers for child subnets.
///
/// Read more at: https://chainagnostic.org/CAIPs/caip-2
/// https://eips.ethereum.org/EIPS/eip-155
#[derive(PartialEq, Eq, Hash, Clone, Debug, Serialize_tuple, Deserialize_tuple)]
pub struct UniversalSubnetId {
    #[serde(with = "chain_id_serde")]
    root: ChainId,
    children: Vec<String>,
}

lazy_static! {
    pub static ref UNDEF: UniversalSubnetId = UniversalSubnetId {
        root: ChainId::from_str("eip155:0").unwrap(),
        children: vec![],
    };
}

// Custom serialization for ChainId
mod chain_id_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(chain_id: &ChainId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = format!("{}:{}", chain_id.namespace, chain_id.reference);
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ChainId, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ChainId::from_str(&s).map_err(serde::de::Error::custom)
    }
}

as_human_readable_str!(UniversalSubnetId);

impl UniversalSubnetId {
    pub fn new(root: ChainId, children: Vec<String>) -> Self {
        Self { root, children }
    }

    /// Create a new universal subnet id from the root network id and a subnet child
    pub fn new_from_parent(parent: &UniversalSubnetId, subnet_child: String) -> Self {
        let mut children = parent.children();
        children.push(subnet_child);
        Self {
            root: parent.root_id(),
            children,
        }
    }

    /// Creates a new rootnet UniversalSubnetId
    pub fn new_root(root_id: ChainId) -> Self {
        Self {
            root: root_id,
            children: vec![],
        }
    }

    /// Attempts to convert this UniversalSubnetId to a SubnetID
    /// Will return an error if:
    /// - The root chain ID reference cannot be converted to a u64
    /// - Any child address strings cannot be parsed as Filecoin addresses
    pub fn to_subnet_id(&self) -> Result<SubnetID, Error> {
        // Check that the namespace is eip155
        if self.root.namespace != "eip155" {
            return Err(Error::InvalidID(
                self.to_string(),
                "only eip155 namespace can be converted to SubnetID".into(),
            ));
        }

        // Extract the chain ID number from the CAIP-2 chain ID
        let root_id = self.root.reference.parse::<u64>().map_err(|_| {
            Error::InvalidID(
                self.to_string(),
                "root chain ID reference cannot be converted to u64".into(),
            )
        })?;

        // Convert child strings to Filecoin addresses
        let mut children = Vec::new();
        for child in &self.children {
            let addr = Address::from_str(child).map_err(|e| {
                Error::InvalidID(
                    self.to_string(),
                    format!("invalid child address {}: {}", child, e),
                )
            })?;
            children.push(addr);
        }

        Ok(SubnetID::new(root_id, children))
    }

    /// Creates a UniversalSubnetId from an existing SubnetID
    /// The root chain ID will be in the eip155 namespace
    pub fn from_subnet_id(subnet_id: &SubnetID) -> Self {
        // Convert root ID to a CAIP-2 chain ID using eip155 namespace
        let root = ChainId {
            namespace: "eip155".into(),
            reference: subnet_id.root_id().to_string(),
        };

        // Convert Filecoin addresses to strings
        let children = subnet_id
            .children()
            .iter()
            .map(|addr| addr.to_string())
            .collect();

        Self::new(root, children)
    }

    /// Returns true if the current subnet is the root network
    pub fn is_root(&self) -> bool {
        self.children_as_ref().is_empty()
    }

    /// Returns the chainID of the root network.
    pub fn root_id(&self) -> ChainId {
        self.root.clone()
    }

    /// Returns the route from the root to the current subnet
    pub fn children(&self) -> Vec<String> {
        self.children.clone()
    }

    /// Returns the route from the root to the current subnet
    pub fn children_as_ref(&self) -> &Vec<String> {
        &self.children
    }

    /// Returns the parent of the current subnet
    pub fn parent(&self) -> Option<UniversalSubnetId> {
        // if the subnet is the root, it has no parent
        if self.children_as_ref().is_empty() {
            return None;
        }

        let children = self.children();
        Some(UniversalSubnetId::new(
            self.root_id(),
            children[..children.len() - 1].to_vec(),
        ))
    }

    /// Returns the network type of the root network
    /// Unknown networks will return None
    // TODO this should be revisited
    pub fn root_network_type(&self) -> Option<ChainType> {
        match self.root.namespace.as_str() {
            "eip155" => Some(ChainType::Fevm),
            "bip122" => Some(ChainType::Btc),
            _ => None,
        }
    }

    /// Returns the network type of the parent network
    /// If there is only one child, returns the root network type
    /// If there are more children, returns FEVM as all intermediate networks are FEVM
    pub fn parent_network_type(&self) -> Option<ChainType> {
        match self.children.len() {
            0 => None,                     // No parent if we're at root
            1 => self.root_network_type(), // We're on L2, so return the root network type
            _ => Some(ChainType::Fevm),    // Anything deeper, parent is Fevm
        }
    }
}

impl fmt::Display for UniversalSubnetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.root.namespace)?;
        write!(f, ":{}", self.root.reference)?;
        for child in &self.children {
            write!(f, "/{}", child)?;
        }
        Ok(())
    }
}

impl Default for UniversalSubnetId {
    fn default() -> Self {
        UNDEF.clone()
    }
}

impl FromStr for UniversalSubnetId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s.starts_with('/') {
            return Err(Error::InvalidID(
                s.into(),
                "expected to start with '/'".into(),
            ));
        }

        let segments: Vec<&str> = s.split('/').skip(1).collect();
        if segments.is_empty() {
            return Err(Error::InvalidID(s.into(), "missing chain ID".into()));
        }

        // Parse root chain ID
        let chain_id_parts: Vec<&str> = segments[0].split(':').collect();
        if chain_id_parts.len() != 2 {
            return Err(Error::InvalidID(
                s.into(),
                "invalid chain ID format, expected namespace:reference".into(),
            ));
        }

        let root = ChainId {
            namespace: chain_id_parts[0].to_string(),
            reference: chain_id_parts[1].to_string(),
        };

        // Collect children
        let children = segments[1..].iter().map(|s| s.to_string()).collect();

        Ok(Self::new(root, children))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_universal_subnet_id_conversion() {
        // Create a SubnetID
        let subnet_id = SubnetID::from_str("/r123/f01001/f02000").unwrap();

        // Convert to UniversalSubnetId
        let universal_id = UniversalSubnetId::from_subnet_id(&subnet_id);

        // Check that it uses the eip155 namespace
        assert_eq!(universal_id.root.namespace, "eip155");
        assert_eq!(universal_id.root.reference, "123");

        // Convert back to SubnetID
        let converted_back = universal_id.to_subnet_id().unwrap();

        // They should be equal
        assert_eq!(subnet_id, converted_back);
    }

    #[test]
    fn test_parse_universal_subnet_id() {
        let id_str = "/eip155:123/f01001/f02000";
        let universal_id = UniversalSubnetId::from_str(id_str).unwrap();

        assert_eq!(universal_id.root.namespace, "eip155");
        assert_eq!(universal_id.root.reference, "123");
        assert_eq!(universal_id.children, vec!["f01001", "f02000"]);
    }

    #[test]
    fn test_display_universal_subnet_id() {
        let id_str = "/eip155:123/f01001/f02000";
        let universal_id = UniversalSubnetId::from_str(id_str).unwrap();

        assert_eq!(universal_id.to_string(), id_str);
    }

    #[test]
    fn test_invalid_universal_subnet_id() {
        assert!(UniversalSubnetId::from_str("invalid").is_err());
        assert!(UniversalSubnetId::from_str("").is_err());
        assert!(UniversalSubnetId::from_str("invalid:chain:id").is_err());
        assert!(UniversalSubnetId::from_str("/eip155").is_err());
    }

    #[test]
    fn test_universal_subnet_id_serialization() {
        let id_str = "/eip155:123/f01001/f02000";
        let universal_id = UniversalSubnetId::from_str(id_str).unwrap();

        let serialized = serde_json::to_string(&universal_id).unwrap();
        let deserialized: UniversalSubnetId = serde_json::from_str(&serialized).unwrap();

        assert_eq!(universal_id, deserialized);
    }

    #[test]
    fn test_bitcoin_mainnet_universal_subnet_id() {
        // Bitcoin mainnet is "bip122:000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        let id_str = "/bip122:000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f/4467317d030d3bcac27b897d05e7c1ad2aa138d669d017e512131852ccfbf287";
        let universal_id = UniversalSubnetId::from_str(id_str).unwrap();

        assert_eq!(universal_id.root.namespace, "bip122");
        assert_eq!(
            universal_id.root.reference,
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
        assert_eq!(
            universal_id.children,
            vec!["4467317d030d3bcac27b897d05e7c1ad2aa138d669d017e512131852ccfbf287"]
        );

        // Test round-trip string conversion
        assert_eq!(universal_id.to_string(), id_str);
    }

    #[test]
    fn test_universal_subnet_id_only_eip155_convertible() {
        // Bitcoin mainnet should fail conversion
        let bitcoin_id_str = "/bip122:000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f/4467317d030d3bcac27b897d05e7c1ad2aa138d669d017e512131852ccfbf287";
        let bitcoin_universal_id = UniversalSubnetId::from_str(bitcoin_id_str).unwrap();
        assert!(bitcoin_universal_id.to_subnet_id().is_err());

        // EIP155 should succeed
        let eip155_id_str = "/eip155:1/f01001";
        let eip155_universal_id = UniversalSubnetId::from_str(eip155_id_str).unwrap();
        assert!(eip155_universal_id.to_subnet_id().is_ok());

        // Verify error message
        match bitcoin_universal_id.to_subnet_id() {
            Err(Error::InvalidID(_, msg)) => {
                assert_eq!(msg, "only eip155 namespace can be converted to SubnetID");
            }
            _ => panic!("Expected InvalidID error with namespace message"),
        }
    }

    #[test]
    fn test_universal_subnet_id_root_network_type() {
        // Test EIP155 (Ethereum) networks
        let eth_mainnet = UniversalSubnetId::from_str("/eip155:1/f01001").unwrap();
        assert!(matches!(
            eth_mainnet.root_network_type(),
            Some(ChainType::Fevm)
        ));
        let fevm_calibration = UniversalSubnetId::from_str("/eip155:314159/f01001").unwrap();
        assert!(matches!(
            fevm_calibration.root_network_type(),
            Some(ChainType::Fevm)
        ));

        // Test BIP122 (Bitcoin) networks
        let btc_mainnet = UniversalSubnetId::from_str("/bip122:000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f/4467317d030d3bcac27b897d05e7c1ad2aa138d669d017e512131852ccfbf287").unwrap();
        assert!(matches!(
            btc_mainnet.root_network_type(),
            Some(ChainType::Btc)
        ));

        let btc_testnet = UniversalSubnetId::from_str("/bip122:000000000933ea01ad0ee984209779baaec3ced90fa3f408719526f8d77f4943/4467317d030d3bcac27b897d05e7c1ad2aa138d669d017e512131852ccfbf287").unwrap();
        assert!(matches!(
            btc_testnet.root_network_type(),
            Some(ChainType::Btc)
        ));

        // Test unknown network
        let unknown_id = UniversalSubnetId::from_str("/unknown:123/child1").unwrap();
        assert!(matches!(unknown_id.root_network_type(), None));
    }
}
