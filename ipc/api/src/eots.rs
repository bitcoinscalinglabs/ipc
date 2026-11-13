//! Recorded EOTS state used for EOTS finality and slashing.

use ipc_eots::EotsNonceCommitment;
use serde::{Deserialize, Serialize};

/// The EOTS key of a validator, both as 32-byte x-only keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EotsKey {
    pub validator: [u8; 32],
    pub pk_eots: [u8; 32],
}

/// A recorded EOTS nonce commitment, with the Bitcoin block that carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedCommitment {
    pub commitment: EotsNonceCommitment,
    pub block_height: u64,
}

/// The latest recorded checkpoint: subnet height and block hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestCheckpoint {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub killed: bool,
    /// The Bitcoin height of the block that recorded it
    pub bitcoin_height: u64,
}

/// The recorded EOTS state at one Bitcoin height: the EOTS keys and EOTS nonce commitments
/// recorded above the height the caller named, and the latest checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EotsRecords {
    pub bitcoin_height: u64,
    pub keys: Vec<(EotsKey, Vec<RecordedCommitment>)>,
    pub checkpoint: Option<LatestCheckpoint>,
}

/// A slash pre-signature of one member for one collateral output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashPreSignature {
    /// The output's txid in its usual (reversed) hex form
    pub txid: String,
    pub vout: u32,
    /// The 64-byte signature of the output's slash transaction
    pub signature: Vec<u8>,
}

/// A collateral output not yet slashable, with the configuration its script names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnslashableOutput {
    pub txid: String,
    pub vout: u32,
    /// The keys of the named configuration in ascending order, with their powers
    pub keys: Vec<([u8; 32], u64)>,
    /// The threshold of the named configuration
    pub threshold: u64,
    /// Whether the validator's key is one of them
    pub signer: bool,
}
