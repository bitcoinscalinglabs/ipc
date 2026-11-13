// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use ethers::types as et;
use fendermint_rpc::response::decode_fevm_return_data;
use fvm_ipld_encoding::RawBytes;
use fvm_shared::error::ExitCode;
use fvm_shared::{address::Address, chainid::ChainID, econ::TokenAmount, BLOCK_GAS_LIMIT};
use num_traits::Zero;
use tendermint_rpc::Client;

use fendermint_crypto::SecretKey;
use fendermint_rpc::message::GasParams;
use fendermint_rpc::query::QueryClient;
use fendermint_rpc::tx::{CallClient, TxClient, TxSync};
use fendermint_rpc::{client::FendermintClient, message::SignedMessageFactory};
use fendermint_vm_message::query::FvmQueryHeight;

macro_rules! retry {
    ($max_retries:expr, $retry_delay:expr, $block:expr) => {{
        let mut attempt = 0;
        let value = loop {
            match $block {
                Err((code, msg)) if attempt == $max_retries || !can_retry(code) => {
                    bail!(msg);
                }
                Err((_, msg)) => {
                    tracing::warn!(error = msg, attempt, "retry broadcast");
                    attempt += 1;
                }
                Ok(value) => {
                    break value;
                }
            }
            tokio::time::sleep($retry_delay).await;
        };
        value
    }};
}

/// Broadcast transactions to Tendermint.
///
/// This is typically something only active validators would want to do
/// from within Fendermint as part of the block lifecycle, for example
/// to submit their signatures to the ledger.
///
/// The broadcaster encapsulates the tactics for figuring out the nonce,
/// the gas limit, potential retries, etc.
#[derive(Clone)]
pub struct Broadcaster<C> {
    client: FendermintClient<C>,
    secret_key: SecretKey,
    addr: Address,
    gas_fee_cap: TokenAmount,
    gas_premium: TokenAmount,
    gas_overestimation_rate: f64,
    max_retries: u8,
    retry_delay: Duration,
}

