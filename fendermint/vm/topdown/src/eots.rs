//! The tally of EOTS votes on subnet blocks and the votes a proposer carries in a block.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use ipc_api::eots::LatestCheckpoint;
use ipc_api::subnet_id::SubnetID;
use ipc_eots::{EotsFinalityData, EotsNonceCommitment, EotsVote, ValidatorVote};
use ipc_ipld_resolver::ValidatorKey;
use ipc_provider::manager::BtcSubnetManager;
use serde::{Deserialize, Serialize};

use crate::voting::Weight;

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
    /// Deletes the votes and block hashes at or below `height` and the power tables that apply
    /// from a height below `power_tables_from`.
    fn delete_up_to(&self, height: u64, power_tables_from: u64) -> anyhow::Result<()>;
    fn load(&self) -> anyhow::Result<StoredState>;
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
    /// The vote's EOTS nonce commitment is not recorded yet: kept for a later check
    Pending,
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

/// The votes checked again after every read of the recorded state.
#[derive(Default)]
struct Pending {
    /// The votes loaded from the store at start
    stored: Vec<ValidatorVote>,
    /// Votes that failed rule 4 only, one per subnet height and EOTS key
    uncommitted: BTreeMap<(u64, [u8; 32]), ValidatorVote>,
}

impl Pending {
    fn take(&mut self) -> Vec<ValidatorVote> {
        let mut votes = std::mem::take(&mut self.stored);
        votes.extend(std::mem::take(&mut self.uncommitted).into_values());
        votes
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.stored.len() + self.uncommitted.len()
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
    /// Whether the power tables come from the store
    stored_power_tables: bool,
    /// The chain's block hashes
    chain: BTreeMap<u64, [u8; 32]>,
    /// The accepted votes by height and EOTS key, at most two per key
    votes: BTreeMap<u64, HashMap<[u8; 32], Vec<ValidatorVote>>>,
    pending: Pending,
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
    envelope_check: EnvelopeCheck,
}

#[derive(Clone)]
pub struct EotsTally {
    inner: Arc<Mutex<Inner>>,
    store: Arc<dyn EotsStore>,
    provider: BtcSubnetManager,
    subnet_id: SubnetID,
    txid20: [u8; 20],
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

    /// Rules 2 to 5 of accepting a vote, against `agreed` or the latest recorded state.
    fn check(
        &self,
        vote: &ValidatorVote,
        agreed: Option<&AgreedState>,
        txid20: &[u8; 20],
    ) -> Result<(), VoteError> {
        if !(self.envelope_check)(vote) {
            return Err(VoteError::BadEnvelope);
        }
        let (checkpointed, killed, max_bitcoin) = match agreed {
            Some(a) => (a.checkpointed, a.killed, Some(a.bitcoin_height)),
            None => (
                self.checkpointed(),
                self.checkpoint.is_some_and(|(_, _, k)| k),
                None,
            ),
        };
        let h = vote.vote.height;
        if h <= checkpointed {
            return Err(VoteError::Checkpointed(h, checkpointed));
        }
        if killed {
            return Err(VoteError::Killed);
        }
        let key = to_key_bytes(&vote.validator).ok_or(VoteError::WrongEotsKey)?;
        if self.eots_keys.get(&x_only(&key)) != Some(&vote.vote.pk_eots) {
            return Err(VoteError::WrongEotsKey);
        }
        if self.power_at(&key, h) == 0 {
            return Err(VoteError::UnpoweredValidator(h));
        }
        let commitment = self
            .commitments
            .get(&vote.vote.pk_eots)
            .into_iter()
            .flatten()
            .filter(|(_, bitcoin_height)| max_bitcoin.is_none_or(|m| *bitcoin_height <= m))
            .map(|(c, _)| c)
            .find(|c| c.covers(h) && c.start == vote.vote.start)
            .ok_or(VoteError::NotCommitted(h))?;
        ipc_eots::verify_vote(txid20, &vote.vote, commitment).map_err(VoteError::Invalid)
    }

