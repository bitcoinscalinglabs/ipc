use super::{verify_bip340, EotsError, EotsNonceCommitment};

// Literal external vectors: https://github.com/bitcoin/bips/blob/master/bip-0340/test-vectors.csv
// The literal vectors exercise BIP340 independently of the vote fixtures below.
fn bytes<const N: usize>(hex: &str) -> [u8; N] {
    hex::decode(hex).unwrap().try_into().unwrap()
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 3.
#[test]
fn bip340_keeps_an_all_ff_message_without_scalar_reduction() {
    let key = bytes::<32>("25D1DFF95105F5253C4022F628A996AD3A0D95FBF21D468A1B33F8C160D8F517");
    let message = bytes::<32>("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF");
    let signature = bytes::<64>("7EB0509757E246F19449885651611CB965ECC1A187DD51B64FDA1EDC9637D5EC97582B9CB13DB3933705B32BA982AF5AF25FD78881EBB32771FC5922EFC66EA3");
    assert_eq!(verify_bip340(&key, &message, &signature), Ok(()));
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 4.
#[test]
fn bip340_accepts_a_nonce_with_leading_zero_bytes() {
    let key = bytes::<32>("D69C3509BB99E412E68B0FE8544E72837DFA30746D8BE2AA65975F29D22DC7B9");
    let message = bytes::<32>("4DF3C3F68FCC83B27E9D42C90431A72499F17875C81A599B566C9889B9696703");
    let signature = bytes::<64>("00000000000000000000003B78CE563F89A0ED9414F5AA28AD0D96D6795F9C6376AFB1548AF603B3EB45C9F8207DEE1060CB71C04E80F593060B07D28308D7F4");
    assert_eq!(verify_bip340(&key, &message, &signature), Ok(()));
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 5.
#[test]
fn bip340_rejects_a_public_key_that_does_not_lift() {
    let key = bytes::<32>("EEFDEA4CDB677750A420FEE807EACF21EB9898AE79B9768766E4FAA04A2D4A34");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidKey)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 6.
#[test]
fn bip340_rejects_an_odd_y_verification_point() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("FFF97BD5755EEEA420453A14355235D382F6472F8568A18B2F057A14602975563CC27944640AC607CD107AE10923D9EF7A73C643E166BE5EBEAFA34B1AC553E2");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 7.
#[test]
fn bip340_rejects_a_signature_of_the_negated_message() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("1FA62E331EDBC21C394792D2AB1100A7B432B013DF3F6FF4F99FCB33E0E1515F28890B3EDB6E7189B630448B515CE4F8622A954CFE545735AAEA5134FCCDB2BD");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 8.
#[test]
fn bip340_rejects_a_negated_signature_scalar() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E177769961764B3AA9B2FFCB6EF947B6887A226E8D7C93E00C5ED0C1834FF0D0C2E6DA6");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 9.
#[test]
fn bip340_rejects_infinity_with_zero_nonce_coordinate() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("0000000000000000000000000000000000000000000000000000000000000000123DDA8328AF9C23A94C1FEECFD123BA4FB73476F0D594DCB65C6425BD186051");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 10.
#[test]
fn bip340_rejects_infinity_with_one_nonce_coordinate() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("00000000000000000000000000000000000000000000000000000000000000017615FBAF5AE28864013C099742DEADB4DBA87F11AC6754F93780D5A1837CF197");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 11.
#[test]
fn bip340_rejects_a_nonce_coordinate_that_does_not_lift() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("4A298DACAE57395A15D0795DDBFD1DCB564DA82B0F269BC70A74F8220429BA1D69E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 12.
#[test]
fn bip340_rejects_a_nonce_coordinate_equal_to_the_field_modulus() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F69E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 13.
#[test]
fn bip340_rejects_a_signature_scalar_equal_to_the_group_order() {
    let key = bytes::<32>("DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E177769FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidSignature)
    );
}

// encoding.md, Notation (EOTS/BIP340): official BIP340 vector 14.
#[test]
fn bip340_rejects_a_public_key_above_the_field_modulus() {
    let key = bytes::<32>("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC30");
    let message = bytes::<32>("243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89");
    let signature = bytes::<64>("6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B");
    assert_eq!(
        verify_bip340(&key, &message, &signature),
        Err(EotsError::InvalidKey)
    );
}

// encoding.md, EOTS nonce commitment: the verifier's commitment uses the same half-open u64 range.
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

// encoding.md, EOTS nonce commitment: count is a u32 range length, not the absolute last height.
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

fn reference_vote_message(subnet: &[u8; 20], height: u64, hash: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
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
    use sha2::{Digest, Sha256};
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

fn spec_signer(seed: u8, start: u64, count: u32) -> super::testing::TestSigner {
    let mut signer = super::testing::TestSigner::new(seed, start, count);
    signer.commitment.root = reference_tree_hash(&signer.leaves);
    signer
}

fn spec_vote(
    signer: &super::testing::TestSigner,
    subnet: &[u8; 20],
    height: u64,
    block_hash: [u8; 32],
) -> super::EotsVote {
    let index = (height - signer.commitment.start) as usize;
    let message = reference_vote_message(subnet, height, &block_hash);
    let (_, signature) = super::testing::sign_with_nonce(signer.d, signer.nonces[index], &message);
    super::EotsVote {
        pk_eots: signer.pk_eots,
        height,
        block_hash,
        signature: super::EotsVote::signature_from_bytes(&signature),
        start: signer.commitment.start,
        proof: reference_path(&signer.leaves, index),
    }
}

// slashing.md, The vote: the domain, subnet, full little-endian u64 height and block hash bind the signature.
#[test]
fn vote_signatures_use_the_specified_domain_and_field_order() {
    let signer = spec_signer(7, 0x0102_0304_0506_0708, 3);
    let height = signer.commitment.start + 1;
    let hash = [9; 32];
    let message = reference_vote_message(&[42; 20], height, &hash);
    let vote = spec_vote(&signer, &[42; 20], height, hash);
    assert_eq!(super::vote_message(&[42; 20], height, &hash), message);
    assert_eq!(
        verify_bip340(&signer.pk_eots, &message, &vote.signature_bytes()),
        Ok(())
    );
    assert_eq!(
        super::verify_vote(&[42; 20], &vote, &signer.commitment),
        Ok(())
    );
    assert!(super::verify_vote(&[43; 20], &vote, &signer.commitment).is_err());
}

// slashing.md, Accepting an EOTS vote 5: a valid BIP340 signature on an arbitrary message is not a vote.
#[test]
fn a_valid_signature_on_the_wrong_message_is_rejected() {
    let signer = spec_signer(7, 1, 3);
    let mut vote = spec_vote(&signer, &[42; 20], 1, [9; 32]);
    let (_, signature) = super::testing::sign_with_nonce(signer.d, signer.nonces[0], &[0; 32]);
    assert_eq!(verify_bip340(&signer.pk_eots, &[0; 32], &signature), Ok(()));
    vote.signature = super::EotsVote::signature_from_bytes(&signature);
    assert_eq!(
        super::verify_vote(&[42; 20], &vote, &signer.commitment),
        Err(EotsError::InvalidSignature)
    );
}

// slashing.md, Accepting an EOTS vote 4: signature validity cannot replace the committed public nonce.
#[test]
fn a_valid_signature_with_an_uncommitted_nonce_is_rejected() {
    let signer = spec_signer(7, 1, 3);
    let mut vote = spec_vote(&signer, &[42; 20], 1, [9; 32]);
    let message = reference_vote_message(&[42; 20], 1, &[9; 32]);
    let (_, signature) =
        super::testing::sign_with_nonce(signer.d, super::testing::scalar(199), &message);
    assert_eq!(verify_bip340(&signer.pk_eots, &message, &signature), Ok(()));
    vote.signature = super::EotsVote::signature_from_bytes(&signature);
    assert_eq!(
        super::verify_vote(&[42; 20], &vote, &signer.commitment),
        Err(EotsError::InvalidProof)
    );
}

// encoding.md, EOTS nonce commitment: uneven RFC 6962 trees authenticate the exact leaf index and complete path.
#[test]
fn an_uneven_commitment_rejects_wrong_missing_and_extra_siblings() {
    let signer = spec_signer(7, 1, 3);
    for height in 1..=3 {
        let vote = spec_vote(&signer, &[42; 20], height, [9; 32]);
        assert_eq!(
            super::verify_vote(&[42; 20], &vote, &signer.commitment),
            Ok(())
        );
        let mut missing = vote.clone();
        missing.proof.pop();
        assert_eq!(
            super::verify_vote(&[42; 20], &missing, &signer.commitment),
            Err(EotsError::InvalidProof)
        );
        let mut extra = vote.clone();
        extra.proof.push([0; 32]);
        assert_eq!(
            super::verify_vote(&[42; 20], &extra, &signer.commitment),
            Err(EotsError::InvalidProof)
        );
        let mut wrong = vote;
        wrong.proof[0][0] ^= 1;
        assert_eq!(
            super::verify_vote(&[42; 20], &wrong, &signer.commitment),
            Err(EotsError::InvalidProof)
        );
    }
}

// encoding.md, EOTS vote: each field has its declared width, including all 64 signature bytes and every proof node.
#[test]
fn truncated_vote_fields_fail_to_decode_without_a_panic() {
    let signer = spec_signer(7, 1, 3);
    let vote = spec_vote(&signer, &[42; 20], 1, [9; 32]);
    let encoded = postcard::to_stdvec(&vote).unwrap();
    assert_eq!(
        postcard::from_bytes::<super::EotsVote>(&encoded).unwrap(),
        vote
    );
    for end in 0..encoded.len() {
        assert!(
            postcard::from_bytes::<super::EotsVote>(&encoded[..end]).is_err(),
            "accepted truncation at byte {end}"
        );
    }
}

// encoding.md, EOTS vote: h is a u64 and cannot wrap a negative or over-wide integer into a valid height.
#[test]
fn a_wire_height_larger_than_u64_fails_to_decode() {
    let signer = spec_signer(7, 1, 3);
    let mut encoded = postcard::to_stdvec(&spec_vote(&signer, &[42; 20], 1, [9; 32])).unwrap();
    encoded.splice(32..33, [0xff; 10].into_iter().chain([0x01]));
    assert!(postcard::from_bytes::<super::EotsVote>(&encoded).is_err());
}

// encoding.md, EOTS vote: list length cannot claim proof nodes that the record does not contain.
#[test]
fn a_forged_proof_length_fails_to_decode() {
    let signer = spec_signer(7, 1, 1);
    let vote = spec_vote(&signer, &[42; 20], 1, [9; 32]);
    let mut encoded = postcard::to_stdvec(&vote).unwrap();
    // pk(32), h(1), b(32), signature(64), start(1), proof length(1).
    assert_eq!(encoded.len(), 131);
    encoded[130] = 1;
    assert!(postcard::from_bytes::<super::EotsVote>(&encoded).is_err());
}
