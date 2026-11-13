use super::testing::*;
use super::*;

fn bytes<const N: usize>(hex: &str) -> [u8; N] {
    hex::decode(hex).unwrap().try_into().unwrap()
}

fn reference_vote_message(subnet: &[u8; 20], height: u64, hash: &[u8; 32]) -> [u8; 32] {
    let tag = Sha256::digest(b"IPC/vote");
    let mut h = Sha256::new();
    h.update(tag);
    h.update(tag);
    h.update(subnet);
    h.update(height.to_le_bytes());
    h.update(hash);
    h.finalize().into()
}

fn reference_tree_hash(leaves: &[[u8; 32]]) -> [u8; 32] {
    assert!(!leaves.is_empty());
    let mut h = Sha256::new();
    if leaves.len() == 1 {
        h.update([0]);
        h.update(leaves[0]);
    } else {
        let split = leaves.len().next_power_of_two() / 2;
        h.update([1]);
        h.update(reference_tree_hash(&leaves[..split]));
        h.update(reference_tree_hash(&leaves[split..]));
    }
    h.finalize().into()
}

fn reference_path(leaves: &[[u8; 32]], index: usize) -> Vec<[u8; 32]> {
    if leaves.len() == 1 {
        return vec![];
    }
    let split = leaves.len().next_power_of_two() / 2;
    let (mut path, sibling) = if index < split {
        (
            reference_path(&leaves[..split], index),
            reference_tree_hash(&leaves[split..]),
        )
    } else {
        (
            reference_path(&leaves[split..], index - split),
            reference_tree_hash(&leaves[..split]),
        )
    };
    path.push(sibling);
    path
}

fn reference_signer(seed: u8, start: u64, count: u32) -> TestSigner {
    let mut signer = TestSigner::new(seed, start, count);
    signer.commitment.root = reference_tree_hash(&signer.leaves);
    signer
}

fn reference_vote(
    signer: &TestSigner,
    subnet: &[u8; 20],
    height: u64,
    block_hash: [u8; 32],
) -> EotsVote {
    let index = (height - signer.commitment.start) as usize;
    let message = reference_vote_message(subnet, height, &block_hash);
    let (_, signature) = sign_with_nonce(signer.d, signer.nonces[index], &message);
    EotsVote {
        pk_eots: signer.pk_eots,
        height,
        block_hash,
        signature: EotsVote::signature_from_bytes(&signature),
        start: signer.commitment.start,
        proof: reference_path(&signer.leaves, index),
    }
}

#[test]
fn audit_paths_verify_for_every_size_and_index() {
    for size in 1..40usize {
        let leaves: Vec<[u8; 32]> = (0..size).map(|i| [i as u8; 32]).collect();
        let root = root_of(&leaves);
        for index in 0..size {
            let path = path_of(&leaves, index);
            assert!(verify_audit_path(
                &leaves[index],
                index as u64,
                size as u64,
                &path,
                &root
            ));
            assert!(!verify_audit_path(
                &[99u8; 32],
                index as u64,
                size as u64,
                &path,
                &root
            ));
            if !path.is_empty() {
                assert!(!verify_audit_path(
                    &leaves[index],
                    index as u64,
                    size as u64,
                    &path[..path.len() - 1],
                    &root
                ));
            }
        }
    }
}

