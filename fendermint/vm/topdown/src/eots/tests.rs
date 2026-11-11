use super::*;
use ipc_eots::testing::TestSigner;
use ipc_eots::EotsVote;
use ipc_ipld_resolver::{SignedVoteRecord, Timestamp, VoteRecord};
use libp2p::identity::Keypair;
use std::str::FromStr;
use std::time::Duration;

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
    fn put_checkpointed(&self, height: u64) -> anyhow::Result<()> {
        self.0.lock().unwrap().checkpointed = height;
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
    fn put_last_cast(&self, height: u64) -> anyhow::Result<()> {
        self.0.lock().unwrap().last_cast = height;
        Ok(())
    }
    fn delete_up_to(&self, height: u64, power_tables_from: u64) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.votes.retain(|v| v.vote.height > height);
        state.blocks.retain(|(h, _)| *h > height);
        state.power_tables.retain(|(h, _)| *h >= power_tables_from);
        Ok(())
    }
    fn load(&self) -> anyhow::Result<StoredState> {
        Ok(self.0.lock().unwrap().clone())
    }
}

fn key(seed: u8) -> ValidatorKey {
    let sk = libp2p::identity::secp256k1::SecretKey::try_from_bytes(&mut [seed; 32]).unwrap();
    ValidatorKey::from(
        libp2p::identity::Keypair::from(libp2p::identity::secp256k1::Keypair::from(sk)).public(),
    )
}

/// A tally over a memory store, with no recorded state read: its state is set directly
fn tally(keys: &[(ValidatorKey, Weight)]) -> EotsTally {
    tally_with_provider("http://127.0.0.1:1", keys)
}

