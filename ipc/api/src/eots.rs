//! Types the Bitcoin provider returns for EOTS finality and slashing.

use ipc_eots::EotsNonceCommitment;
use serde::{Deserialize, Serialize};

/// The EOTS key of a validator, both as 32-byte x-only keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EotsKey {
    pub validator: [u8; 32],
    pub pk_eots: [u8; 32],
}

/// An EOTS nonce commitment the monitor recorded, with the Bitcoin block that carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedCommitment {
    pub commitment: EotsNonceCommitment,
    pub block_height: u64,
}

/// The latest checkpoint the monitor recorded: subnet height and block hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestCheckpoint {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub bitcoin_height: u64,
    /// Whether it is the kill checkpoint
    pub killed: bool,
}

/// A slash pre-signature of one member for one collateral output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashPreSignature {
    /// The output's txid in its usual (reversed) hex form
    pub txid: String,
    pub vout: u32,
    /// The signer's index in the ascending key order of the named configuration
    pub index: u16,
    /// The 64-byte signature
    pub signature: Vec<u8>,
}

/// A collateral output not yet slashable, with the configuration its script names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnslashableOutput {
    pub txid: String,
    pub vout: u32,
    /// The owner's validator key, x-only
    pub owner: [u8; 32],
    /// The join (reveal) transaction of the collateral
    pub join_txid: String,
    pub block_height: u64,
    pub threshold: u64,
    /// The keys of the named configuration in ascending order, with their powers
    pub keys: Vec<([u8; 32], u64)>,
    /// Whether this provider's validator key is one of them
    pub signer: bool,
}
