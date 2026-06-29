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
            "secret key is not compatible with bitcoin, the corresponding public key parity is not even"
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

/// Take a serialized secp256k1 public key in any libsecp256k1-parseable format
/// (33-byte compressed or 65-byte uncompressed) and return its 32-byte x-only
/// Taproot representation. Errors on odd Y parity or on input the parser rejects.
pub fn xonly_from_pubkey_bytes(bytes: &[u8]) -> Result<[u8; 32]> {
    let public_key = PublicKey::parse_slice(bytes, None)?;
    let compressed = public_key.serialize_compressed();
    if compressed[0] != libsecp256k1_core::util::TAG_PUBKEY_EVEN {
        return Err(anyhow!(
            "public key parity is not even (tag 0x{:02x})",
            compressed[0]
        ));
    }
    let mut x = [0u8; 32];
    x.copy_from_slice(&compressed[1..]);
    Ok(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a (secret_key, compressed_pubkey, uncompressed_pubkey) triple where
    /// the public key has the requested Y parity.
    fn key_with_parity(even: bool) -> (SecretKey, [u8; 33], [u8; 65]) {
        loop {
            let sk = SecretKey::random(&mut rand::thread_rng());
            let pk = PublicKey::from_secret_key(&sk);
            let compressed = pk.serialize_compressed();
            let is_even = compressed[0] == libsecp256k1_core::util::TAG_PUBKEY_EVEN;
            if is_even == even {
                return (sk, compressed, pk.serialize());
            }
        }
    }

    #[test]
    fn get_xonly_from_sk_even_succeeds() {
        let (sk, compressed, _) = key_with_parity(true);
        let xonly = get_xonly_public_key_serialized(&sk).unwrap();
        assert_eq!(xonly.len(), 32);
        assert_eq!(xonly, compressed[1..]);
    }

    #[test]
    fn get_xonly_from_sk_odd_errors() {
        let (sk, _, _) = key_with_parity(false);
        assert!(get_xonly_public_key_serialized(&sk).is_err());
    }

    #[test]
    fn xonly_from_compressed_even_succeeds() {
        let (_, compressed, _) = key_with_parity(true);
        let xonly = xonly_from_pubkey_bytes(&compressed).unwrap();
        assert_eq!(&xonly[..], &compressed[1..]);
    }

    #[test]
    fn xonly_from_uncompressed_even_succeeds() {
        let (_, compressed, uncompressed) = key_with_parity(true);
        let xonly = xonly_from_pubkey_bytes(&uncompressed).unwrap();
        assert_eq!(&xonly[..], &compressed[1..]);
    }

    #[test]
    fn xonly_from_compressed_odd_errors() {
        let (_, compressed, _) = key_with_parity(false);
        assert!(xonly_from_pubkey_bytes(&compressed).is_err());
    }

    #[test]
    fn xonly_from_uncompressed_odd_errors() {
        let (_, _, uncompressed) = key_with_parity(false);
        assert!(xonly_from_pubkey_bytes(&uncompressed).is_err());
    }

    #[test]
    fn xonly_from_garbage_errors() {
        assert!(xonly_from_pubkey_bytes(&[]).is_err());
        assert!(xonly_from_pubkey_bytes(&[0u8; 33]).is_err());
        assert!(xonly_from_pubkey_bytes(&[0xffu8; 65]).is_err());
    }
}
