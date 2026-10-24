//! Live-source authentication (B0): the RSA identity + per-piece signing scheme a broadcast
//! source uses to let consumers verify pieces genuinely come from it.
//!
//! **Signing scheme — CRACKED (docs/protocol/notes/27-b0-signing-cracked.md), not just the
//! identity.** Recovered by hooking the official engine's own PyCryptodome hash calls
//! (`SHA1_update`/`SHA1_digest` — the engine uses PyCryptodome, not OpenSSL, for this) while
//! it signed real pieces, then confirming the recovered preimage/signature against a real
//! captured piece with `sig.pow(e) mod n`:
//!
//! - **Preimage:** `SHA1(piece_bytes[0 .. piece_length - sig_len])` — i.e. the piece's own
//!   payload with its trailing signature-sized region excluded.
//! - **Signature:** standard **RSASSA-PKCS1-v1_5 with SHA1** (`sig = pow(pad(digest), d, n)`)
//!   — textbook, no custom padding. `sig_len` = the RSA modulus's byte length (96 for a
//!   768-bit key).
//! - **Placement:** the signature is **embedded in-band as the last `sig_len` bytes of the
//!   piece itself** — not a separate wire message. This is why the pure P2P relay path (S1/S2,
//!   `PieceStore`/`SeederSession`) never needed to know about it: a piece's bytes already
//!   *are* `payload || signature` end to end; relaying them verbatim (as we already do)
//!   carries the signature along for free. Only the **source** (origination, B1) needs this
//!   module to actually produce a valid signature instead of the old zero/placeholder tail.
//!
//! Confirmed against a real engine-produced `.sauth` file + two real captured pieces
//! (`tests/vectors/live-source-auth/piece-{0,1}.bin`) — `verify_piece` passes on both.
//! Committed interoperability fixtures retain only public keys and signed pieces;
//! signing and PEM round-trip tests generate ephemeral identities at runtime.
//!
//! Identity/transport details (from note 25, still accurate):
//! - The private key is plain **PKCS#1 PEM** (`-----BEGIN RSA PRIVATE KEY-----`).
//! - The transport descriptor's `pubkey` field is the matching **X.509 SubjectPublicKeyInfo
//!   DER** encoding (124 bytes for a 768-bit key) — verified byte-for-byte against
//!   `openssl rsa -pubout -outform DER` on a real captured `.sauth`.
//! - `authmethod` = literal ASCII `"RSA"`.

use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey};
use rsa::pkcs1v15::Pkcs1v15Sign;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha1::{Digest, Sha1};

/// Key size matching the real engine's own source-node-generated keys (note 25: a captured
/// `.sauth` was 768 bits). Not a protocol requirement — any RSA key verifies fine — but
/// matching it avoids being fingerprinted as an obviously-different implementation.
const KEY_BITS: usize = 768;

/// A broadcast source's live-auth RSA identity.
pub struct LiveSourceAuth {
    key: RsaPrivateKey,
}

impl LiveSourceAuth {
    /// Mint a fresh keypair.
    pub fn generate() -> Self {
        let key =
            RsaPrivateKey::new(&mut rand::thread_rng(), KEY_BITS).expect("RSA key generation");
        LiveSourceAuth { key }
    }

    /// Load an identity from its PKCS#1 PEM private-key text (the `.sauth` format).
    pub fn from_pkcs1_pem(pem: &str) -> Result<Self, crate::WireError> {
        let key = RsaPrivateKey::from_pkcs1_pem(pem)
            .map_err(|_| crate::WireError::Invalid("bad PKCS#1 RSA private key PEM"))?;
        Ok(LiveSourceAuth { key })
    }

    /// Serialize to PKCS#1 PEM text — the same format as a real engine's `.sauth` file.
    pub fn to_pkcs1_pem(&self) -> String {
        self.key
            .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
            .expect("PKCS#1 PEM encode")
            .to_string()
    }

    /// The DER-encoded X.509 SubjectPublicKeyInfo — exactly what the transport descriptor's
    /// `pubkey` field carries.
    pub fn pubkey_der(&self) -> Vec<u8> {
        let pubkey = RsaPublicKey::from(&self.key);
        pubkey
            .to_public_key_der()
            .expect("DER SPKI encode")
            .as_bytes()
            .to_vec()
    }