fn tally_with_provider(provider_url: &str, keys: &[(ValidatorKey, Weight)]) -> EotsTally {
    fvm_shared::address::set_current_network(fvm_shared::address::Network::Testnet);
    let subnet_id = SubnetID::from_str("/b4/t410fhor637l2pmjle6whfq7go5upmf74qg6dbr4uzei").unwrap();
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
        Arc::new(MemoryStore::default()),
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
    inner.monitor_read = true;
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
fn votes_make_blocks_final_in_order() {
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
    assert_eq!(
        t.add_vote(vote_of(&t, &k1, &s1, 1, [1; 32]), None).unwrap(),
        VoteOutcome::New
    );
    assert_eq!(t.lock().known_final(), 0);
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
    assert_eq!(t.lock().votes[&2].len(), 3);
    t.set_recorded_final(2).unwrap();
    assert!(t.lock().votes.range(..=2).next().is_none());
    assert!(t.lock().chain.range(..=2).next().is_none());
    t.lock().checkpoint = Some((3, [3; 32], false));
    assert_eq!(
        t.add_vote(vote_of(&t, &k2, &s2, 3, [3; 32]), None),
        Err(VoteError::Checkpointed(3, 3))
    );
    assert_eq!(
        t.add_vote(vote_of(&t, &k2, &s2, 4, [4; 32]), None).unwrap(),
        VoteOutcome::New
    );
    assert_eq!(t.lock().known_final(), 3);
}

#[test]
fn a_vote_at_or_below_the_recorded_subnet_final_height_is_refused() {
    let k1 = key(1);
    let t = tally(&[(k1.clone(), 1)]);
    let s1 = signer_for(&t, &k1, 11);
    for h in 1..=3 {
        t.add_block(h, [h as u8; 32]).unwrap();
    }
    t.set_recorded_final(2).unwrap();
    for h in 1..=2 {
        assert_eq!(
            t.add_vote(vote_of(&t, &k1, &s1, h, [h as u8; 32]), None),
            Err(VoteError::Final(h, 2))
        );
    }
    assert!(t.lock().votes.is_empty());
    assert_eq!(
        t.add_vote(vote_of(&t, &k1, &s1, 3, [3; 32]), None),
        Ok(VoteOutcome::New)
    );
}

#[test]
fn power_tables_superseded_at_the_subnet_final_height_are_deleted() {
    let (k1, k2) = (key(1), key(2));
    let t = tally(&[(k1.clone(), 1)]);
    t.set_power_table(5, vec![(k1, 1), (k2.clone(), 1)]).unwrap();
    t.set_power_table(8, vec![(k2, 1)]).unwrap();
    // The table of 5 is in force at 7, the first height above the subnet final height
    t.set_recorded_final(6).unwrap();
    assert_eq!(
        t.lock().power_tables.keys().copied().collect::<Vec<_>>(),
        [5, 8]
    );
    let mut stored: Vec<u64> = t
        .store
        .load()
        .unwrap()
        .power_tables
        .iter()
        .map(|(h, _)| *h)
        .collect();
    stored.sort();
    assert_eq!(stored, [5, 8]);
}

#[test]
fn the_final_height_stays_one_below_the_first_conflict() {
    assert_eq!(below_conflict(10, None), 10);
    assert_eq!(below_conflict(10, Some(5)), 4);
    assert_eq!(below_conflict(4, Some(5)), 4);
    assert_eq!(below_conflict(3, Some(5)), 3);
}

#[test]
fn a_subnet_block_executed_again_after_its_votes_became_final_is_applied_without_error() {
    let (k1, k2) = (key(1), key(2));
    let (s1, s2) = (TestSigner::new(11, 1, 20), TestSigner::new(12, 1, 20));
    let (t, next) = node_data(&k1, &k2, &s1, &s2);
    let agreed = AgreedState {
        checkpointed: 0,
        killed: false,
        bitcoin_height: 100,
    };
    assert!(t.apply_proposal(&next, &agreed).unwrap().is_empty());
    assert_eq!(t.recorded_final_height(), 3);
    let reloaded = EotsTally::new(
        t.store.clone(),
        t.provider.clone(),
        t.subnet_id.clone(),
        vec![],
        Arc::new(|_| true),
    )
    .unwrap();
    assert!(reloaded.apply_proposal(&next, &agreed).unwrap().is_empty());
    assert_eq!(reloaded.recorded_final_height(), 3);
}

#[test]
fn a_restarted_validator_casts_no_vote_at_or_below_the_greatest_height_it_voted_at() {
    let k1 = key(1);
    let t = tally(&[(k1.clone(), 1)]);
    let _ = signer_for(&t, &k1, 11);
    for h in 1..=5 {
        t.add_block(h, [h as u8; 32]).unwrap();
    }
    for h in 1..=3 {
        t.mark_cast(h);
    }
    let r = EotsTally::new(
        t.store.clone(),
        t.provider.clone(),
        t.subnet_id.clone(),
        vec![],
        Arc::new(|_| true),
    )
    .unwrap();
    r.lock().monitor_read = true;
    assert_eq!(r.votes_to_cast(5), vec![(4, [4; 32]), (5, [5; 32])]);
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
    assert_eq!(t.agreed_checkpointed_height(), 0);
    assert!(t.apply_proposal(&next, &checkpointed).unwrap().is_empty());
    assert_eq!(t.agreed_checkpointed_height(), 2);
    assert_eq!(t.recorded_final_height(), 3);
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
    assert!(reloaded.lock().chain.is_empty());
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

/// The recorded EOTS keys (validator x-only key, EOTS key, commitment, Bitcoin
/// height), and the checkpoint it answers with.
#[derive(Default)]
struct FakeMonitor {
    keys: Vec<([u8; 32], [u8; 32], Option<EotsNonceCommitment>, u64)>,
    checkpoint: Option<(u64, [u8; 32])>,
}

/// The recorded key and commitment of `signer` for `key`.
fn recorded(
    key: &ValidatorKey,
    signer: &ipc_eots::testing::TestSigner,
    bitcoin_height: u64,
) -> ([u8; 32], [u8; 32], Option<EotsNonceCommitment>, u64) {
    (
        x_only(&key_bytes(key)),
        signer.pk_eots,
        Some(signer.commitment),
        bitcoin_height,
    )
}

/// The `after_block_height` and `max_block_height` of every request for recorded EOTS state.
type Requests = Arc<Mutex<Vec<(Option<u64>, Option<u64>)>>>;

/// A fake source that answers `geteotsrecords` from `monitor`, with the records of the Bitcoin
/// heights above `after_block_height` and at or below the answer's height. A Bitcoin height
/// above `processed` is refused and counted in `refused`.
async fn fake_provider(
    listener: tokio::net::TcpListener,
    monitor: Arc<FakeMonitor>,
    processed: Arc<std::sync::atomic::AtomicU64>,
    refused: Arc<std::sync::atomic::AtomicU64>,
    requests: Requests,
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
        assert_eq!(request["method"], "geteotsrecords");
        let params = &request["params"];
        let after = params["after_block_height"].as_u64();
        let asked = params["max_block_height"].as_u64();
        requests.lock().unwrap().push((after, asked));
        let at = match asked {
            Some(asked) if asked > processed.load(Ordering::SeqCst) => {
                refused.fetch_add(1, Ordering::SeqCst);
                let answer = serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {
                    "code": -32004,
                    "message": format!("the monitor has not processed Bitcoin height {asked} yet")
                }});
                respond(&mut stream, answer).await;
                continue;
            }
            Some(asked) => asked,
            None => processed.load(Ordering::SeqCst),
        };
        let mut by_key: BTreeMap<([u8; 32], [u8; 32]), Vec<serde_json::Value>> = BTreeMap::new();
        for (v, pk, c, h) in &monitor.keys {
            if after.is_some_and(|a| *h <= a) || *h > at {
                continue;
            }
            let commitments = by_key.entry((*v, *pk)).or_default();
            if let Some(c) = c {
                commitments.push(serde_json::json!({
                    "start": c.start,
                    "count": c.count,
                    "root": hex::encode(c.root),
                    "block_height": h
                }));
            }
        }
        let eots_keys: Vec<serde_json::Value> = by_key
            .into_iter()
            .map(|((v, pk), commitments)| {
                serde_json::json!({
                    "pubkey": hex::encode(v),
                    "pk_eots": hex::encode(pk),
                    "commitments": commitments
                })
            })
            .collect();
        let checkpoint = monitor.checkpoint.filter(|_| at >= 100).map_or(
            serde_json::Value::Null,
            |(h, hash)| {
                serde_json::json!({
                    "checkpoint_height": h,
                    "checkpoint_hash": hex::encode(hash),
                    "block_height": 100,
                    "is_kill_checkpoint": false
                })
            },
        );
        let result = serde_json::json!({
            "block_height": at,
            "eots_keys": eots_keys,
            "checkpoint": checkpoint
        });
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

/// A fake source on a free port, serving the records of `monitor` from `processed` on.
async fn serve(monitor: FakeMonitor, processed: u64) -> (String, tokio::task::JoinHandle<()>) {
    serve_at(
        monitor,
        Arc::new(std::sync::atomic::AtomicU64::new(processed)),
        Requests::default(),
    )
    .await
}

/// A fake source on a free port, serving the records of `monitor` at the Bitcoin height `processed` holds.
async fn serve_at(
    monitor: FakeMonitor,
    processed: Arc<std::sync::atomic::AtomicU64>,
    requests: Requests,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(fake_provider(
        listener,
        Arc::new(monitor),
        processed,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        requests,
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
    t.lock().monitor_read = true;
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

/// The tally of a node that restarts on the data of `t`, with `provider` as its source of recorded state.
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
    // not read the recorded state yet
    assert!(r.has_stored_power_tables());
    assert_eq!(r.recorded_final_height(), 2);
    assert!(!r.lock().monitor_read);
    // The store holds the votes above the subnet final height only
    assert_eq!(r.lock().pending.len(), 2);
    // The next block carries the votes of height 3: executed as on every other node
    r.execute_proposal(&next).await.unwrap();
    assert_eq!(r.recorded_final_height(), 3);
    assert_eq!(r.final_height(), 3);
    // The stored votes were checked at the read of the recorded state, and the votes at or below the
    // new subnet final height are deleted
    assert!(r.lock().pending.is_empty());
    assert!(r.lock().votes.is_empty());
    assert!(r.store.load().unwrap().votes.is_empty());
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
    // This node never recorded the EOTS key of k2
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
    // The recorded final height follows the block
    assert_eq!(r.recorded_final_height(), 3);
    assert!(r.lock().votes.is_empty());
    server.abort();
}

/// The log lines of a test, as a `tracing` writer
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

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
    // k2 registered s2's EOTS key but signs its vote at height 3 with s3
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
    // The execution of the committed block goes on without the vote
    r.execute_proposal(&next).await.unwrap();
    assert_eq!(r.recorded_final_height(), 3);
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
async fn a_node_that_read_its_monitor_while_it_was_behind_accepts_an_honest_proposal() {
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
    // This node read the recorded state of Bitcoin height 99 only, before k2's
    // key and commitment were recorded
    {
        let mut inner = r.lock();
        inner.eots_keys.insert(x_only(&key_bytes(&k1)), s1.pk_eots);
        inner
            .commitments
            .insert(s1.pk_eots, vec![(s1.commitment, 100)]);
        inner.monitor_read = true;
        inner.monitor_read_for = 99;
    }
    // The decision at Bitcoin height 100 reads the recorded state again, so the honest proposal
    // that carries k2's vote passes as on every other node
    let agreed = r.agreed_state(100).await.unwrap();
    assert_eq!(r.lock().monitor_read_for, 100);
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
    // A port nobody listens on yet: the recorded state is unreachable
    let port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let t = tally_with_provider(&format!("http://127.0.0.1:{port}"), &[(key(1), 1)]);
    assert!(t.agreed_state(100).await.is_err(), "unreachable");

    // The recorded state comes up one Bitcoin block behind, then catches up
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
            fake_provider(
                listener,
                Arc::new(monitor),
                processed,
                refused,
                Requests::default(),
            )
            .await;
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
    let agreed =
        ask_until_answered("the agreed state", retry, timeout, || t.agreed_state(100)).await;
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
    assert!(
        refused.load(Ordering::SeqCst) >= 1,
        "the lagging monitor refused"
    );
    assert!(catch_up.is_finished());

    // Once the recorded state is there, the plain question has the same answer at once
    let again = t.agreed_state(100).await.unwrap();
    assert_eq!(again, agreed);
    // A height above the recorded state's is still refused, not answered with an older checkpoint
    assert!(t.agreed_state(101).await.is_err());
    server.abort();
}

#[tokio::test]
async fn the_monitor_is_read_once_and_then_only_above_the_held_bitcoin_height() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let (k1, k2) = (key(1), key(2));
    let (s1, s2) = (TestSigner::new(11, 1, 20), TestSigner::new(12, 1, 20));
    let processed = Arc::new(AtomicU64::new(100));
    let requests = Requests::default();
    let (url, server) = serve_at(
        FakeMonitor {
            keys: vec![recorded(&k1, &s1, 100), recorded(&k2, &s2, 101)],
            checkpoint: None,
        },
        processed.clone(),
        requests.clone(),
    )
    .await;
    let t = tally_with_provider(&url, &[(k1.clone(), 1), (k2.clone(), 1)]);
    t.read_monitor().await.unwrap();
    t.read_monitor().await.unwrap();
    assert_eq!(*requests.lock().unwrap(), [(None, None), (Some(100), None)]);
    assert!(!t.lock().eots_keys.contains_key(&x_only(&key_bytes(&k2))));
    processed.store(101, Ordering::SeqCst);
    t.read_monitor().await.unwrap();
    {
        let inner = t.lock();
        assert_eq!(inner.monitor_read_for, 101);
        assert_eq!(
            inner.eots_keys.get(&x_only(&key_bytes(&k2))),
            Some(&s2.pk_eots)
        );
        assert_eq!(inner.commitments[&s1.pk_eots].len(), 1);
    }
    server.abort();
}

#[tokio::test]
async fn the_agreed_state_comes_from_memory_while_the_held_records_cover_the_parent_finality() {
    use std::sync::atomic::AtomicU64;
    let k1 = key(1);
    let s1 = TestSigner::new(11, 1, 20);
    let requests = Requests::default();
    // The checkpoint is recorded at Bitcoin height 100, the recorded state reaches 101
    let (url, server) = serve_at(
        FakeMonitor {
            keys: vec![recorded(&k1, &s1, 100)],
            checkpoint: Some((42, [7; 32])),
        },
        Arc::new(AtomicU64::new(101)),
        requests.clone(),
    )
    .await;
    let t = tally_with_provider(&url, &[(k1, 1)]);
    t.read_monitor().await.unwrap();
    requests.lock().unwrap().clear();
    for bitcoin_height in [100, 101] {
        assert_eq!(
            t.agreed_state(bitcoin_height).await.unwrap().checkpointed,
            42
        );
    }
    assert!(requests.lock().unwrap().is_empty());
    // Below the Bitcoin height of the held checkpoint: one call, which changes nothing held
    assert_eq!(t.agreed_state(99).await.unwrap().checkpointed, 0);
    assert_eq!(*requests.lock().unwrap(), [(Some(101), Some(99))]);
    assert_eq!(t.lock().monitor_read_for, 101);
    assert_eq!(t.lock().checkpointed(), 42);
    // Above the recorded Bitcoin height: refused
    assert!(t.agreed_state(102).await.is_err());
    server.abort();
}

#[tokio::test]
async fn a_vote_waiting_for_its_commitment_is_accepted_at_the_read_that_brings_it() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let (k1, k2, k3) = (key(1), key(2), key(3));
    let s1 = TestSigner::new(11, 1, 20);
    // k2 joined with the heights 1 to 20, its next range from 21 is recorded at 101
    let (s2, s2_next) = (TestSigner::new(12, 1, 20), TestSigner::new(12, 21, 20));
    let s3 = TestSigner::new(13, 1, 20);
    let processed = Arc::new(AtomicU64::new(100));
    let (url, server) = serve_at(
        FakeMonitor {
            keys: vec![
                recorded(&k1, &s1, 100),
                recorded(&k2, &s2, 100),
                recorded(&k2, &s2_next, 101),
                recorded(&k3, &s3, 101),
            ],
            checkpoint: None,
        },
        processed.clone(),
        Requests::default(),
    )
    .await;
    let t = tally_with_provider(&url, &[(k1, 1), (k2.clone(), 1), (k3.clone(), 1)]);
    t.read_monitor().await.unwrap();
    for h in 1..=25 {
        t.add_block(h, block(h)).unwrap();
    }
    let vote = vote_of(&t, &k2, &s2_next, 25, block(25));
    assert_eq!(t.add_vote(vote.clone(), None), Ok(VoteOutcome::Pending));
    // A key that is not recorded yet is rejected, not kept
    assert_eq!(
        t.add_vote(vote_of(&t, &k3, &s3, 5, block(5)), None),
        Err(VoteError::WrongEotsKey)
    );
    processed.store(101, Ordering::SeqCst);
    t.read_monitor().await.unwrap();
    let inner = t.lock();
    assert_eq!(inner.commitments[&s2.pk_eots].len(), 2);
    assert!(inner.pending.is_empty());
    assert_eq!(inner.votes[&25][&s2.pk_eots], [vote]);
    drop(inner);
    server.abort();
}

/// A tally holding the chain's blocks 1 to 25 and `k`'s EOTS key with the commitment of
/// `s`, before any read of the recorded state.
fn awaiting_tally(k: &ValidatorKey, s: &TestSigner) -> EotsTally {
    let t = tally(&[(k.clone(), 1)]);
    t.lock().eots_keys.insert(x_only(&key_bytes(k)), s.pk_eots);
    t.lock()
        .commitments
        .insert(s.pk_eots, vec![(s.commitment, 100)]);
    for h in 1..=25 {
        t.add_block(h, block(h)).unwrap();
    }
    t
}

#[test]
fn a_vote_waiting_for_its_commitment_is_kept_before_the_first_read_of_the_monitor() {
    let k = key(2);
    let (s, s_next) = (TestSigner::new(12, 1, 20), TestSigner::new(12, 21, 20));
    let t = awaiting_tally(&k, &s);
    assert!(!t.lock().monitor_read);
    assert_eq!(
        t.add_vote(vote_of(&t, &k, &s_next, 25, block(25)), None),
        Ok(VoteOutcome::Pending)
    );
    assert_eq!(t.lock().pending.len(), 1);
}

#[test]
fn one_waiting_vote_is_kept_per_eots_key_and_height() {
    let k = key(2);
    let (s, s_next) = (TestSigner::new(12, 1, 20), TestSigner::new(12, 21, 20));
    let t = awaiting_tally(&k, &s);
    for hash in [block(25), [0xA5; 32]] {
        assert_eq!(
            t.add_vote(vote_of(&t, &k, &s_next, 25, hash), None),
            Ok(VoteOutcome::Pending)
        );
    }
    assert_eq!(t.lock().pending.len(), 1);
    assert_eq!(
        t.add_vote(vote_of(&t, &k, &s_next, 24, block(24)), None),
        Ok(VoteOutcome::Pending)
    );
    assert_eq!(t.lock().pending.len(), 2);
}

#[test]
fn a_vote_naming_a_recorded_commitment_that_does_not_cover_its_height_is_not_kept() {
    let k = key(2);
    let s = TestSigner::new(12, 1, 20);
    let t = awaiting_tally(&k, &s);
    // The recorded commitment's EOTS key and start, with more heights
    let longer = TestSigner::new(12, 1, 30);
    assert_eq!(
        t.add_vote(vote_of(&t, &k, &longer, 25, block(25)), None),
        Err(VoteError::NotCommitted(25))
    );
    assert!(t.lock().pending.is_empty());
}

#[test]
fn a_vote_whose_nonce_range_starts_above_its_height_is_not_kept() {
    let k = key(2);
    let (s, s_next) = (TestSigner::new(12, 1, 20), TestSigner::new(12, 21, 20));
    let t = awaiting_tally(&k, &s);
    let mut early = vote_of(&t, &k, &s_next, 25, block(25));
    early.vote.height = 15;
    assert_eq!(t.add_vote(early, None), Err(VoteError::NotCommitted(15)));
    assert!(t.lock().pending.is_empty());
}

#[tokio::test]
async fn a_waiting_vote_at_or_below_a_newly_read_checkpoint_is_dropped() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let k = key(2);
    let (s, s_next) = (TestSigner::new(12, 1, 20), TestSigner::new(12, 21, 20));
    let processed = Arc::new(AtomicU64::new(99));
    let (url, server) = serve_at(
        FakeMonitor {
            keys: vec![recorded(&k, &s, 99)],
            checkpoint: Some((25, block(25))),
        },
        processed.clone(),
        Requests::default(),
    )
    .await;
    let t = tally_with_provider(&url, &[(k.clone(), 1)]);
    t.read_monitor().await.unwrap();
    for h in 1..=25 {
        t.add_block(h, block(h)).unwrap();
    }
    assert_eq!(
        t.add_vote(vote_of(&t, &k, &s_next, 25, block(25)), None),
        Ok(VoteOutcome::Pending)
    );
    // The checkpoint of height 25 is recorded at Bitcoin height 100
    processed.store(100, Ordering::SeqCst);
    t.read_monitor().await.unwrap();
    assert_eq!(t.lock().checkpointed(), 25);
    assert!(t.lock().pending.is_empty());
    server.abort();
}

