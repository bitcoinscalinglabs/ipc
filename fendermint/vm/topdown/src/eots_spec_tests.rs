use super::*;
use ipc_eots::testing::TestSigner;
use ipc_eots::EotsVote;
use ipc_ipld_resolver::{SignedVoteRecord, Timestamp, VoteRecord};
use libp2p::identity::Keypair;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const BITCOIN_HEIGHT: u64 = 100;

fn agreed() -> AgreedState {
    AgreedState {
        checkpointed: 0,
        killed: false,
        bitcoin_height: BITCOIN_HEIGHT,
    }
}

fn block(height: u64) -> [u8; 32] {
    [height as u8; 32]
}

#[derive(Default)]
struct MemoryStore(Mutex<StoredState>);

impl EotsStore for MemoryStore {
    fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        if !state.votes.contains(vote) {
            state.votes.push(vote.clone());
        }
        Ok(())
    }
    fn put_block(&self, height: u64, hash: [u8; 32]) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.blocks.retain(|(h, _)| *h != height);
        state.blocks.push((height, hash));
        Ok(())
    }
    fn put_recorded_final(&self, height: u64) -> anyhow::Result<()> {
        self.0.lock().unwrap().recorded_final = height;
        Ok(())
    }
    fn put_evidence_carried(&self, pk: [u8; 32]) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        if !state.evidence_carried.contains(&pk) {
            state.evidence_carried.push(pk);
        }
        Ok(())
    }
    fn put_power_table(&self, height: u64, table: &[(KeyBytes, Weight)]) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.power_tables.retain(|(h, _)| *h != height);
        state.power_tables.push((height, table.to_vec()));
        Ok(())
    }
    fn load(&self) -> anyhow::Result<StoredState> {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct Fixture {
    subnet: SubnetID,
    txid: [u8; 20],
    keys: Vec<Keypair>,
    signers: Vec<TestSigner>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            subnet: SubnetID::new_btc(4, &hex::encode([42; 20])).unwrap(),
            txid: [42; 20],
            keys: (0..4).map(|_| Keypair::generate_secp256k1()).collect(),
            signers: (1..=4).map(|seed| TestSigner::new(seed, 1, 4)).collect(),
        }
    }

    fn wrap(&self, actor: usize, vote: EotsVote) -> ValidatorVote {
        let signed =
            VoteRecord::signed(&self.keys[actor], self.subnet.clone(), vote.clone()).unwrap();
        ValidatorVote {
            validator: key_bytes_of(&self.keys[actor].public()).to_vec(),
            vote,
            timestamp: signed.record().timestamp.into(),
            signature: signed.signature(),
        }
    }

    fn vote(&self, actor: usize, height: u64, hash: [u8; 32]) -> ValidatorVote {
        self.wrap(actor, self.signers[actor].vote(&self.txid, height, hash))
    }

    fn node(&self, store: Arc<MemoryStore>, url: &str) -> EotsTally {
        let parent = ipc_provider::config::Config::from_toml_str(&format!(
            "[[subnets]]\nid = \"/b4\"\n[subnets.config]\nnetwork_type = \"btc\"\nprovider_http = \"{url}\"\n"
        )).unwrap();
        let provider = BtcSubnetManager::new(parent.subnets.values().next().unwrap()).unwrap();
        let subnet = self.subnet.clone();
        let check: EnvelopeCheck = Arc::new(move |v| {
            let Ok(key) = libp2p::identity::secp256k1::PublicKey::try_from_bytes(&v.validator)
            else {
                return false;
            };
            let key = libp2p::identity::PublicKey::from(key);
            let record = VoteRecord {
                public_key: ValidatorKey::from(key.clone()),
                subnet_id: subnet.clone(),
                content: v.vote.clone(),
                timestamp: Timestamp::from(v.timestamp),
            };
            SignedVoteRecord::from_parts(&key, record, v.signature.clone()).is_ok()
        });
        let table = self
            .keys
            .iter()
            .map(|k| (ValidatorKey::from(k.public()), 1))
            .collect();
        let tally = EotsTally::new(store, provider, self.subnet.clone(), table, check).unwrap();
        {
            let mut inner = tally.lock();
            for (key, signer) in self.keys.iter().zip(&self.signers) {
                inner
                    .eots_keys
                    .insert(x_only(&key_bytes_of(&key.public())), signer.pk_eots);
                inner
                    .commitments
                    .insert(signer.pk_eots, vec![(signer.commitment, BITCOIN_HEIGHT)]);
            }
            inner.refreshed = true;
        }
        tally
    }

    fn fresh_node(&self) -> (EotsTally, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        (self.node(store.clone(), "http://127.0.0.1:1/api"), store)
    }

    fn quorum(&self, height: u64) -> Vec<ValidatorVote> {
        (0..3)
            .map(|actor| self.vote(actor, height, block(height)))
            .collect()
    }

    fn live_after_rejection(&self, node: &EotsTally) {
        for height in 1..=2 {
            node.add_block(height, block(height)).unwrap();
            for vote in self.quorum(height) {
                node.add_vote(vote, Some(&agreed())).unwrap();
            }
        }
        let proposal = node
            .proposal(&agreed())
            .expect("honest quorum must keep producing blocks");
        node.check_proposal(&proposal, &agreed()).unwrap();
        assert!(node
            .apply_proposal(&proposal, &agreed())
            .unwrap()
            .is_empty());
        assert_eq!(node.recorded_final_height(), 2);
        assert_eq!(node.final_height(), 2);
    }

    fn reject_on_every_receiver(&self, bad: ValidatorVote) {
        for _ in 0..3 {
            let (node, store) = self.fresh_node();
            node.add_block(1, block(1)).unwrap();
            let before = node.status();
            assert!(node.add_vote(bad.clone(), None).is_err());
            assert_eq!(node.status(), before);
            assert!(store.load().unwrap().votes.is_empty());
            assert!(node.lock().pending.is_empty());
            let mut votes = self.quorum(1);
            votes.push(bad.clone());
            let proposal = EotsFinalityData {
                bitcoin_height: BITCOIN_HEIGHT,
                votes,
                evidence: vec![],
            };
            assert!(
                node.check_proposal(&proposal, &agreed()).is_err(),
                "one bad carried vote rejects the whole proposal"
            );
            assert_eq!(node.status(), before);
            assert!(store.load().unwrap().votes.is_empty());
            self.live_after_rejection(&node);
        }
    }
}

