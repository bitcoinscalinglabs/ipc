//! The EOTS round in the application: the persistent store of votes, the voting loop of a
//! validator, and the finality query.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use fendermint_storage::{Codec, KVCollection, KVReadable, KVStore, KVWritable};
use fendermint_vm_genesis::{Power, Validator};
use fendermint_vm_topdown::eots::{
    key_bytes_of, EnvelopeCheck, EotsStore, EotsTally, KeyBytes, StoredState, VoteOutcome,
};
use fendermint_vm_topdown::voting::Weight;
use ipc_api::subnet_id::SubnetID;
use ipc_eots::ValidatorVote;
use ipc_ipld_resolver::{Client, SignedVoteRecord, Timestamp, ValidatorKey, VoteRecord};
use serde::{Deserialize, Serialize};
use tendermint::abci::response;

use crate::ipc::AppVote;

/// The ABCI query path of the finality query.
pub const QUERY_PATH: &str = "/eots";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub enum EotsKey {
    Vote([u8; 32], u64, [u8; 32]),
    Block(u64),
    Final,
    Checkpointed,
    EvidenceCarried([u8; 32]),
    PowerTable(u64),
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
}

/// The store over the application's key-value database.
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
                _ => {}
            }
        }
        Ok(state)
    }
}

/// The finality query's answer: the subnet final height the node serves (the recorded one,
/// below a conflict), the recorded one itself, and for a height up to the served one its block
/// hash with the votes held for it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FinalityAnswer {
    pub final_height: u64,
    pub recorded_final_height: u64,
    pub checkpointed_height: u64,
    pub block: Option<FinalBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FinalBlock {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub votes: Vec<ValidatorVote>,
}

/// Answers the finality query; `data` is empty or the JSON of a height.
/// Answers the finality query. `gateway_final_height` is the subnet final height the gateway
/// holds in the committed state, the answer's final height when it is known.
pub fn answer_query(
    tally: Option<&EotsTally>,
    gateway_final_height: Option<u64>,
    data: &[u8],
) -> response::Query {
    let Some(tally) = tally else {
        return response::Query {
            code: 1.into(),
            info: "no EOTS finality on this subnet".to_string(),
            ..Default::default()
        };
    };
    let mut status = tally.status();
    if let Some(final_height) = gateway_final_height {
        status.final_height = final_height;
    }
    let block = if data.is_empty() {
        None
    } else {
        match serde_json::from_slice::<u64>(data) {
            Ok(height) => tally
                .final_block(height)
                .map(|(block_hash, votes)| FinalBlock {
                    height,
                    block_hash,
                    votes,
                }),
            Err(e) => {
                return response::Query {
                    code: 2.into(),
                    info: format!("the query data is not a height: {e}"),
                    ..Default::default()
                }
            }
        }
    };
    let answer = FinalityAnswer {
        final_height: status.final_height,
        recorded_final_height: status.recorded_final_height,
        checkpointed_height: status.checkpointed_height,
        block,
    };
    response::Query {
        value: serde_json::to_vec(&answer)
            .expect("the answer serializes")
            .into(),
        height: tendermint::block::Height::try_from(status.final_height).unwrap_or_default(),
        ..Default::default()
    }
}

/// Rule 1 of accepting a vote: the vote's gossip envelope, rebuilt from the validator's key,
/// the subnet id, the vote and the envelope's timestamp, verifies under the stored signature.
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

/// Publishes a vote or evidence under the node's key.
pub async fn publish_vote(
    client: &Client<AppVote>,
    key: &libp2p::identity::Keypair,
    subnet_id: &SubnetID,
    content: AppVote,
) {
    if let Some(record) = sign(key, subnet_id, content) {
        publish(client, record);
    }
}

/// Publishes again this node's own votes that no block carries yet and every evidence that no
/// block carries yet.
async fn republish(
    tally: &EotsTally,
    key: &libp2p::identity::Keypair,
    subnet_id: &SubnetID,
    client: &Client<AppVote>,
) {
    let own = key_bytes_of(&key.public());
    for v in tally.own_votes_above_recorded(&own) {
        publish_vote(client, key, subnet_id, AppVote::Eots(v.vote)).await;
    }
    for (a, b) in tally.evidence_to_publish() {
        publish_vote(client, key, subnet_id, AppVote::EotsEvidence(a, b)).await;
    }
}

