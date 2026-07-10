//! Pipe protocol messages (docs/relay/pipe.md), shared byte-for-byte by the
//! relay server and the relay client.
//!
//! Two planes:
//!
//! 1. **Control plane** — [`PipeSignal`] (`Invite`/`Accept`/`Reject`) rides the
//!    existing E2E `Message` channel (sealed, `live=true`). It brokers the
//!    rendezvous: a single-use `connection_id`, the negotiated suite/compression,
//!    and the opaque handshake material.
//! 2. **Data plane** — [`PipeChallenge`] / [`PipeAuth`] are the first two frames
//!    on the new `/v1/pipe` WebSocket. They authenticate each peer to the relay
//!    (signature + namespace membership + cross-dest) so the relay can match the
//!    two sides and splice opaque ciphertext.
//!
//! Wire format is **MsgPack** (`rmp-serde`, named maps for forward-compat). Byte
//! fields are carried as `Vec<u8>` and length-validated by the consumer via
//! [`to_array`]; the relay decodes [`PipeAuth`] strictly (it is the only
//! untrusted input it deserializes).
//!
//! **Forward-compat:** the handshake material is an opaque blob keyed by
//! [`PipeSuite`]; adding a Noise suite (for client↔client) is a new variant with
//! the same wire shape, not a schema change. The control plane is symmetric
//! (initiator/responder by role in the exchange, never agent-vs-client).

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Handshake suite discriminator. v1 ships only [`PipeSuite::X25519Sealed`]:
/// an ephemeral X25519 exchange whose authenticity rests on the surrounding E2E
/// channel that carries the signaling. A future `Noise*` suite (mutual-auth,
/// for client↔client without a pre-shared key) is a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipeSuite {
    #[serde(rename = "x25519-sealed")]
    X25519Sealed,
}

/// Per-direction compression codec negotiated in the handshake. v1 advertises
/// and selects `None` only; `Zlib` is reserved so the negotiation exists for
/// forward-compat (stateful deflate is deferred, see pipe.md §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipeCompress {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "zlib")]
    Zlib,
}

/// `pipe_invite` (initiator → peer), sealed over the E2E channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipeInvite {
    /// 32-byte random single-use rendezvous key. NOT a security boundary on its
    /// own (the data-plane signature is).
    pub connection_id: Vec<u8>,
    pub suite: PipeSuite,
    /// Opaque, suite-defined handshake material. For `X25519Sealed` this is the
    /// initiator's 32-byte ephemeral X25519 public key.
    pub handshake: Vec<u8>,
    /// App-level discriminator so the responder can route/accept by purpose.
    pub stream_type: String,
    /// Codecs the initiator supports (responder selects one).
    pub compress: Vec<PipeCompress>,
    /// Arbitrary app-defined headers (filename, size, filters, …).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// `pipe_accept` (peer → initiator), sealed over the E2E channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipeAccept {
    pub connection_id: Vec<u8>,
    pub suite: PipeSuite,
    /// Responder's opaque handshake material (32-byte ephemeral X25519 pub for
    /// `X25519Sealed`).
    pub handshake: Vec<u8>,
    /// The single codec the responder selected from the invite's list.
    pub compress: PipeCompress,
}

/// `pipe_reject` (peer → initiator): the responder declined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipeReject {
    pub connection_id: Vec<u8>,
    pub reason: String,
}

/// A control-plane signaling message. Externally tagged so a single MsgPack blob
/// is self-describing (`{ "Invite": {…} }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipeSignal {
    Invite(PipeInvite),
    Accept(PipeAccept),
    Reject(PipeReject),
}

/// Data-plane frame #1: the relay speaks first with a fresh challenge nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipeChallenge {
    /// 32 random bytes.
    pub nonce: Vec<u8>,
}

/// Data-plane frame #2: the peer's reply, authenticating it to the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipeAuth {
    /// Echoes the rendezvous key from the signaling.
    pub connection_id: Vec<u8>,
    /// This peer's ed25519 public key (32B).
    pub pubkey: Vec<u8>,
    /// `SHA256(peer_ed25519_pub)` (32B): the intended counterparty.
    pub dest: Vec<u8>,
    /// Raw 32-byte namespace_id this pipe belongs to.
    pub namespace_id: Vec<u8>,
    /// `sign_ed25519(priv, PIPE_AUTH_DOMAIN ‖ 0x00 ‖ nonce ‖ connection_id)` (64B).
    pub signature: Vec<u8>,
}

/// Serialize a pipe message to MsgPack (named maps; stable across field reorder
/// / additive fields). Infallible for these plain structs.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    rmp_serde::to_vec_named(v).expect("MsgPack encode of a plain struct is infallible")
}

/// Deserialize a pipe message from MsgPack. `Err` on malformed input.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, rmp_serde::decode::Error> {
    rmp_serde::from_slice(bytes)
}

/// View a byte slice as a fixed-size array, or `None` on length mismatch. Used
/// to validate every length-pinned `Vec<u8>` field (pubkeys 32B, signature 64B).
pub fn to_array<const N: usize>(v: &[u8]) -> Option<[u8; N]> {
    if v.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(v);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_round_trips_externally_tagged() {
        let invite = PipeSignal::Invite(PipeInvite {
            connection_id: vec![0x11; 32],
            suite: PipeSuite::X25519Sealed,
            handshake: vec![0x22; 32],
            stream_type: "log".into(),
            compress: vec![PipeCompress::None, PipeCompress::Zlib],
            headers: BTreeMap::from([("path".into(), "/var/log/x".into())]),
        });
        let bytes = encode(&invite);
        let back: PipeSignal = decode(&bytes).expect("decode");
        assert_eq!(invite, back);
    }

    #[test]
    fn accept_and_reject_round_trip() {
        let accept = PipeAccept {
            connection_id: vec![0x33; 32],
            suite: PipeSuite::X25519Sealed,
            handshake: vec![0x44; 32],
            compress: PipeCompress::None,
        };
        assert_eq!(accept, decode::<PipeAccept>(&encode(&accept)).unwrap());

        let reject = PipeReject { connection_id: vec![0x55; 32], reason: "busy".into() };
        assert_eq!(reject, decode::<PipeReject>(&encode(&reject)).unwrap());
    }

    #[test]
    fn auth_round_trips_and_length_validates() {
        let auth = PipeAuth {
            connection_id: vec![1; 32],
            pubkey: vec![2; 32],
            dest: vec![3; 32],
            namespace_id: vec![4; 32],
            signature: vec![5; 64],
        };
        let back: PipeAuth = decode(&encode(&auth)).expect("decode");
        assert_eq!(auth, back);
        assert!(to_array::<32>(&back.pubkey).is_some());
        assert!(to_array::<64>(&back.signature).is_some());
        assert!(to_array::<32>(&back.signature).is_none());
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode::<PipeAuth>(&[0xff, 0x00, 0x13, 0x37]).is_err());
    }

    #[test]
    fn headers_default_when_absent() {
        // An invite encoded without headers (older/minimal sender) still decodes.
        #[derive(Serialize)]
        struct Minimal {
            connection_id: Vec<u8>,
            suite: PipeSuite,
            handshake: Vec<u8>,
            stream_type: String,
            compress: Vec<PipeCompress>,
        }
        let m = Minimal {
            connection_id: vec![0; 32],
            suite: PipeSuite::X25519Sealed,
            handshake: vec![0; 32],
            stream_type: "x".into(),
            compress: vec![PipeCompress::None],
        };
        let invite: PipeInvite = decode(&rmp_serde::to_vec_named(&m).unwrap()).expect("decode");
        assert!(invite.headers.is_empty());
    }
}