macro_rules! rejected_vote {
    ($name:ident, $f:ident, $v:ident, $mutation:block) => {
        // slashing.md, Accepting an EOTS vote / Agreed subnet finality: drop bad gossip and reject its proposal.
        #[test]
        fn $name() {
            let $f = Fixture::new();
            let mut $v = $f.signers[3].vote(&$f.txid, 1, block(1));
            $mutation
            let bad = $f.wrap(3, $v);
            $f.reject_on_every_receiver(bad);
        }
    };
}

rejected_vote!(a_signature_for_another_message_is_rejected, f, v, {
    v.signature = f.signers[3].vote(&f.txid, 1, [99; 32]).signature;
});
rejected_vote!(a_signature_for_another_subnet_is_rejected, f, v, {
    v.signature = f.signers[3].vote(&[43; 20], 1, block(1)).signature;
});
rejected_vote!(a_signature_for_another_height_is_rejected, f, v, {
    v.signature[1] = f.signers[3].vote(&f.txid, 2, block(1)).signature[1];
});
rejected_vote!(an_unregistered_eots_key_is_rejected, f, v, {
    v = TestSigner::new(19, 1, 4).vote(&f.txid, 1, block(1));
});
rejected_vote!(an_unparseable_eots_key_is_rejected, f, v, {
    v.pk_eots = [255; 32];
});
rejected_vote!(a_corrupted_signature_scalar_is_rejected, f, v, {
    v.signature[1][7] ^= 1;
});
rejected_vote!(an_out_of_range_signature_scalar_is_rejected, f, v, {
    v.signature[1] = [255; 32];
});
rejected_vote!(an_unparseable_public_nonce_is_rejected, f, v, {
    v.signature[0] = [255; 32];
});
rejected_vote!(
    a_nonce_for_a_different_committed_height_is_rejected,
    f,
    v,
    {
        v.signature = f.signers[3].vote(&f.txid, 2, block(1)).signature;
    }
);
rejected_vote!(a_corrupted_audit_path_is_rejected, f, v, {
    v.proof[0][0] ^= 1;
});
rejected_vote!(a_truncated_audit_path_is_rejected, f, v, {
    v.proof.pop();
});
rejected_vote!(an_extended_audit_path_is_rejected, f, v, {
    v.proof.push([0; 32]);
});
rejected_vote!(an_audit_path_for_a_different_index_is_rejected, f, v, {
    v.proof = f.signers[3].vote(&f.txid, 2, block(1)).proof;
});
rejected_vote!(a_false_commitment_start_is_rejected, f, v, {
    v.start = 2;
});
rejected_vote!(a_height_before_genesis_is_rejected, f, v, {
    v.height = 0;
});
rejected_vote!(a_height_past_the_commitment_is_rejected, f, v, {
    v.height = 5;
});
rejected_vote!(the_maximum_height_does_not_overflow_validation, f, v, {
    v.height = u64::MAX;
});

