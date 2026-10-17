use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

use super::{DecodeError, Field, ObjectKind, Reader, Record, fields, tagged};
use crate::id::hex;
use crate::{AcceptanceId, ParseIdError, RevisionId, StatementId, TargetId, TreeId};

pub use ed25519_dalek::Signature;

/// One landing on a target, signed by the target's authority.
///
/// Acceptances form a hash chain per target: each names the one before it,
/// and its sequence number is its position in the target's log.
///
/// In JSON, the body's fields and the signature, in hex.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Acceptance {
    #[serde(flatten)]
    body: AcceptanceBody,
    #[serde(with = "signature_hex")]
    #[schema(value_type = String)]
    signature: Signature,
}

fields!(Acceptance { body, signature });

impl Record for Acceptance {
    const KIND: ObjectKind = ObjectKind::Acceptance;
}

impl Acceptance {
    /// What landed, and where.
    #[must_use]
    pub fn body(&self) -> &AcceptanceBody {
        &self.body
    }

    /// The authority's signature of the body.
    #[must_use]
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Checks that `authority` signed this acceptance.
    ///
    /// # Errors
    ///
    /// Fails if the signature is not `authority`'s signature of the body.
    pub fn verify(&self, authority: &NodePublicKey) -> Result<(), BadSignature> {
        authority
            .0
            .verify_strict(&self.body.signed_bytes(), &self.signature)
            .map_err(BadSignature)
    }
}

/// An acceptance without its signature.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct AcceptanceBody {
    /// The target landed on.
    pub target: TargetId,
    /// The position in the target's log, from 0.
    pub seq: u64,
    /// The acceptance before this one, absent only for the first.
    pub prev: Option<AcceptanceId>,
    /// The target's snapshot after this landing.
    pub state: TreeId,
    /// What landed.
    pub landed: Vec<Source>,
    /// The statements the policy relied on.
    pub evidence: Vec<StatementId>,
    /// The acceptance of the `policy` target in force when this one landed.
    /// Absent only for the first acceptance of `policy`, which nothing
    /// precedes.
    pub policy: Option<AcceptanceId>,
}

fields!(AcceptanceBody {
    target,
    seq,
    prev,
    state,
    landed,
    evidence,
    policy,
});

impl AcceptanceBody {
    /// Signs the body as `authority`.
    #[must_use]
    pub fn sign(self, authority: &NodeKey) -> Acceptance {
        let signature = authority.0.sign(&self.signed_bytes());
        Acceptance {
            body: self,
            signature,
        }
    }

    /// The bytes signed: the acceptance's encoding without its signature,
    /// which starts with the object kind, so the signature can't be passed
    /// off as one of anything else.
    fn signed_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![ObjectKind::Acceptance as u8];
        self.write(&mut bytes);
        bytes
    }
}

/// What an acceptance landed.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A revision of a change.
    Revision(RevisionId),
    /// Another target, as of one of its acceptances.
    Target(AcceptanceId),
}

tagged!(Source {
    Revision = 1,
    Target = 2,
});

impl Field for Signature {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_bytes());
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        reader.array().map(|bytes| Self::from_bytes(&bytes))
    }
}

/// Signatures as lowercase hex, in JSON.
mod signature_hex {
    use serde::{Deserialize, Deserializer, Serializer, de};

    use super::{Signature, hex};

    pub fn serialize<S: Serializer>(
        signature: &Signature,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::string(&signature.to_bytes()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Signature, D::Error> {
        let text = String::deserialize(deserializer)?;
        hex::decode::<{ ed25519_dalek::SIGNATURE_LENGTH }>(&text)
            .map(|bytes| Signature::from_bytes(&bytes))
            .ok_or_else(|| de::Error::custom(format!("invalid signature `{text}`")))
    }
}

/// A node's ed25519 key pair. As a target's authority, the node signs the
/// acceptances it writes.
pub struct NodeKey(SigningKey);

impl NodeKey {
    /// Generates a new key pair.
    ///
    /// # Errors
    ///
    /// Fails if the operating system has no randomness to give.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut secret = [0; ed25519_dalek::SECRET_KEY_LENGTH];
        getrandom::fill(&mut secret)?;
        Ok(Self::from_bytes(&secret))
    }

    /// Rebuilds a key pair from its secret key.
    #[must_use]
    pub fn from_bytes(secret: &[u8; ed25519_dalek::SECRET_KEY_LENGTH]) -> Self {
        Self(SigningKey::from_bytes(secret))
    }

    /// The secret key, to keep the key pair somewhere safe.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; ed25519_dalek::SECRET_KEY_LENGTH] {
        self.0.to_bytes()
    }

    /// The public half, which verifies the node's signatures.
    #[must_use]
    pub fn public(&self) -> NodePublicKey {
        NodePublicKey(self.0.verifying_key())
    }
}

/// Never prints the secret key.
impl NodeKey {
    /// Signs `bytes`, which must start with a domain of their own, so they
    /// never pass for anything else this key signs.
    pub(crate) fn sign(&self, bytes: &[u8]) -> Signature {
        self.0.sign(bytes)
    }
}

impl NodePublicKey {
    /// Whether `signature` is this key's over `bytes`.
    pub(crate) fn verifies(&self, bytes: &[u8], signature: &Signature) -> bool {
        self.0.verify_strict(bytes, signature).is_ok()
    }
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("NodeKey").field(&self.public()).finish()
    }
}

/// A node's public key, displayed as lowercase hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodePublicKey(VerifyingKey);

impl fmt::Display for NodePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::string(self.0.as_bytes()))
    }
}

/// Parses the hex [`NodePublicKey`] displays as.
impl FromStr for NodePublicKey {
    type Err = ParseIdError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        hex::decode(input)
            .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
            .map(Self)
            .ok_or_else(|| ParseIdError {
                what: "node public key",
                input: input.to_owned(),
            })
    }
}

/// A signature that the claimed signer did not make.
#[derive(Debug, thiserror::Error)]
#[error("the signature does not match the signer's key")]
pub struct BadSignature(#[source] ed25519_dalek::SignatureError);
