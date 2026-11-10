use super::*;

#[derive(Default)]
struct Store {
    stored: StoredState,
}

impl EotsStore for Store {
    fn put_vote(&self, _vote: &ValidatorVote) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_block(&self, _height: u64, _hash: [u8; 32]) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_recorded_final(&self, _height: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_checkpointed(&self, _height: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_evidence_carried(&self, _pk_eots: [u8; 32]) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_power_table(&self, _height: u64, _table: &[(KeyBytes, Weight)]) -> anyhow::Result<()> {
        Ok(())
    }
    fn put_last_cast(&self, _height: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn delete_up_to(&self, _height: u64, _power_tables_from: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn load(&self) -> anyhow::Result<StoredState> {
        Ok(self.stored.clone())
    }
}

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
    let record = VoteRecord::signed(&key, subnet_id.clone(), AppVote::Eots(vote.clone())).unwrap();
    let own = ValidatorVote {
        validator: key_bytes_of(&key.public()).to_vec(),
        vote,
        timestamp: record.record().timestamp.into(),
        signature: record.signature(),
    };
    let check = envelope_check(subnet_id);
    assert!(check(&own));
    assert!(!envelope_check(SubnetID::new_root(8))(&own));

    let changes: [(&str, fn(&mut ValidatorVote)); 15] = [
        ("block hash", |v| v.vote.block_hash[0] ^= 1),
        ("height", |v| v.vote.height += 1),
        ("EOTS key", |v| v.vote.pk_eots[0] ^= 1),
        ("EOTS signature", |v| v.vote.signature[1][0] ^= 1),
        ("commitment start", |v| v.vote.start += 1),
        ("proof", |v| v.vote.proof[0][0] ^= 1),
        ("timestamp", |v| v.timestamp += 1),
        ("corrupted signature", |v| v.signature[5] ^= 1),
        ("empty signature", |v| v.signature.clear()),
        ("truncated signature", |v| {
            v.signature.pop();
        }),
        ("extended signature", |v| v.signature.push(0)),
        ("truncated validator key", |v| {
            v.validator.pop();
        }),
        ("extended validator key", |v| v.validator.push(0)),
        ("unparseable validator key", |v| v.validator = vec![255; 33]),
        ("other validator key", |v| {
            v.validator =
                key_bytes_of(&libp2p::identity::Keypair::generate_secp256k1().public()).to_vec()
        }),
    ];
    for (change, apply) in changes {
        let mut changed = own.clone();
        apply(&mut changed);
        assert!(!check(&changed), "accepted a vote with a {change}");
    }
}

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
        Arc::new(Store::default()),
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

    let restarted = EotsTally::new(
        Arc::new(Store {
            stored: StoredState {
                power_tables: vec![(0, vec![([2; 33], 1)])],
                ..StoredState::default()
            },
        }),
        ipc_provider::manager::BtcSubnetManager::new(&parent).unwrap(),
        subnet_id,
        Vec::new(),
        Arc::new(|_| true),
    )
    .unwrap();
    assert!(seed_power_table(&restarted, &Ledger(None)).unwrap());
}
