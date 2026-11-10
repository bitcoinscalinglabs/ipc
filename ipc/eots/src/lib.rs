//! Verification of EOTS votes: BIP340 signatures with nonces committed in an RFC 6962 tree.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VOTE_TAG: &[u8] = b"IPC/vote";

/// A commitment of an EOTS key to one public nonce per subnet height in `[start, start + count)`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EotsNonceCommitment {
    pub start: u64,
    pub count: u32,
    pub root: [u8; 32],
}

impl EotsNonceCommitment {
    pub fn covers(&self, height: u64) -> bool {
        height >= self.start && height - self.start < u64::from(self.count)
    }
}

/// An EOTS vote; `signature` is the 64-byte signature as two halves.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct EotsVote {
    pub pk_eots: [u8; 32],
    pub height: u64,
    pub block_hash: [u8; 32],
    pub signature: [[u8; 32]; 2],
    pub start: u64,
    pub proof: Vec<[u8; 32]>,
}

impl EotsVote {
    pub fn signature_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.signature[0]);
        out[32..].copy_from_slice(&self.signature[1]);
        out
    }

    pub fn signature_from_bytes(bytes: &[u8; 64]) -> [[u8; 32]; 2] {
        let mut sig = [[0u8; 32]; 2];
        sig[0].copy_from_slice(&bytes[..32]);
        sig[1].copy_from_slice(&bytes[32..]);
        sig
    }

    /// The public nonce `R_h`, the first half of the signature.
    pub fn nonce(&self) -> &[u8; 32] {
        &self.signature[0]
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EotsError {
    #[error("the vote's start {start} is not the commitment's, or the commitment does not cover height {height}")]
    HeightNotCommitted { start: u64, height: u64 },
    #[error("the audit path does not prove the nonce against the commitment root")]
    InvalidProof,
    #[error("invalid EOTS key")]
    InvalidKey,
    #[error("the signature does not verify under the EOTS key")]
    InvalidSignature,
}

/// `sha256(sha256(tag) ‖ sha256(tag) ‖ data)`.
pub fn tagged_sha256(tag: &[u8], data: &[u8]) -> [u8; 32] {
    let tag_hash = Sha256::digest(tag);
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher.update(data);
    hasher.finalize().into()
}

/// The message an EOTS vote signs: `tagged_sha256("IPC/vote", txid20 ‖ h ‖ b)`.
pub fn vote_message(txid20: &[u8; 20], height: u64, block_hash: &[u8; 32]) -> [u8; 32] {
    let mut data = Vec::with_capacity(60);
    data.extend_from_slice(txid20);
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(block_hash);
    tagged_sha256(VOTE_TAG, &data)
}

fn leaf_hash(leaf: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0u8]);
    hasher.update(leaf);
    hasher.finalize().into()
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([1u8]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// Checks the RFC 6962 audit path `path` of `leaf` at `index` in a tree of `size` leaves against
/// `root` (RFC 9162, section 2.1.3.2).
pub fn verify_audit_path(
    leaf: &[u8; 32],
    index: u64,
    size: u64,
    path: &[[u8; 32]],
    root: &[u8; 32],
) -> bool {
    if size == 0 || index >= size {
        return false;
    }
    let mut node_index = index;
    let mut last_index = size - 1;
    let mut current_hash = leaf_hash(leaf);
    for sibling_hash in path {
        if last_index == 0 {
            return false;
        }
        if node_index & 1 == 1 || node_index == last_index {
            current_hash = node_hash(sibling_hash, &current_hash);
            while node_index & 1 == 0 && node_index != 0 {
                node_index >>= 1;
                last_index >>= 1;
            }
        } else {
            current_hash = node_hash(&current_hash, sibling_hash);
        }
        node_index >>= 1;
        last_index >>= 1;
    }
    last_index == 0 && current_hash == *root
}

/// Verifies a 64-byte BIP340 signature on `msg` under the x-only key `pk`.
pub fn verify_bip340(pk: &[u8; 32], msg: &[u8; 32], signature: &[u8; 64]) -> Result<(), EotsError> {
    let key = k256::schnorr::VerifyingKey::from_bytes(pk).map_err(|_| EotsError::InvalidKey)?;
    let signature = k256::schnorr::Signature::try_from(&signature[..])
        .map_err(|_| EotsError::InvalidSignature)?;
    key.verify_raw(msg, &signature)
        .map_err(|_| EotsError::InvalidSignature)
}

/// Checks a vote's proof against `commitment` and its signature under its key, for the subnet
/// `txid20`.
pub fn verify_vote(
    txid20: &[u8; 20],
    vote: &EotsVote,
    commitment: &EotsNonceCommitment,
) -> Result<(), EotsError> {
    if vote.start != commitment.start || !commitment.covers(vote.height) {
        return Err(EotsError::HeightNotCommitted {
            start: vote.start,
            height: vote.height,
        });
    }
    let ok = verify_audit_path(
        vote.nonce(),
        vote.height - commitment.start,
        u64::from(commitment.count),
        &vote.proof,
        &commitment.root,
    );
    if !ok {
        return Err(EotsError::InvalidProof);
    }
    let msg = vote_message(txid20, vote.height, &vote.block_hash);
    verify_bip340(&vote.pk_eots, &msg, &vote.signature_bytes())
}

/// Whether two accepted votes of one key at one height are a double vote.
pub fn is_double_vote(a: &EotsVote, b: &EotsVote) -> bool {
    a.pk_eots == b.pk_eots && a.height == b.height && a.block_hash != b.block_hash
}

/// A vote with its validator (compressed secp256k1 key, 33 bytes) and the gossip envelope's
/// timestamp and signature.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValidatorVote {
    pub validator: Vec<u8>,
    pub vote: EotsVote,
    pub timestamp: u64,
    pub signature: Vec<u8>,
}

