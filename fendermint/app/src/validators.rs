// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use anyhow::{anyhow, Ok, Result};
use fendermint_crypto::PublicKey;
use fendermint_vm_genesis::{Power, Validator};
use fendermint_vm_interpreter::fvm::state::ipc::GatewayCaller;
use fendermint_vm_interpreter::fvm::state::FvmExecState;
use std::collections::HashMap;

use tendermint::account::Id as TendermintId;
use tendermint::PublicKey as TendermintPubKey;

use fvm_ipld_blockstore::Blockstore;

#[derive(Clone)]
// Tracks the validator ID from Tendermint to their corresponding public key.
pub(crate) struct ValidatorCache {
    map: HashMap<TendermintId, PublicKey>,
}

impl ValidatorCache {
    /// Keep removed proposers available during CometBFT's two-height delay.
    pub fn new_from_state<SS>(state: &mut FvmExecState<SS>) -> Result<Self>
    where
        SS: Blockstore + Clone + 'static,
    {
        let gateway = GatewayCaller::default();
        let (_, current) = gateway.current_power_table(state)?;
        let last = gateway.last_power_table(state)?;
        Self::from_validators(current.into_iter().chain(last))
    }

    pub fn from_validators(validators: impl IntoIterator<Item = Validator<Power>>) -> Result<Self> {
        let map = validators
            .into_iter()
            .map(|v| {
                let tendermint_pub_key: TendermintPubKey =
                    TendermintPubKey::try_from(v.public_key.clone())?;
                let id = TendermintId::from(tendermint_pub_key);
                let key = *v.public_key.public_key();
                Ok((id, key))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        Ok(Self { map })
    }

    pub fn get_validator(&self, id: &tendermint::account::Id) -> Result<PublicKey> {
        self.map
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("validator not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::ValidatorCache;
    use fendermint_crypto::SecretKey;
    use fendermint_vm_genesis::{Power, Validator, ValidatorKey};
    use rand::SeedableRng;
    use tendermint::account::Id as TendermintId;
    use tendermint::PublicKey as TendermintPubKey;

    fn validator(rng: &mut impl rand::Rng) -> Validator<Power> {
        Validator {
            public_key: ValidatorKey(SecretKey::random(rng).public_key()),
            power: Power(1),
        }
    }

    fn id_of(v: &Validator<Power>) -> TendermintId {
        TendermintId::from(TendermintPubKey::try_from(v.public_key.clone()).unwrap())
    }

    /// A configuration change at `h` removes V; V proposes `h + 1`, and the lookup built after
    /// the change still finds V from the configuration before it.
    #[test]
    fn removed_validator_still_found() {
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1);
        let v = validator(&mut rng);
        let w = validator(&mut rng);
        let stranger = validator(&mut rng);

        let last = vec![v.clone(), w.clone()];
        let current = vec![w.clone()];
        let cache = ValidatorCache::from_validators(current.into_iter().chain(last)).unwrap();

        assert_eq!(cache.get_validator(&id_of(&v)).unwrap(), *v.public_key.public_key());
        assert_eq!(cache.get_validator(&id_of(&w)).unwrap(), *w.public_key.public_key());
        assert!(cache.get_validator(&id_of(&stranger)).is_err());
    }

    /// Configurations applied at the end of subnet blocks 10 and 11 (a checkpoint period of 1). CometBFT
    /// uses each from two subnet blocks later; the gateway moves the current membership to the last one at
    /// once, and the lookup rebuilt from both finds every validator CometBFT can pick as proposer.
    #[test]
    fn proposers_found_around_configuration_changes() {
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(2);
        let [a, b, c, d] = [0; 4].map(|_| validator(&mut rng));
        let c0 = vec![a.clone(), b.clone()];
        let changes = [(10, vec![b.clone(), c.clone()]), (11, vec![c, d])];

        let in_cometbft = |height: u64| {
            changes
                .iter()
                .rev()
                .find(|(applied, _)| applied + 2 <= height)
                .map_or(&c0, |(_, configuration)| configuration)
        };

        let mut current = c0.clone();
        let mut cache = ValidatorCache::from_validators(current.clone()).unwrap();
        for height in 9..=14 {
            for proposer in in_cometbft(height) {
                assert!(
                    cache.get_validator(&id_of(proposer)).is_ok(),
                    "proposer of subnet block {height} not found"
                );
            }
            if let Some((_, next)) = changes.iter().find(|(applied, _)| *applied == height) {
                let last = std::mem::replace(&mut current, next.clone());
                cache =
                    ValidatorCache::from_validators(current.iter().chain(&last).cloned()).unwrap();
            }
        }
        assert!(cache.get_validator(&id_of(&a)).is_err());
    }
}
