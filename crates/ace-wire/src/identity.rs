//! Node identity: the Ed25519 keypair + handshake-signature scheme that peers require
//! before they will unchoke us (see `docs/protocol/notes/17-node-signature-cracked.md`).
//!
//! Scheme (verified 6/6 against live engine handshakes):
//! ```text
//! digest32  = SHA256( bencode(handshake_dict, signature := 64 × 0x00) )   # canonical
//! signature = Ed25519_detached_sign(secret_key, digest32)
//! node_id   = Ed25519 public key
//! ```
//! `node_id` is self-generated — we mint our own keypair, no engine key needed.

use crate::bencode::Bencode;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Length of the `signature` field placeholder used while computing the digest.
pub const SIGNATURE_LEN: usize = 64;

/// A node identity (Ed25519 keypair). `node_id` is the public key.
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// Deterministically derive an identity from a 32-byte seed (the engine's
    /// `device.key` is such a seed; ours can be any 32 random bytes).
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Identity {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// Mint a fresh random identity.
    pub fn generate() -> Self {
        // Fill the Ed25519 seed directly so RSA's rand 0.8 dependency does not
        // have to implement ed25519-dalek's newer rand_core traits.
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
        Self::from_seed(seed)
    }

    /// The `node_id` peers see: the 32-byte Ed25519 public key.
    pub fn node_id(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// Ed25519 detached signature over `msg` (we feed it the 32-byte digest).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }
}

/// Compute the 32-byte digest that gets signed: canonical bencode of the handshake dict
/// with the `signature` key forced to 64 zero bytes, then SHA-256.
pub fn handshake_digest(fields: &BTreeMap<Vec<u8>, Bencode>) -> [u8; 32] {
    let mut d = fields.clone();
    d.insert(
        b"signature".to_vec(),
        Bencode::Bytes(vec![0u8; SIGNATURE_LEN]),
    );
    let encoded = Bencode::Dict(d).encode();
    let mut h = Sha256::new();
    h.update(&encoded);
    h.finalize().into()
}

/// Verify a node handshake: recompute the digest from `fields` (its own `signature` value
/// is ignored — replaced with zeros) and check `signature` against `node_id`.
pub fn verify_handshake(
    node_id: &[u8; 32],
    signature: &[u8; 64],
    fields: &BTreeMap<Vec<u8>, Bencode>,
) -> bool {
    let digest = handshake_digest(fields);
    let vk = match VerifyingKey::from_bytes(node_id) {
        Ok(v) => v,
        Err(_) => return false,
    };
    vk.verify(&digest, &Signature::from_bytes(signature))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict_of(b: &Bencode) -> BTreeMap<Vec<u8>, Bencode> {
        match b {
            Bencode::Dict(d) => d.clone(),
            _ => panic!("not a dict"),
        }
    }

    /// Known-answer against a real engine extended handshake (note 17). Proves the
    /// digest formula + node_id-as-pubkey by Ed25519-verifying the captured signature.
    #[test]
    fn verifies_live_engine_handshake() {
        let hexstr =
            include_str!("../../../tests/vectors/node-identity/engine-extended-handshake.hex")
                .trim();
        let raw = hex::decode(hexstr).unwrap();
        let dict = dict_of(&Bencode::parse(&raw).unwrap());

        let node_id: [u8; 32] = dict[b"node_id".as_slice()]
            .as_bytes()
            .unwrap()
            .try_into()
            .unwrap();
        let signature: [u8; 64] = dict[b"signature".as_slice()]
            .as_bytes()
            .unwrap()
            .try_into()
            .unwrap();

        assert!(verify_handshake(&node_id, &signature, &dict));
        // tampering a signed field must break verification
        let mut tampered = dict.clone();
        tampered.insert(b"ts".to_vec(), Bencode::Int(999999));
        assert!(!verify_handshake(&node_id, &signature, &tampered));
    }

    #[test]
    fn from_seed_is_deterministic() {
        let seed = [7u8; 32];
        // Independent Ed25519 vector: a dependency upgrade must not rotate
        // the public identity derived from an existing persisted seed.
        let expected = [
            234, 74, 108, 99, 226, 156, 82, 10, 190, 245, 80, 123, 19, 46, 197, 249, 149, 71, 118,
            174, 190, 190, 123, 146, 66, 30, 234, 105, 20, 70, 210, 44,
        ];
        assert_eq!(Identity::from_seed(seed).node_id(), expected);
    }

    /// RFC 8032 section 7.1, TEST 1 (empty message): fixed independent output
    /// bytes catch signing changes that a same-library roundtrip cannot detect.
    /// https://www.rfc-editor.org/rfc/rfc8032#section-7.1
    #[test]
    fn sign_matches_rfc8032_ed25519_test_1() {
        let seed = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        let expected_public_key = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        let expected_signature = [
            0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e,
            0x82, 0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65,
            0x22, 0x49, 0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e,
            0x39, 0x70, 0x1c, 0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24,
            0x65, 0x51, 0x41, 0x43, 0x8e, 0x7a, 0x10, 0x0b,
        ];
        let identity = Identity::from_seed(seed);
        assert_eq!(identity.node_id(), expected_public_key);
        assert_eq!(identity.sign(b""), expected_signature);
    }

    #[test]
    fn generated_identity_preserves_its_seed_and_signatures() {
        let generated = Identity::generate();
        let restored = Identity::from_seed(generated.key.to_bytes());
        let message = b"identity persistence regression";
        assert_eq!(generated.node_id(), restored.node_id());
        assert_eq!(generated.sign(message), restored.sign(message));
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let id = Identity::generate();
        let mut fields: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        fields.insert(b"node_id".to_vec(), Bencode::Bytes(id.node_id().to_vec()));
        fields.insert(b"ts".to_vec(), Bencode::Int(749872));
        fields.insert(b"p".to_vec(), Bencode::Int(8621));

        let digest = handshake_digest(&fields);
        let sig = id.sign(&digest);
        fields.insert(b"signature".to_vec(), Bencode::Bytes(sig.to_vec()));

        assert!(verify_handshake(&id.node_id(), &sig, &fields));
    }
}
