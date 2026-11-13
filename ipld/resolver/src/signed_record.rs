// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: MIT

use libp2p::core::signed_envelope;
use libp2p::identity::PublicKey;
use libp2p::{core::SignedEnvelope, identity::Keypair};
use serde::de::DeserializeOwned;
use serde::Serialize;

const DOMAIN_SEP: &str = "/ipc/ipld/resolver";

pub trait Record {
    /// Payload type for the [`SignedEnvelope`].
    fn payload_type() -> &'static str;
    /// Check that the [`PublicKey`] recovered from the [`SignedEnvelope`]
    /// is consistent with the payload.
    fn check_signing_key(&self, key: &PublicKey) -> bool;
}

/// A [`ProviderRecord`] with a [`SignedEnvelope`] proving that the
/// peer indeed is ready to provide the data for the listed subnets.
#[derive(Debug, Clone)]
pub struct SignedRecord<R> {
    /// The deserialized and validated record.
    record: R,
    /// The [`SignedEnvelope`] from which the record was deserialized from.
    envelope: SignedEnvelope,
}

// Based on `libp2p_core::peer_record::PeerRecord`
impl<R> SignedRecord<R>
where
    R: Record + Serialize + DeserializeOwned,
{
    /// Create a new [`SignedRecord`] with a signed envelope
    /// which can be shared with others.
    pub fn new(key: &Keypair, record: R) -> anyhow::Result<Self> {
        let payload = fvm_ipld_encoding::to_vec(&record)?;
        let envelope = SignedEnvelope::new(
            key,
            DOMAIN_SEP.to_owned(),
            R::payload_type().as_bytes().to_vec(),
            payload,
        )?;
        Ok(Self { record, envelope })
    }

    pub fn from_signed_envelope(envelope: SignedEnvelope) -> Result<Self, FromEnvelopeError> {
        let (payload, signing_key) = envelope
            .payload_and_signing_key(DOMAIN_SEP.to_owned(), R::payload_type().as_bytes())?;

        let record = fvm_ipld_encoding::from_slice::<R>(payload)?;

        if !record.check_signing_key(signing_key) {
            return Err(FromEnvelopeError::MismatchedSignature);
        }

        Ok(Self { record, envelope })
    }

    /// Deserialize then check the domain tags and the signature.
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        let envelope = SignedEnvelope::from_protobuf_encoding(bytes)?;
        let signed_record = Self::from_signed_envelope(envelope)?;
        Ok(signed_record)
    }

    /// Rebuilds a signed record from its parts and verifies it as a received one.
    pub fn from_parts(key: &PublicKey, record: R, signature: Vec<u8>) -> anyhow::Result<Self> {
        let payload = fvm_ipld_encoding::to_vec(&record)?;
        let bytes = encode_envelope(
            &key.encode_protobuf(),
            R::payload_type().as_bytes(),
            &payload,
            &signature,
        );
        let signed_record = Self::from_bytes(&bytes)?;
        Ok(signed_record)
    }

    pub fn record(&self) -> &R {
        &self.record
    }

    pub fn signature(&self) -> Vec<u8> {
        envelope_signature(&self.envelope.clone().into_protobuf_encoding())
    }

    pub fn envelope(&self) -> &SignedEnvelope {
        &self.envelope
    }

    pub fn into_record(self) -> R {
        self.record
    }

    pub fn into_envelope(self) -> SignedEnvelope {
        self.envelope
    }
}

/// Field numbers of the envelope's protobuf message (libp2p RFC 0002), all length-delimited.
const ENVELOPE_FIELDS: [u8; 4] = [1, 2, 3, 5];
const ENVELOPE_SIGNATURE_FIELD: u8 = 5;

fn put_varint(buf: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        buf.push((n as u8) | 0x80);
        n >>= 7;
    }
    buf.push(n as u8);
}

fn get_varint(bytes: &[u8], pos: &mut usize) -> Option<usize> {
    let mut n = 0usize;
    let mut shift = 0;
    loop {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        n |= usize::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(n);
        }
        shift += 7;
        if shift > usize::BITS {
            return None;
        }
    }
}

/// The protobuf encoding of an envelope, as `SignedEnvelope::into_protobuf_encoding` writes it.
fn encode_envelope(key: &[u8], payload_type: &[u8], payload: &[u8], signature: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (field, value) in ENVELOPE_FIELDS
        .iter()
        .zip([key, payload_type, payload, signature])
    {
        buf.push(field << 3 | 2);
        put_varint(&mut buf, value.len());
        buf.extend_from_slice(value);
    }
    buf
}

fn envelope_signature(bytes: &[u8]) -> Vec<u8> {
    let mut pos = 0;
    while let Some(tag) = get_varint(bytes, &mut pos) {
        let Some(len) = get_varint(bytes, &mut pos) else {
            break;
        };
        let Some(value) = bytes.get(pos..pos + len) else {
            break;
        };
        if tag == usize::from(ENVELOPE_SIGNATURE_FIELD) << 3 | 2 {
            return value.to_vec();
        }
        pos += len;
    }
    Vec::new()
}

#[derive(thiserror::Error, Debug)]
pub enum FromEnvelopeError {
    /// Failed to extract the payload from the envelope.
    #[error("Failed to extract payload from envelope")]
    BadPayload(#[from] signed_envelope::ReadPayloadError),
    /// Failed to decode the provided bytes as the record type.
    #[error("Failed to decode bytes as record")]
    InvalidRecord(#[from] fvm_ipld_encoding::Error),
    /// The signer of the envelope is different than the peer id in the record.
    #[error("The signer of the envelope is different than the peer id in the record")]
    MismatchedSignature,
}

#[cfg(test)]
pub mod tests {
    use fvm_ipld_encoding::de::DeserializeOwned;
    use libp2p::core::SignedEnvelope;
    use serde::Serialize;

    use super::{Record, SignedRecord};

    pub fn prop_roundtrip<R>(signed_record: SignedRecord<R>) -> bool
    where
        R: Serialize + DeserializeOwned + Record + PartialEq,
    {
        let envelope_bytes = signed_record.envelope().clone().into_protobuf_encoding();

        let envelope =
            SignedEnvelope::from_protobuf_encoding(&envelope_bytes).expect("envelope roundtrip");

        let signed_record2 =
            SignedRecord::<R>::from_signed_envelope(envelope).expect("record roundtrip");

        signed_record2.into_record() == *signed_record.record()
    }

    pub fn prop_from_parts<R>(signed_record: SignedRecord<R>, key: &libp2p::identity::PublicKey) -> bool
    where
        R: Serialize + DeserializeOwned + Record + PartialEq + Clone,
    {
        let signature = signed_record.signature();
        let rebuilt = SignedRecord::from_parts(key, signed_record.record().clone(), signature.clone())
            .expect("the envelope rebuilds from its parts");
        let mut tampered = signature.clone();
        tampered[0] ^= 1;
        rebuilt.record() == signed_record.record()
            && rebuilt.signature() == signature
            && SignedRecord::from_parts(key, signed_record.record().clone(), tampered).is_err()
    }
}
