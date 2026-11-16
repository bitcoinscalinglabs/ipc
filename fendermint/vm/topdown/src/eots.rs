//! The tally of EOTS votes on subnet blocks and the votes a proposer carries in a block.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use ipc_api::eots::LatestCheckpoint;
use ipc_api::subnet_id::SubnetID;
use ipc_eots::{EotsFinalityData, EotsNonceCommitment, ValidatorVote};
use ipc_ipld_resolver::ValidatorKey;
use ipc_provider::manager::BtcSubnetManager;
use serde::{Deserialize, Serialize};

use crate::voting::Weight;
use kept_votes::KeptVotes;

mod kept_votes;

pub type KeyBytes = [u8; 33];

/// The most carried-vote bytes a block holds: CometBFT's default `max_tx_bytes`.
pub const MAX_CARRIED_BYTES: usize = 1024 * 1024;

/// How long a node waits between two requests for the recorded state.
const MONITOR_RETRY: Duration = Duration::from_secs(1);

/// How long one request for the recorded state may take before it is repeated, for a connection
/// that hangs.
const MONITOR_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Clone)]
pub struct StoredState {
    pub votes: Vec<ValidatorVote>,
    pub blocks: Vec<(u64, [u8; 32])>,
    pub recorded_final: u64,
    /// The checkpointed subnet height at the Bitcoin height of the last carried proposal
    pub checkpointed: u64,
    /// The EOTS keys whose evidence a block carried
    pub evidence_carried: Vec<[u8; 32]>,
    /// The power tables by the first height they apply to
    pub power_tables: Vec<(u64, Vec<(KeyBytes, Weight)>)>,
    /// The greatest height this validator voted at
    pub last_cast: u64,
}

/// Rule 1 of accepting a vote: the vote's envelope signature verifies for this subnet. The
/// application supplies it, as the envelope's content type lives there.
pub type EnvelopeCheck = Arc<dyn Fn(&ValidatorVote) -> bool + Send + Sync>;

/// Persistence of the accepted votes, the chain's block hashes, the recorded final height,
/// the power tables and the greatest height this validator voted at.
pub trait EotsStore: Send + Sync {
    fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()>;
    fn put_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()>;
    fn put_recorded_final(&self, height: u64) -> anyhow::Result<()>;
    fn put_checkpointed(&self, height: u64) -> anyhow::Result<()>;
    fn put_evidence_carried(&self, pk_eots: [u8; 32]) -> anyhow::Result<()>;
    fn put_power_table(&self, height: u64, table: &[(KeyBytes, Weight)]) -> anyhow::Result<()>;
    fn put_last_cast(&self, height: u64) -> anyhow::Result<()>;
    fn delete(&self, entries: &FinalEntries) -> anyhow::Result<()>;
    fn load(&self) -> anyhow::Result<StoredState>;
}

/// The store entries of votes, block hashes and power tables at or below the subnet final height.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FinalEntries {
    /// `(pk_eots, height, block hash)` of each vote
    pub votes: Vec<([u8; 32], u64, [u8; 32])>,
    /// The heights of the block hashes
    pub blocks: Vec<u64>,
    /// The first heights the power tables apply to
    pub power_tables: Vec<u64>,
}

impl FinalEntries {
    pub fn vote_key(v: &ValidatorVote) -> ([u8; 32], u64, [u8; 32]) {
        (v.vote.pk_eots, v.vote.height, v.vote.block_hash)
    }

    pub fn is_empty(&self) -> bool {
        self.votes.is_empty() && self.blocks.is_empty() && self.power_tables.is_empty()
    }

    fn extend(&mut self, other: Self) {
        self.votes.extend(other.votes);
        self.blocks.extend(other.blocks);
        self.power_tables.extend(other.power_tables);
    }
}

enum Deletion {
    Entries(FinalEntries),
    /// Answered once every earlier entry is deleted
    #[cfg(test)]
    Wait(mpsc::Sender<()>),
}

/// Deletes from the store, in the order received, the entries the tally drops from memory; what
/// arrived meanwhile goes in one deletion, and the entries of a failed deletion go in the next.
fn delete_in_background(store: Arc<dyn EotsStore>, deletions: mpsc::Receiver<Deletion>) {
    let mut failed = FinalEntries::default();
    while let Ok(first) = deletions.recv() {
        let mut entries = FinalEntries::default();
        #[cfg(test)]
        let mut waiting = Vec::new();
        for deletion in std::iter::once(first).chain(deletions.try_iter()) {
            match deletion {
                Deletion::Entries(e) => entries.extend(e),
                #[cfg(test)]
                Deletion::Wait(done) => waiting.push(done),
            }
        }
        if !entries.is_empty() {
            entries.extend(std::mem::take(&mut failed));
            if let Err(e) = store.delete(&entries) {
                // Retried with the next deletion, or at the next start.
                tracing::error!(error = %e, "failed to delete final EOTS entries from the store");
                failed = entries;
            }
        }
        #[cfg(test)]
        for done in waiting {
            let _ = done.send(());
        }
    }
}

/// The recorded state at an agreed Bitcoin L1 height, against which carried votes are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgreedState {
    /// The latest checkpointed subnet height at that Bitcoin height, 0 while there is none
    pub checkpointed: u64,
    /// Whether that checkpoint is the kill checkpoint
    pub killed: bool,
    /// The Bitcoin height: commitments recorded above it do not count
    pub bitcoin_height: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VoteError {
    #[error("the vote's envelope signature does not verify for this subnet")]
    BadEnvelope,
    #[error("height {0} is at or below the checkpointed height {1}")]
    Checkpointed(u64, u64),
    #[error("height {0} is at or below the subnet final height {1}")]
    Final(u64, u64),
    #[error("height {0} is above the chain's latest block {1}")]
    AboveLatest(u64, u64),
    #[error("finality ended with the kill checkpoint")]
    Killed,
    #[error("the vote's key is not the EOTS key of the validator")]
    WrongEotsKey,
    #[error("the validator has no power at height {0}")]
    UnpoweredValidator(u64),
    #[error("no recorded commitment covers height {0}")]
    NotCommitted(u64),
    #[error("invalid vote: {0}")]
    Invalid(ipc_eots::EotsError),
}