    /// Stores an accepted vote: the first for its key and height, or the first for another block.
    fn store(&mut self, vote: ValidatorVote) -> (VoteOutcome, Option<ValidatorVote>) {
        let held = self
            .votes
            .entry(vote.vote.height)
            .or_default()
            .entry(vote.vote.pk_eots)
            .or_default();
        if held.iter().any(|v| v.vote == vote.vote) {
            return (VoteOutcome::Known, None);
        }
        match held.len() {
            0 => {
                held.push(vote.clone());
                (VoteOutcome::New, Some(vote))
            }
            1 if held[0].vote.block_hash != vote.vote.block_hash => {
                held.push(vote.clone());
                (VoteOutcome::Evidence, Some(vote))
            }
            _ => (VoteOutcome::Excess, None),
        }
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

    /// Whether a vote that failed rule 4 is kept for a check after the next read of the recorded
    /// state: the EOTS nonce commitment the vote names by its start is not recorded, and
    /// the vote's height is above the subnet final height and at most the chain's latest block.
    fn awaits_commitment(&self, vote: &EotsVote) -> bool {
        let named_recorded = self
            .commitments
            .get(&vote.pk_eots)
            .into_iter()
            .flatten()
            .any(|(c, _)| c.start == vote.start);
        let latest = self.chain.keys().next_back().copied().unwrap_or(0);
        !named_recorded
            && vote.start <= vote.height
            && vote.height > self.recorded_final
            && vote.height <= latest
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
    /// the power tables before the one in force above it, whose first height it returns.
    fn delete_final(&mut self) -> u64 {
        let above = self.recorded_final + 1;
        self.votes = self.votes.split_off(&above);
        self.chain = self.chain.split_off(&above);
        self.pending.stored.retain(|v| v.vote.height >= above);
        self.pending.uncommitted.retain(|(h, _), _| *h >= above);
        let in_force = self
            .power_tables
            .range(..=above)
            .next_back()
            .map_or(0, |(h, _)| *h);
        self.power_tables = self.power_tables.split_off(&in_force);
        in_force
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
        let mut inner = Inner {
            power_tables,
            eots_keys: HashMap::new(),
            commitments: HashMap::new(),
            checkpoint: None,
            checkpoint_bitcoin_height: 0,
            monitor_read: false,
            monitor_read_for: 0,
            stored_power_tables,
            chain: stored.blocks.into_iter().collect(),
            votes: BTreeMap::new(),
            pending: Pending {
                stored: stored.votes,
                ..Default::default()
            },
            recorded_final: stored.recorded_final,
            agreed_checkpointed: stored.checkpointed,
            conflicts: BTreeSet::new(),
            evidence_carried: stored.evidence_carried.into_iter().collect(),
            last_cast: stored.last_cast,
            on_dead_fork: false,
            envelope_check,
        };
        let power_tables_from = inner.delete_final();
        store.delete_up_to(inner.recorded_final, power_tables_from)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            store,
            provider,
            subnet_id,
            txid20,
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
        let pending = {
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
            inner.pending.take()
        };
        for vote in pending {
            let _ = self.add_vote(vote, None);
        }
        Ok(records.checkpoint)
    }

    pub fn add_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()> {
        self.store.put_block(height, hash)?;
        let mut inner = self.lock();
        inner.chain.insert(height, hash);
        inner.check_conflict(height);
        Ok(())
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

    /// Accepts a vote received on the gossip or carried by a block.
    pub fn add_vote(
        &self,
        vote: ValidatorVote,
        agreed: Option<&AgreedState>,
    ) -> Result<VoteOutcome, VoteError> {
        let stored = {
            let mut inner = self.lock();
            if vote.vote.height <= inner.recorded_final {
                return Err(VoteError::Final(vote.vote.height, inner.recorded_final));
            }
            match inner.check(&vote, agreed, &self.txid20) {
                Ok(()) => {}
                Err(VoteError::NotCommitted(_))
                    if agreed.is_none() && inner.awaits_commitment(&vote.vote) =>
                {
                    let key = (vote.vote.height, vote.vote.pk_eots);
                    inner.pending.uncommitted.entry(key).or_insert(vote);
                    return Ok(VoteOutcome::Pending);
                }
                Err(e) => return Err(e),
            }
            let (outcome, stored) = inner.store(vote);
            if let Some(v) = &stored {
                if inner.chain.get(&v.vote.height) != Some(&v.vote.block_hash) {
                    inner.check_conflict(v.vote.height);
                }
            }
            (outcome, stored)
        };
        if let (outcome, Some(v)) = stored {
            if let Err(e) = self.store.put_vote(&v) {
                tracing::error!(error = %e, "failed to persist an EOTS vote");
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
        // No checkpoint was recorded between the held one and `monitor_read_for`
        let held = {
            let inner = self.lock();
            (inner.monitor_read
                && inner.monitor_read_for >= bitcoin_height
                && inner.checkpoint_bitcoin_height <= bitcoin_height)
                .then_some(inner.checkpoint)
        };
        let checkpoint = match held {
            Some(checkpoint) => checkpoint,
            None => self
                .read_monitor_at(Some(bitcoin_height))
                .await?
                .map(|c| (c.height, c.block_hash, c.killed)),
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

    /// Apply committed finality and log the carried votes that are not held.
    pub async fn execute_proposal(&self, data: &EotsFinalityData) -> anyhow::Result<()> {
        let agreed = self.agreed_state_wait(data.bitcoin_height).await;
        for (vote, e) in self.apply_proposal(data, &agreed)? {
            tracing::error!(
                height = vote.vote.height,
                validator = hex::encode(&vote.validator),
                error = %e,
                "carried vote rejected at execution: not held, the block stands"
            );
        }
        Ok(())
    }

    /// The final heights above the recorded one that fit in a block, with their votes and evidence.
    pub fn proposal(&self, agreed: &AgreedState) -> Option<EotsFinalityData> {
        let inner = self.lock();
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
            let votes: Vec<ValidatorVote> = inner
                .votes
                .get(&next)
                .into_iter()
                .flatten()
                .flat_map(|(_, vs)| vs.iter().filter(|v| v.vote.block_hash == *hash))
                .filter(|v| inner.check(v, Some(agreed), &self.txid20).is_ok())
                .cloned()
                .collect();
            let weight: Weight = votes
                .iter()
                .filter_map(|v| to_key_bytes(&v.validator))
                .map(|k| inner.power_at(&k, next))
                .sum();
            if weight < threshold(table) {
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
        // Only evidence that passes the checks every node applies against `agreed`, one per key.
        let mut taken = HashSet::new();
        for (a, b) in inner.evidence() {
            let passes = |v| inner.check(v, Some(agreed), &self.txid20).is_ok();
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

    /// Checks the carried votes, evidence and heights (continuity, threshold) against `agreed`.
    pub fn check_proposal(
        &self,
        data: &EotsFinalityData,
        agreed: &AgreedState,
    ) -> Result<(), String> {
        let inner = self.lock();
        let mut by_height: BTreeMap<u64, Vec<&ValidatorVote>> = BTreeMap::new();
        for v in &data.votes {
            inner
                .check(v, Some(agreed), &self.txid20)
                .map_err(|e| format!("carried vote at height {}: {e}", v.vote.height))?;
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
            for v in [a, b] {
                inner
                    .check(v, Some(agreed), &self.txid20)
                    .map_err(|e| format!("carried evidence at height {}: {e}", v.vote.height))?;
            }
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

    /// Holds the carried votes and evidence, advances the final height, returns the rejected votes.
    pub fn apply_proposal(
        &self,
        data: &EotsFinalityData,
        agreed: &AgreedState,
    ) -> anyhow::Result<Vec<(ValidatorVote, VoteError)>> {
        let mut rejected = Vec::new();
        for v in data
            .votes
            .iter()
            .chain(data.evidence.iter().flat_map(|(a, b)| [a, b]))
        {
            match self.add_vote(v.clone(), Some(agreed)) {
                // A subnet block executed again after a restart: its votes are final already
                Ok(_) | Err(VoteError::Final(..)) => {}
                Err(e) => rejected.push((v.clone(), e)),
            }
        }
        if let Some(last) = data.votes.iter().map(|v| v.vote.height).max() {
            self.set_recorded_final(last)?;
        }
        let mut inner = self.lock();
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
    /// deletes what the tally holds at or below it.
    pub fn set_recorded_final(&self, height: u64) -> anyhow::Result<()> {
        let mut inner = self.lock();
        if height > inner.recorded_final {
            inner.recorded_final = height;
            self.store.put_recorded_final(height)?;
            let power_tables_from = inner.delete_final();
            self.store.delete_up_to(height, power_tables_from)?;
        }
        Ok(())
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
