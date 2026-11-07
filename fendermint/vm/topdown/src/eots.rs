//! The EOTS voting round: every validator votes on each committed subnet block with its EOTS
//! key, a block is final once the votes for it reach the threshold, and the proposer carries
//! the votes of the final heights in a later block.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use ipc_api::subnet_id::SubnetID;
use ipc_eots::{EotsFinalityData, EotsNonceCommitment, ValidatorVote};
use ipc_ipld_resolver::ValidatorKey;
use ipc_provider::manager::BtcSubnetManager;
use serde::{Deserialize, Serialize};

use crate::voting::Weight;
use crate::{ask_until_answered, MONITOR_RETRY, MONITOR_TIMEOUT};

/// A compressed secp256k1 public key.
pub type KeyBytes = [u8; 33];

/// The size of carried votes a block holds at most: CometBFT's default `max_tx_bytes`, which
/// holds the votes of two heights from 350 members, envelopes included. The first height is
/// carried whatever its size.
pub const MAX_CARRIED_BYTES: usize = 1024 * 1024;

/// What the store keeps across restarts.
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
}

/// Rule 1 of accepting a vote: the vote's envelope signature verifies for this subnet. The
/// application supplies it, as the envelope's content type lives there.
pub type EnvelopeCheck = Arc<dyn Fn(&ValidatorVote) -> bool + Send + Sync>;

/// Persistence of the accepted votes, the chain's block hashes, the recorded final height
/// and the power tables.
pub trait EotsStore: Send + Sync {
    fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()>;
    fn put_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()>;
    fn put_recorded_final(&self, height: u64) -> anyhow::Result<()>;
    /// A store that keeps nothing of it restarts at 0: the next carried proposal sets it again.
    fn put_checkpointed(&self, _height: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_evidence_carried(&self, pk_eots: [u8; 32]) -> anyhow::Result<()>;
    fn put_power_table(&self, height: u64, table: &[(KeyBytes, Weight)]) -> anyhow::Result<()>;
    fn load(&self) -> anyhow::Result<StoredState>;
}

/// The monitor's state at an agreed Bitcoin L1 height, against which carried votes are checked.
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
    #[error("the vote is not for the chain's block at height {0}")]
    NotOurBlock(u64),
}

impl VoteError {
    /// Whether no honest validator sends such a vote at any monitor state: a forged envelope,
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

/// What a received vote turned out to be.
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
    /// Rule 4 cannot be checked yet because the monitor is behind: kept for a later check
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

struct Inner {
    /// The power tables by the first height they apply to
    power_tables: BTreeMap<u64, HashMap<KeyBytes, Weight>>,
    /// Validator key, x-only as Bitcoin names it, to EOTS key
    eots_keys: HashMap<[u8; 32], [u8; 32]>,
    /// EOTS key to its recorded commitments with their Bitcoin heights
    commitments: HashMap<[u8; 32], Vec<(EotsNonceCommitment, u64)>>,
    /// The latest checkpoint the monitor holds: subnet height, block hash, kill flag
    checkpoint: Option<(u64, [u8; 32], bool)>,
    /// Whether the monitor was reached at least once
    refreshed: bool,
    /// The greatest Bitcoin height the monitor had processed before a refresh
    refreshed_for: u64,
    /// Whether the power tables come from the store
    stored_power_tables: bool,
    /// The chain's block hashes
    chain: BTreeMap<u64, [u8; 32]>,
    /// The accepted votes by height and EOTS key, at most two per key
    votes: BTreeMap<u64, HashMap<[u8; 32], Vec<ValidatorVote>>>,
    /// Votes that failed rule 4 while the monitor may be behind
    pending: Vec<ValidatorVote>,
    /// The subnet final height: the greatest height whose votes a block of the chain carries
    recorded_final: u64,
    /// The checkpointed subnet height at the Bitcoin height of the last carried proposal: the
    /// monitor has recorded a checkpoint of that height at a Bitcoin height the chain committed
    agreed_checkpointed: u64,
    /// The heights at which the node holds threshold votes for a block other than the chain's:
    /// a second final block, above which nothing is served as final
    conflicts: BTreeSet<u64>,
    /// The EOTS keys whose evidence a block carried
    evidence_carried: HashSet<[u8; 32]>,
    /// The greatest height this node asked its provider to sign
    last_cast: u64,
    /// Set once the chain is seen to have left the checkpointed block behind
    on_dead_fork: bool,
    envelope_check: EnvelopeCheck,
}

/// The tally of EOTS votes of one subnet.
#[derive(Clone)]
pub struct EotsTally {
    inner: Arc<Mutex<Inner>>,
    store: Arc<dyn EotsStore>,
    provider: BtcSubnetManager,
    subnet_id: SubnetID,
    txid20: [u8; 20],
    /// The latest committed height, for the voting loop
    committed: tokio::sync::watch::Sender<u64>,
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

    /// Rules 2 to 5 of accepting a vote, against `agreed` or the monitor's latest state.
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

    /// The weight of the votes for `hash` at `height`.
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
        match self.conflicts.first() {
            Some(conflict) => self.recorded_final.min(conflict - 1),
            None => self.recorded_final,
        }
    }

    /// Reports the votes held at `height` for blocks other than the chain's: double votes, or a
    /// second final block when they reach the threshold, which stops the served finality below
    /// `height`. The chain is kept.
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

