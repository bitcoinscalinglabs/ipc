//! EOTS in the application: the vote store, the validator's voting loop, and the finality query.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use fendermint_storage::{Codec, KVCollection, KVReadable, KVStore, KVWritable};
use fendermint_vm_genesis::{Power, Validator};
use fendermint_vm_topdown::eots::{
    below_conflict, key_bytes_of, EnvelopeCheck, EotsStore, EotsTally, KeyBytes, StoredState,
    VoteOutcome,
};
use fendermint_vm_topdown::voting::Weight;
use ipc_api::subnet_id::SubnetID;
use ipc_eots::{FinalityAnswer, ValidatorVote};
use ipc_ipld_resolver::{Client, SignedVoteRecord, Timestamp, ValidatorKey, VoteRecord};
use serde::{Deserialize, Serialize};
use tendermint::abci::response;

use crate::ipc::AppVote;

pub const QUERY_PATH: &str = "/eots";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub enum EotsKey {
    Vote([u8; 32], u64, [u8; 32]),
    Block(u64),
    Final,
    Checkpointed,
    EvidenceCarried([u8; 32]),
    PowerTable(u64),
    LastCast,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum EotsValue {
    Vote(ValidatorVote),
    Block([u8; 32]),
    Final(u64),
    Checkpointed(u64),
    Carried,
    /// Validator keys with their weights (serde has no array of 33 bytes)
    PowerTable(Vec<(Vec<u8>, Weight)>),
    LastCast(u64),
}

pub struct AppEotsStore<DB, S: KVStore> {
    db: DB,
    collection: KVCollection<S, EotsKey, EotsValue>,
}

impl<DB, S> AppEotsStore<DB, S>
where
    S: KVStore + Codec<EotsKey> + Codec<EotsValue>,
    DB: KVWritable<S> + KVReadable<S>,
{
    pub fn new(db: DB, ns: S::Namespace) -> Self {
        Self {
            db,
            collection: KVCollection::new(ns),
        }
    }

    fn put(&self, key: &EotsKey, value: &EotsValue) -> anyhow::Result<()> {
        self.db
            .with_write(|tx| self.collection.put(tx, key, value))
            .context("EOTS store write failed")
    }
}

impl<DB, S> EotsStore for AppEotsStore<DB, S>
where
    S: KVStore + Codec<EotsKey> + Codec<EotsValue> + Send + Sync,
    S::Repr: Ord + 'static,
    S::Namespace: Send + Sync,
    DB: KVWritable<S> + KVReadable<S> + Send + Sync,
{
    fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()> {
        self.put(
            &EotsKey::Vote(vote.vote.pk_eots, vote.vote.height, vote.vote.block_hash),
            &EotsValue::Vote(vote.clone()),
        )
    }

    fn put_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()> {
        self.put(&EotsKey::Block(height), &EotsValue::Block(hash))
    }

    fn put_recorded_final(&self, height: u64) -> anyhow::Result<()> {
        self.put(&EotsKey::Final, &EotsValue::Final(height))
    }

    fn put_checkpointed(&self, height: u64) -> anyhow::Result<()> {
        self.put(&EotsKey::Checkpointed, &EotsValue::Checkpointed(height))
    }

    fn put_evidence_carried(&self, pk_eots: [u8; 32]) -> anyhow::Result<()> {
        self.put(&EotsKey::EvidenceCarried(pk_eots), &EotsValue::Carried)
    }

    fn put_power_table(&self, height: u64, table: &[(KeyBytes, Weight)]) -> anyhow::Result<()> {
        let table = table.iter().map(|(k, w)| (k.to_vec(), *w)).collect();
        self.put(&EotsKey::PowerTable(height), &EotsValue::PowerTable(table))
    }

    fn put_last_cast(&self, height: u64) -> anyhow::Result<()> {
        self.put(&EotsKey::LastCast, &EotsValue::LastCast(height))
    }

    fn delete_up_to(&self, height: u64, power_tables_from: u64) -> anyhow::Result<()> {
        self.db
            .with_write(|tx| {
                let deleted = self
                    .collection
                    .iterate(&*tx)
                    .filter_map(|item| match item {
                        Ok((key, _)) => match key {
                            EotsKey::Vote(_, h, _) | EotsKey::Block(h) if h <= height => {
                                Some(Ok(key))
                            }
                            EotsKey::PowerTable(h) if h < power_tables_from => Some(Ok(key)),
                            _ => None,
                        },
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for key in &deleted {
                    self.collection.delete(tx, key)?;
                }
                Ok(())
            })
            .context("EOTS store delete failed")
    }

    fn load(&self) -> anyhow::Result<StoredState> {
        let tx = self.db.read();
        let mut state = StoredState::default();
        for item in self.collection.iterate(&tx) {
            match item.context("EOTS store read failed")? {
                (_, EotsValue::Vote(v)) => state.votes.push(v),
                (EotsKey::Block(height), EotsValue::Block(hash)) => state.blocks.push((height, hash)),
                (_, EotsValue::Final(h)) => state.recorded_final = h,
                (_, EotsValue::Checkpointed(h)) => state.checkpointed = h,
                (EotsKey::EvidenceCarried(pk), EotsValue::Carried) => {
                    state.evidence_carried.push(pk)
                }
                (EotsKey::PowerTable(height), EotsValue::PowerTable(table)) => {
                    let table = table
                        .into_iter()
                        .filter_map(|(k, w)| Some((KeyBytes::try_from(k).ok()?, w)))
                        .collect();
                    state.power_tables.push((height, table))
                }
                (_, EotsValue::LastCast(h)) => state.last_cast = h,
                _ => {}
            }
        }
        Ok(state)
    }
}

/// `gateway_final_height` comes from the committed state.
pub fn answer_query(
    tally: Option<&EotsTally>,
    gateway_final_height: Option<u64>,
) -> response::Query {
    let Some(tally) = tally else {
        return response::Query {
            code: 1.into(),
            info: "no EOTS finality on this subnet".to_string(),
            ..Default::default()
        };
    };
    let mut status = tally.status();
    let conflict_height = tally.first_conflict();
    status.final_height = below_conflict(
        gateway_final_height.unwrap_or(status.final_height),
        conflict_height,
    );
    let answer = FinalityAnswer {
        final_height: status.final_height,
        recorded_final_height: status.recorded_final_height,
        checkpointed_height: status.checkpointed_height,
        conflict_height,
    };
    response::Query {
        value: serde_json::to_vec(&answer)
            .expect("the answer serializes")
            .into(),
        height: tendermint::block::Height::try_from(status.final_height).unwrap_or_default(),
        ..Default::default()
    }
}

/// Verifies the signature of a vote's gossip envelope.
pub fn envelope_check(subnet_id: SubnetID) -> EnvelopeCheck {
    Arc::new(move |vote: &ValidatorVote| {
        let Ok(key) = libp2p::identity::secp256k1::PublicKey::try_from_bytes(&vote.validator)
        else {
            return false;
        };
        let key = libp2p::identity::PublicKey::from(key);
        let record = VoteRecord {
            public_key: ValidatorKey::from(key.clone()),
            subnet_id: subnet_id.clone(),
            content: AppVote::Eots(vote.vote.clone()),
            timestamp: Timestamp::from(vote.timestamp),
        };
        SignedVoteRecord::from_parts(&key, record, vote.signature.clone()).is_ok()
    })
}

/// Signs and publishes the validator's votes after each committed height.
pub async fn vote_loop(
    tally: EotsTally,
    key: libp2p::identity::Keypair,
    subnet_id: SubnetID,
    client: Client<AppVote>,
) {
    let own = key_bytes_of(&key.public());
    let mut committed = tally.committed();
    loop {
        if committed.changed().await.is_err() {
            return;
        }
        let height = *committed.borrow();
        if let Err(e) = tally.read_monitor().await {
            tracing::warn!(error = %e, "EOTS vote loop: monitor unreachable, not voting");
            continue;
        }
        for (h, hash) in tally.votes_to_cast(height) {
            let vote = match tally.provider().sign_eots_vote(&subnet_id, h, &hash).await {
                Ok(vote) => vote,
                Err(e) => {
                    tracing::warn!(height = h, error = %e, "EOTS vote refused by the provider");
                    break;
                }
            };
            let Some(record) = sign(&key, &subnet_id, AppVote::Eots(vote.clone())) else {
                break;
            };
            let own_vote = ValidatorVote {
                validator: own.to_vec(),
                vote,
                timestamp: record.record().timestamp.into(),
                signature: record.signature(),
            };
            match tally.add_vote(own_vote, None) {
                Ok(VoteOutcome::New) | Ok(VoteOutcome::Known) => {}
                Ok(outcome) => tracing::warn!(height = h, ?outcome, "own EOTS vote"),
                Err(e) => {
                    // Stop here: marking a later height as cast would skip this one for good.
                    tracing::error!(height = h, error = %e, "own EOTS vote rejected locally");
                    break;
                }
            }
            // Future work: implement resending this vote.
            publish(&client, record);
            tally.mark_cast(h);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn sign(
    key: &libp2p::identity::Keypair,
    subnet_id: &SubnetID,
    content: AppVote,
) -> Option<SignedVoteRecord<AppVote>> {
    match VoteRecord::signed(key, subnet_id.clone(), content) {
        Ok(record) => Some(record),
        Err(e) => {
            tracing::error!(error = %e, "failed to sign an EOTS vote record");
            None
        }
    }
}

fn publish(client: &Client<AppVote>, record: SignedVoteRecord<AppVote>) {
    if let Err(e) = client.publish_vote(record) {
        tracing::error!(error = %e, "failed to publish an EOTS vote");
    }
}

/// Initializes the tally's power table from committed state.
pub fn seed_power_table<Q>(tally: &EotsTally, query: &Q) -> anyhow::Result<bool>
where
    Q: fendermint_vm_topdown::sync::ParentFinalityStateQuery,
{
    if tally.has_stored_power_tables() {
        return Ok(true);
    }
    let Some(validators) = query.get_power_table()? else {
        return Ok(false);
    };
    set_genesis_power_table(tally, validators)?;
    Ok(true)
}

pub fn set_genesis_power_table(
    tally: &EotsTally,
    validators: Vec<Validator<Power>>,
) -> anyhow::Result<()> {
    let table = validators
        .into_iter()
        .map(|v| {
            (
                fendermint_vm_topdown::voting::ValidatorKey::from(v.public_key.0),
                v.power.0,
            )
        })
        .collect();
    tally.set_power_table(0, table)
}

#[cfg(test)]
mod tests;