#[test]
fn a_vote_with_a_committed_nonce_verifies_and_tampering_is_rejected() {
    let txid20 = [7u8; 20];
    for seed in 1u8..6 {
        let signer = TestSigner::new(seed, 10, 5);
        let commitment = signer.commitment;
        let vote = signer.vote(&txid20, 12, [3u8; 32]);
        verify_vote(&txid20, &vote, &commitment).unwrap();
        let mut other_block = vote.clone();
        other_block.block_hash = [4u8; 32];
        assert_eq!(
            verify_vote(&txid20, &other_block, &commitment),
            Err(EotsError::InvalidSignature)
        );
        assert_eq!(
            verify_vote(&[8u8; 20], &vote, &commitment),
            Err(EotsError::InvalidSignature)
        );
        let mut wrong_start = vote.clone();
        wrong_start.start = 9;
        assert!(matches!(
            verify_vote(&txid20, &wrong_start, &commitment),
            Err(EotsError::HeightNotCommitted { .. })
        ));
        let mut wrong_proof = vote.clone();
        wrong_proof.proof[0][0] ^= 1;
        assert_eq!(
            verify_vote(&txid20, &wrong_proof, &commitment),
            Err(EotsError::InvalidProof)
        );
        let mut out_of_range = vote.clone();
        out_of_range.height = 15;
        assert!(matches!(
            verify_vote(&txid20, &out_of_range, &commitment),
            Err(EotsError::HeightNotCommitted { .. })
        ));
        assert!(is_double_vote(&vote, &other_block));
        assert!(!is_double_vote(&vote, &vote));
        // A second vote at the same height for another block also verifies: the evidence
        let second = signer.vote(&txid20, 12, [4u8; 32]);
        verify_vote(&txid20, &second, &commitment).unwrap();
        assert_eq!(second.nonce(), vote.nonce());
    }
}

#[test]
fn tagged_hash_matches_bip340_definition() {
    let tag_hash = Sha256::digest(b"x");
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher.update(b"y");
    let expected: [u8; 32] = hasher.finalize().into();
    assert_eq!(tagged_sha256(b"x", b"y"), expected);
}

/// BIP340 test vectors 3 to 14: https://github.com/bitcoin/bips/blob/master/bip-0340/test-vectors.csv
#[test]
fn bip340_test_vectors() {
    const KEY: &str = "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659";
    const MESSAGE: &str = "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89";
    let vectors = [
        (3, "25D1DFF95105F5253C4022F628A996AD3A0D95FBF21D468A1B33F8C160D8F517", "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", "7EB0509757E246F19449885651611CB965ECC1A187DD51B64FDA1EDC9637D5EC97582B9CB13DB3933705B32BA982AF5AF25FD78881EBB32771FC5922EFC66EA3", Ok(())),
        (4, "D69C3509BB99E412E68B0FE8544E72837DFA30746D8BE2AA65975F29D22DC7B9", "4DF3C3F68FCC83B27E9D42C90431A72499F17875C81A599B566C9889B9696703", "00000000000000000000003B78CE563F89A0ED9414F5AA28AD0D96D6795F9C6376AFB1548AF603B3EB45C9F8207DEE1060CB71C04E80F593060B07D28308D7F4", Ok(())),
        (5, "EEFDEA4CDB677750A420FEE807EACF21EB9898AE79B9768766E4FAA04A2D4A34", MESSAGE, "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B", Err(EotsError::InvalidKey)),
        (6, KEY, MESSAGE, "FFF97BD5755EEEA420453A14355235D382F6472F8568A18B2F057A14602975563CC27944640AC607CD107AE10923D9EF7A73C643E166BE5EBEAFA34B1AC553E2", Err(EotsError::InvalidSignature)),
        (7, KEY, MESSAGE, "1FA62E331EDBC21C394792D2AB1100A7B432B013DF3F6FF4F99FCB33E0E1515F28890B3EDB6E7189B630448B515CE4F8622A954CFE545735AAEA5134FCCDB2BD", Err(EotsError::InvalidSignature)),
        (8, KEY, MESSAGE, "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E177769961764B3AA9B2FFCB6EF947B6887A226E8D7C93E00C5ED0C1834FF0D0C2E6DA6", Err(EotsError::InvalidSignature)),
        (9, KEY, MESSAGE, "0000000000000000000000000000000000000000000000000000000000000000123DDA8328AF9C23A94C1FEECFD123BA4FB73476F0D594DCB65C6425BD186051", Err(EotsError::InvalidSignature)),
        (10, KEY, MESSAGE, "00000000000000000000000000000000000000000000000000000000000000017615FBAF5AE28864013C099742DEADB4DBA87F11AC6754F93780D5A1837CF197", Err(EotsError::InvalidSignature)),
        (11, KEY, MESSAGE, "4A298DACAE57395A15D0795DDBFD1DCB564DA82B0F269BC70A74F8220429BA1D69E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B", Err(EotsError::InvalidSignature)),
        (12, KEY, MESSAGE, "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F69E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B", Err(EotsError::InvalidSignature)),
        (13, KEY, MESSAGE, "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E177769FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141", Err(EotsError::InvalidSignature)),
        (14, "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC30", MESSAGE, "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B", Err(EotsError::InvalidKey)),
    ];
    for (index, key, message, signature, expected) in vectors {
        assert_eq!(
            verify_bip340(&bytes(key), &bytes(message), &bytes(signature)),
            expected,
            "vector {index}"
        );
    }
}