// slashing.md, Accepting an EOTS vote 1: unsigned changes of the envelope cannot become accepted votes.
#[test]
fn altered_envelopes_leave_every_receiver_live() {
    let f = Fixture::new();
    let original = f.vote(3, 1, block(1));
    let mut bad = original.clone();
    bad.signature.clear();
    f.reject_on_every_receiver(bad);
    let mut bad = original.clone();
    bad.timestamp ^= 1;
    f.reject_on_every_receiver(bad);
    let mut bad = original;
    bad.validator = vec![0; 33];
    f.reject_on_every_receiver(bad);
}

// slashing.md, Accepting an EOTS vote 3: power comes from the table at the vote's height.
#[test]
fn a_registered_validator_without_power_cannot_pollute_a_proposal() {
    let f = Fixture::new();
    for _ in 0..3 {
        let (node, store) = f.fresh_node();
        node.update_power_table(1, vec![(ValidatorKey::from(f.keys[3].public()), 0)])
            .unwrap();
        let bad = f.vote(3, 1, block(1));
        assert!(matches!(
            node.add_vote(bad.clone(), None),
            Err(VoteError::UnpoweredValidator(1))
        ));
        node.add_block(1, block(1)).unwrap();
        let mut votes = f.quorum(1);
        votes.push(bad);
        assert!(node
            .check_proposal(
                &EotsFinalityData {
                    bitcoin_height: BITCOIN_HEIGHT,
                    votes,
                    evidence: vec![]
                },
                &agreed()
            )
            .is_err());
        assert!(store.load().unwrap().votes.is_empty());
        f.live_after_rejection(&node);
    }
}

// slashing.md, Accepting an EOTS vote 3: a new seat cannot vote at earlier heights.
#[test]
fn future_power_does_not_authorize_a_vote_in_the_past() {
    let f = Fixture::new();
    let (node, _) = f.fresh_node();
    let key = ValidatorKey::from(f.keys[3].public());
    node.update_power_table(1, vec![(key.clone(), 0)]).unwrap();
    node.update_power_table(2, vec![(key, 1)]).unwrap();
    assert!(matches!(
        node.add_vote(f.vote(3, 1, block(1)), None),
        Err(VoteError::UnpoweredValidator(1))
    ));
    assert_eq!(
        node.add_vote(f.vote(3, 2, block(2)), None).unwrap(),
        VoteOutcome::New
    );
}

// slashing.md, Accepting an EOTS vote 4: a caught-up monitor drops a vote with no recorded range.
#[test]
fn a_missing_commitment_does_not_enter_the_store_or_finality() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    node.lock().commitments.remove(&f.signers[3].pk_eots);
    assert!(matches!(
        node.add_vote(f.vote(3, 1, block(1)), None),
        Err(VoteError::NotCommitted(1))
    ));
    assert!(store.load().unwrap().votes.is_empty());
    assert!(node.lock().pending.is_empty());
    f.live_after_rejection(&node);
}

// slashing.md, Accepting an EOTS vote 2: checkpointed heights cannot vote again.
#[test]
fn votes_at_or_below_the_checkpoint_are_dropped() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    node.lock().checkpoint = Some((2, block(2), false));
    for height in [1, 2] {
        assert!(matches!(
            node.add_vote(f.vote(3, height, block(height)), None),
            Err(VoteError::Checkpointed(_, 2))
        ));
    }
    assert!(store.load().unwrap().votes.is_empty());
    assert_eq!(
        node.add_vote(f.vote(3, 3, block(3)), None).unwrap(),
        VoteOutcome::New
    );
}