    fn evidence(&self) -> Vec<(ValidatorVote, ValidatorVote)> {
        self.votes
            .values()
            .flat_map(|by_key| by_key.values())
            .filter(|vs| vs.len() == 2)
            .map(|vs| (vs[0].clone(), vs[1].clone()))
            .collect()
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
            refreshed: false,
            refreshed_for: 0,
            stored_power_tables,
            chain: stored.blocks.into_iter().collect(),
            votes: BTreeMap::new(),
            pending: stored.votes,
            recorded_final: stored.recorded_final,
            agreed_checkpointed: stored.checkpointed,
            conflicts: BTreeSet::new(),
            evidence_carried: stored.evidence_carried.into_iter().collect(),
            last_cast: 0,
            on_dead_fork: false,
            envelope_check,
        };
        inner.last_cast = inner.recorded_final;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            store,
            provider,
            subnet_id,
            txid20,
            committed: tokio::sync::watch::channel(0).0,
        })
    }

    pub fn provider(&self) -> &BtcSubnetManager {
        &self.provider
    }

    /// Tells the voting loop that `height` is committed.
    pub fn notify_committed(&self, height: u64) {
        self.committed.send_replace(height);
    }

    pub fn committed(&self) -> tokio::sync::watch::Receiver<u64> {
        self.committed.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("EOTS tally lock")
    }

    pub fn subnet_id(&self) -> &SubnetID {
        &self.subnet_id
    }

    /// Reads the EOTS keys, the commitments and the latest checkpoint from the monitor, then
    /// checks the votes that waited for it.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let keys = self.provider.get_eots_keys(&self.subnet_id).await?;
        let mut commitments = HashMap::new();
        for key in &keys {
            let recorded = self
                .provider
                .get_eots_commitments(&self.subnet_id, &key.pk_eots)
                .await?;
            commitments.insert(
                key.pk_eots,
                recorded
                    .into_iter()
                    .map(|r| (r.commitment, r.block_height))
                    .collect::<Vec<_>>(),
            );
        }
        let checkpoint = self
            .provider
            .latest_checkpoint(&self.subnet_id, None)
            .await?;
        let pending = {
            let mut inner = self.lock();
            inner.eots_keys = keys.iter().map(|k| (k.validator, k.pk_eots)).collect();
            inner.commitments = commitments;
            inner.checkpoint = checkpoint.map(|c| (c.height, c.block_hash, c.killed));
            inner.refreshed = true;
            std::mem::take(&mut inner.pending)
        };
        for vote in pending {
            let _ = self.add_vote(vote, None);
        }
        Ok(())
    }

    /// The chain's block at `height`.
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
            match inner.check(&vote, agreed, &self.txid20) {
                Ok(()) => {}
                Err(VoteError::NotCommitted(_)) if agreed.is_none() && !inner.refreshed => {
                    inner.pending.push(vote);
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
    pub fn final_height(&self) -> u64 {
        self.lock().served_final()
    }

    pub fn recorded_final_height(&self) -> u64 {
        self.lock().recorded_final
    }

    /// The checkpointed subnet height at the Bitcoin height of the last carried proposal, 0
    /// while the monitor had recorded no checkpoint there.
    pub fn agreed_checkpointed_height(&self) -> u64 {
        self.lock().agreed_checkpointed
    }

    /// The final block at `height` with the votes held for it, for heights up to the served
    /// subnet final height.
    pub fn final_block(&self, height: u64) -> Option<([u8; 32], Vec<ValidatorVote>)> {
        let inner = self.lock();
        if height > inner.served_final() || height == 0 {
            return None;
        }
        let hash = *inner.chain.get(&height)?;
        let votes = inner
            .votes
            .get(&height)
            .into_iter()
            .flatten()
            .flat_map(|(_, vs)| vs.iter().filter(|v| v.vote.block_hash == hash).cloned())
            .collect();
        Some((hash, votes))
    }

    /// The monitor's state at the Bitcoin height of the chain's parent finality. An error when
    /// the provider is unreachable or its monitor has not processed `bitcoin_height` yet (the
    /// provider refuses such a height rather than answer with an older checkpoint).
    pub async fn agreed_state(&self, bitcoin_height: u64) -> anyhow::Result<AgreedState> {
        let checkpoint = self
            .provider
            .latest_checkpoint(&self.subnet_id, Some(bitcoin_height))
            .await?;
        // The checks against the agreed state need every key and commitment recorded up to
        // `bitcoin_height`: the monitor has processed it now, so a refresh holds them all
        if self.lock().refreshed_for < bitcoin_height {
            self.refresh().await?;
            let mut inner = self.lock();
            inner.refreshed_for = inner.refreshed_for.max(bitcoin_height);
        }
        Ok(AgreedState {
            checkpointed: checkpoint.as_ref().map_or(0, |c| c.height),
            killed: checkpoint.is_some_and(|c| c.killed),
            bitcoin_height,
        })
    }

    /// `agreed_state` for the decision on a proposal or the execution of a block: the result
    /// depends on the Bitcoin height alone, so a node whose provider is unreachable, or whose
    /// monitor has not processed that height, waits and asks again until it gets it. It never
    /// fails the ABCI call, so it never rejects the proposal, skips the votes or stops.
    pub async fn agreed_state_wait(&self, bitcoin_height: u64) -> AgreedState {
        self.agreed_state_wait_every(bitcoin_height, MONITOR_RETRY, MONITOR_TIMEOUT)
            .await
    }

    async fn agreed_state_wait_every(
        &self,
        bitcoin_height: u64,
        retry: std::time::Duration,
        timeout: std::time::Duration,
    ) -> AgreedState {
        ask_until_answered(
            &format!("the agreed Bitcoin L1 state at height {bitcoin_height}"),
            retry,
            timeout,
            || self.agreed_state(bitcoin_height),
        )
        .await
    }

    /// `refresh` until the monitor answers, for the execution of a block.
    async fn refresh_wait(&self) {
        ask_until_answered(
            "the EOTS keys and commitments",
            MONITOR_RETRY,
            MONITOR_TIMEOUT,
            || self.refresh(),
        )
        .await
    }

    /// The execution of a block that carries `data`. Every node must get the same result, on
    /// first execution and on replay: it waits for the agreed L1 state, and the recorded final
    /// height and the carried evidence follow the block. A vote the check rejects is checked
    /// again after a refresh, as the tally's keys and commitments may be stale (a node without
    /// the voting loop refreshes only here). A vote still rejected is logged and not held: the
    /// block is committed, and the vote set is not part of the chain's state.
    pub async fn execute_proposal(&self, data: &EotsFinalityData) -> anyhow::Result<()> {
        let agreed = self.agreed_state_wait(data.bitcoin_height).await;
        let rejected = self.apply_proposal(data, &agreed)?;
        if rejected.is_empty() {
            return Ok(());
        }
        self.refresh_wait().await;
        for (vote, _) in rejected {
            if let Err(e) = self.add_vote(vote.clone(), Some(&agreed)) {
                tracing::error!(
                    height = vote.vote.height,
                    validator = hex::encode(&vote.validator),
                    error = %e,
                    "carried vote rejected at execution: not held, the block stands"
                );
            }
        }
        Ok(())
    }

    /// What the proposer carries: the votes it holds that reach the threshold for the chain's
    /// blocks above the recorded final height, contiguous, if they pass the checks against
    /// `agreed`, as many heights as fit, and evidence.
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
        // Only evidence that passes the checks every node applies against `agreed`.
        for (a, b) in inner.evidence() {
            let passes = |v| inner.check(v, Some(agreed), &self.txid20).is_ok();
            if !inner.evidence_carried.contains(&a.vote.pk_eots) && passes(&a) && passes(&b) {
                data.evidence.push((a, b));
            }
        }
        (!data.votes.is_empty() || !data.evidence.is_empty()).then_some(data)
    }

    /// The checks of a carried proposal: every vote accepted against `agreed`, the carried
    /// heights continue the recorded final height without a gap and reach the threshold for
    /// the chain's blocks, and every evidence is a double vote.
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
        for (a, b) in &data.evidence {
            for v in [a, b] {
                inner
                    .check(v, Some(agreed), &self.txid20)
                    .map_err(|e| format!("carried evidence at height {}: {e}", v.vote.height))?;
            }
            if !ipc_eots::is_double_vote(&a.vote, &b.vote) || a.validator != b.validator {
                return Err("carried evidence is not a double vote of one key".to_string());
            }
        }
        Ok(())
    }

    /// Applies a carried proposal: its votes and evidence are held as if received, and the
    /// recorded final height advances to its last height. The votes the check rejects are
    /// returned, each with its error.
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
            if let Err(e) = self.add_vote(v.clone(), Some(agreed)) {
                rejected.push((v.clone(), e));
            }
        }
        let last = data.votes.iter().map(|v| v.vote.height).max();
        let mut inner = self.lock();
        if let Some(last) = last {
            if last > inner.recorded_final {
                inner.recorded_final = last;
                self.store.put_recorded_final(last)?;
            }
        }
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

    /// The heights this validator votes at now, in order, up to `committed`: those above the
    /// checkpointed height and the blocks it already knows to be final by the votes it holds,
    /// that it has not cast yet, while its chain holds the checkpointed block.
    pub fn votes_to_cast(&self, committed: u64) -> Vec<(u64, [u8; 32])> {
        let mut inner = self.lock();
        if !inner.refreshed {
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

    /// Records that this validator's vote at `height` is signed and published: the height is
    /// not asked for again. A height whose signing failed is asked again at the next attempt.
    pub fn mark_cast(&self, height: u64) {
        let mut inner = self.lock();
        inner.last_cast = inner.last_cast.max(height);
    }

    /// Takes the subnet final height from the gateway: raises the recorded one to `height`.
    pub fn set_recorded_final(&self, height: u64) -> anyhow::Result<()> {
        let mut inner = self.lock();
        if height > inner.recorded_final {
            inner.recorded_final = height;
            self.store.put_recorded_final(height)?;
        }
        Ok(())
    }

    /// Every double vote held.
    pub fn evidence(&self) -> Vec<(ValidatorVote, ValidatorVote)> {
        self.lock().evidence()
    }

    /// The evidence no block carries yet, which the node publishes until one does.
    pub fn evidence_to_publish(&self) -> Vec<(ValidatorVote, ValidatorVote)> {
        let inner = self.lock();
        inner
            .evidence()
            .into_iter()
            .filter(|(a, _)| !inner.evidence_carried.contains(&a.vote.pk_eots))
            .collect()
    }

    /// The evidence held for one EOTS key.
    pub fn evidence_for(&self, pk_eots: &[u8; 32]) -> Option<(ValidatorVote, ValidatorVote)> {
        self.lock()
            .evidence()
            .into_iter()
            .find(|(a, _)| a.vote.pk_eots == *pk_eots)
    }

    /// This node's own votes above the recorded final height, to publish again.
    pub fn own_votes_above_recorded(&self, own: &KeyBytes) -> Vec<ValidatorVote> {
        let inner = self.lock();
        inner
            .votes
            .range(inner.recorded_final + 1..)
            .flat_map(|(_, by_key)| by_key.values().flatten())
            .filter(|v| to_key_bytes(&v.validator).as_ref() == Some(own))
            .cloned()
            .collect()
    }
}

fn vote_size(v: &ValidatorVote) -> usize {
    v.validator.len() + 32 + 8 + 32 + 64 + 8 + 32 * v.vote.proof.len() + 8 + v.signature.len() + 16
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

#[cfg(test)]
mod tests {
    use super::*;
    use ipc_eots::EotsVote;
    use std::str::FromStr;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MemStore(StdMutex<StoredState>);

    impl EotsStore for MemStore {
        fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()> {
            self.0.lock().unwrap().votes.push(vote.clone());
            Ok(())
        }
        fn put_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()> {
            self.0.lock().unwrap().blocks.push((height, hash));
            Ok(())
        }
        fn put_evidence_carried(&self, _pk_eots: [u8; 32]) -> anyhow::Result<()> {
            Ok(())
        }
        fn put_recorded_final(&self, height: u64) -> anyhow::Result<()> {
            self.0.lock().unwrap().recorded_final = height;
            Ok(())
        }
        fn put_checkpointed(&self, height: u64) -> anyhow::Result<()> {
            self.0.lock().unwrap().checkpointed = height;
            Ok(())
        }
        fn put_power_table(&self, height: u64, table: &[(KeyBytes, Weight)]) -> anyhow::Result<()> {
            let tables = &mut self.0.lock().unwrap().power_tables;
            tables.retain(|(h, _)| *h != height);
            tables.push((height, table.to_vec()));
            Ok(())
        }
        fn load(&self) -> anyhow::Result<StoredState> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn key(seed: u8) -> ValidatorKey {
        let sk = libp2p::identity::secp256k1::SecretKey::try_from_bytes(&mut [seed; 32]).unwrap();
        ValidatorKey::from(
            libp2p::identity::Keypair::from(libp2p::identity::secp256k1::Keypair::from(sk))
                .public(),
        )
    }

    /// A tally over a memory store, with no monitor: its state is set directly
    fn tally(keys: &[(ValidatorKey, Weight)]) -> EotsTally {
        tally_with_provider("http://127.0.0.1:1", keys)
    }

    fn tally_with_provider(provider_url: &str, keys: &[(ValidatorKey, Weight)]) -> EotsTally {
        fvm_shared::address::set_current_network(fvm_shared::address::Network::Testnet);
        let subnet_id =
            SubnetID::from_str("/b4/t410fhor637l2pmjle6whfq7go5upmf74qg6dbr4uzei").unwrap();
        let subnet = ipc_provider::config::subnet::Subnet {
            id: subnet_id.parent().unwrap(),
            config: ipc_provider::config::subnet::SubnetConfig::Btc(
                ipc_provider::config::subnet::BTCSubnet {
                    provider_http: provider_url.parse().unwrap(),
                    provider_timeout: None,
                    auth_token: None,
                },
            ),
        };
        let provider = BtcSubnetManager::new(&subnet).unwrap();
        let t = EotsTally::new(
            Arc::new(MemStore::default()),
            provider,
            subnet_id,
            vec![],
            Arc::new(|_| true),
        )
        .unwrap();
        // As at genesis: the first table is set, and persisted, after the tally exists
        t.set_power_table(0, keys.to_vec()).unwrap();
        t
    }

    fn signer_for(t: &EotsTally, key: &ValidatorKey, seed: u8) -> ipc_eots::testing::TestSigner {
        let signer = ipc_eots::testing::TestSigner::new(seed, 1, 20);
        let mut inner = t.lock();
        inner
            .eots_keys
            .insert(x_only(&key_bytes(key)), signer.pk_eots);
        inner
            .commitments
            .insert(signer.pk_eots, vec![(signer.commitment, 100)]);
        inner.refreshed = true;
        signer
    }

    fn vote_of(
        t: &EotsTally,
        key: &ValidatorKey,
        signer: &ipc_eots::testing::TestSigner,
        h: u64,
        hash: [u8; 32],
    ) -> ValidatorVote {
        ValidatorVote {
            validator: key_bytes(key).to_vec(),
            vote: signer.vote(&t.txid20, h, hash),
            timestamp: 0,
            signature: Vec::new(),
        }
    }

    #[test]
    fn votes_make_blocks_final_in_order_and_double_votes_are_evidence() {
        let (k1, k2, k3) = (key(1), key(2), key(3));
        let t = tally(&[(k1.clone(), 1), (k2.clone(), 1), (k3.clone(), 1)]);
        let (s1, s2, s3) = (
            signer_for(&t, &k1, 11),
            signer_for(&t, &k2, 12),
            signer_for(&t, &k3, 13),
        );
        for h in 1..=4 {
            t.add_block(h, [h as u8; 32]).unwrap();
        }
        // Height 1: one vote is below the threshold 3
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, 1, [1; 32]), None).unwrap(),
            VoteOutcome::New
        );
        assert_eq!(t.lock().known_final(), 0);
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, 1, [1; 32]), None).unwrap(),
            VoteOutcome::Known
        );
        // Votes at height 2 before height 1 is final: held, not final
        assert_eq!(
            t.add_vote(vote_of(&t, &k2, &s2, 2, [2; 32]), None).unwrap(),
            VoteOutcome::New
        );
        assert_eq!(
            t.add_vote(vote_of(&t, &k3, &s3, 2, [2; 32]), None).unwrap(),
            VoteOutcome::New
        );
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, 2, [2; 32]), None).unwrap(),
            VoteOutcome::New
        );
        assert_eq!(t.lock().known_final(), 0);
        // Height 1 reaches the threshold: 1 and 2 are known final; the served subnet final
        // height follows the chain's blocks, not the held votes
        t.add_vote(vote_of(&t, &k2, &s2, 1, [1; 32]), None).unwrap();
        t.add_vote(vote_of(&t, &k3, &s3, 1, [1; 32]), None).unwrap();
        assert_eq!(t.lock().known_final(), 2);
        assert_eq!(t.final_height(), 0);
        // Rejections: unknown validator, wrong EOTS key, uncommitted height, wrong block
        let k4 = key(4);
        let s4 = ipc_eots::testing::TestSigner::new(14, 1, 20);
        assert_eq!(
            t.add_vote(vote_of(&t, &k4, &s4, 3, [3; 32]), None),
            Err(VoteError::WrongEotsKey)
        );
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s2, 3, [3; 32]), None),
            Err(VoteError::WrongEotsKey)
        );
        let s1_short = ipc_eots::testing::TestSigner::new(11, 1, 2);
        let mut far = vote_of(&t, &k1, &s1_short, 2, [9; 32]);
        far.vote.height = 30;
        assert!(matches!(
            t.add_vote(far, None),
            Err(VoteError::NotCommitted(30))
        ));
        let mut tampered = vote_of(&t, &k1, &s1, 3, [3; 32]);
        tampered.vote.signature[1][0] ^= 1;
        assert!(matches!(
            t.add_vote(tampered, None),
            Err(VoteError::Invalid(_))
        ));
        // A vote for another block at height 3 is accepted and, with the chain's, is evidence
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, 3, [3; 32]), None).unwrap(),
            VoteOutcome::New
        );
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, 3, [7; 32]), None).unwrap(),
            VoteOutcome::Evidence
        );
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, 3, [8; 32]), None).unwrap(),
            VoteOutcome::Excess
        );
        assert_eq!(t.evidence().len(), 1);
        // Final blocks are served up to the recorded final height only
        assert!(t.final_block(2).is_none());
        t.lock().recorded_final = 2;
        assert_eq!(t.final_block(2).unwrap().1.len(), 3);
        assert!(t.final_block(3).is_none());
        // Below the checkpointed height nothing is accepted
        t.lock().checkpoint = Some((3, [3; 32], false));
        assert_eq!(
            t.add_vote(vote_of(&t, &k2, &s2, 3, [3; 32]), None),
            Err(VoteError::Checkpointed(3, 3))
        );
        assert_eq!(
            t.add_vote(vote_of(&t, &k2, &s2, 4, [4; 32]), None).unwrap(),
            VoteOutcome::New
        );
        // The checkpointed height counts as known final
        assert_eq!(t.lock().known_final(), 3);
    }

    #[test]
    fn proposals_carry_final_heights_and_are_checked_against_the_agreed_state() {
        let (k1, k2) = (key(1), key(2));
        let t = tally(&[(k1.clone(), 2), (k2.clone(), 1)]);
        let (s1, s2) = (signer_for(&t, &k1, 11), signer_for(&t, &k2, 12));
        for h in 1..=3 {
            t.add_block(h, [h as u8; 32]).unwrap();
        }
        for h in 1..=2 {
            t.add_vote(vote_of(&t, &k1, &s1, h, [h as u8; 32]), None)
                .unwrap();
            t.add_vote(vote_of(&t, &k2, &s2, h, [h as u8; 32]), None)
                .unwrap();
        }
        t.add_vote(vote_of(&t, &k2, &s2, 3, [3; 32]), None).unwrap();
        assert_eq!(t.lock().known_final(), 2);
        let agreed = AgreedState {
            checkpointed: 0,
            killed: false,
            bitcoin_height: 100,
        };
        let data = t.proposal(&agreed).unwrap();
        assert_eq!(data.votes.len(), 4);
        assert_eq!(data.bitcoin_height, 100);
        t.check_proposal(&data, &agreed).unwrap();
        // Commitments recorded above the agreed Bitcoin height do not count
        let earlier = AgreedState {
            checkpointed: 0,
            killed: false,
            bitcoin_height: 99,
        };
        assert!(t.check_proposal(&data, &earlier).is_err());
        assert!(t.proposal(&earlier).is_none());
        // A gap, a missing threshold, and a bad evidence are rejected
        let mut gap = data.clone();
        gap.votes.retain(|v| v.vote.height == 2);
        assert!(t.check_proposal(&gap, &agreed).is_err());
        let mut thin = data.clone();
        thin.votes.retain(|v| {
            v.vote.height == 1 && v.validator == key_bytes(&k2).to_vec() || v.vote.height == 2
        });
        assert!(t.check_proposal(&thin, &agreed).is_err());
        let mut bad_evidence = data.clone();
        bad_evidence
            .evidence
            .push((data.votes[0].clone(), data.votes[0].clone()));
        assert!(t.check_proposal(&bad_evidence, &agreed).is_err());
        // Applying records the final height; the next proposal starts after it
        assert!(t.apply_proposal(&data, &agreed).unwrap().is_empty());
        assert_eq!(t.recorded_final_height(), 2);
        assert!(t.proposal(&agreed).is_none());
        // A checkpointed height above the recorded one is the new floor of carried heights
        let checkpointed = AgreedState {
            checkpointed: 2,
            killed: false,
            bitcoin_height: 100,
        };
        t.add_vote(vote_of(&t, &k1, &s1, 3, [3; 32]), None).unwrap();
        let next = t.proposal(&checkpointed).unwrap();
        assert!(next.votes.iter().all(|v| v.vote.height == 3));
        t.check_proposal(&next, &checkpointed).unwrap();
        // Applying keeps the agreed checkpointed height, which never goes back
        assert_eq!(t.agreed_checkpointed_height(), 0);
        assert!(t.apply_proposal(&next, &checkpointed).unwrap().is_empty());
        assert_eq!(t.agreed_checkpointed_height(), 2);
        assert_eq!(t.recorded_final_height(), 3);
        // A second tally built from the store holds the recorded final height, the agreed
        // checkpointed height and the blocks
        let reloaded = EotsTally::new(
            t.store.clone(),
            t.provider.clone(),
            t.subnet_id.clone(),
            vec![],
            Arc::new(|_| true),
        )
        .unwrap();
        assert_eq!(reloaded.recorded_final_height(), 3);
        assert_eq!(reloaded.agreed_checkpointed_height(), 2);
        assert_eq!(reloaded.lock().chain.len(), 3);
        assert_eq!(
            reloaded.lock().pending.len(),
            t.store.load().unwrap().votes.len()
        );
    }

    #[test]
    fn a_validator_votes_above_the_checkpointed_height_on_its_own_chain_only() {
        let k1 = key(1);
        let t = tally(&[(k1.clone(), 1)]);
        let _ = signer_for(&t, &k1, 11);
        for h in 1..=5 {
            t.add_block(h, [h as u8; 32]).unwrap();
        }
        t.lock().checkpoint = Some((2, [2; 32], false));
        assert_eq!(
            t.votes_to_cast(5),
            vec![(3, [3; 32]), (4, [4; 32]), (5, [5; 32])]
        );
        // Asked again until the votes are signed and published
        assert_eq!(t.votes_to_cast(5).len(), 3);
        for h in 3..=5 {
            t.mark_cast(h);
        }
        assert!(t.votes_to_cast(5).is_empty());
        t.add_block(6, [6; 32]).unwrap();
        assert_eq!(t.votes_to_cast(6), vec![(6, [6; 32])]);
        // Another block at the checkpointed height: a dead fork, no vote
        t.lock().checkpoint = Some((2, [9; 32], false));
        t.add_block(7, [7; 32]).unwrap();
        assert!(t.votes_to_cast(7).is_empty());
    }

    #[test]
    fn a_validator_resumes_above_the_blocks_it_already_knows_final_by_its_votes() {
        let (t, signers) = three_voters(4);
        // The held votes show 1 and 2 final; 3 holds one vote, 4 none
        for h in 1..=2 {
            vote_all(&t, &signers, h, [h as u8; 32]);
        }
        vote_all(&t, &signers[..1], 3, [3u8; 32]);
        assert_eq!(t.lock().known_final(), 2);
        assert_eq!(t.recorded_final_height(), 0);
        assert_eq!(t.votes_to_cast(4), vec![(3, [3u8; 32]), (4, [4u8; 32])]);
        // Votes at 4 before 3 is final do not make 4 known final: 3 is still to be cast
        let (t, signers) = three_voters(4);
        for h in [1, 2, 4] {
            vote_all(&t, &signers, h, [h as u8; 32]);
        }
        assert_eq!(t.lock().known_final(), 2);
        assert_eq!(t.votes_to_cast(4), vec![(3, [3u8; 32]), (4, [4u8; 32])]);
    }

    #[test]
    fn a_height_whose_signing_failed_is_asked_again() {
        let k1 = key(1);
        let t = tally(&[(k1.clone(), 1)]);
        let _ = signer_for(&t, &k1, 11);
        for h in 1..=5 {
            t.add_block(h, [h as u8; 32]).unwrap();
        }
        assert_eq!(t.votes_to_cast(5).len(), 5);
        // The votes at 1 and 2 are signed and published, signing at 3 fails and the pass
        // stops there: 3 is asked again, with the rest.
        t.mark_cast(1);
        t.mark_cast(2);
        assert_eq!(
            t.votes_to_cast(5),
            vec![(3, [3; 32]), (4, [4; 32]), (5, [5; 32])]
        );
        t.mark_cast(3);
        assert_eq!(t.votes_to_cast(5), vec![(4, [4; 32]), (5, [5; 32])]);
    }

    #[test]
    fn a_power_update_of_subnet_block_h_applies_from_h_plus_2() {
        let (k1, k2) = (key(1), key(2));
        let t = tally(&[(k1.clone(), 1), (k2.clone(), 1)]);
        // Returned by the execution of subnet block `h`, installed as `chain.rs` does
        let h = 10;
        t.update_power_table(h + 2, vec![(k1.clone(), 10)]).unwrap();
        let r = restarted(&t, "http://127.0.0.1:1");
        for tally in [&t, &r] {
            let inner = tally.lock();
            assert_eq!(inner.power_at(&key_bytes(&k1), h), 1);
            assert_eq!(inner.power_at(&key_bytes(&k1), h + 1), 1);
            assert_eq!(inner.power_at(&key_bytes(&k1), h + 2), 10);
            assert_eq!(inner.power_at(&key_bytes(&k1), h + 3), 10);
            assert_eq!(inner.power_at(&key_bytes(&k2), h + 2), 1);
        }
    }

    #[test]
    fn carried_votes_sizes_stay_under_the_limit_for_350_members() {
        let vote = ValidatorVote {
            validator: vec![0; 33],
            vote: EotsVote {
                pk_eots: [0; 32],
                height: 0,
                block_hash: [0; 32],
                signature: [[0; 32]; 2],
                start: 0,
                proof: vec![[0; 32]; 24],
            },
            timestamp: 0,
            signature: vec![0; 72],
        };
        assert!(350 * vote_size(&vote) * 2 <= MAX_CARRIED_BYTES);
    }

    /// A provider that answers `getsubnetcheckpoint` like the real one: a height above what
    /// its monitor processed is refused, a processed one gets checkpoint 42 of Bitcoin block 100.
    /// What a fake monitor holds: the EOTS keys with their commitments (validator x-only key,
    /// EOTS key, commitment, Bitcoin height), and the checkpoint it answers with.
    #[derive(Default)]
    struct FakeMonitor {
        keys: Vec<([u8; 32], [u8; 32], EotsNonceCommitment, u64)>,
        checkpoint: Option<(u64, [u8; 32])>,
    }

    /// A monitor's recorded key and commitment of `signer` for `key`.
    fn recorded(
        key: &ValidatorKey,
        signer: &ipc_eots::testing::TestSigner,
        bitcoin_height: u64,
    ) -> ([u8; 32], [u8; 32], EotsNonceCommitment, u64) {
        (
            x_only(&key_bytes(key)),
            signer.pk_eots,
            signer.commitment,
            bitcoin_height,
        )
    }

    /// A provider that answers the tally's questions from `monitor`: the checkpoint of a
    /// Bitcoin height above `processed` is refused and counted in `refused`.
    async fn fake_provider(
        listener: tokio::net::TcpListener,
        monitor: Arc<FakeMonitor>,
        processed: Arc<std::sync::atomic::AtomicU64>,
        refused: Arc<std::sync::atomic::AtomicU64>,
    ) {
        use std::sync::atomic::Ordering;
        use tokio::io::AsyncReadExt;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0u8; 1024];
            let body = loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse().unwrap())
                        .unwrap_or(0);
                    while raw.len() < end + 4 + length {
                        let n = stream.read(&mut chunk).await.unwrap();
                        raw.extend_from_slice(&chunk[..n]);
                    }
                    break Some(raw[end + 4..end + 4 + length].to_vec());
                }
            };
            let Some(body) = body else { continue };
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let checkpoint = || {
                monitor
                    .checkpoint
                    .map_or(serde_json::Value::Null, |(h, hash)| {
                        serde_json::json!({
                            "checkpoint_height": h,
                            "checkpoint_hash": hex::encode(hash),
                            "block_height": 100,
                            "is_kill_checkpoint": false
                        })
                    })
            };
            let result = match request["method"].as_str().unwrap() {
                "geteotskeys" => serde_json::Value::Array(
                    monitor
                        .keys
                        .iter()
                        .map(|(v, pk, _, _)| {
                            serde_json::json!({"pubkey": hex::encode(v), "pk_eots": hex::encode(pk)})
                        })
                        .collect(),
                ),
                "geteotscommitments" => {
                    let pk = request["params"]["pk_eots"].as_str().unwrap();
                    serde_json::Value::Array(
                        monitor
                            .keys
                            .iter()
                            .filter(|(_, k, _, _)| hex::encode(k) == pk)
                            .map(|(_, _, c, h)| {
                                serde_json::json!({
                                    "start": c.start,
                                    "count": c.count,
                                    "root": hex::encode(c.root),
                                    "block_height": h
                                })
                            })
                            .collect(),
                    )
                }
                "getsubnetcheckpoint" => {
                    match request["params"]["max_block_height"].as_u64() {
                        Some(asked) if asked > processed.load(Ordering::SeqCst) => {
                            refused.fetch_add(1, Ordering::SeqCst);
                            let answer = serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {
                                "code": -32004,
                                "message": format!("the monitor has not processed Bitcoin height {asked} yet")
                            }});
                            respond(&mut stream, answer).await;
                            continue;
                        }
                        _ => checkpoint(),
                    }
                }
                other => panic!("unexpected method {other}"),
            };
            respond(
                &mut stream,
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": result}),
            )
            .await;
        }
    }

    async fn respond(stream: &mut tokio::net::TcpStream, answer: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let answer = answer.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            answer.len(),
            answer
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.ok();
    }

    /// A fake provider on a free port, serving `monitor` from `processed` on.
    async fn serve(monitor: FakeMonitor, processed: u64) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(fake_provider(
            listener,
            Arc::new(monitor),
            Arc::new(std::sync::atomic::AtomicU64::new(processed)),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ));
        (url, server)
    }

    /// A tally whose store holds blocks 1 to 3, votes of `k1` and `k2` at every one of them,
    /// and the carried votes of heights 1 and 2: the data a node restarts on. With it, the
    /// proposal that carries height 3.
    fn node_data(
        k1: &ValidatorKey,
        k2: &ValidatorKey,
        s1: &ipc_eots::testing::TestSigner,
        s2: &ipc_eots::testing::TestSigner,
    ) -> (EotsTally, EotsFinalityData) {
        let t = tally(&[(k1.clone(), 2), (k2.clone(), 1)]);
        t.lock()
            .eots_keys
            .insert(x_only(&key_bytes(k1)), s1.pk_eots);
        t.lock()
            .eots_keys
            .insert(x_only(&key_bytes(k2)), s2.pk_eots);
        for (s, _) in [(s1, k1), (s2, k2)] {
            t.lock()
                .commitments
                .insert(s.pk_eots, vec![(s.commitment, 100)]);
        }
        t.lock().refreshed = true;
        for h in 1..=3 {
            t.add_block(h, [h as u8; 32]).unwrap();
            t.add_vote(vote_of(&t, k1, s1, h, [h as u8; 32]), None)
                .unwrap();
            t.add_vote(vote_of(&t, k2, s2, h, [h as u8; 32]), None)
                .unwrap();
        }
        let agreed = AgreedState {
            checkpointed: 0,
            killed: false,
            bitcoin_height: 100,
        };
        let mut data = t.proposal(&agreed).unwrap();
        data.votes.retain(|v| v.vote.height <= 2);
        assert!(t.apply_proposal(&data, &agreed).unwrap().is_empty());
        assert_eq!(t.recorded_final_height(), 2);
        let next = t.proposal(&agreed).unwrap();
        assert!(next.votes.iter().all(|v| v.vote.height == 3));
        assert_eq!(next.votes.len(), 2);
        (t, next)
    }

    /// The tally of a node that restarts on the data of `t`, with `provider` as its monitor.
    fn restarted(t: &EotsTally, provider_url: &str) -> EotsTally {
        let fresh = tally_with_provider(provider_url, &[]);
        EotsTally::new(
            t.store.clone(),
            fresh.provider.clone(),
            t.subnet_id.clone(),
            vec![],
            Arc::new(|_| true),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_restarted_node_executes_the_carried_votes_of_the_next_block() {
        let (k1, k2) = (key(1), key(2));
        let (s1, s2) = (
            ipc_eots::testing::TestSigner::new(11, 1, 20),
            ipc_eots::testing::TestSigner::new(12, 1, 20),
        );
        let (t, next) = node_data(&k1, &k2, &s1, &s2);
        let (url, server) = serve(
            FakeMonitor {
                keys: vec![recorded(&k1, &s1, 100), recorded(&k2, &s2, 100)],
                checkpoint: None,
            },
            100,
        )
        .await;
        let r = restarted(&t, &url);
        // The restarted tally holds the power tables and the recorded final height, and has
        // not reached its monitor yet
        assert!(r.has_stored_power_tables());
        assert_eq!(r.recorded_final_height(), 2);
        assert!(!r.lock().refreshed);
        assert_eq!(r.lock().pending.len(), 6);
        // The next block carries the votes of height 3: executed as on every other node
        r.execute_proposal(&next).await.unwrap();
        assert_eq!(r.recorded_final_height(), 3);
        assert_eq!(r.final_height(), 3);
        assert_eq!(r.final_block(3).unwrap().1.len(), 2);
        // The stored votes were checked at the refresh and are held again
        assert!(r.lock().pending.is_empty());
        assert_eq!(r.final_block(2).unwrap().1.len(), 2);
        // No height is signed again after the restart
        assert!(r.votes_to_cast(3).is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn a_carried_vote_the_monitor_rejects_is_not_held_and_the_block_stands() {
        let (k1, k2) = (key(1), key(2));
        let (s1, s2) = (
            ipc_eots::testing::TestSigner::new(11, 1, 20),
            ipc_eots::testing::TestSigner::new(12, 1, 20),
        );
        let (t, next) = node_data(&k1, &k2, &s1, &s2);
        // This node's monitor never recorded the EOTS key of k2
        let (url, server) = serve(
            FakeMonitor {
                keys: vec![recorded(&k1, &s1, 100)],
                checkpoint: None,
            },
            100,
        )
        .await;
        let r = restarted(&t, &url);
        r.execute_proposal(&next).await.unwrap();
        // The recorded final height follows the block; the rejected vote is not held
        assert_eq!(r.recorded_final_height(), 3);
        assert_eq!(r.final_block(3).unwrap().1.len(), 1);
        server.abort();
    }

    /// The log lines of a test, as a `tracing` writer
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<StdMutex<Vec<u8>>>);

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;
        fn make_writer(&'a self) -> CapturedLog {
            self.clone()
        }
    }

    /// The ERROR lines of every test, from one process-wide subscriber: a thread-local one loses
    /// events to the callsite interest cache that the other tests' threads rebuild concurrently.
    /// Tests assert on lines of their own (distinct hashes).
    fn captured_log() -> CapturedLog {
        static LOG: std::sync::OnceLock<CapturedLog> = std::sync::OnceLock::new();
        LOG.get_or_init(|| {
            let log = CapturedLog::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::ERROR)
                    .with_ansi(false)
                    .with_writer(log.clone())
                    .finish(),
            )
            .expect("the subscriber is set once");
            log
        })
        .clone()
    }

    #[tokio::test]
    async fn a_vote_signed_with_another_key_than_the_registered_one_is_rejected_and_logged() {
        let (k1, k2) = (key(1), key(2));
        let (s1, s2, s3) = (
            ipc_eots::testing::TestSigner::new(11, 1, 20),
            ipc_eots::testing::TestSigner::new(12, 1, 20),
            ipc_eots::testing::TestSigner::new(13, 1, 20),
        );
        let (t, mut next) = node_data(&k1, &k2, &s1, &s2);
        // k2 registered s2's EOTS key with the monitor but signs its vote at height 3 with s3
        next.votes.retain(|v| v.vote.pk_eots != s2.pk_eots);
        next.votes.push(vote_of(&t, &k2, &s3, 3, [3u8; 32]));
        let (url, server) = serve(
            FakeMonitor {
                keys: vec![recorded(&k1, &s1, 100), recorded(&k2, &s2, 100)],
                checkpoint: None,
            },
            100,
        )
        .await;
        let log = captured_log();
        let r = restarted(&t, &url);
        // The proposal check refuses the block that carries the vote
        let agreed = r.agreed_state(100).await.unwrap();
        let refused = r.check_proposal(&next, &agreed).unwrap_err();
        assert!(
            refused.contains("not the EOTS key of the validator"),
            "{refused}"
        );
        // The execution of the committed block goes on without the vote: the node holds k1's
        // carried vote and k2's genuine vote from its own store
        r.execute_proposal(&next).await.unwrap();
        assert_eq!(r.recorded_final_height(), 3);
        let (_, held) = r.final_block(3).unwrap();
        let mut keys: Vec<_> = held.iter().map(|v| v.vote.pk_eots).collect();
        keys.sort();
        let mut expected = vec![s1.pk_eots, s2.pk_eots];
        expected.sort();
        assert_eq!(keys, expected);
        let text = log.text();
        assert!(text.contains("ERROR"), "{text}");
        assert!(
            text.contains("carried vote rejected at execution"),
            "{text}"
        );
        assert!(text.contains("not the EOTS key of the validator"), "{text}");
        server.abort();
    }

    #[tokio::test]
    async fn a_node_refreshed_while_its_monitor_was_behind_accepts_an_honest_proposal() {
        let (k1, k2) = (key(1), key(2));
        let (s1, s2) = (
            ipc_eots::testing::TestSigner::new(11, 1, 20),
            ipc_eots::testing::TestSigner::new(12, 1, 20),
        );
        let (t, next) = node_data(&k1, &k2, &s1, &s2);
        let (url, server) = serve(
            FakeMonitor {
                keys: vec![recorded(&k1, &s1, 100), recorded(&k2, &s2, 100)],
                checkpoint: None,
            },
            100,
        )
        .await;
        let r = restarted(&t, &url);
        // This node read its monitor when it had processed height 99 only, before k2's
        // key and commitment were recorded
        {
            let mut inner = r.lock();
            inner.eots_keys.insert(x_only(&key_bytes(&k1)), s1.pk_eots);
            inner
                .commitments
                .insert(s1.pk_eots, vec![(s1.commitment, 100)]);
            inner.refreshed = true;
            inner.refreshed_for = 99;
        }
        // The decision at Bitcoin height 100 reads the monitor again, so the honest proposal
        // that carries k2's vote passes as on every other node
        let agreed = r.agreed_state(100).await.unwrap();
        assert_eq!(r.lock().refreshed_for, 100);
        r.check_proposal(&next, &agreed).unwrap();
        server.abort();
    }

    #[test]
    fn power_tables_survive_a_restart() {
        let (k1, k2, k3) = (key(1), key(2), key(3));
        let t = tally(&[(k1.clone(), 2), (k2.clone(), 1)]);
        assert!(!t.has_stored_power_tables());
        t.update_power_table(5, vec![(k3.clone(), 4), (k2.clone(), 0)])
            .unwrap();
        let r = restarted(&t, "http://127.0.0.1:1");
        assert!(r.has_stored_power_tables());
        let inner = r.lock();
        assert_eq!(inner.power_at(&key_bytes(&k1), 4), 2);
        assert_eq!(inner.power_at(&key_bytes(&k2), 4), 1);
        assert_eq!(inner.power_at(&key_bytes(&k3), 4), 0);
        assert_eq!(inner.power_at(&key_bytes(&k1), 5), 2);
        assert_eq!(inner.power_at(&key_bytes(&k2), 5), 0);
        assert_eq!(inner.power_at(&key_bytes(&k3), 5), 4);
    }

    #[tokio::test]
    async fn the_agreed_state_waits_for_an_unreachable_provider_and_a_lagging_monitor() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let retry = std::time::Duration::from_millis(50);
        let timeout = std::time::Duration::from_secs(2);
        // A port nobody listens on yet: the provider is unreachable
        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let t = tally_with_provider(&format!("http://127.0.0.1:{port}"), &[(key(1), 1)]);
        assert!(t.agreed_state(100).await.is_err(), "unreachable");

        // The provider comes up with its monitor one block behind, then catches up
        let processed = Arc::new(AtomicU64::new(99));
        let refused = Arc::new(AtomicU64::new(0));
        let server = {
            let (processed, refused) = (processed.clone(), refused.clone());
            tokio::spawn(async move {
                tokio::time::sleep(retry * 4).await;
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                    .await
                    .unwrap();
                let monitor = FakeMonitor {
                    keys: vec![],
                    checkpoint: Some((42, [7; 32])),
                };
                fake_provider(listener, Arc::new(monitor), processed, refused).await;
            })
        };
        let catch_up = {
            let processed = processed.clone();
            tokio::spawn(async move {
                tokio::time::sleep(retry * 10).await;
                processed.store(100, Ordering::SeqCst);
            })
        };

        let started = std::time::Instant::now();
        let agreed = t.agreed_state_wait_every(100, retry, timeout).await;
        assert_eq!(
            agreed,
            AgreedState {
                checkpointed: 42,
                killed: false,
                bitcoin_height: 100
            }
        );
        // It waited through both the refused connections and the refused height
        assert!(started.elapsed() >= retry * 10, "{:?}", started.elapsed());
        assert!(refused.load(Ordering::SeqCst) >= 1, "the lagging monitor refused");
        assert!(catch_up.is_finished());

        // Once the monitor is there, the plain question has the same answer at once
        let again = t.agreed_state(100).await.unwrap();
        assert_eq!(again, agreed);
        // A height above the monitor's is still refused, not answered with an older checkpoint
        assert!(t.agreed_state(101).await.is_err());
        server.abort();
    }

    /// Three validators of weight 1, threshold 3, the chain's blocks `[h; 32]` at heights 1..=n
    fn three_voters(
        n: u64,
    ) -> (
        EotsTally,
        Vec<(ValidatorKey, ipc_eots::testing::TestSigner)>,
    ) {
        let keys = [key(1), key(2), key(3)];
        let t = tally(&keys.iter().map(|k| (k.clone(), 1)).collect::<Vec<_>>());
        let signers = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), signer_for(&t, k, 11 + i as u8)))
            .collect();
        for h in 1..=n {
            t.add_block(h, [h as u8; 32]).unwrap();
        }
        (t, signers)
    }

    fn vote_all(
        t: &EotsTally,
        signers: &[(ValidatorKey, ipc_eots::testing::TestSigner)],
        h: u64,
        hash: [u8; 32],
    ) -> Vec<VoteOutcome> {
        signers
            .iter()
            .map(|(k, s)| t.add_vote(vote_of(t, k, s, h, hash), None).unwrap())
            .collect()
    }

    const SERVES_NONE: &str = "another block at this height is final";
    const DOUBLE_VOTES: &str = "double EOTS votes for another block";

    #[test]
    fn a_second_final_block_stops_the_served_finality_below_it_even_when_its_votes_come_late() {
        let log = captured_log();
        let (t, signers) = three_voters(3);
        for h in 1..=3 {
            vote_all(&t, &signers, h, [h as u8; 32]);
        }
        // Blocks carried the votes up to height 3
        t.lock().recorded_final = 3;
        assert_eq!(t.final_height(), 3);
        let other = [0xA1u8; 32];
        // Two double votes for another block at height 2: below the threshold, finality stays
        assert_eq!(
            vote_all(&t, &signers[..2], 2, other),
            [VoteOutcome::Evidence, VoteOutcome::Evidence]
        );
        assert_eq!(t.final_height(), 3);
        assert_eq!(t.status().final_height, 3);
        let text = log.text();
        assert_eq!(
            text.lines()
                .filter(|l| l.contains(DOUBLE_VOTES) && l.contains(&hex::encode(other)))
                .count(),
            2,
            "{text}"
        );
        assert!(!text
            .lines()
            .any(|l| l.contains(SERVES_NONE) && l.contains(&hex::encode(other))));
        // The third reaches the threshold: height 2 and above are served as final no more
        assert_eq!(
            vote_all(&t, &signers[2..], 2, other),
            [VoteOutcome::Evidence]
        );
        assert_eq!(t.final_height(), 1);
        assert_eq!(t.status().final_height, 1);
        assert!(t.final_block(1).is_some());
        assert!(t.final_block(2).is_none());
        assert!(t.final_block(3).is_none());
        let text = log.text();
        let reported: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(SERVES_NONE) && l.contains(&hex::encode(other)))
            .collect();
        assert_eq!(reported.len(), 1, "{text}");
        assert!(reported[0].contains("height=2"), "{}", reported[0]);
        assert!(
            reported[0].contains(&hex::encode([2u8; 32])),
            "{}",
            reported[0]
        );
        // The chain is kept, and the recorded final height is not the node's to change:
        // block execution follows it past the conflict, the served one stays
        assert_eq!(t.lock().chain[&2], [2u8; 32]);
        assert_eq!(t.recorded_final_height(), 3);
        assert_eq!(t.final_height(), 1);
        assert_eq!(t.status().recorded_final_height, 3);
        // More votes at height 2 report nothing new
        assert_eq!(
            vote_all(&t, &signers[..1], 2, [0xA2u8; 32]),
            [VoteOutcome::Excess]
        );
        assert_eq!(
            log.text()
                .lines()
                .filter(|l| l.contains(SERVES_NONE) && l.contains(&hex::encode(other)))
                .count(),
            1
        );
    }

    #[test]
    fn a_final_block_other_than_the_chains_keeps_the_served_finality_below_it() {
        let log = captured_log();
        let (t, signers) = three_voters(1);
        vote_all(&t, &signers, 1, [1u8; 32]);
        t.lock().recorded_final = 1;
        assert_eq!(t.final_height(), 1);
        // The votes for the other block at height 2 come before the chain's block
        let other = [0xB2u8; 32];
        assert_eq!(
            vote_all(&t, &signers, 2, other),
            [VoteOutcome::New, VoteOutcome::New, VoteOutcome::New]
        );
        assert!(!log.text().contains(&hex::encode(other)));
        t.add_block(2, [2u8; 32]).unwrap();
        assert_eq!(t.final_height(), 1);
        let text = log.text();
        let reported: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(SERVES_NONE) && l.contains(&hex::encode(other)))
            .collect();
        assert_eq!(reported.len(), 1, "{text}");
        assert!(reported[0].contains("ours_weight=0"), "{}", reported[0]);
        // The chain's block reaches the threshold too, and the chain goes on: still below 2
        assert_eq!(
            vote_all(&t, &signers, 2, [2u8; 32]),
            [
                VoteOutcome::Evidence,
                VoteOutcome::Evidence,
                VoteOutcome::Evidence
            ]
        );
        t.add_block(3, [3u8; 32]).unwrap();
        vote_all(&t, &signers, 3, [3u8; 32]);
        assert_eq!(t.lock().known_final(), 3);
        assert_eq!(t.final_height(), 1);
        assert_eq!(t.status().final_height, 1);
        assert!(t.final_block(2).is_none());
        assert_eq!(t.evidence().len(), 3);
    }

    #[test]
    fn double_votes_below_the_threshold_are_reported_and_finality_goes_on() {
        let log = captured_log();
        let (t, signers) = three_voters(3);
        vote_all(&t, &signers, 1, [1u8; 32]);
        let other = [0xC2u8; 32];
        assert_eq!(vote_all(&t, &signers[..1], 2, other), [VoteOutcome::New]);
        assert_eq!(
            vote_all(&t, &signers[..1], 2, [2u8; 32]),
            [VoteOutcome::Evidence]
        );
        vote_all(&t, &signers[1..], 2, [2u8; 32]);
        vote_all(&t, &signers, 3, [3u8; 32]);
        t.lock().recorded_final = 3;
        assert_eq!(t.final_height(), 3);
        assert_eq!(t.status().final_height, 3);
        assert_eq!(t.final_block(2).unwrap().1.len(), 3);
        assert_eq!(t.evidence().len(), 1);
        let text = log.text();
        let reported: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(&hex::encode(other)))
            .collect();
        assert_eq!(reported.len(), 1, "{text}");
        assert!(reported[0].contains(DOUBLE_VOTES), "{}", reported[0]);
        assert!(
            reported[0].contains("weight=1 threshold=3"),
            "{}",
            reported[0]
        );
    }

    #[test]
    fn threshold_is_two_thirds_plus_one() {
        let table = HashMap::from([([1u8; 33], 3u64), ([2u8; 33], 3), ([3u8; 33], 3)]);
        assert_eq!(threshold(&table), 7);
    }

    #[test]
    fn store_keeps_two_votes_per_key_and_height() {
        let mut inner = Inner {
            power_tables: BTreeMap::new(),
            eots_keys: HashMap::new(),
            commitments: HashMap::new(),
            checkpoint: None,
            refreshed: true,
            refreshed_for: 0,
            stored_power_tables: false,
            chain: BTreeMap::new(),
            votes: BTreeMap::new(),
            pending: vec![],
            recorded_final: 0,
            agreed_checkpointed: 0,
            conflicts: BTreeSet::new(),
            evidence_carried: HashSet::new(),
            last_cast: 0,
            on_dead_fork: false,
            envelope_check: Arc::new(|_| true),
        };
        let vote = |b: u8| ValidatorVote {
            validator: vec![1; 33],
            vote: EotsVote {
                pk_eots: [2; 32],
                height: 5,
                block_hash: [b; 32],
                signature: [[b; 32]; 2],
                start: 1,
                proof: vec![],
            },
            timestamp: 0,
            signature: Vec::new(),
        };
        assert_eq!(inner.store(vote(1)).0, VoteOutcome::New);
        assert_eq!(inner.store(vote(1)).0, VoteOutcome::Known);
        assert_eq!(inner.store(vote(2)).0, VoteOutcome::Evidence);
        assert_eq!(inner.store(vote(3)).0, VoteOutcome::Excess);
        assert_eq!(inner.evidence().len(), 1);
        let _ = MemStore::default();
    }
}
#[cfg(test)]
#[path = "eots_spec_tests.rs"]
mod spec_tests;