#[test]
fn verification_commitment_keeps_large_heights_and_excludes_both_neighbors() {
    let start = (1_u64 << 32) + 17;
    let c = EotsNonceCommitment {
        start,
        count: 2,
        root: [0; 32],
    };
    assert!(!c.covers(start - 1));
    assert!(c.covers(start));
    assert!(c.covers(start + 1));
    assert!(!c.covers(start + 2));
}

#[test]
fn verification_commitment_accepts_the_last_height_of_a_maximum_count_range() {
    let c = EotsNonceCommitment {
        start: 2,
        count: u32::MAX,
        root: [0; 32],
    };
    assert!(c.covers(1_u64 << 32));
    assert!(!c.covers((1_u64 << 32) + 1));
}

#[test]
fn vote_signatures_use_the_specified_domain_and_field_order() {
    let signer = reference_signer(7, 0x0102_0304_0506_0708, 3);
    let height = signer.commitment.start + 1;
    let hash = [9; 32];
    let message = reference_vote_message(&[42; 20], height, &hash);
    let vote = reference_vote(&signer, &[42; 20], height, hash);
    assert_eq!(vote_message(&[42; 20], height, &hash), message);
    assert_eq!(
        verify_bip340(&signer.pk_eots, &message, &vote.signature_bytes()),
        Ok(())
    );
    assert_eq!(verify_vote(&[42; 20], &vote, &signer.commitment), Ok(()));
    assert!(verify_vote(&[43; 20], &vote, &signer.commitment).is_err());
}

#[test]
fn a_valid_signature_on_the_wrong_message_is_rejected() {
    let signer = reference_signer(7, 1, 3);
    let mut vote = reference_vote(&signer, &[42; 20], 1, [9; 32]);
    let (_, signature) = sign_with_nonce(signer.d, signer.nonces[0], &[0; 32]);
    assert_eq!(verify_bip340(&signer.pk_eots, &[0; 32], &signature), Ok(()));
    vote.signature = EotsVote::signature_from_bytes(&signature);
    assert_eq!(
        verify_vote(&[42; 20], &vote, &signer.commitment),
        Err(EotsError::InvalidSignature)
    );
}

#[test]
fn a_valid_signature_with_an_uncommitted_nonce_is_rejected() {
    let signer = reference_signer(7, 1, 3);
    let mut vote = reference_vote(&signer, &[42; 20], 1, [9; 32]);
    let message = reference_vote_message(&[42; 20], 1, &[9; 32]);
    let (_, signature) = sign_with_nonce(signer.d, scalar(199), &message);
    assert_eq!(verify_bip340(&signer.pk_eots, &message, &signature), Ok(()));
    vote.signature = EotsVote::signature_from_bytes(&signature);
    assert_eq!(
        verify_vote(&[42; 20], &vote, &signer.commitment),
        Err(EotsError::InvalidProof)
    );
}