// slashing.md, Accepting an EOTS vote 2: the kill checkpoint ends all later voting.
#[test]
fn no_vote_is_accepted_after_the_kill_checkpoint() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    node.lock().checkpoint = Some((1, block(1), true));
    assert_eq!(
        node.add_vote(f.vote(3, 2, block(2)), None),
        Err(VoteError::Killed)
    );
    assert!(store.load().unwrap().votes.is_empty());
}

// slashing.md, Final blocks / Double votes: fork votes do not finalize the local block and cap served finality.
#[test]
fn fork_votes_are_held_but_do_not_finalize_the_local_block() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    for actor in 0..3 {
        node.add_vote(f.vote(actor, 1, [99; 32]), None).unwrap();
    }
    assert_eq!(node.lock().known_final(), 0);
    assert!(node.proposal(&agreed()).is_none());
    assert_eq!(store.load().unwrap().blocks, vec![(1, block(1))]);
    assert_eq!(
        node.add_vote(f.vote(0, 1, block(1)), None).unwrap(),
        VoteOutcome::Evidence
    );
    assert_eq!(node.evidence().len(), 1);
    for actor in 1..3 {
        node.add_vote(f.vote(actor, 1, block(1)), None).unwrap();
    }
    assert_eq!(node.lock().known_final(), 1);
    let proposal = node.proposal(&agreed()).unwrap();
    node.check_proposal(&proposal, &agreed()).unwrap();
    assert!(node
        .apply_proposal(&proposal, &agreed())
        .unwrap()
        .is_empty());
    assert_eq!(node.recorded_final_height(), 1);
    assert_eq!(node.final_height(), 0);
    assert!(node.final_block(1).is_none());
}

// slashing.md, Accepting an EOTS vote: exact repeats and further forks cannot amplify power or storage.
#[test]
fn repeated_and_third_votes_do_not_amplify_the_tally() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    let first = f.vote(3, 1, block(1));
    assert_eq!(
        node.add_vote(first.clone(), None).unwrap(),
        VoteOutcome::New
    );
    for _ in 0..8 {
        assert_eq!(
            node.add_vote(first.clone(), None).unwrap(),
            VoteOutcome::Known
        );
    }
    assert_eq!(
        node.add_vote(f.vote(3, 1, [9; 32]), None).unwrap(),
        VoteOutcome::Evidence
    );
    assert_eq!(
        node.add_vote(f.vote(3, 1, [8; 32]), None).unwrap(),
        VoteOutcome::Excess
    );
    assert_eq!(store.load().unwrap().votes.len(), 2);
    assert_eq!(node.lock().known_final(), 0);
    assert_eq!(node.evidence().len(), 1);
    f.live_after_rejection(&node);
}

// slashing.md, Accepting an EOTS vote: a fresh envelope around the same EOTS vote uses no extra storage or power.
#[test]
fn resigning_an_envelope_does_not_create_another_vote() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    let first = f.vote(3, 1, block(1));
    assert_eq!(
        node.add_vote(first.clone(), None).unwrap(),
        VoteOutcome::New
    );
    let record = VoteRecord {
        public_key: ValidatorKey::from(f.keys[3].public()),
        subnet_id: f.subnet.clone(),
        content: first.vote.clone(),
        timestamp: Timestamp::from(first.timestamp + 1),
    };
    let signed = SignedVoteRecord::new(&f.keys[3], record).unwrap();
    let repeated = ValidatorVote {
        timestamp: first.timestamp + 1,
        signature: signed.signature(),
        ..first
    };
    assert_eq!(node.add_vote(repeated, None).unwrap(), VoteOutcome::Known);
    assert_eq!(store.load().unwrap().votes.len(), 1);
    f.live_after_rejection(&node);
}