#[tokio::test]
async fn both_votes_of_a_double_vote_stored_before_a_restart_are_held_after_the_first_read() {
    let (k1, k2) = (key(1), key(2));
    let t = tally(&[(k1.clone(), 1), (k2.clone(), 1)]);
    let (s1, s2) = (signer_for(&t, &k1, 11), signer_for(&t, &k2, 12));
    let (url, server) = serve(
        FakeMonitor {
            keys: vec![recorded(&k1, &s1, 100), recorded(&k2, &s2, 100)],
            checkpoint: None,
        },
        100,
    )
    .await;
    t.add_block(1, block(1)).unwrap();
    t.add_vote(vote_of(&t, &k1, &s1, 1, block(1)), None)
        .unwrap();
    assert_eq!(
        t.add_vote(vote_of(&t, &k1, &s1, 1, [0xA1; 32]), None),
        Ok(VoteOutcome::Evidence)
    );
    assert_eq!(t.store.load().unwrap().votes.len(), 2);
    let r = restarted(&t, &url);
    assert_eq!(r.lock().pending.len(), 2);
    r.read_monitor().await.unwrap();
    assert!(r.lock().pending.is_empty());
    assert_eq!(r.evidence().len(), 1);
    server.abort();
}

#[tokio::test]
async fn two_reads_of_the_monitor_run_one_after_the_other() {
    let k1 = key(1);
    let s1 = TestSigner::new(11, 1, 20);
    let requests = Requests::default();
    let (url, server) = serve_at(
        FakeMonitor {
            keys: vec![recorded(&k1, &s1, 100)],
            checkpoint: None,
        },
        Arc::new(std::sync::atomic::AtomicU64::new(100)),
        requests.clone(),
    )
    .await;
    let t = tally_with_provider(&url, &[(k1, 1)]);
    let (first, second) = tokio::join!(t.read_monitor(), t.read_monitor());
    first.unwrap();
    second.unwrap();
    // The second read asks only above the Bitcoin height of the first one's answer
    assert_eq!(*requests.lock().unwrap(), [(None, None), (Some(100), None)]);
    server.abort();
}