#[test]
fn an_uneven_commitment_rejects_wrong_missing_and_extra_siblings() {
    let signer = reference_signer(7, 1, 3);
    for height in 1..=3 {
        let vote = reference_vote(&signer, &[42; 20], height, [9; 32]);
        assert_eq!(verify_vote(&[42; 20], &vote, &signer.commitment), Ok(()));
        let mut missing = vote.clone();
        missing.proof.pop();
        assert_eq!(
            verify_vote(&[42; 20], &missing, &signer.commitment),
            Err(EotsError::InvalidProof)
        );
        let mut extra = vote.clone();
        extra.proof.push([0; 32]);
        assert_eq!(
            verify_vote(&[42; 20], &extra, &signer.commitment),
            Err(EotsError::InvalidProof)
        );
        let mut wrong = vote;
        wrong.proof[0][0] ^= 1;
        assert_eq!(
            verify_vote(&[42; 20], &wrong, &signer.commitment),
            Err(EotsError::InvalidProof)
        );
    }
}

#[test]
fn truncated_postcard_votes_fail_to_decode() {
    let signer = reference_signer(7, 1, 3);
    let vote = reference_vote(&signer, &[42; 20], 1, [9; 32]);
    let encoded = postcard::to_stdvec(&vote).unwrap();
    assert_eq!(postcard::from_bytes::<EotsVote>(&encoded).unwrap(), vote);
    for end in 0..encoded.len() {
        assert!(
            postcard::from_bytes::<EotsVote>(&encoded[..end]).is_err(),
            "accepted truncation at byte {end}"
        );
    }
}

#[test]
fn a_postcard_height_larger_than_u64_fails_to_decode() {
    let signer = reference_signer(7, 1, 3);
    let mut encoded = postcard::to_stdvec(&reference_vote(&signer, &[42; 20], 1, [9; 32])).unwrap();
    encoded.splice(32..33, [0xff; 10].into_iter().chain([0x01]));
    assert!(postcard::from_bytes::<EotsVote>(&encoded).is_err());
}

#[test]
fn a_forged_postcard_proof_length_fails_to_decode() {
    let signer = reference_signer(7, 1, 1);
    let vote = reference_vote(&signer, &[42; 20], 1, [9; 32]);
    let mut encoded = postcard::to_stdvec(&vote).unwrap();
    // pk(32), h(1), b(32), signature(64), start(1), proof length(1).
    assert_eq!(encoded.len(), 131);
    encoded[130] = 1;
    assert!(postcard::from_bytes::<EotsVote>(&encoded).is_err());
}

#[test]
fn json_vote_fields_with_a_wrong_width_fail_to_decode() {
    let vote = TestSigner::new(7, 1, 4).vote(&[42; 20], 1, [1; 32]);
    for field in [
        "/pk_eots",
        "/block_hash",
        "/signature/0",
        "/signature/1",
        "/proof/0",
    ] {
        for length in [0, 1, 31, 33, 64, 65] {
            let mut value = serde_json::to_value(&vote).unwrap();
            *value.pointer_mut(field).unwrap() = serde_json::json!(vec![0; length]);
            assert!(
                serde_json::from_value::<EotsVote>(value).is_err(),
                "accepted {field} with {length} bytes"
            );
        }
    }
}

#[test]
fn json_heights_outside_u64_fail_to_decode() {
    let vote = TestSigner::new(7, 1, 4).vote(&[42; 20], 1, [1; 32]);
    for number in ["-1", "18446744073709551616"] {
        let mut value = serde_json::to_value(&vote).unwrap();
        value["height"] = serde_json::from_str(number).unwrap();
        assert!(serde_json::from_value::<EotsVote>(value).is_err());
    }
}

#[test]
fn finality_answer_json_form() {
    let answer = FinalityAnswer {
        final_height: 3,
        recorded_final_height: 4,
        checkpointed_height: 2,
        conflict_height: None,
    };
    let json = r#"{"final_height":3,"recorded_final_height":4,"checkpointed_height":2}"#;
    assert_eq!(serde_json::to_string(&answer).unwrap(), json);
    assert_eq!(
        serde_json::from_str::<FinalityAnswer>(json).unwrap(),
        answer
    );
}
