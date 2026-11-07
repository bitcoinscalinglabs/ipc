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
    /// The lookup from the validators of the current configuration and of the one before it:
    /// CometBFT applies a configuration change two subnet blocks after the gateway, so a member
    /// the change removed can still propose the next subnet block.
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
}
