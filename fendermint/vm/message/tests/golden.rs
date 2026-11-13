// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

/// Examples of `ChainMessage`, which is what the client has to send,
/// or at least what appears in blocks.
mod chain {
    use fendermint_testing::golden_cbor;
    use fendermint_vm_message::{chain::ChainMessage, ipc::IpcMessage};
    use quickcheck::Arbitrary;

    golden_cbor! { "chain", signed, |g| {
        loop {
            if let msg @ ChainMessage::Signed(_) = ChainMessage::arbitrary(g) {
                return msg
            }
        }
      }
    }

    golden_cbor! { "chain", ipc_bottom_up_resolve, |g| {
        loop {
            if let msg @ ChainMessage::Ipc(IpcMessage::BottomUpResolve(_)) = ChainMessage::arbitrary(g) {
                return msg
            }
        }
      }
    }

    golden_cbor! { "chain", ipc_bottom_up_exec, |g| {
        loop {
            if let msg @ ChainMessage::Ipc(IpcMessage::BottomUpExec(_)) = ChainMessage::arbitrary(g) {
                return msg
            }
        }
      }
    }

    golden_cbor! { "chain", ipc_top_down, |g| {
        loop {
            if let msg @ ChainMessage::Ipc(IpcMessage::TopDownExec(_)) = ChainMessage::arbitrary(g) {
                return msg
            }
        }
      }
    }

    golden_cbor! { "chain", ipc_eots_finality, |_g| {
        let vote = |block_hash| ipc_eots::ValidatorVote {
            validator: vec![2; 33],
            vote: ipc_eots::EotsVote {
                pk_eots: [1; 32],
                height: 7,
                block_hash,
                signature: [[3; 32], [4; 32]],
                start: 5,
                proof: vec![[6; 32]],
            },
            timestamp: 8,
            signature: vec![9; 65],
        };
        ChainMessage::Ipc(IpcMessage::EotsFinality(ipc_eots::EotsFinalityData {
            bitcoin_height: 100,
            votes: vec![vote([10; 32])],
            evidence: vec![(vote([10; 32]), vote([11; 32]))],
        }))
      }
    }
}

/// Examples of FVM messages, which is what the client needs to sign.
mod fvm {
    use fendermint_testing::golden_cid;
    use fendermint_vm_message::signed::SignedMessage;
    use quickcheck::Arbitrary;

    golden_cid! { "fvm", message, |g| SignedMessage::arbitrary(g).message, |m| SignedMessage::cid(m).unwrap() }
}

/// Examples of query requests the client needs to send, and client responses it will receive.
mod query {
    mod request {
        use fendermint_testing::golden_cbor;
        use fendermint_vm_message::query::FvmQuery;
        use quickcheck::Arbitrary;

        golden_cbor! { "query/request", ipld, |g| {
            loop {
                if let msg @ FvmQuery::Ipld(_) = FvmQuery::arbitrary(g) {
                    return msg
                }
            }
        }}

        golden_cbor! { "query/request", actor_state, |g| {
            loop {
                if let msg @ FvmQuery::ActorState { .. } = FvmQuery::arbitrary(g) {
                    return msg
                }
            }
        }}
    }

    mod response {
        use fendermint_testing::golden_cbor;
        use fendermint_vm_message::query::ActorState;
        use quickcheck::Arbitrary;

        golden_cbor! { "query/response", actor_state, |g| {
            ActorState::arbitrary(g)
        }}
    }
}