/// A validator's voting loop: after every committed height, it refreshes the monitor's state,
/// asks its provider to sign the votes it has not cast, and publishes them. At start and every
/// tenth round it publishes its votes above the recorded final height and the evidence no block
/// carries yet again.
pub async fn vote_loop(
    tally: EotsTally,
    key: libp2p::identity::Keypair,
    subnet_id: SubnetID,
    client: Client<AppVote>,
) {
    let own = key_bytes_of(&key.public());
    let mut committed = tally.committed();
    let mut rounds: u64 = 0;
    republish(&tally, &key, &subnet_id, &client).await;
    loop {
        if committed.changed().await.is_err() {
            return;
        }
        let height = *committed.borrow();
        if let Err(e) = tally.refresh().await {
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
            publish(&client, record);
            tally.mark_cast(h);
        }
        rounds += 1;
        if rounds % 10 == 0 {
            republish(&tally, &key, &subnet_id, &client).await;
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

/// Seeds the tally's first power table from the ledger, unless the tally holds the tables of
/// an earlier run. Before genesis the ledger has no state and nothing is seeded: `init_chain`
/// seeds it once the genesis state is in place.
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

/// Installs `validators` as the tally's power table from height 0 on.
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

pub fn new_store<DB, S>(db: DB, ns: S::Namespace) -> Arc<dyn EotsStore>
where
    S: KVStore + Codec<EotsKey> + Codec<EotsValue> + Send + Sync + 'static,
    S::Repr: Ord + 'static,
    S::Namespace: Send + Sync + 'static,
    DB: KVWritable<S> + KVReadable<S> + Send + Sync + 'static,
{
    Arc::new(AppEotsStore::new(db, ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vote built from its signed envelope passes the envelope check, and a changed
    /// signature, timestamp or subnet does not.
    #[test]
    fn envelope_check_verifies_the_rebuilt_envelope() {
        let key = libp2p::identity::Keypair::generate_secp256k1();
        let subnet_id = SubnetID::new_root(7);
        let vote = ipc_eots::EotsVote {
            pk_eots: [1; 32],
            height: 3,
            block_hash: [2; 32],
            signature: [[3; 32]; 2],
            start: 1,
            proof: vec![[4; 32]; 2],
        };
        let record = VoteRecord::signed(&key, subnet_id.clone(), AppVote::Eots(vote.clone()))
            .unwrap();
        let own = ValidatorVote {
            validator: key_bytes_of(&key.public()).to_vec(),
            vote,
            timestamp: record.record().timestamp.into(),
            signature: record.signature(),
        };
        let check = envelope_check(subnet_id);
        assert!(check(&own));

        let mut tampered = own.clone();
        tampered.signature[5] ^= 1;
        assert!(!check(&tampered));
        let mut later = own.clone();
        later.timestamp += 1;
        assert!(!check(&later));
        assert!(!envelope_check(SubnetID::new_root(8))(&own));
    }

    /// An empty ledger seeds nothing, as `init_chain` seeds the genesis table later; a ledger
    /// with validators seeds its table.
    #[test]
    fn seed_power_table_waits_for_the_ledger() {
        use fendermint_vm_genesis::ValidatorKey;
        use fendermint_vm_topdown::sync::ParentFinalityStateQuery;
        use quickcheck::Arbitrary;

        struct Ledger(Option<Vec<Validator<Power>>>);
        impl ParentFinalityStateQuery for Ledger {
            fn get_latest_committed_finality(
                &self,
            ) -> anyhow::Result<Option<fendermint_vm_topdown::IPCParentFinality>> {
                Ok(None)
            }
            fn get_power_table(&self) -> anyhow::Result<Option<Vec<Validator<Power>>>> {
                Ok(self.0.clone())
            }
        }

        struct NoStore;
        impl EotsStore for NoStore {
            fn put_vote(&self, _vote: &ValidatorVote) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_block(&self, _height: u64, _hash: [u8; 32]) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_recorded_final(&self, _height: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_evidence_carried(&self, _pk_eots: [u8; 32]) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_power_table(
                &self,
                _height: u64,
                _table: &[(KeyBytes, Weight)],
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn load(&self) -> anyhow::Result<StoredState> {
                Ok(StoredState::default())
            }
        }

        fvm_shared::address::set_current_network(fvm_shared::address::Network::Testnet);
        let subnet_id: SubnetID = "/b4/t410fhor637l2pmjle6whfq7go5upmf74qg6dbr4uzei"
            .parse()
            .unwrap();
        let parent = ipc_provider::config::subnet::Subnet {
            id: subnet_id.parent().unwrap(),
            config: ipc_provider::config::subnet::SubnetConfig::Btc(
                ipc_provider::config::subnet::BTCSubnet {
                    provider_http: "http://127.0.0.1:1".parse().unwrap(),
                    provider_timeout: None,
                    auth_token: None,
                },
            ),
        };
        let tally = EotsTally::new(
            Arc::new(NoStore),
            ipc_provider::manager::BtcSubnetManager::new(&parent).unwrap(),
            subnet_id.clone(),
            Vec::new(),
            Arc::new(|_| true),
        )
        .unwrap();

        assert!(!seed_power_table(&tally, &Ledger(None)).unwrap());
        let validator = Validator {
            public_key: ValidatorKey::arbitrary(&mut quickcheck::Gen::new(8)),
            power: Power(1),
        };
        assert!(seed_power_table(&tally, &Ledger(Some(vec![validator]))).unwrap());

        // A store with the tables of an earlier run: seeded without the ledger
        struct StoredTables;
        impl EotsStore for StoredTables {
            fn put_vote(&self, _vote: &ValidatorVote) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_block(&self, _height: u64, _hash: [u8; 32]) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_recorded_final(&self, _height: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_evidence_carried(&self, _pk_eots: [u8; 32]) -> anyhow::Result<()> {
                Ok(())
            }
            fn put_power_table(
                &self,
                _height: u64,
                _table: &[(KeyBytes, Weight)],
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn load(&self) -> anyhow::Result<StoredState> {
                Ok(StoredState {
                    power_tables: vec![(0, vec![([2; 33], 1)])],
                    ..StoredState::default()
                })
            }
        }
        let restarted = EotsTally::new(
            Arc::new(StoredTables),
            ipc_provider::manager::BtcSubnetManager::new(&parent).unwrap(),
            subnet_id,
            Vec::new(),
            Arc::new(|_| true),
        )
        .unwrap();
        assert!(seed_power_table(&restarted, &Ledger(None)).unwrap());
    }
}
#[cfg(test)]
#[path = "eots_spec_tests.rs"]
mod spec_tests;
