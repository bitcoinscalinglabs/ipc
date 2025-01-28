use anyhow::anyhow;
use anyhow::Result;

use crate::evm::KeyInfo;
use libsecp256k1::{PublicKey, SecretKey};

pub const DEFAULT_BTC_KEYSTORE_NAME: &str = "btc_keystore.json";

/// Generate a random secp256k1 private key, subject to the constraint that the
/// public key partity is even.
pub fn random_btc_secret_key() -> KeyInfo {
    let secret_key = loop {
        let secret_key = SecretKey::random(&mut rand::thread_rng());
        let public_key = PublicKey::from_secret_key(&secret_key);

        let compressed = public_key.serialize_compressed();
        if compressed[0] == libsecp256k1_core::util::TAG_PUBKEY_EVEN {
            break secret_key;
        }
    };
    KeyInfo::new(secret_key.serialize().to_vec())
}

/// Parse a private key from a hex string, throwing an error if the corresponding
/// public key is not even.
pub fn parse_and_validate_secret_key(private_key_data: &[u8]) -> Result<SecretKey> {
    let secret_key = SecretKey::parse_slice(private_key_data)?;
    let public_key = PublicKey::from_secret_key(&secret_key);

    let compressed = public_key.serialize_compressed();
    if compressed[0] != libsecp256k1_core::util::TAG_PUBKEY_EVEN {
        return Err(anyhow!(
            "Invalid secretkey, the corresponding public key parity is not even"
        ));
    }

    Ok(secret_key)
}

/// Serialize a public key to the xonly format, which is used in Bitcoin.
pub fn get_xonly_public_key_serialized(secret_key: &libsecp256k1::SecretKey) -> Result<Vec<u8>> {
    let public_key = PublicKey::from_secret_key(&secret_key);
    let serialized = public_key.serialize_compressed();
    if serialized[0] == libsecp256k1_core::util::TAG_PUBKEY_EVEN {
        Ok(serialized[1..].to_vec())
    } else {
        Err(anyhow!("Public key parity is not even"))
    }
}