    /// The signature length in bytes — the RSA modulus's byte length (96 for a 768-bit key).
    /// This is exactly how many trailing bytes of each *wire piece* the signature occupies.
    pub fn signature_len(&self) -> usize {
        self.key.size()
    }

    /// Sign `payload` (the piece's real content, i.e. everything except where the signature
    /// will go): `SHA1(payload)` then RSASSA-PKCS1-v1_5. Returns exactly `signature_len()`
    /// bytes, appended as-is to `payload` to form the wire piece — see the module docs.
    pub fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let digest = Sha1::digest(payload);
        self.key
            .sign(Pkcs1v15Sign::new::<Sha1>(), &digest)
            .expect("RSA-PKCS1v15-SHA1 sign")
    }
}

/// Verify a live-source signature: `payload` is the piece's real content (everything before
/// the signature tail), `signature` is the trailing bytes, `pubkey_der` is the broadcaster's
/// DER SubjectPublicKeyInfo (the transport descriptor's `pubkey` field). Scheme: RSASSA-
/// PKCS1-v1_5 with SHA1 — see the module docs for how this was recovered.
pub fn verify_piece(pubkey_der: &[u8], payload: &[u8], signature: &[u8]) -> bool {
    LivePieceVerifier::from_der(pubkey_der)
        .is_some_and(|verifier| verifier.verify(payload, signature))
}

/// A source key parsed once for all pieces in a live stream.
pub(crate) struct LivePieceVerifier(RsaPublicKey);

impl LivePieceVerifier {
    pub(crate) fn from_der(der: &[u8]) -> Option<Self> {
        RsaPublicKey::from_public_key_der(der).ok().map(Self)
    }

    pub(crate) fn signature_len(&self) -> usize {
        self.0.size()
    }

    pub(crate) fn verify(&self, payload: &[u8], signature: &[u8]) -> bool {
        let digest = Sha1::digest(payload);
        self.0
            .verify(Pkcs1v15Sign::new::<Sha1>(), &digest, signature)
            .is_ok()
    }
}

/// The live-source signature length for a broadcaster's `pubkey` (the transport descriptor's
/// DER SubjectPublicKeyInfo) — the RSA modulus's byte length, i.e. how many trailing bytes of
/// each wire piece are the in-band signature (see the module docs / B0). Returns `None` if the
/// bytes aren't a parseable RSA public key.
pub fn signature_len_from_pubkey_der(pubkey_der: &[u8]) -> Option<usize> {
    RsaPublicKey::from_public_key_der(pubkey_der)
        .ok()
        .map(|k| k.size())
}