impl<C> Broadcaster<C>
where
    C: Client + Clone + Send + Sync,
{
    pub fn new(
        client: C,
        addr: Address,
        secret_key: SecretKey,
        gas_fee_cap: TokenAmount,
        gas_premium: TokenAmount,
        gas_overestimation_rate: f64,
    ) -> Self {
        let client = FendermintClient::new(client);
        Self {
            client,
            secret_key,
            addr,
            gas_fee_cap,
            gas_premium,
            gas_overestimation_rate,
            max_retries: 0,
            // Set the retry delay to rougly the block creation time.
            retry_delay: Duration::from_secs(1),
        }
    }

    pub fn with_max_retries(mut self, max_retries: u8) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    pub fn retry_delay(&self) -> Duration {
        self.retry_delay
    }

    /// Run a read-only call of a contract on the committed state and return its return data.
    pub async fn fevm_view(
        &self,
        contract: Address,
        calldata: et::Bytes,
        chain_id: ChainID,
    ) -> anyhow::Result<Vec<u8>> {
        // Sequence zero makes the call use the sender's sequence in the committed state, which is
        // behind the pending one while a transaction of this validator waits in the mempool.
        let factory = SignedMessageFactory::new(self.secret_key.clone(), self.addr, 0, chain_id);
        let mut client = self.client.clone().bind(factory);
        let gas_params = GasParams {
            gas_limit: BLOCK_GAS_LIMIT,
            gas_fee_cap: self.gas_fee_cap.clone(),
            gas_premium: self.gas_premium.clone(),
        };
        let res = client
            .fevm_call(
                contract,
                calldata.0,
                TokenAmount::zero(),
                gas_params,
                FvmQueryHeight::Committed,
            )
            .await
            .context("failed to call the contract")?;
        res.return_data.ok_or_else(|| {
            anyhow!(
                "contract call failed: {:?} - {}",
                res.response.value.code,
                res.response.value.info
            )
        })
    }

    /// Send a transaction to the chain and return is hash.
    ///
    /// It currently doesn't wait for the execution, only that it has successfully been added to the mempool,
    /// or if not then an error is returned. The reason for not waiting for the commit is that the Tendermint
    /// client seems to time out if the check fails, waiting for the inclusion which will never come, instead of
    /// returning the result with no `deliver_tx` and a failed `check_tx`. We can add our own mechanism to wait
    /// for commits if we have to.
    pub async fn fevm_invoke(
        &self,
        contract: Address,
        calldata: et::Bytes,
        chain_id: ChainID,
    ) -> anyhow::Result<tendermint::hash::Hash> {
        let tx_hash = retry!(self.max_retries, self.retry_delay, {
            let sequence = self
                .sequence()
                .await
                .context("failed to get broadcaster sequence")?;

            let factory =
                SignedMessageFactory::new(self.secret_key.clone(), self.addr, sequence, chain_id);

            // Using the bound client as a one-shot transaction sender.
            let mut client = self.client.clone().bind(factory);

            // TODO: Maybe we should implement something like the Ethereum facade for estimating fees?
            // I don't want to call the Ethereum API directly (it would be one more dependency).
            // Another option is for Fendermint to recognise transactions coming from validators
            // and always put them into the block to facilitate checkpointing.
            let mut gas_params = GasParams {
                gas_limit: BLOCK_GAS_LIMIT,
                gas_fee_cap: self.gas_fee_cap.clone(),
                gas_premium: self.gas_premium.clone(),
            };

            // Not expecting to send any tokens to the contracts.
            let value = TokenAmount::zero();

            // We can use the `Committed` state to execute the message, which is more efficient than doing it on `Pending`.
            let gas_estimate = client
                .fevm_estimate_gas(
                    contract,
                    calldata.0.clone(),
                    value.clone(),
                    gas_params.clone(),
                    FvmQueryHeight::Committed,
                )
                .await
                .context("failed to estimate gas")?;

            if gas_estimate.value.exit_code.is_success() {
                gas_params.gas_limit =
                    (gas_estimate.value.gas_limit as f64 * self.gas_overestimation_rate) as u64;
            } else {
                bail!(
                    "failed to estimate gas: {} - {}",
                    gas_estimate.value.exit_code,
                    gas_estimate.value.info
                );
            }

            // Using TxSync instead of TxCommit because TxCommit times out if the `check_tx` part fails,
            // instead of returning as soon as the check failed with some default values for `deliver_tx`.
            let res = TxClient::<TxSync>::fevm_invoke(
                &mut client,
                contract,
                calldata.0.clone(),
                value,
                gas_params,
            )
            .await
            .context("failed to invoke contract")?;

            if res.response.code.is_err() {
                // Not sure what exactly arrives in the data and how it's encoded.
                // It might need the Base64 decoding or it may not. Let's assume
                // that it doesn't because unlike `DeliverTx::data`, this response
                // does have some Base64 lreated annotations.
                let data = decode_fevm_return_data(RawBytes::new(res.response.data.to_vec()))
                    .map(hex::encode)
                    .unwrap_or_else(|_| hex::encode(res.response.data));

                Err((
                    res.response.code,
                    format!(
                        "broadcasted transaction failed during check: {}; data = {}",
                        res.response.code.value(),
                        data
                    ),
                ))
            } else {
                Ok(res.response.hash)
            }
        });

        Ok(tx_hash)
    }

    /// Fetch the current nonce to be used in the next message.
    async fn sequence(&self) -> anyhow::Result<u64> {
        // Using the `Pending` state to query just in case there are other transactions initiated by the validator.
        let res = self
            .client
            .actor_state(&self.addr, FvmQueryHeight::Pending)
            .await
            .context("failed to get broadcaster actor state")?;

        match res.value {
            Some((_, state)) => Ok(state.sequence),
            None => Err(anyhow!("broadcaster actor {} cannot be found", self.addr)),
        }
    }
}