// slashing.md, Double votes / Subnet final height: evidence preserves power until removal and carried votes advance finality.
#[test]
fn evidence_preserves_power_until_the_validator_is_removed() {
    let f = Fixture::new();
    let (node, _) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    node.add_vote(f.vote(3, 1, block(1)), None).unwrap();
    assert_eq!(
        node.add_vote(f.vote(3, 1, [99; 32]), None).unwrap(),
        VoteOutcome::Evidence
    );
    for actor in 0..2 {
        node.add_vote(f.vote(actor, 1, block(1)), None).unwrap();
    }
    assert_eq!(
        node.lock().known_final(),
        1,
        "the equivocator's valid vote still counts"
    );
    assert_eq!(node.final_height(), 0);
    let proposal = node.proposal(&agreed()).unwrap();
    node.check_proposal(&proposal, &agreed()).unwrap();
    assert!(node
        .apply_proposal(&proposal, &agreed())
        .unwrap()
        .is_empty());
    assert_eq!(node.final_height(), 1);
    node.update_power_table(2, vec![(ValidatorKey::from(f.keys[3].public()), 0)])
        .unwrap();
    node.add_block(2, block(2)).unwrap();
    for actor in 0..2 {
        node.add_vote(f.vote(actor, 2, block(2)), None).unwrap();
    }
    assert!(matches!(
        node.add_vote(f.vote(3, 2, block(2)), None),
        Err(VoteError::UnpoweredValidator(2))
    ));
    assert_eq!(node.lock().known_final(), 1);
    node.add_vote(f.vote(2, 2, block(2)), None).unwrap();
    assert_eq!(node.lock().known_final(), 2);
    assert_eq!(node.final_height(), 1);
    let proposal = node.proposal(&agreed()).unwrap();
    node.check_proposal(&proposal, &agreed()).unwrap();
    assert!(node
        .apply_proposal(&proposal, &agreed())
        .unwrap()
        .is_empty());
    assert_eq!(node.final_height(), 2);
}

// slashing.md, Agreed subnet finality 2: repeated copies of one validator never form a quorum.
#[test]
fn a_proposer_cannot_count_one_vote_more_than_once() {
    let f = Fixture::new();
    let (node, _) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    let data = EotsFinalityData {
        bitcoin_height: BITCOIN_HEIGHT,
        votes: vec![f.vote(3, 1, block(1)); 4],
        evidence: vec![],
    };
    assert!(node.check_proposal(&data, &agreed()).is_err());
    f.live_after_rejection(&node);
}

// slashing.md, Agreed subnet finality 2: a quorum for a different block cannot finalize this chain.
#[test]
fn a_carried_quorum_for_another_block_rejects_the_proposal() {
    let f = Fixture::new();
    let (node, _) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    let votes = (0..3).map(|actor| f.vote(actor, 1, [99; 32])).collect();
    assert!(node
        .check_proposal(
            &EotsFinalityData {
                bitcoin_height: BITCOIN_HEIGHT,
                votes,
                evidence: vec![]
            },
            &agreed()
        )
        .is_err());
    f.live_after_rejection(&node);
}

// slashing.md, Agreed subnet finality 3: finality advances without missing intermediate heights.
#[test]
fn a_proposer_cannot_skip_an_unfinalized_height() {
    let f = Fixture::new();
    let (node, _) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    node.add_block(2, block(2)).unwrap();
    assert!(node
        .check_proposal(
            &EotsFinalityData {
                bitcoin_height: BITCOIN_HEIGHT,
                votes: f.quorum(2),
                evidence: vec![]
            },
            &agreed()
        )
        .is_err());
    f.live_after_rejection(&node);
}

// slashing.md, Agreed subnet finality 4: evidence needs two accepted votes of one key and height on distinct blocks.
#[test]
fn invalid_evidence_rejects_an_otherwise_honest_proposal() {
    let f = Fixture::new();
    let (node, store) = f.fresh_node();
    node.add_block(1, block(1)).unwrap();
    let a = f.vote(3, 1, block(1));
    let mut forged = f.vote(3, 1, [99; 32]);
    forged.vote.signature[1][0] ^= 1;
    forged = f.wrap(3, forged.vote);
    for pair in [
        (a.clone(), a.clone()),
        (a.clone(), f.vote(2, 1, [99; 32])),
        (a.clone(), f.vote(3, 2, [99; 32])),
        (a, forged),
    ] {
        let data = EotsFinalityData {
            bitcoin_height: BITCOIN_HEIGHT,
            votes: f.quorum(1),
            evidence: vec![pair],
        };
        assert!(node.check_proposal(&data, &agreed()).is_err());
        assert!(store.load().unwrap().votes.is_empty());
        assert_eq!(node.recorded_final_height(), 0);
    }
    f.live_after_rejection(&node);
}