#[test]
fn the_size_of_carried_votes_is_their_encoded_length() {
    let (t, signers) = three_voters(1);
    vote_all(&t, &signers, 1, [1; 32]);
    let data = t.proposal(&agreed()).unwrap();
    assert_eq!(data.votes.len(), 3);
    let sizes: usize = data.votes.iter().map(vote_size).sum();
    // The encoded list adds one byte, its length
    assert_eq!(
        fvm_ipld_encoding::to_vec(&data.votes).unwrap().len(),
        sizes + 1
    );
}

#[test]
fn the_first_conflict_is_the_lowest_height_with_a_second_final_block() {
    let (t, signers) = three_voters(3);
    for h in 1..=3 {
        vote_all(&t, &signers, h, [h as u8; 32]);
    }
    assert_eq!(t.first_conflict(), None);
    vote_all(&t, &signers, 3, [0xA3; 32]);
    assert_eq!(t.first_conflict(), Some(3));
    vote_all(&t, &signers, 2, [0xA2; 32]);
    assert_eq!(t.first_conflict(), Some(2));
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
    // Blocks carried the votes up to height 1
    t.lock().recorded_final = 1;
    assert_eq!(t.final_height(), 1);
    let other = [0xA1u8; 32];
    // Two double votes for another block at height 2: below the threshold, finality stays
    assert_eq!(
        vote_all(&t, &signers[..2], 2, other),
        [VoteOutcome::Evidence, VoteOutcome::Evidence]
    );
    assert_eq!(t.final_height(), 1);
    assert_eq!(t.status().final_height, 1);
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
    // The recorded final height is not the node's to change: block execution follows it
    // past the conflict, the served one stays
    t.set_recorded_final(3).unwrap();
    assert_eq!(t.recorded_final_height(), 3);
    assert_eq!(t.final_height(), 1);
    assert_eq!(t.status().recorded_final_height, 3);
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
            inner.monitor_read = true;
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

    fn assert_rejected_and_live(&self, bad: ValidatorVote) {
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

macro_rules! rejected_vote {
    ($name:ident, $f:ident, $v:ident, $mutation:block) => {
        #[test]
        fn $name() {
            let $f = Fixture::new();
            let mut $v = $f.signers[3].vote(&$f.txid, 1, block(1));
            $mutation
            let bad = $f.wrap(3, $v);
            $f.assert_rejected_and_live(bad);
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
    v = TestSigner::new(19, 1, 4).vote(&f.txid, v.height, v.block_hash);
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

#[test]
fn altered_envelopes_leave_every_receiver_live() {
    let f = Fixture::new();
    let original = f.vote(3, 1, block(1));
    let mut bad = original.clone();
    bad.signature.clear();
    f.assert_rejected_and_live(bad);
    let mut bad = original.clone();
    bad.timestamp ^= 1;
    f.assert_rejected_and_live(bad);
    let mut bad = original;
    bad.validator = vec![0; 33];
    f.assert_rejected_and_live(bad);
}

#[test]
fn a_registered_validator_without_power_cannot_pollute_a_proposal() {
    let f = Fixture::new();
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
}

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

#[test]
fn a_proposal_with_more_than_one_evidence_of_one_eots_key_is_rejected() {
    let f = Fixture::new();
    let (node, _) = f.fresh_node();
    for height in 1..=2 {
        node.add_block(height, block(height)).unwrap();
    }
    let evidence: Vec<_> = (1..=2)
        .map(|h| (f.vote(3, h, block(h)), f.vote(3, h, [99; 32])))
        .collect();
    let data = |evidence: Vec<(ValidatorVote, ValidatorVote)>| EotsFinalityData {
        bitcoin_height: BITCOIN_HEIGHT,
        votes: f.quorum(1),
        evidence,
    };
    node.check_proposal(&data(evidence[..1].to_vec()), &agreed())
        .unwrap();
    let refused = node
        .check_proposal(&data(evidence.clone()), &agreed())
        .unwrap_err();
    assert!(
        refused.contains("more than one carried evidence"),
        "{refused}"
    );
    // A proposer that holds both double votes carries one of them
    for (a, b) in evidence {
        node.add_vote(a, None).unwrap();
        node.add_vote(b, None).unwrap();
    }
    assert_eq!(node.evidence().len(), 2);
    assert_eq!(node.proposal(&agreed()).unwrap().evidence.len(), 1);
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
    let (receiver, _) = f.fresh_node();
    for height in 1..=2 {
        receiver.add_block(height, block(height)).unwrap();
    }
    // Other local gossip and recorded-head views must not affect the proposal decision.
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

#[test]
fn future_commitment_gossip_cannot_pollute_an_honest_block() {
    pollution_fixture(false, false);
}

#[test]
fn future_commitment_evidence_cannot_pollute_an_honest_block() {
    pollution_fixture(true, false);
}

#[test]
fn checkpointed_evidence_cannot_pollute_an_honest_block() {
    pollution_fixture(true, true);
}

async fn committed_bad_vote(omit_commitment: bool) {
    let f = Fixture::new();
    let monitor = FakeMonitor {
        keys: f
            .keys
            .iter()
            .zip(&f.signers)
            .enumerate()
            .map(|(i, (key, signer))| {
                (
                    x_only(&key_bytes_of(&key.public())),
                    signer.pk_eots,
                    (!(omit_commitment && i == 3)).then_some(signer.commitment),
                    BITCOIN_HEIGHT,
                )
            })
            .collect(),
        checkpoint: None,
    };
    let (url, server) = serve(monitor, u64::MAX).await;
    let logs = CapturedLog::default();
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
    // The commitments held in memory match the recorded ones
    let monitored_node = |store: Arc<MemoryStore>| {
        let node = f.node(store, &url);
        if omit_commitment {
            node.lock().commitments.remove(&f.signers[3].pk_eots);
        }
        node
    };
    let store = Arc::new(MemoryStore::default());
    let node = monitored_node(store.clone());
    node.add_block(1, block(1)).unwrap();
    // First delivery, repeat delivery, restart/replay, and a node syncing only from blocks.
    for pass in 0..4 {
        let current = match pass {
            0 | 1 => node.clone(),
            2 => monitored_node(store.clone()),
            _ => {
                let syncing = monitored_node(Arc::new(MemoryStore::default()));
                syncing.add_block(1, block(1)).unwrap();
                syncing
            }
        };
        let start = logs.0.lock().unwrap().len();
        let final_before = current.recorded_final_height() >= 1;
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
        // Votes at heights already final at the node are not checked again
        if !final_before {
            let text = String::from_utf8(logs.0.lock().unwrap()[start..].to_vec()).unwrap();
            assert!(
                text.contains("ERROR") && text.contains("carried vote rejected"),
                "each execution must log the rejected vote at ERROR: {text}"
            );
        }
        let clean = f.node(Arc::new(MemoryStore::default()), &url);
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
    server.abort();
}

#[tokio::test]
async fn a_committed_vote_with_a_bad_signature_is_dropped_on_delivery_replay_and_sync() {
    committed_bad_vote(false).await;
}

#[tokio::test]
async fn a_committed_vote_without_a_monitor_commitment_is_dropped_on_delivery_replay_and_sync() {
    committed_bad_vote(true).await;
}
