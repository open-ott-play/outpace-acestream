//! Acestream identifier math.
//!
//! The swarm infohash is not the SHA-1 of the wrapped `.acelive` transport file.
//! The official engine hashes a bencoded, ordered subset of the decoded live
//! descriptor. It also computes SHA-1 over the raw transport bytes separately
//! for transport-file identity/cache naming.

use crate::bencode::Bencode;
use crate::{Result, WireError};
use sha1::{Digest, Sha1};

/// The magic header every Acestream transport file starts with.
pub const TRANSPORT_MAGIC: &[u8] = b"AceStreamTransport";

fn sha1_bytes(bytes: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(bytes);
    h.finalize().into()
}

/// SHA-1 of the whole wrapped transport file, including the `AceStreamTransport`
/// magic and encrypted body. This is a transport-file identifier, not the swarm
/// infohash used in peer handshakes.
pub fn transport_file_hash(bytes: &[u8]) -> [u8; 20] {
    sha1_bytes(bytes)
}

/// Fallible form of [`infohash_of_transport`].
pub fn try_infohash_of_transport(bytes: &[u8]) -> Result<[u8; 20]> {
    let d = crate::transport::decode_transport(bytes)?;
    infohash_of_descriptor(&d.raw)
}

/// The official Acestream swarm infohash for a live transport.
///
/// Ground truth from `Transport.load_transport_file_from_string(...).get_infohash()`:
/// `SHA1(bencode([[name, v], [authmethod, v], [pubkey, v], [piece_length, v],
/// [chunk_length, v], [bitrate, v]]))`.
pub fn infohash_of_transport(bytes: &[u8]) -> [u8; 20] {
    try_infohash_of_transport(bytes).expect("valid Acestream transport descriptor")
}

/// Compute the official swarm infohash from a decoded descriptor dict.
/// Provider-backed transports append `provider_key` and `provider_sid`, when present,
/// after the base fields. Omitting them hashes a different swarm, even though the
/// descriptor still decodes and displays the correct channel metadata.
pub fn infohash_of_descriptor(descriptor: &Bencode) -> Result<[u8; 20]> {
    let mut pairs = Vec::new();
    for key in [
        b"name".as_slice(),
        b"authmethod".as_slice(),
        b"pubkey".as_slice(),
        b"piece_length".as_slice(),
        b"chunk_length".as_slice(),
        b"bitrate".as_slice(),
    ] {
        let value = descriptor
            .get(key)
            .ok_or(WireError::Invalid("missing descriptor infohash field"))?;
        pairs.push(Bencode::List(vec![
            Bencode::Bytes(key.to_vec()),
            value.clone(),
        ]));
    }
    for key in [b"provider_key".as_slice(), b"provider_sid".as_slice()] {
        if let Some(value) = descriptor.get(key) {
            pairs.push(Bencode::List(vec![
                Bencode::Bytes(key.to_vec()),
                value.clone(),
            ]));
        }
    }
    Ok(sha1_bytes(&Bencode::List(pairs).encode()))
}

/// True iff `bytes` begins with the Acestream transport magic.
pub fn is_transport_file(bytes: &[u8]) -> bool {
    bytes.starts_with(TRANSPORT_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_descriptor() -> Bencode {
        Bencode::parse(
            include_bytes!("../../../tests/vectors/provider-descriptor.bencode")
                .strip_suffix(b"\n")
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn provider_descriptor_hashes_the_extended_wire_preimage() {
        // Synthetic descriptor with the provider fields after the six base fields.
        // The order is protocol order, not the descriptor dictionary's sort order.
        let preimage = b"ll4:name7:fixtureel10:authmethod3:RSAel6:pubkey3:pubel12:piece_lengthi131072eel12:chunk_lengthi16384eel7:bitratei100000eel12:provider_key3:keyel12:provider_sid3:sidee";
        assert_eq!(
            infohash_of_descriptor(&provider_descriptor()).unwrap(),
            sha1_bytes(preimage),
        );
    }

    #[test]
    fn provider_identity_separates_otherwise_identical_streams() {
        let descriptor = provider_descriptor();
        let original = infohash_of_descriptor(&descriptor).unwrap();
        for field in [b"provider_key".as_slice(), b"provider_sid".as_slice()] {
            let mut changed = descriptor.clone();
            let Bencode::Dict(ref mut fields) = changed else {
                unreachable!()
            };
            fields.insert(field.to_vec(), Bencode::Bytes(b"other".to_vec()));
            assert_ne!(infohash_of_descriptor(&changed).unwrap(), original);
        }
    }

    #[test]
    fn each_optional_provider_field_is_appended_in_protocol_order() {
        let Bencode::Dict(mut fields) = provider_descriptor() else {
            unreachable!()
        };
        fields.remove(b"provider_key".as_slice());
        fields.remove(b"provider_sid".as_slice());
        let base = b"ll4:name7:fixtureel10:authmethod3:RSAel6:pubkey3:pubel12:piece_lengthi131072eel12:chunk_lengthi16384eel7:bitratei100000ee";
        for (field, encoded) in [
            (
                b"provider_key".as_slice(),
                b"l12:provider_key3:keye".as_slice(),
            ),
            (
                b"provider_sid".as_slice(),
                b"l12:provider_sid3:keye".as_slice(),
            ),
        ] {
            let mut only_one = fields.clone();
            only_one.insert(field.to_vec(), Bencode::Bytes(b"key".to_vec()));
            let preimage = [base.as_slice(), encoded, b"e"].concat();
            assert_eq!(
                infohash_of_descriptor(&Bencode::Dict(only_one)).unwrap(),
                sha1_bytes(&preimage),
            );
        }
        let legacy = [base.as_slice(), b"e"].concat();
        assert_eq!(
            infohash_of_descriptor(&Bencode::Dict(fields)).unwrap(),
            sha1_bytes(&legacy),
            "ordinary descriptors retain their six-field hash",
        );
    }
}
