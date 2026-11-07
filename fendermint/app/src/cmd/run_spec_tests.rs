use super::{dispatch_vote, AppVote, EotsTally, Keypair, SubnetID, VoteTally};
use fendermint_vm_topdown::eots::{AgreedState, EotsStore, KeyBytes, StoredState};
use fendermint_vm_topdown::voting::Weight;
use ipc_eots::{testing::TestSigner, ValidatorVote};
use ipc_ipld_resolver::{SignedVoteRecord, VoteRecord};
use ipc_provider::{config::Config, manager::BtcSubnetManager};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Store(Mutex<Vec<ValidatorVote>>);

impl EotsStore for Store {
    fn put_vote(&self, vote: &ValidatorVote) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(vote.clone());
        Ok(())
    }
    fn put_block(&self, _: u64, _: [u8; 32]) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_recorded_final(&self, _: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_evidence_carried(&self, _: [u8; 32]) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_power_table(&self, _: u64, _: &[(KeyBytes, Weight)]) -> anyhow::Result<()> {
        Ok(())
    }
    fn load(&self) -> anyhow::Result<StoredState> {
        Ok(StoredState::default())
    }
}

// slashing.md, Accepting an EOTS vote 1,3: unsupported or unseated senders supply no stored or carryable votes.
#[tokio::test]
async fn an_ed25519_gossip_sender_cannot_crash_the_vote_dispatcher() {
    let subnet = SubnetID::new_btc(4, &hex::encode([42; 20])).unwrap();
    let config = Config::from_toml_str(
        "[[subnets]]\nid = \"/b4\"\n[subnets.config]\nnetwork_type = \"btc\"\nprovider_http = \"http://127.0.0.1:1/api\"\n"
    ).unwrap();
    let provider = BtcSubnetManager::new(config.subnets.values().next().unwrap()).unwrap();
    let store = Arc::new(Store::default());
    let tally = EotsTally::new(
        store.clone(),
        provider,
        subnet.clone(),
        vec![],
        fendermint_app::eots::envelope_check(subnet.clone()),
    )
    .unwrap();
    tally.add_block(1, [1; 32]).unwrap();
    let agreed = AgreedState {
        checkpointed: 0,
        killed: false,
        bitcoin_height: 100,
    };
    let parent: VoteTally = VoteTally::new(vec![], (0, Default::default()));
    let eots = TestSigner::new(7, 1, 2).vote(&[42; 20], 1, [1; 32]);
    for key in [Keypair::generate_ed25519(), Keypair::generate_secp256k1()] {
        let signed = VoteRecord::signed(&key, subnet.clone(), AppVote::Eots(eots.clone())).unwrap();
        let wire = signed.into_envelope().into_protobuf_encoding();
        let received = SignedVoteRecord::<AppVote>::from_bytes(&wire).unwrap();
        dispatch_vote(received, &parent, Some(&tally), None, false).await;
        assert!(store.0.lock().unwrap().is_empty());
        assert!(tally.proposal(&agreed).is_none());
    }
}
