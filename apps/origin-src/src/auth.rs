//! Authenticating callers of the HTTP API by their bearer token: a token
//! the server signed, scoped and short-lived (see [`Token`]), or a static
//! one from its configuration, checked by an [`Authenticator`].

use std::collections::BTreeMap;
use std::fmt;

use origin_shared::Principal;
use origin_shared::access::{InvalidToken, Token};
use serde::Deserialize;

use crate::permissions::Scope;
use crate::service::{Caller, Node};

/// Turns static credentials into principals. The server configuration's
/// tokens are one implementation; external identity providers can be
/// others.
pub trait Authenticator: Send + Sync + fmt::Debug {
    /// The principal a bearer token belongs to, or `None` if no one has it.
    fn authenticate(&self, token: &str) -> Option<Principal>;
}

/// Static tokens, from the server configuration:
///
/// ```toml
/// [tokens]
/// "user:alice" = "a long random secret"
/// "node:ci-1"  = "another one"
/// ```
#[derive(Default, Deserialize)]
pub struct StaticTokens(BTreeMap<Principal, String>);

impl fmt::Debug for StaticTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the secrets.
        f.debug_list().entries(self.0.keys()).finish()
    }
}

impl Authenticator for StaticTokens {
    fn authenticate(&self, token: &str) -> Option<Principal> {
        // Comparing hashes, which compare in constant time, keeps response
        // times from revealing how much of a token matched.
        let token = blake3::hash(token.as_bytes());
        self.0
            .iter()
            .find(|(_, known)| blake3::hash(known.as_bytes()) == token)
            .map(|(principal, _)| principal.clone())
    }
}

/// How a server tells its callers apart: its node, which verifies the
/// tokens it signed, and its static credentials.
#[derive(Debug)]
pub struct Credentials {
    node: Node,
    authenticator: Box<dyn Authenticator>,
}

impl Credentials {
    /// Callers of `node`, whose static credentials `authenticator` checks.
    #[must_use]
    pub fn new(node: Node, authenticator: Box<dyn Authenticator>) -> Self {
        Self {
            node,
            authenticator,
        }
    }

    /// The caller an `Authorization` header's value stands for: anonymous
    /// without one.
    ///
    /// # Errors
    ///
    /// Fails if it isn't a bearer token, or no valid one.
    pub fn caller(&self, authorization: Option<&str>) -> Result<Caller, InvalidToken> {
        let Some(authorization) = authorization else {
            return Ok(Caller::Anonymous);
        };
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or(InvalidToken::Malformed)?;
        if Token::is_signed(token) {
            let token = Token::verify(token, &self.node.key.public())?;
            return Ok(Caller::Principal {
                principal: token.principal,
                scope: Some(Scope {
                    capabilities: token.scope,
                    expires: token.expires,
                }),
            });
        }
        let principal = self
            .authenticator
            .authenticate(token)
            .ok_or(InvalidToken::Unknown)?;
        Ok(Caller::Principal {
            principal,
            scope: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use origin_shared::NodeKey;

    use super::*;

    #[test]
    fn tells_callers_apart_by_token() {
        let tokens: StaticTokens =
            toml::from_str("\"user:alice\" = \"a-secret\"\n\"node:ci\" = \"c-secret\"\n").unwrap();
        let node = Node {
            name: "hosted".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[5; 32])),
        };
        let token = Token::new(
            "user:bob".parse().unwrap(),
            vec!["write src".parse().unwrap()],
            Duration::from_mins(1),
        );
        let signed = token.sign(&node.key);
        let credentials = Credentials::new(node, Box::new(tokens));
        let caller = |header: Option<&str>| credentials.caller(header);
        let principal = |caller: Caller| caller.principal().map(ToString::to_string);

        assert_eq!(caller(None), Ok(Caller::Anonymous));
        assert_eq!(
            principal(caller(Some("Bearer a-secret")).unwrap()).as_deref(),
            Some("user:alice")
        );
        assert_eq!(
            principal(caller(Some("Bearer c-secret")).unwrap()).as_deref(),
            Some("node:ci")
        );
        let Ok(Caller::Principal { principal, scope }) = caller(Some(&format!("Bearer {signed}")))
        else {
            panic!("a signed token");
        };
        assert_eq!(principal.to_string(), "user:bob");
        let scope = scope.unwrap();
        assert_eq!(scope.capabilities[0].to_string(), "write src");
        assert_eq!(scope.expires, token.expires, "it carries its expiry");
        assert!(caller(Some("Bearer a-secre")).is_err());
        assert!(caller(Some("a-secret")).is_err(), "not a bearer token");
    }
}