fn pollution_fixture(with_evidence: bool, obsolete: bool) {
    let f = Fixture::new();
    let (proposer, _) = f.fresh_node();
    let start = if obsolete { 2 } else { 1 };
    for height in 1..=2 {
        proposer.add_block(height, block(height)).unwrap();
        for vote in f.quorum(height) {
            proposer.add_vote(vote, None).unwrap();
        }
    }
    if !obsolete {
        proposer
            .lock()
            .commitments
            .get_mut(&f.signers[3].pk_eots)
            .unwrap()[0]
            .1 = BITCOIN_HEIGHT + 1;
    }
    // The non-proposer's messages pass the proposer's current gossip checks.
    assert_eq!(
        proposer.add_vote(f.vote(3, 1, block(1)), None).unwrap(),
        VoteOutcome::New
    );
    if with_evidence {
        assert_eq!(
            proposer.add_vote(f.vote(3, 1, [99; 32]), None).unwrap(),
            VoteOutcome::Evidence
        );
    }
    let state = AgreedState {
        checkpointed: start - 1,
        ..agreed()
    };
    let proposal = proposer
        .proposal(&state)
        .expect("an honest quorum is available");
    assert!(!proposal
        .votes
        .iter()
        .any(|v| v.vote.pk_eots == f.signers[3].pk_eots));
    assert!(
        proposal.evidence.is_empty(),
        "locally accepted evidence must pass the agreed historical L1 checks too"
    );
    for _ in 0..3 {
        let (receiver, _) = f.fresh_node();
        for height in 1..=2 {
            receiver.add_block(height, block(height)).unwrap();
        }
        // Other local gossip and monitor-head views must not affect the proposal decision.
        receiver.lock().checkpoint = Some((2, block(2), true));
        receiver
            .check_proposal(&proposal, &state)
            .expect("non-proposer gossip cannot poison an honest block");
        assert!(receiver
            .apply_proposal(&proposal, &state)
            .unwrap()
            .is_empty());
        assert_eq!(receiver.recorded_final_height(), 2);
        receiver.add_block(3, block(3)).unwrap();
        for vote in f.quorum(3) {
            receiver.add_vote(vote, Some(&state)).unwrap();
        }
        let next = receiver.proposal(&state).unwrap();
        receiver.check_proposal(&next, &state).unwrap();
        receiver.apply_proposal(&next, &state).unwrap();
        assert_eq!(receiver.recorded_final_height(), 3);
    }
}

// slashing.md, Agreed subnet finality 1: a commitment above the agreed L1 height cannot pollute carried votes.
#[test]
fn future_commitment_gossip_cannot_pollute_an_honest_block() {
    pollution_fixture(false, false);
}

// slashing.md, Agreed subnet finality 1,4: future-commitment evidence cannot pollute an honest proposer.
#[test]
fn future_commitment_evidence_cannot_pollute_an_honest_block() {
    pollution_fixture(true, false);
}

// slashing.md, Agreed subnet finality 1,4: evidence invalidated by the checkpoint must stay out of proposals.
#[test]
fn checkpointed_evidence_cannot_pollute_an_honest_block() {
    pollution_fixture(true, true);
}