/// Split a wire piece into `(payload, signature)` given the signer's signature length —
/// the inverse of `sign`'s "append to payload" convention. `piece` must be at least
/// `sig_len` bytes.
pub fn split_piece(piece: &[u8], sig_len: usize) -> Option<(&[u8], &[u8])> {
    if piece.len() < sig_len {
        return None;
    }
    Some(piece.split_at(piece.len() - sig_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pubkey_der_round_trips_through_pem_reload() {
        let a = LiveSourceAuth::generate();
        let pem = a.to_pkcs1_pem();
        let b = LiveSourceAuth::from_pkcs1_pem(&pem).unwrap();
        assert_eq!(
            a.pubkey_der(),
            b.pubkey_der(),
            "same key reloaded from PEM -> same pubkey"
        );
    }

    #[test]
    fn pem_matches_real_engine_pkcs1_format() {
        let a = LiveSourceAuth::generate();
        let pem = a.to_pkcs1_pem();
        assert!(pem.starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(pem.trim_end().ends_with("-----END RSA PRIVATE KEY-----"));
    }

    #[test]
    fn rejects_garbage_pem() {
        assert!(LiveSourceAuth::from_pkcs1_pem("not a key").is_err());
    }

    /// The captured transport's public identity needs no retained private material.
    #[test]
    fn captured_transport_pubkey_has_expected_der_and_signature_geometry() {
        let der =
            include_bytes!("../../../tests/vectors/live-source-auth/transport-public-key.der");
        let key = RsaPublicKey::from_public_key_der(der).unwrap();
        assert_eq!(key.to_public_key_der().unwrap().as_bytes(), der);
        assert_eq!(signature_len_from_pubkey_der(der), Some(96));
        assert_eq!(
            LivePieceVerifier::from_der(der).unwrap().signature_len(),
            96
        );
    }

    #[test]
    fn split_piece_splits_off_the_trailing_signature() {
        let piece = [1u8, 2, 3, 4, 5];
        let (payload, sig) = split_piece(&piece, 2).unwrap();
        assert_eq!(payload, &[1, 2, 3]);
        assert_eq!(sig, &[4, 5]);
    }

    #[test]
    fn split_piece_rejects_a_piece_shorter_than_the_signature() {
        assert!(split_piece(&[1, 2], 5).is_none());
    }

    // Public-only interoperability evidence: the captured source's public key and
    // its original signed pieces. Private identities are unnecessary for verification.
    const CAPTURED_SOURCE_DER: &[u8] =
        include_bytes!("../../../tests/vectors/live-source-auth/public-key.der");

    fn captured_piece(idx: u32) -> Vec<u8> {
        let path = format!(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/vectors/live-source-auth/piece-{}.bin"
            ),
            idx
        );
        std::fs::read(path).expect("real captured piece vector")
    }

    #[test]
    fn signature_len_is_96_for_the_captured_768_bit_key() {
        assert_eq!(signature_len_from_pubkey_der(CAPTURED_SOURCE_DER), Some(96));
    }

    #[test]
    fn verify_piece_passes_on_real_captured_pieces() {
        let pubkey_der = CAPTURED_SOURCE_DER;
        let signature_len = signature_len_from_pubkey_der(pubkey_der).unwrap();
        for idx in [0, 1] {
            let piece = captured_piece(idx);
            let (payload, sig) = split_piece(&piece, signature_len).unwrap();
            assert!(
                verify_piece(pubkey_der, payload, sig),
                "piece {idx}: real captured signature must verify"
            );
        }
    }

    #[test]
    fn verify_piece_rejects_a_tampered_payload() {
        let pubkey_der = CAPTURED_SOURCE_DER;
        let signature_len = signature_len_from_pubkey_der(pubkey_der).unwrap();
        let piece = captured_piece(0);
        let (payload, sig) = split_piece(&piece, signature_len).unwrap();
        let mut tampered = payload.to_vec();
        tampered[0] ^= 0xFF;
        assert!(!verify_piece(pubkey_der, &tampered, sig));
    }

    #[test]
    fn generated_identity_signing_is_deterministic_and_survives_pem_reload() {
        let auth = LiveSourceAuth::generate();
        let reloaded = LiveSourceAuth::from_pkcs1_pem(&auth.to_pkcs1_pem()).unwrap();
        let pubkey_der = auth.pubkey_der();
        for payload in [
            b"synthetic first payload".as_slice(),
            b"synthetic second payload",
        ] {
            let signature = auth.sign(payload);
            assert_eq!(signature, auth.sign(payload));
            assert_eq!(signature, reloaded.sign(payload));
            assert!(verify_piece(&pubkey_der, payload, &signature));
            assert!(!verify_piece(&pubkey_der, b"tampered payload", &signature));
        }
    }

    #[test]
    fn cached_verifier_matches_der_verification_for_valid_corrupt_and_wrong_key_pieces() {
        let source = LiveSourceAuth::generate();
        let other = LiveSourceAuth::generate();
        let der = source.pubkey_der();
        let verifier = LivePieceVerifier::from_der(&der).unwrap();
        assert_eq!(verifier.signature_len(), source.signature_len());
        for payload in [
            b"synthetic first piece".as_slice(),
            b"synthetic second piece",
        ] {
            let valid = source.sign(payload);
            let mut corrupt = valid.clone();
            corrupt[0] ^= 1;
            for signature in [&valid, &corrupt, &other.sign(payload)] {
                assert_eq!(
                    verifier.verify(payload, signature),
                    verify_piece(&der, payload, signature)
                );
            }
            assert!(verifier.verify(payload, &valid));
            assert!(!verifier.verify(b"changed payload", &valid));
        }
        assert!(LivePieceVerifier::from_der(&[]).is_none());
        assert!(LivePieceVerifier::from_der(b"invalid DER").is_none());
    }
}
