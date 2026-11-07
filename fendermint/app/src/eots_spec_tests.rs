use super::envelope_check;
use crate::ipc::AppVote;
use fendermint_vm_topdown::eots::key_bytes_of;
use ipc_api::subnet_id::SubnetID;
use ipc_eots::{testing::TestSigner, ValidatorVote};
use ipc_ipld_resolver::VoteRecord;
use libp2p::identity::Keypair;

fn subnet(byte: u8) -> SubnetID {
    SubnetID::new_btc(4, &hex::encode([byte; 20])).unwrap()
}

fn signed() -> ValidatorVote {
    let key = Keypair::generate_secp256k1();
    let vote = TestSigner::new(7, 1, 4).vote(&[42; 20], 1, [1; 32]);
    let signed = VoteRecord::signed(&key, subnet(42), AppVote::Eots(vote.clone())).unwrap();
    let result = ValidatorVote {
        validator: key_bytes_of(&key.public()).to_vec(),
        vote,
        timestamp: signed.record().timestamp.into(),
        signature: signed.signature(),
    };
    assert!((envelope_check(subnet(42)))(&result));
    result
}

macro_rules! reject_envelope {
    ($name:ident, $v:ident, $mutate:block) => {
        // slashing.md, Accepting an EOTS vote 1: every envelope field is authenticated before vote validation.
        #[test]
        fn $name() {
            let mut $v = signed();
            $mutate
            assert!(!(envelope_check(subnet(42)))(&$v));
            assert!((envelope_check(subnet(42)))(&signed()));
        }
    };
}

reject_envelope!(an_altered_block_hash_invalidates_the_envelope, v, {
    v.vote.block_hash[0] ^= 1;
});
reject_envelope!(an_altered_height_invalidates_the_envelope, v, {
    v.vote.height += 1;
});
reject_envelope!(an_altered_eots_key_invalidates_the_envelope, v, {
    v.vote.pk_eots[0] ^= 1;
});
reject_envelope!(an_altered_eots_signature_invalidates_the_envelope, v, {
    v.vote.signature[1][0] ^= 1;
});
reject_envelope!(an_altered_commitment_start_invalidates_the_envelope, v, {
    v.vote.start += 1;
});
reject_envelope!(an_altered_proof_invalidates_the_envelope, v, {
    v.vote.proof[0][0] ^= 1;
});
reject_envelope!(an_altered_timestamp_invalidates_the_envelope, v, {
    v.timestamp ^= 1;
});
reject_envelope!(a_corrupted_envelope_signature_is_rejected, v, {
    v.signature[0] ^= 1;
});
reject_envelope!(an_empty_envelope_signature_is_rejected, v, {
    v.signature.clear();
});
reject_envelope!(a_truncated_envelope_signature_is_rejected, v, {
    v.signature.pop();
});
reject_envelope!(an_extended_envelope_signature_is_rejected, v, {
    v.signature.push(0);
});
reject_envelope!(a_truncated_validator_key_is_rejected, v, {
    v.validator.pop();
});
reject_envelope!(an_extended_validator_key_is_rejected, v, {
    v.validator.push(0);
});
reject_envelope!(an_unparseable_validator_key_is_rejected, v, {
    v.validator = vec![255; 33];
});
reject_envelope!(another_envelope_key_cannot_impersonate_the_signer, v, {
    v.validator = key_bytes_of(&Keypair::generate_secp256k1().public()).to_vec();
});

// slashing.md, Accepting an EOTS vote 1: a valid envelope from another subnet is still rejected.
#[test]
fn another_subnets_signed_envelope_is_rejected() {
    let vote = signed();
    assert!(!(envelope_check(subnet(43)))(&vote));
    assert!((envelope_check(subnet(42)))(&vote));
}

macro_rules! reject_vote_width {
    ($name:ident, $value:ident, $mutate:block) => {
        // encoding.md, EOTS vote: fixed-width fields reject missing or extra bytes before cryptographic validation.
        #[test]
        fn $name() {
            for length in [0, 1, 31, 33, 64, 65] {
                let mut $value = serde_json::to_value(signed().vote).unwrap();
                let replacement = serde_json::json!(vec![0; length]);
                ($mutate)(&mut $value, replacement);
                assert!(serde_json::from_value::<ipc_eots::EotsVote>($value).is_err());
            }
        }
    };
}

reject_vote_width!(wrong_key_widths_are_rejected, value, {
    |v: &mut serde_json::Value, r| v["pk_eots"] = r
});
reject_vote_width!(wrong_block_hash_widths_are_rejected, value, {
    |v: &mut serde_json::Value, r| v["block_hash"] = r
});
reject_vote_width!(wrong_nonce_widths_are_rejected, value, {
    |v: &mut serde_json::Value, r| v["signature"][0] = r
});
reject_vote_width!(wrong_scalar_widths_are_rejected, value, {
    |v: &mut serde_json::Value, r| v["signature"][1] = r
});
reject_vote_width!(wrong_proof_node_widths_are_rejected, value, {
    |v: &mut serde_json::Value, r| v["proof"][0] = r
});

// encoding.md, EOTS vote: negative and over-wide heights cannot be represented as a u64.
#[test]
fn negative_and_overflowing_heights_fail_to_parse() {
    for number in ["-1", "18446744073709551616"] {
        let mut value = serde_json::to_value(signed().vote).unwrap();
        value["height"] = serde_json::from_str(number).unwrap();
        assert!(serde_json::from_value::<ipc_eots::EotsVote>(value).is_err());
    }
}