impl VoteError {
    /// Whether no honest validator sends such a vote at any recorded state: a forged envelope,
    /// another key than the registered EOTS key, no power, or a signature that does not verify.
    pub fn never_valid(&self) -> bool {
        matches!(
            self,
            VoteError::BadEnvelope
                | VoteError::WrongEotsKey
                | VoteError::UnpoweredValidator(_)
                | VoteError::Invalid(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteOutcome {
    /// Accepted for the first time
    New,
    /// Already held
    Known,
    /// Accepted, and with an earlier vote it is a double vote
    Evidence,
    /// A third vote of the key at the height: dropped
    Excess,
    /// Kept for a later check: the vote's EOTS key or EOTS nonce commitment is not recorded yet,
    /// or its height is above the chain's latest block
    Pending,
}

/// A cryptographic check of a vote: its envelope signature (rule 1), or its audit path and
/// signature against a commitment (rules 4 and 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CryptoCheck {
    Envelope,
    Vote(EotsNonceCommitment),
}

/// The outcome of [`Inner::check`].
enum Checked {
    /// The vote passes, with the commitment its audit path was checked against
    Passed(EotsNonceCommitment),
    Failed(VoteError),
    /// The vote has not passed this cryptographic check, which comes before the rules not checked
    Unverified(CryptoCheck),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinalityStatus {
    /// The subnet final height the node serves: the recorded one, below the first height at
    /// which the node holds a final block other than the chain's
    pub final_height: u64,
    /// The subnet final height of the chain: the greatest height whose votes a block carries
    pub recorded_final_height: u64,
    pub checkpointed_height: u64,
}

/// The most votes of one validator kept in each of `Pending::uncommitted`, `Pending::wrong_key` and
/// `Pending::above_latest`.
const MAX_KEPT_VOTES: usize = 256;

/// The most heights above the chain's latest block at which a vote received on the vote gossip is
/// kept.
const MAX_HEIGHTS_ABOVE_LATEST: u64 = 10;

/// The votes checked again after every read of the recorded state.
struct Pending {
    /// The votes loaded from the store at start
    stored: Vec<ValidatorVote>,
    /// Votes that failed rule 4 only, by x-only validator key
    uncommitted: KeptVotes<[u8; 32], ValidatorVote>,
    /// Votes that failed rule 3 before the first read of the recorded state, by x-only validator
    /// key
    wrong_key: KeptVotes<[u8; 32], ValidatorVote>,
    /// Votes received on the vote gossip above the chain's latest block, checked again when the
    /// chain has the block at their height, by x-only validator key
    above_latest: KeptVotes<[u8; 32], ValidatorVote>,
}

impl Pending {
    fn new(stored: Vec<ValidatorVote>) -> Self {
        Self {
            stored,
            uncommitted: KeptVotes::new(MAX_KEPT_VOTES),
            wrong_key: KeptVotes::new(MAX_KEPT_VOTES),
            above_latest: KeptVotes::new(MAX_KEPT_VOTES),
        }
    }

    /// The votes loaded from the store at start, and those kept after failing rule 4 only.
    fn take(&mut self) -> (Vec<ValidatorVote>, Vec<ValidatorVote>) {
        (
            std::mem::take(&mut self.stored),
            self.uncommitted.take().collect(),
        )
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.stored.len() + self.uncommitted.len() + self.wrong_key.len() + self.above_latest.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Inner {
    /// The power tables by the first height they apply to
    power_tables: BTreeMap<u64, HashMap<KeyBytes, Weight>>,
    /// Validator key, x-only as Bitcoin names it, to EOTS key
    eots_keys: HashMap<[u8; 32], [u8; 32]>,
    /// EOTS key to its recorded commitments with their Bitcoin heights
    commitments: HashMap<[u8; 32], Vec<(EotsNonceCommitment, u64)>>,
    /// The latest recorded checkpoint: subnet height, block hash, kill flag
    checkpoint: Option<(u64, [u8; 32], bool)>,
    /// The Bitcoin height of the block that recorded `checkpoint`, 0 while there is none
    checkpoint_bitcoin_height: u64,
    /// Whether the recorded state was read at least once
    monitor_read: bool,
    /// The Bitcoin height up to which the tally holds every recorded EOTS key and
    /// commitment
    monitor_read_for: u64,
    /// The latest Bitcoin height read for an agreed state, with the checkpoint recorded at or
    /// below it
    agreed_read: Option<(u64, Option<(u64, [u8; 32], bool)>)>,
    /// Whether the power tables come from the store
    stored_power_tables: bool,
    /// The chain's block hashes
    chain: BTreeMap<u64, [u8; 32]>,
    /// The height of the chain's latest block, also when `chain` holds no hash at it, as after a
    /// snapshot import
    latest_height: u64,
    /// The accepted votes by height and EOTS key, at most two per key
    votes: BTreeMap<u64, HashMap<[u8; 32], Vec<ValidatorVote>>>,
    /// The commitment each held vote was verified against, by height, EOTS key and block hash
    verified: BTreeMap<u64, HashMap<([u8; 32], [u8; 32]), EotsNonceCommitment>>,
    /// The cryptographic checks that the carried votes of the latest checked proposal passed, for
    /// the votes not held
    checked_proposal: Vec<(ValidatorVote, CryptoCheck)>,
    pending: Pending,
    /// `(height, pk_eots, block hash)` of each vote loaded from the store at start, deleted from
    /// the store once at or below the subnet final height, whether the vote was accepted or not
    loaded: BTreeSet<(u64, [u8; 32], [u8; 32])>,
    /// The subnet final height: the greatest height whose votes a block of the chain carries
    recorded_final: u64,
    /// The checkpointed subnet height at the Bitcoin height of the last carried proposal
    agreed_checkpointed: u64,
    /// The heights at which the node holds threshold votes for a block other than the chain's:
    /// a second final block, above which nothing is served as final
    conflicts: BTreeSet<u64>,
    /// The EOTS keys whose evidence a block carried
    evidence_carried: HashSet<[u8; 32]>,
    /// The greatest height this validator voted at
    last_cast: u64,
    /// Set once the chain is seen to have left the checkpointed block behind
    on_dead_fork: bool,
}

#[derive(Clone)]
pub struct EotsTally {
    inner: Arc<Mutex<Inner>>,
    /// The entries to delete from the store in the background
    deletions: mpsc::Sender<Deletion>,
    store: Arc<dyn EotsStore>,
    provider: BtcSubnetManager,
    subnet_id: SubnetID,
    txid20: [u8; 20],
    envelope_check: EnvelopeCheck,
    /// The latest committed height, for the voting loop
    committed: tokio::sync::watch::Sender<u64>,
    /// Held across a whole read of the recorded state, so that two reads never overlap
    monitor_reading: Arc<tokio::sync::Mutex<()>>,
}

pub fn key_bytes(key: &ValidatorKey) -> KeyBytes {
    key_bytes_of(&libp2p::identity::PublicKey::from(key.clone()))
}

pub fn key_bytes_of(key: &libp2p::identity::PublicKey) -> KeyBytes {
    try_key_bytes_of(key).expect("validator keys are secp256k1")
}

/// The secp256k1 key bytes of a gossip sender, `None` for any other key type.
pub fn try_key_bytes(key: &ValidatorKey) -> Option<KeyBytes> {
    try_key_bytes_of(&libp2p::identity::PublicKey::from(key.clone()))
}

pub fn try_key_bytes_of(key: &libp2p::identity::PublicKey) -> Option<KeyBytes> {
    key.clone().try_into_secp256k1().ok().map(|k| k.to_bytes())
}

/// The x-only form of a compressed key, as Bitcoin names validators.
pub fn x_only(key: &KeyBytes) -> [u8; 32] {
    key[1..].try_into().expect("32 bytes")
}

fn threshold(table: &HashMap<KeyBytes, Weight>) -> Weight {
    table.values().sum::<Weight>() * 2 / 3 + 1
}

/// `final_height`, at most one below `conflict`: the height at which a node holds a final block
/// other than its chain's.
pub fn below_conflict(final_height: u64, conflict: Option<u64>) -> u64 {
    conflict.map_or(final_height, |c| final_height.min(c.saturating_sub(1)))
}

fn to_key_bytes(validator: &[u8]) -> Option<KeyBytes> {
    validator.try_into().ok()
}

impl Inner {
    fn power_table_at(&self, height: u64) -> Option<&HashMap<KeyBytes, Weight>> {
        self.power_tables
            .range(..=height)
            .next_back()
            .map(|(_, t)| t)
    }

    fn power_at(&self, key: &KeyBytes, height: u64) -> Weight {
        self.power_table_at(height)
            .and_then(|t| t.get(key))
            .copied()
            .unwrap_or(0)
    }

    fn checkpointed(&self) -> u64 {
        self.checkpoint.map_or(0, |(h, _, _)| h)
    }

    /// Rules 1 to 5 of accepting a vote, without running a cryptographic check: a check that the
    /// vote passed (in `passed`, as a held vote, or in the latest checked proposal) counts, the
    /// first other one is returned.
    fn check(
        &self,
        vote: &ValidatorVote,
        carried: Option<(&AgreedState, u64)>,
        passed: &[CryptoCheck],
    ) -> Checked {
        let proven = |check| passed.contains(&check) || self.proven(vote, &check);
        if !proven(CryptoCheck::Envelope) {
            return Checked::Unverified(CryptoCheck::Envelope);
        }
        match self.check_rules(vote, carried) {
            Err(e) => Checked::Failed(e),
            Ok(c) if proven(CryptoCheck::Vote(c)) => Checked::Passed(c),
            Ok(c) => Checked::Unverified(CryptoCheck::Vote(c)),
        }
    }

    /// Whether `vote` passed `check` as a held vote or as a carried vote of the latest checked
    /// proposal.
    fn proven(&self, vote: &ValidatorVote, check: &CryptoCheck) -> bool {
        let v = &vote.vote;
        let held = self
            .votes
            .get(&v.height)
            .and_then(|by_key| by_key.get(&v.pk_eots))
            .and_then(|vs| vs.iter().find(|held| held.vote.block_hash == v.block_hash))
            .is_some_and(|held| match check {
                CryptoCheck::Envelope => held == vote,
                CryptoCheck::Vote(c) => {
                    held.vote == *v
                        && self
                            .verified
                            .get(&v.height)
                            .and_then(|by_key| by_key.get(&(v.pk_eots, v.block_hash)))
                            == Some(c)
                }
            });
        held || self
            .checked_proposal
            .iter()
            .any(|(p, c)| p == vote && c == check)
    }

    /// Rules 2 to 4 of accepting a vote, without the audit path: the commitment that must prove
    /// its public nonce. A carried vote is checked against `agreed` and the height of the subnet
    /// block that carries it, whose parent is the chain's latest block; a gossiped one against the
    /// latest recorded state and the chain's latest block.
    fn check_rules(
        &self,
        vote: &ValidatorVote,
        carried: Option<(&AgreedState, u64)>,
    ) -> Result<EotsNonceCommitment, VoteError> {
        // Fails rule 4 at every recorded state, so it is never kept for a later check.
        if vote.vote.proof.len() > ipc_eots::MAX_AUDIT_PATH {
            return Err(VoteError::Invalid(ipc_eots::EotsError::InvalidProof));
        }
        let (checkpointed, killed, max_bitcoin, latest) = match carried {
            Some((a, carrying)) => (
                a.checkpointed,
                a.killed,
                Some(a.bitcoin_height),
                carrying.saturating_sub(1),
            ),
            None => (
                self.checkpointed(),
                self.checkpoint.is_some_and(|(_, _, k)| k),
                None,
                self.latest(),
            ),
        };
        let h = vote.vote.height;
        if h <= checkpointed {
            return Err(VoteError::Checkpointed(h, checkpointed));
        }
        if killed {
            return Err(VoteError::Killed);
        }
        if h > latest {
            return Err(VoteError::AboveLatest(h, latest));
        }
        let key = to_key_bytes(&vote.validator).ok_or(VoteError::WrongEotsKey)?;
        if self.eots_keys.get(&x_only(&key)) != Some(&vote.vote.pk_eots) {
            return Err(VoteError::WrongEotsKey);
        }
        if self.power_at(&key, h) == 0 {
            return Err(VoteError::UnpoweredValidator(h));
        }
        self.commitments
            .get(&vote.vote.pk_eots)
            .into_iter()
            .flatten()
            .filter(|(_, bitcoin_height)| max_bitcoin.is_none_or(|m| *bitcoin_height <= m))
            .map(|(c, _)| *c)
            .find(|c| c.covers(h) && c.start == vote.vote.start)
            .ok_or(VoteError::NotCommitted(h))
    }

    /// Stores an accepted vote, verified against `commitment`: the first for its key and height,
    /// or the first for another block.
    fn store(
        &mut self,
        vote: ValidatorVote,
        commitment: EotsNonceCommitment,
    ) -> (VoteOutcome, Option<ValidatorVote>) {
        let held = self
            .votes
            .entry(vote.vote.height)
            .or_default()
            .entry(vote.vote.pk_eots)
            .or_default();
        if held.iter().any(|v| v.vote == vote.vote) {
            return (VoteOutcome::Known, None);
        }
        let outcome = match held.len() {
            0 => VoteOutcome::New,
            1 if held[0].vote.block_hash != vote.vote.block_hash => VoteOutcome::Evidence,
            _ => return (VoteOutcome::Excess, None),
        };
        held.push(vote.clone());
        self.verified
            .entry(vote.vote.height)
            .or_default()
            .insert((vote.vote.pk_eots, vote.vote.block_hash), commitment);
        (outcome, Some(vote))
    }

    fn weight_for(&self, height: u64, hash: &[u8; 32]) -> Weight {
        self.votes
            .get(&height)
            .into_iter()
            .flatten()
            .filter_map(|(_, votes)| votes.iter().find(|v| v.vote.block_hash == *hash))
            .filter_map(|v| to_key_bytes(&v.validator))
            .map(|k| self.power_at(&k, height))
            .sum()
    }

    /// Whether the votes held for the chain's block at `height` reach the threshold.
    fn held_threshold(&self, height: u64) -> bool {
        let Some(hash) = self.chain.get(&height) else {
            return false;
        };
        let Some(table) = self.power_table_at(height) else {
            return false;
        };
        self.weight_for(height, hash) >= threshold(table)
    }

    /// The greatest height whose block the node knows to be final by the votes it holds:
    /// contiguous from the recorded final and checkpointed heights.
    fn known_final(&self) -> u64 {
        let mut h = self.recorded_final.max(self.checkpointed());
        while self.held_threshold(h + 1) {
            h += 1;
        }
        h
    }

    /// The subnet final height this node serves: the recorded one, below the first height at
    /// which it holds a final block other than the chain's.
    fn served_final(&self) -> u64 {
        below_conflict(self.recorded_final, self.conflicts.first().copied())
    }

    /// The x-only key of the validator of a vote that failed rule 4, when the vote is kept for a
    /// check after the next read of the recorded state: the EOTS nonce commitment the vote names
    /// by its start is not recorded, and the vote's height is above the subnet final height and
    /// at most the chain's latest block.
    fn awaits_commitment(&self, vote: &ValidatorVote) -> Option<[u8; 32]> {
        let key = to_key_bytes(&vote.validator)?;
        let vote = &vote.vote;
        let named_recorded = self
            .commitments
            .get(&vote.pk_eots)
            .into_iter()
            .flatten()
            .any(|(c, _)| c.start == vote.start);
        (!named_recorded && vote.start <= vote.height && self.keeps_height(vote.height))
            .then(|| x_only(&key))
    }

    /// The x-only key of the validator of a vote that failed rule 3, when the vote is kept for a
    /// check after the first read of the recorded state: no read happened yet, the validator has
    /// power at the vote's height, and the height is above the subnet final height and at most the
    /// chain's latest block.
    fn awaits_eots_key(&self, vote: &ValidatorVote) -> Option<[u8; 32]> {
        let key = to_key_bytes(&vote.validator)?;
        (!self.monitor_read
            && self.power_at(&key, vote.vote.height) > 0
            && self.keeps_height(vote.vote.height))
        .then(|| x_only(&key))
    }

    /// The x-only key of the validator of a vote above the chain's latest block, when the vote is
    /// kept for a check when the chain has the block at its height: the height is at most
    /// `MAX_HEIGHTS_ABOVE_LATEST` above that block, and the vote's EOTS key is the one recorded
    /// for the validator, or, before the first read of the recorded state, the validator has
    /// power at the height. The power at the height is checked when the vote is checked again: a
    /// configuration that seats the validator there may not be executed yet.
    fn awaits_block(&self, vote: &ValidatorVote) -> Option<[u8; 32]> {
        let key = to_key_bytes(&vote.validator)?;
        let height = vote.vote.height;
        let known = if self.monitor_read {
            self.eots_keys.get(&x_only(&key)) == Some(&vote.vote.pk_eots)
        } else {
            self.power_at(&key, height) > 0
        };
        (height - self.latest() <= MAX_HEIGHTS_ABOVE_LATEST && known).then(|| x_only(&key))
    }

    fn keeps_height(&self, height: u64) -> bool {
        height > self.recorded_final && height <= self.latest()
    }

    /// The height of the chain's latest block, at least the subnet final height.
    fn latest(&self) -> u64 {
        self.latest_height.max(self.recorded_final)
    }

    /// Reports double votes and a second final block at `height`, which caps the served finality.
    fn check_conflict(&mut self, height: u64) {
        let Some(ours) = self.chain.get(&height).copied() else {
            return;
        };
        let Some(table) = self.power_table_at(height) else {
            return;
        };
        let threshold = threshold(table);
        let others: BTreeSet<[u8; 32]> = self
            .votes
            .get(&height)
            .into_iter()
            .flatten()
            .flat_map(|(_, vs)| vs.iter().map(|v| v.vote.block_hash))
            .filter(|b| *b != ours)
            .collect();
        for other in others {
            let weight = self.weight_for(height, &other);
            if weight < threshold {
                tracing::error!(
                    height,
                    ours = hex::encode(ours),
                    other = hex::encode(other),
                    weight,
                    threshold,
                    "double EOTS votes for another block at this height"
                );
            } else if self.conflicts.insert(height) {
                tracing::error!(
                    height,
                    ours = hex::encode(ours),
                    ours_weight = self.weight_for(height, &ours),
                    other = hex::encode(other),
                    weight,
                    threshold,
                    "another block at this height is final: this node serves no block at or above it as final"
                );
            }
        }
    }

    fn evidence(&self) -> impl Iterator<Item = (&ValidatorVote, &ValidatorVote)> + '_ {
        self.votes
            .values()
            .flat_map(|by_key| by_key.values())
            .filter(|vs| vs.len() == 2)
            .map(|vs| (&vs[0], &vs[1]))
    }

    /// Deletes the votes, block hashes and pending votes at or below the subnet final height and
    /// the power tables before the one in force above it, and returns their store entries.
    fn delete_final(&mut self) -> FinalEntries {
        let above = self.recorded_final + 1;
        let kept = self.votes.split_off(&above);
        let votes = std::mem::replace(&mut self.votes, kept);
        self.verified = self.verified.split_off(&above);
        let kept = self.chain.split_off(&above);
        let blocks = std::mem::replace(&mut self.chain, kept);
        self.pending.stored.retain(|v| v.vote.height >= above);
        let kept = self.loaded.split_off(&(above, [0; 32], [0; 32]));
        let loaded = std::mem::replace(&mut self.loaded, kept);
        self.pending.uncommitted.delete_up_to(self.recorded_final);
        self.pending.wrong_key.delete_up_to(self.recorded_final);
        self.pending.above_latest.delete_up_to(self.recorded_final);
        let in_force = self
            .power_tables
            .range(..=above)
            .next_back()
            .map_or(0, |(h, _)| *h);
        let kept = self.power_tables.split_off(&in_force);
        let power_tables = std::mem::replace(&mut self.power_tables, kept);
        FinalEntries {
            votes: votes
                .into_values()
                .flat_map(HashMap::into_values)
                .flatten()
                .map(|v| FinalEntries::vote_key(&v))
                .chain(loaded.into_iter().map(|(h, pk, b)| (pk, h, b)))
                .collect(),
            blocks: blocks.into_keys().collect(),
            power_tables: power_tables.into_keys().collect(),
        }
    }
}

impl EotsTally {
    pub fn new(
        store: Arc<dyn EotsStore>,
        provider: BtcSubnetManager,
        subnet_id: SubnetID,
        power_table: Vec<(ValidatorKey, Weight)>,
        envelope_check: EnvelopeCheck,
    ) -> anyhow::Result<Self> {
        let txid20 = subnet_txid20(&subnet_id)?;
        let stored = store.load().context("failed to load the EOTS store")?;
        // The stored tables hold the history of a node that restarts on its data
        let stored_power_tables = !stored.power_tables.is_empty();
        let power_tables = if !stored_power_tables {
            BTreeMap::from([(
                0,
                power_table
                    .iter()
                    .map(|(k, w)| (key_bytes(k), *w))
                    .collect(),
            )])
        } else {
            stored
                .power_tables
                .into_iter()
                .map(|(h, t)| (h, t.into_iter().collect()))
                .collect()
        };
        let loaded = stored
            .votes
            .iter()
            .map(|v| (v.vote.height, v.vote.pk_eots, v.vote.block_hash))
            .collect();
        let mut inner = Inner {
            power_tables,
            eots_keys: HashMap::new(),
            commitments: HashMap::new(),
            checkpoint: None,
            checkpoint_bitcoin_height: 0,
            monitor_read: false,
            monitor_read_for: 0,
            agreed_read: None,
            stored_power_tables,
            chain: stored.blocks.into_iter().collect(),
            latest_height: 0,
            votes: BTreeMap::new(),
            verified: BTreeMap::new(),
            checked_proposal: Vec::new(),
            pending: Pending::new(stored.votes),
            loaded,
            recorded_final: stored.recorded_final,
            agreed_checkpointed: stored.checkpointed,
            conflicts: BTreeSet::new(),
            evidence_carried: stored.evidence_carried.into_iter().collect(),
            last_cast: stored.last_cast,
            on_dead_fork: false,
        };
        inner.latest_height = inner.chain.keys().next_back().copied().unwrap_or(0);
        // Also what a crash left behind: entries dropped from memory but not yet from the store.
        store.delete(&inner.delete_final())?;
        let (deletions, received) = mpsc::channel();
        let deleting = store.clone();
        std::thread::Builder::new()
            .name("eots-store-deletion".into())
            .spawn(move || delete_in_background(deleting, received))
            .context("failed to start the EOTS store deletion")?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            deletions,
            store,
            provider,
            subnet_id,
            txid20,
            envelope_check,
            committed: tokio::sync::watch::channel(0).0,
            monitor_reading: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn provider(&self) -> &BtcSubnetManager {
        &self.provider
    }

    pub fn notify_committed(&self, height: u64) {
        self.committed.send_replace(height);
    }

    pub fn committed(&self) -> tokio::sync::watch::Receiver<u64> {
        self.committed.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("EOTS tally lock")
    }

    /// Runs the cryptographic check `check` of `vote`.
    fn verify(&self, vote: &ValidatorVote, check: &CryptoCheck) -> Result<(), VoteError> {
        match check {
            CryptoCheck::Envelope if (self.envelope_check)(vote) => Ok(()),
            CryptoCheck::Envelope => Err(VoteError::BadEnvelope),
            CryptoCheck::Vote(c) => {
                ipc_eots::verify_vote(&self.txid20, &vote.vote, c).map_err(VoteError::Invalid)
            }
        }
    }

    /// Reads the latest recorded state: see [`Self::read_monitor_at`].
    pub async fn read_monitor(&self) -> anyhow::Result<()> {
        self.read_monitor_at(None).await.map(|_| ())
    }

    /// Reads in one request the EOTS keys and commitments recorded above `monitor_read_for` and the
    /// latest checkpoint, at `max_bitcoin_height` or the latest recorded one. An answer that
    /// advances `monitor_read_for` is added to the tally, and the pending votes are checked
    /// again. Returns the answer's checkpoint.
    async fn read_monitor_at(
        &self,
        max_bitcoin_height: Option<u64>,
    ) -> anyhow::Result<Option<LatestCheckpoint>> {
        let _reading = self.monitor_reading.lock().await;
        let after = {
            let inner = self.lock();
            inner.monitor_read.then_some(inner.monitor_read_for)
        };
        let records = tokio::time::timeout(
            MONITOR_TIMEOUT,
            self.provider
                .eots_records(&self.subnet_id, after, max_bitcoin_height),
        )
        .await
        .map_err(|_| anyhow::anyhow!("no answer within {MONITOR_TIMEOUT:?}"))??;
        let (stored, uncommitted, wrong_key) = {
            let mut inner = self.lock();
            if inner.monitor_read && records.bitcoin_height <= inner.monitor_read_for {
                return Ok(records.checkpoint);
            }
            for (key, commitments) in records.keys {
                inner.eots_keys.insert(key.validator, key.pk_eots);
                inner.commitments.entry(key.pk_eots).or_default().extend(
                    commitments
                        .into_iter()
                        .map(|c| (c.commitment, c.block_height)),
                );
            }
            inner.checkpoint = records
                .checkpoint
                .as_ref()
                .map(|c| (c.height, c.block_hash, c.killed));
            inner.checkpoint_bitcoin_height =
                records.checkpoint.as_ref().map_or(0, |c| c.bitcoin_height);
            inner.monitor_read_for = records.bitcoin_height;
            inner.monitor_read = true;
            let wrong_key: Vec<_> = inner.pending.wrong_key.take().collect();
            let (stored, uncommitted) = inner.pending.take();
            (stored, uncommitted, wrong_key)
        };
        for vote in stored {
            let _ = self.add_vote(vote, None);
        }
        for vote in uncommitted {
            let _ = self.add_vote_passing(vote, None, vec![CryptoCheck::Envelope]);
        }
        self.check_again(wrong_key);
        Ok(records.checkpoint)
    }

    /// Checks kept votes again, which passed rule 1 when kept; a vote that fails is dropped and
    /// logged.
    fn check_again(&self, votes: Vec<ValidatorVote>) {
        for vote in votes {
            match self.add_vote_passing(vote, None, vec![CryptoCheck::Envelope]) {
                Err(e) if e.never_valid() => tracing::error!(error = %e, "EOTS vote dropped"),
                Err(e) => tracing::debug!(error = %e, "EOTS vote dropped"),
                Ok(_) => {}
            }
        }
    }

    /// Adds the chain's block at `height` and checks again the votes kept up to the chain's latest
    /// block.
    pub fn add_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()> {
        self.store.put_block(height, hash)?;
        let reached = {
            let mut inner = self.lock();
            inner.chain.insert(height, hash);
            inner.latest_height = inner.latest_height.max(height);
            inner.check_conflict(height);
            let latest = inner.latest();
            inner.pending.above_latest.take_up_to(latest)
        };
        self.check_again(reached);
        Ok(())
    }

    /// Raises the height of the chain's latest block to the committed `height`, at a start and
    /// after a snapshot import, when the tally may hold no hash at it, and checks again the votes
    /// kept up to it.
    pub fn set_latest_height(&self, height: u64) {
        let reached = {
            let mut inner = self.lock();
            inner.latest_height = inner.latest_height.max(height);
            let latest = inner.latest();
            inner.pending.above_latest.take_up_to(latest)
        };
        self.check_again(reached);
    }

    /// Whether the tally holds a power table from its store, that is from an earlier run.
    pub fn has_stored_power_tables(&self) -> bool {
        self.lock().stored_power_tables
    }

    /// The power table that applies from `height` on.
    pub fn set_power_table(
        &self,
        height: u64,
        table: Vec<(ValidatorKey, Weight)>,
    ) -> anyhow::Result<()> {
        let table = table.iter().map(|(k, w)| (key_bytes(k), *w)).collect();
        self.put_power_table(height, table)
    }

    /// Power changes that apply from `height` on, as the top-down tally receives them.
    pub fn update_power_table(
        &self,
        height: u64,
        updates: Vec<(ValidatorKey, Weight)>,
    ) -> anyhow::Result<()> {
        let mut table = self
            .lock()
            .power_table_at(height)
            .cloned()
            .unwrap_or_default();
        for (k, w) in updates {
            if w == 0 {
                table.remove(&key_bytes(&k));
            } else {
                table.insert(key_bytes(&k), w);
            }
        }
        self.put_power_table(height, table)
    }

    fn put_power_table(&self, height: u64, table: HashMap<KeyBytes, Weight>) -> anyhow::Result<()> {
        let entries: Vec<(KeyBytes, Weight)> = table.iter().map(|(k, w)| (*k, *w)).collect();
        self.store
            .put_power_table(height, &entries)
            .context("failed to persist the EOTS power table")?;
        self.lock().power_tables.insert(height, table);
        Ok(())
    }

    /// Accepts a vote received on the gossip, or carried by the block at the given height and
    /// checked against `agreed`.
    pub fn add_vote(
        &self,
        vote: ValidatorVote,
        carried: Option<(&AgreedState, u64)>,
    ) -> Result<VoteOutcome, VoteError> {
        self.add_vote_passing(vote, carried, Vec::new())
    }

    /// [`Self::add_vote`] for a vote that passed the cryptographic checks in `passed`. The checks
    /// it has not passed run outside the lock.
    fn add_vote_passing(
        &self,
        vote: ValidatorVote,
        carried: Option<(&AgreedState, u64)>,
        mut passed: Vec<CryptoCheck>,
    ) -> Result<VoteOutcome, VoteError> {
        let stored = loop {
            let mut inner = self.lock();
            if vote.vote.height <= inner.recorded_final {
                return Err(VoteError::Final(vote.vote.height, inner.recorded_final));
            }
            let commitment = match inner.check(&vote, carried, &passed) {
                Checked::Passed(commitment) => commitment,
                Checked::Unverified(check) => {
                    drop(inner);
                    self.verify(&vote, &check)?;
                    passed.push(check);
                    continue;
                }
                Checked::Failed(e @ VoteError::AboveLatest(..)) if carried.is_none() => {
                    let Some(validator) = inner.awaits_block(&vote) else {
                        return Err(e);
                    };
                    let height = vote.vote.height;
                    if !inner.pending.above_latest.keep(validator, height, vote) {
                        return Err(e);
                    }
                    return Ok(VoteOutcome::Pending);
                }
                Checked::Failed(e @ VoteError::NotCommitted(_)) if carried.is_none() => {
                    let Some(validator) = inner.awaits_commitment(&vote) else {
                        return Err(e);
                    };
                    let height = vote.vote.height;
                    if !inner.pending.uncommitted.keep(validator, height, vote) {
                        return Err(e);
                    }
                    return Ok(VoteOutcome::Pending);
                }
                Checked::Failed(e @ VoteError::WrongEotsKey) if carried.is_none() => {
                    let Some(validator) = inner.awaits_eots_key(&vote) else {
                        return Err(e);
                    };
                    let height = vote.vote.height;
                    if !inner.pending.wrong_key.keep(validator, height, vote) {
                        return Err(e);
                    }
                    return Ok(VoteOutcome::Pending);
                }
                Checked::Failed(e) => return Err(e),
            };
            let (outcome, stored) = inner.store(vote, commitment);
            if let Some(v) = &stored {
                if inner.chain.get(&v.vote.height) != Some(&v.vote.block_hash) {
                    inner.check_conflict(v.vote.height);
                }
            }
            break (outcome, stored);
        };
        if let (outcome, Some(v)) = stored {
            if let Err(e) = self.store.put_vote(&v) {
                tracing::error!(error = %e, "failed to persist an EOTS vote");
            } else if v.vote.height <= self.lock().recorded_final {
                // The subnet final height passed the vote, and dropped it from memory, while the
                // vote was being stored.
                self.delete(FinalEntries {
                    votes: vec![FinalEntries::vote_key(&v)],
                    ..Default::default()
                });
            }
            if outcome == VoteOutcome::Evidence {
                tracing::warn!(
                    height = v.vote.height,
                    pk_eots = hex::encode(v.vote.pk_eots),
                    "double EOTS vote: evidence held"
                );
            }
            return Ok(outcome);
        }
        Ok(stored.0)
    }

    pub fn status(&self) -> FinalityStatus {
        let inner = self.lock();
        FinalityStatus {
            final_height: inner.served_final(),
            recorded_final_height: inner.recorded_final,
            checkpointed_height: inner.checkpointed(),
        }
    }

    /// The subnet final height this node serves: see [`FinalityStatus::final_height`].
    #[cfg(test)]
    pub fn final_height(&self) -> u64 {
        self.lock().served_final()
    }

    pub fn recorded_final_height(&self) -> u64 {
        self.lock().recorded_final
    }

    /// The first height at which this node holds a final block other than its chain's.
    pub fn first_conflict(&self) -> Option<u64> {
        self.lock().conflicts.first().copied()
    }

    /// The checkpointed subnet height at the Bitcoin height of the last carried proposal, 0
    /// while no checkpoint was recorded there.
    pub fn agreed_checkpointed_height(&self) -> u64 {
        self.lock().agreed_checkpointed
    }

    /// The recorded state at `bitcoin_height`; an error until it reaches that Bitcoin height.
    pub async fn agreed_state(&self, bitcoin_height: u64) -> anyhow::Result<AgreedState> {
        // No checkpoint was recorded between the held one and `monitor_read_for`; otherwise the
        // answer of the latest read for an agreed state, if at this Bitcoin height
        let held = {
            let inner = self.lock();
            (inner.monitor_read
                && inner.monitor_read_for >= bitcoin_height
                && inner.checkpoint_bitcoin_height <= bitcoin_height)
                .then_some(inner.checkpoint)
                .or_else(|| {
                    inner
                        .agreed_read
                        .filter(|(b, _)| *b == bitcoin_height)
                        .map(|(_, c)| c)
                })
        };
        let checkpoint = match held {
            Some(checkpoint) => checkpoint,
            None => {
                let checkpoint = self
                    .read_monitor_at(Some(bitcoin_height))
                    .await?
                    .map(|c| (c.height, c.block_hash, c.killed));
                self.lock().agreed_read = Some((bitcoin_height, checkpoint));
                checkpoint
            }
        };
        Ok(AgreedState {
            checkpointed: checkpoint.map_or(0, |(h, _, _)| h),
            killed: checkpoint.is_some_and(|(_, _, k)| k),
            bitcoin_height,
        })
    }

    /// `agreed_state`, asked again until it answers.
    pub async fn agreed_state_wait(&self, bitcoin_height: u64) -> AgreedState {
        ask_until_answered(
            &format!("the agreed Bitcoin L1 state at height {bitcoin_height}"),
            MONITOR_RETRY,
            MONITOR_TIMEOUT,
            || self.agreed_state(bitcoin_height),
        )
        .await
    }

    /// Apply the committed finality of the block at `carrying` and log the carried votes that
    /// are not held.
    pub async fn execute_proposal(
        &self,
        data: &EotsFinalityData,
        carrying: u64,
    ) -> anyhow::Result<()> {
        let agreed = self.agreed_state_wait(data.bitcoin_height).await;
        for (vote, e) in self.apply_proposal(data, &agreed, carrying)? {
            tracing::error!(
                height = vote.vote.height,
                validator = hex::encode(&vote.validator),
                error = %e,
                "carried vote rejected at execution: not held, the block stands"
            );
        }
        Ok(())
    }

    /// The final heights above the recorded one that fit in the block at `carrying`, with their
    /// votes and evidence.
    pub fn proposal(&self, agreed: &AgreedState, carrying: u64) -> Option<EotsFinalityData> {
        let inner = self.lock();
        let carried = Some((agreed, carrying));
        // A held vote passed its cryptographic checks; one whose audit path was verified against
        // another commitment than the one checked here fails against it.
        let passes = |v: &ValidatorVote| matches!(inner.check(v, carried, &[]), Checked::Passed(_));
        let mut data = EotsFinalityData {
            bitcoin_height: agreed.bitcoin_height,
            ..Default::default()
        };
        let mut size = 0usize;
        let mut h = inner.recorded_final.max(agreed.checkpointed);
        loop {
            let next = h + 1;
            let Some(hash) = inner.chain.get(&next) else {
                break;
            };
            let Some(table) = inner.power_table_at(next) else {
                break;
            };
            let mut held: Vec<(Weight, &ValidatorVote)> = inner
                .votes
                .get(&next)
                .into_iter()
                .flatten()
                .flat_map(|(_, vs)| vs.iter().filter(|v| v.vote.block_hash == *hash))
                .map(|v| {
                    let power = to_key_bytes(&v.validator).map_or(0, |k| inner.power_at(&k, next));
                    (power, v)
                })
                .collect();
            // The fewest votes that pass the checks and reach the threshold: the greatest powers
            // first, equal powers by EOTS key.
            held.sort_by(|(pa, a), (pb, b)| pb.cmp(pa).then(a.vote.pk_eots.cmp(&b.vote.pk_eots)));
            let needed = threshold(table);
            let mut weight: Weight = 0;
            let mut votes = Vec::new();
            for (power, v) in held {
                if weight >= needed {
                    break;
                }
                if passes(v) {
                    weight += power;
                    votes.push(v.clone());
                }
            }
            if weight < needed {
                break;
            }
            let bytes: usize = votes.iter().map(vote_size).sum();
            if !data.votes.is_empty() && size + bytes > MAX_CARRIED_BYTES {
                break;
            }
            size += bytes;
            data.votes.extend(votes);
            h = next;
        }
        // Only evidence that passes the checks every node applies against `agreed` and
        // `carrying`, one per key.
        let mut taken = HashSet::new();
        for (a, b) in inner.evidence() {
            if !inner.evidence_carried.contains(&a.vote.pk_eots)
                && passes(a)
                && passes(b)
                && taken.insert(a.vote.pk_eots)
            {
                data.evidence.push((a.clone(), b.clone()));
            }
        }
        (!data.votes.is_empty() || !data.evidence.is_empty()).then_some(data)
    }

    /// Checks the votes, evidence and heights (continuity, threshold) that the block at
    /// `carrying` carries, against `agreed`.
    pub fn check_proposal(
        &self,
        data: &EotsFinalityData,
        agreed: &AgreedState,
        carrying: u64,
    ) -> Result<(), String> {
        let carried = Some((agreed, carrying));
        let all: Vec<(&str, &ValidatorVote)> = data
            .votes
            .iter()
            .map(|v| ("vote", v))
            .chain(
                data.evidence
                    .iter()
                    .flat_map(|(a, b)| [("evidence", a), ("evidence", b)]),
            )
            .collect();
        let failed = |kind: &str, v: &ValidatorVote, e: VoteError| {
            format!("carried {kind} at height {}: {e}", v.vote.height)
        };
        self.lock().checked_proposal.clear();
        // Every carried vote is checked again after the cryptographic checks it has not passed
        // run outside the lock.
        let inner = loop {
            let inner = self.lock();
            let mut unverified = Vec::new();
            for (kind, v) in &all {
                match inner.check(v, carried, &[]) {
                    Checked::Passed(_) => {}
                    Checked::Failed(e) => return Err(failed(kind, v, e)),
                    Checked::Unverified(check) => unverified.push((*kind, *v, check)),
                }
            }
            if unverified.is_empty() {
                break inner;
            }
            drop(inner);
            for (kind, v, check) in unverified {
                self.verify(v, &check).map_err(|e| failed(kind, v, e))?;
                self.lock().checked_proposal.push((v.clone(), check));
            }
        };
        let mut by_height: BTreeMap<u64, Vec<&ValidatorVote>> = BTreeMap::new();
        for v in &data.votes {
            by_height.entry(v.vote.height).or_default().push(v);
        }
        let mut expected = inner.recorded_final.max(agreed.checkpointed) + 1;
        for (height, votes) in &by_height {
            if *height != expected {
                return Err(format!(
                    "carried height {height} does not continue {}",
                    expected - 1
                ));
            }
            let hash = inner
                .chain
                .get(height)
                .ok_or_else(|| format!("no block at carried height {height}"))?;
            let table = inner
                .power_table_at(*height)
                .ok_or_else(|| format!("no power table at height {height}"))?;
            let mut seen = HashSet::new();
            let weight: Weight = votes
                .iter()
                .filter(|v| v.vote.block_hash == *hash)
                .filter(|v| seen.insert(v.vote.pk_eots))
                .filter_map(|v| to_key_bytes(&v.validator))
                .map(|k| inner.power_at(&k, *height))
                .sum();
            if weight < threshold(table) {
                return Err(format!(
                    "carried votes at height {height} do not reach the threshold"
                ));
            }
            expected += 1;
        }
        let mut evidence_keys = HashSet::new();
        for (a, b) in &data.evidence {
            if !ipc_eots::is_double_vote(&a.vote, &b.vote) || a.validator != b.validator {
                return Err("carried evidence is not a double vote of one key".to_string());
            }
            if !evidence_keys.insert(a.vote.pk_eots) {
                return Err(format!(
                    "more than one carried evidence of EOTS key {}",
                    hex::encode(a.vote.pk_eots)
                ));
            }
        }
        Ok(())
    }

    /// Holds the votes and evidence that the block at `carrying` carries, advances the final
    /// height, returns the rejected votes.
    pub fn apply_proposal(
        &self,
        data: &EotsFinalityData,
        agreed: &AgreedState,
        carrying: u64,
    ) -> anyhow::Result<Vec<(ValidatorVote, VoteError)>> {
        let mut rejected = Vec::new();
        for v in data
            .votes
            .iter()
            .chain(data.evidence.iter().flat_map(|(a, b)| [a, b]))
        {
            match self.add_vote(v.clone(), Some((agreed, carrying))) {
                // A subnet block executed again after a restart: its votes are final already
                Ok(_) | Err(VoteError::Final(..)) => {}
                Err(e) => rejected.push((v.clone(), e)),
            }
        }
        if let Some(last) = data.votes.iter().map(|v| v.vote.height).max() {
            self.set_recorded_final(last)?;
        }
        let mut inner = self.lock();
        inner.checked_proposal.clear();
        if agreed.checkpointed > inner.agreed_checkpointed {
            inner.agreed_checkpointed = agreed.checkpointed;
            self.store.put_checkpointed(agreed.checkpointed)?;
        }
        for (a, _) in &data.evidence {
            if inner.evidence_carried.insert(a.vote.pk_eots) {
                self.store.put_evidence_carried(a.vote.pk_eots)?;
            }
        }
        Ok(rejected)
    }

    /// The heights up to `committed` this validator has not voted at yet.
    pub fn votes_to_cast(&self, committed: u64) -> Vec<(u64, [u8; 32])> {
        let mut inner = self.lock();
        if !inner.monitor_read {
            return Vec::new();
        }
        if let Some((h_c, b_c, _)) = inner.checkpoint {
            match inner.chain.get(&h_c) {
                Some(hash) if *hash != b_c => {
                    if !inner.on_dead_fork {
                        tracing::error!(
                            height = h_c,
                            ours = hex::encode(hash),
                            checkpointed = hex::encode(b_c),
                            "this chain does not hold the checkpointed block: voting stops"
                        );
                        inner.on_dead_fork = true;
                    }
                    return Vec::new();
                }
                None if h_c > 0 && inner.chain.keys().next().is_some_and(|first| *first <= h_c) => {
                    return Vec::new();
                }
                _ => {}
            }
        }
        let from = inner.last_cast.max(inner.known_final()) + 1;
        if from > committed {
            return Vec::new();
        }
        inner
            .chain
            .range(from..=committed)
            .map(|(h, hash)| (*h, *hash))
            .collect()
    }

    /// Records, across restarts, that this validator's vote at `height` is signed and published:
    /// the height is not asked for again. A height whose signing failed is asked again at the
    /// next attempt.
    pub fn mark_cast(&self, height: u64) {
        let mut inner = self.lock();
        if height > inner.last_cast {
            inner.last_cast = height;
            if let Err(e) = self.store.put_last_cast(height) {
                tracing::error!(error = %e, "failed to persist the greatest height voted at");
            }
        }
    }

    /// Takes the subnet final height from the gateway: raises the recorded one to `height` and
    /// deletes what the tally holds at or below it, in memory here and in the store in the
    /// background.
    pub fn set_recorded_final(&self, height: u64) -> anyhow::Result<()> {
        let mut inner = self.lock();
        if height > inner.recorded_final {
            self.store.put_recorded_final(height)?;
            inner.recorded_final = height;
            self.delete(inner.delete_final());
        }
        Ok(())
    }

    /// Hands entries dropped from memory to the background deletion from the store.
    fn delete(&self, entries: FinalEntries) {
        if !entries.is_empty() {
            // Fails only if the deletion thread has stopped; the next start deletes the entries.
            let _ = self.deletions.send(Deletion::Entries(entries));
        }
    }

    /// Waits until the store has deleted every entry handed to the background deletion so far.
    #[cfg(test)]
    pub fn wait_for_deletions(&self) {
        let (done, deleted) = mpsc::channel();
        if self.deletions.send(Deletion::Wait(done)).is_ok() {
            let _ = deleted.recv();
        }
    }

    /// The height of the block that would carry votes now: one above the chain's latest block.
    #[cfg(test)]
    pub fn next_height(&self) -> u64 {
        self.lock().latest() + 1
    }

    /// Every double vote held.
    #[cfg(test)]
    pub fn evidence(&self) -> Vec<(ValidatorVote, ValidatorVote)> {
        self.lock()
            .evidence()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect()
    }
}

fn vote_size(v: &ValidatorVote) -> usize {
    fvm_ipld_encoding::to_vec(v)
        .expect("an EOTS vote encodes")
        .len()
}

/// The 20 bytes inside the subnet's id: the subaddress of its delegated actor address.
pub fn subnet_txid20(subnet_id: &SubnetID) -> anyhow::Result<[u8; 20]> {
    use fvm_shared::address::Payload;
    let actor = subnet_id.subnet_actor();
    match actor.payload() {
        Payload::Delegated(d) => d
            .subaddress()
            .try_into()
            .map_err(|_| anyhow::anyhow!("the subnet actor address is not 20 bytes")),
        _ => anyhow::bail!("the subnet actor address is not a delegated address"),
    }
}

/// Asks for the recorded state until it answers, every `retry`, each request within `timeout`. It warns on
/// the first unanswered question and on every 30th.
async fn ask_until_answered<T, F, Fut>(what: &str, retry: Duration, timeout: Duration, ask: F) -> T
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempts = 0u64;
    loop {
        let result = match tokio::time::timeout(timeout, ask()).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("no answer within {timeout:?}")),
        };
        match result {
            Ok(answer) => {
                if attempts > 0 {
                    tracing::info!(what, attempts, "monitor answered after waiting");
                }
                return answer;
            }
            Err(e) => {
                attempts += 1;
                if attempts == 1 || attempts % 30 == 0 {
                    tracing::warn!(what, attempts, error = %e, "waiting for the monitor to answer");
                }
                tokio::time::sleep(retry).await;
            }
        }
    }
}

#[cfg(test)]
mod tests;