struct Monitor {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Monitor {
    fn new(f: &Fixture, omit_commitment: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/api", listener.local_addr().unwrap());
        let keys: Vec<_> = f.keys.iter().zip(&f.signers).map(|(key, signer)| serde_json::json!({
            "pubkey": hex::encode(x_only(&key_bytes_of(&key.public()))), "pk_eots": hex::encode(signer.pk_eots)
        })).collect();
        let commitments: HashMap<_, _> = f.signers.iter().enumerate().map(|(i, signer)| {
            let c = signer.commitment;
            let list = if omit_commitment && i == 3 { vec![] } else { vec![serde_json::json!({
                "start": c.start, "count": c.count, "root": hex::encode(c.root), "block_height": BITCOIN_HEIGHT
            })] };
            (hex::encode(signer.pk_eots), list)
        }).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("mock monitor accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let request: serde_json::Value = loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    assert_ne!(n, 0, "incomplete mock RPC request");
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = std::str::from_utf8(&bytes[..end]).unwrap();
                        let length: usize = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break serde_json::from_slice(&bytes[end + 4..end + 4 + length])
                                .unwrap();
                        }
                    }
                };
                let result = match request["method"].as_str().unwrap() {
                    "getsubnetcheckpoint" => serde_json::Value::Null,
                    "geteotskeys" => serde_json::json!(keys),
                    "geteotscommitments" => serde_json::json!(
                        commitments[request["params"]["pk_eots"].as_str().unwrap()]
                    ),
                    method => panic!("unexpected mock monitor RPC: {method}"),
                };
                let body =
                    serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                        .to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[derive(Clone, Default)]
struct LogBytes(Arc<Mutex<Vec<u8>>>);
impl Write for LogBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBytes {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn committed_bad_vote(omit_commitment: bool) {
    let f = Fixture::new();
    let monitor = Monitor::new(&f, omit_commitment);
    let logs = LogBytes::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::ERROR)
        .with_writer(logs.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let mut bad = f.vote(3, 1, block(1));
    if !omit_commitment {
        bad.vote.signature[1][0] ^= 1;
        bad = f.wrap(3, bad.vote);
    }
    let honest_votes = f.quorum(1);
    let mut votes = honest_votes.clone();
    votes.push(bad.clone());
    let data = EotsFinalityData {
        bitcoin_height: BITCOIN_HEIGHT,
        votes,
        evidence: vec![],
    };
    let store = Arc::new(MemoryStore::default());
    let node = f.node(store.clone(), &monitor.url);
    node.add_block(1, block(1)).unwrap();
    // First delivery, repeat delivery, restart/replay, and a node syncing only from blocks.
    for pass in 0..4 {
        let current = match pass {
            0 | 1 => node.clone(),
            2 => f.node(store.clone(), &monitor.url),
            _ => {
                let syncing = f.node(Arc::new(MemoryStore::default()), &monitor.url);
                syncing.add_block(1, block(1)).unwrap();
                syncing
            }
        };
        let start = logs.0.lock().unwrap().len();
        tokio::time::timeout(Duration::from_secs(5), current.execute_proposal(&data))
            .await
            .expect("committed delivery must finish")
            .expect("a rejected carried vote must not abort execution");
        assert_eq!(current.recorded_final_height(), 1);
        assert_eq!(current.final_height(), 1);
        assert!(!current.store.load().unwrap().votes.contains(&bad));
        assert!(!current
            .lock()
            .votes
            .values()
            .flat_map(|m| m.values())
            .flatten()
            .any(|v| v == &bad));
        assert!(current.lock().pending.is_empty());
        let text = String::from_utf8(logs.0.lock().unwrap()[start..].to_vec()).unwrap();
        assert!(
            text.contains("ERROR") && text.contains("carried vote rejected"),
            "each execution must log the rejected vote at ERROR: {text}"
        );
        let clean = f.node(Arc::new(MemoryStore::default()), &monitor.url);
        clean.add_block(1, block(1)).unwrap();
        let honest = EotsFinalityData {
            bitcoin_height: BITCOIN_HEIGHT,
            votes: honest_votes.clone(),
            evidence: vec![],
        };
        clean.execute_proposal(&honest).await.unwrap();
        assert_eq!(current.status(), clean.status());
        assert_eq!(
            current.store.load().unwrap().blocks,
            clean.store.load().unwrap().blocks
        );
        let held = current.store.load().unwrap().votes;
        let expected = clean.store.load().unwrap().votes;
        assert_eq!(held.len(), expected.len());
        assert!(expected.iter().all(|v| held.contains(v)));
        if pass == 3 {
            current.add_block(2, block(2)).unwrap();
            let next = EotsFinalityData {
                bitcoin_height: BITCOIN_HEIGHT,
                votes: f.quorum(2),
                evidence: vec![],
            };
            current.check_proposal(&next, &agreed()).unwrap();
            current.execute_proposal(&next).await.unwrap();
            assert_eq!(current.recorded_final_height(), 2);
        }
    }
}

// slashing.md, Agreed subnet finality / brief committed-block rule: rejected votes never abort delivery, replay or sync.
#[tokio::test]
async fn a_committed_vote_with_a_bad_signature_is_dropped_on_delivery_replay_and_sync() {
    committed_bad_vote(false).await;
}

// slashing.md, Agreed subnet finality / brief committed-block rule: monitor rejection logs ERROR and preserves agreed state.
#[tokio::test]
async fn a_committed_vote_without_a_monitor_commitment_is_dropped_on_delivery_replay_and_sync() {
    committed_bad_vote(true).await;
}