/// The finality query's answer. `final_height` is the height the node serves: the recorded final
/// height, or below it after a conflict.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FinalityAnswer {
    pub final_height: u64,
    pub recorded_final_height: u64,
    pub checkpointed_height: u64,
    /// The first height at which the node holds a final block other than its chain's
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_height: Option<u64>,
}

/// Votes (oldest first) and one double-vote evidence per equivocating key.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct EotsFinalityData {
    /// Bitcoin height whose monitor state the votes are checked against.
    pub bitcoin_height: u64,
    pub votes: Vec<ValidatorVote>,
    pub evidence: Vec<(ValidatorVote, ValidatorVote)>,
}

/// Signing helpers for tests: the signer of the protocol lives in the Bitcoin provider.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use k256::elliptic_curve::sec1::ToEncodedPoint;
    use k256::elliptic_curve::PrimeField;
    use k256::{ProjectivePoint, Scalar};

    pub fn point_x_and_parity(point: &ProjectivePoint) -> ([u8; 32], bool) {
        let encoded = point.to_affine().to_encoded_point(false);
        let x: [u8; 32] = encoded.x().unwrap().as_slice().try_into().unwrap();
        (x, encoded.y().unwrap()[31] & 1 == 1)
    }

    /// A BIP340 signature with the nonce `k` on `msg` under `d`: the public key and the signature
    pub fn sign_with_nonce(d: Scalar, k: Scalar, msg: &[u8; 32]) -> ([u8; 32], [u8; 64]) {
        let (px, p_odd) = point_x_and_parity(&(ProjectivePoint::GENERATOR * d));
        let d = if p_odd { -d } else { d };
        let (rx, r_odd) = point_x_and_parity(&(ProjectivePoint::GENERATOR * k));
        let k = if r_odd { -k } else { k };
        let mut data = Vec::new();
        data.extend_from_slice(&rx);
        data.extend_from_slice(&px);
        data.extend_from_slice(msg);
        let e = <Scalar as k256::elliptic_curve::ops::Reduce<k256::U256>>::reduce_bytes(
            &tagged_sha256(b"BIP0340/challenge", &data).into(),
        );
        let s = k + e * d;
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&rx);
        sig[32..].copy_from_slice(&s.to_bytes());
        (px, sig)
    }

    pub fn scalar(seed: u8) -> Scalar {
        Scalar::from_repr([seed; 32].into()).unwrap()
    }

    pub fn root_of(leaves: &[[u8; 32]]) -> [u8; 32] {
        if leaves.len() == 1 {
            return leaf_hash(&leaves[0]);
        }
        let k = 1usize << (usize::BITS - 1 - (leaves.len() - 1).leading_zeros());
        node_hash(&root_of(&leaves[..k]), &root_of(&leaves[k..]))
    }

    pub fn path_of(leaves: &[[u8; 32]], index: usize) -> Vec<[u8; 32]> {
        if leaves.len() == 1 {
            return vec![];
        }
        let k = 1usize << (usize::BITS - 1 - (leaves.len() - 1).leading_zeros());
        if index < k {
            let mut p = path_of(&leaves[..k], index);
            p.push(root_of(&leaves[k..]));
            p
        } else {
            let mut p = path_of(&leaves[k..], index - k);
            p.push(root_of(&leaves[..k]));
            p
        }
    }

    /// A test signer: a key with nonces committed for heights `[start, start + count)`.
    pub struct TestSigner {
        pub d: Scalar,
        pub pk_eots: [u8; 32],
        pub nonces: Vec<Scalar>,
        pub leaves: Vec<[u8; 32]>,
        pub commitment: EotsNonceCommitment,
    }

    impl TestSigner {
        pub fn new(seed: u8, start: u64, count: u32) -> Self {
            let d = scalar(seed);
            let (pk_eots, _) = point_x_and_parity(&(ProjectivePoint::GENERATOR * d));
            let nonces: Vec<Scalar> = (0..count)
                .map(|i| scalar(seed.wrapping_mul(7).wrapping_add(i as u8 + 1).max(1)))
                .collect();
            let leaves: Vec<[u8; 32]> = nonces
                .iter()
                .map(|k| point_x_and_parity(&(ProjectivePoint::GENERATOR * *k)).0)
                .collect();
            let commitment = EotsNonceCommitment {
                start,
                count,
                root: root_of(&leaves),
            };
            Self {
                d,
                pk_eots,
                nonces,
                leaves,
                commitment,
            }
        }

        pub fn vote(&self, txid20: &[u8; 20], height: u64, block_hash: [u8; 32]) -> EotsVote {
            let index = (height - self.commitment.start) as usize;
            let (_, sig) = sign_with_nonce(
                self.d,
                self.nonces[index],
                &vote_message(txid20, height, &block_hash),
            );
            EotsVote {
                pk_eots: self.pk_eots,
                height,
                block_hash,
                signature: EotsVote::signature_from_bytes(&sig),
                start: self.commitment.start,
                proof: path_of(&self.leaves, index),
            }
        }
    }
}

#[cfg(test)]
mod tests;