/// Decide if it's worth retrying the transaction.
fn can_retry(code: tendermint::abci::Code) -> bool {
    match ExitCode::new(code.value()) {
        // If the sender doesn't exist it doesn't matter how many times we try.
        ExitCode::SYS_SENDER_INVALID => false,
        // If the nonce was invalid, it might be because of a race condition, and we can try again.
        ExitCode::SYS_SENDER_STATE_INVALID => true,
        // If the sender doesn't have enough funds to cover the gas, it's unlikely that repeating imemediately will help.
        ExitCode::SYS_INSUFFICIENT_FUNDS => false,
        ExitCode::USR_INSUFFICIENT_FUNDS => false,
        // If we estimate the gas wrong, there's no point trying it will probably go wrong again.
        ExitCode::SYS_OUT_OF_GAS => false,
        // Unknown errors should not be retried.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use cid::Cid;
    use ethers::types as et;
    use fendermint_crypto::SecretKey;
    use fendermint_vm_message::query::{ActorState, FvmQuery};
    use fvm_ipld_encoding::to_vec;
    use fvm_shared::{address::Address, econ::TokenAmount};
    use num_traits::Zero;
    use tendermint::abci::Code;
    use tendermint_rpc::endpoint::abci_query::{self, AbciQuery};
    use tendermint_rpc::{Client, Error, Response, SimpleRequest};

    use super::Broadcaster;

    /// The sender's sequence in the committed state; one of its transactions is pending.
    const COMMITTED_SEQUENCE: u64 = 33;

    #[derive(Clone)]
    struct Node;

    #[async_trait]
    impl Client for Node {
        async fn perform<R>(&self, request: R) -> Result<R::Output, Error>
        where
            R: SimpleRequest,
        {
            let request: serde_json::Value = serde_json::from_str(&request.into_json()).unwrap();
            let params = &request["params"];
            let height: u64 = params["height"].as_str().unwrap().parse().unwrap();
            let data = hex::decode(params["data"].as_str().unwrap()).unwrap();
            let response = match fvm_ipld_encoding::from_slice(&data).unwrap() {
                FvmQuery::ActorState(_) => {
                    let pending = height == i64::MAX as u64;
                    let state = ActorState {
                        code: Cid::default(),
                        state: Cid::default(),
                        sequence: COMMITTED_SEQUENCE + u64::from(pending),
                        balance: TokenAmount::zero(),
                        delegated_address: None,
                    };
                    AbciQuery {
                        key: to_vec(&100u64).unwrap(),
                        value: to_vec(&state).unwrap(),
                        ..Default::default()
                    }
                }
                // Zero stands for the sequence in the state. The value is an empty
                // `ResponseDeliverTx`: exit code 0 and no return data.
                FvmQuery::Call(msg) if msg.sequence == 0 || msg.sequence == COMMITTED_SEQUENCE => {
                    AbciQuery {
                        value: to_vec(&Vec::<u8>::new()).unwrap(),
                        ..Default::default()
                    }
                }
                FvmQuery::Call(msg) => AbciQuery {
                    code: Code::from(2),
                    info: format!(
                        "pre-validation failed: Actor sequence invalid: {} != {COMMITTED_SEQUENCE}",
                        msg.sequence
                    ),
                    ..Default::default()
                },
                query => panic!("unexpected query {query:?}"),
            };
            let result = serde_json::to_value(abci_query::Response { response }).unwrap();
            let response = serde_json::json!({ "jsonrpc": "2.0", "id": "", "result": result });
            Ok(R::Response::from_string(response.to_string())?.into())
        }
    }

    #[tokio::test]
    async fn a_view_runs_on_the_committed_state_while_a_transaction_is_pending() {
        let broadcaster = Broadcaster::new(
            Node,
            Address::new_id(100),
            SecretKey::try_from(vec![1u8; 32]).unwrap(),
            TokenAmount::zero(),
            TokenAmount::zero(),
            1.0,
        );
        let data = broadcaster
            .fevm_view(Address::new_id(1), et::Bytes::default(), 1.into())
            .await
            .unwrap();
        assert!(data.is_empty());
    }
}
