//! Authenticating callers of the HTTP API.

use std::collections::BTreeMap;

use origin_shared::access::Identity;
use serde::Deserialize;

/// Turns credentials into identities. The server's users are one
/// implementation; external identity providers can be others.
pub trait Authenticator: Send + Sync {
    /// The identity a bearer token belongs to, or `None` if no one has it.
    fn authenticate(&self, token: &str) -> Option<Identity>;
}

/// Users with static tokens, from the server configuration:
///
/// ```toml
/// [users.alice]
/// token = "a long random secret"
/// groups = ["dev"]
/// ```
#[derive(Debug, Default, Deserialize)]
pub struct Users(BTreeMap<String, User>);

/// One user of [`Users`].
#[derive(Debug, Deserialize)]
struct User {
    token: String,
    #[serde(default)]
    groups: Vec<String>,
}

impl Authenticator for Users {
    fn authenticate(&self, token: &str) -> Option<Identity> {
        // Comparing hashes, which compare in constant time, keeps response
        // times from revealing how much of a token matched.
        let token = blake3::hash(token.as_bytes());
        self.0
            .iter()
            .find(|(_, user)| blake3::hash(user.token.as_bytes()) == token)
            .map(|(name, user)| Identity {
                user: name.clone(),
                groups: user.groups.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_users_by_token() {
        let users: Users = toml::from_str(
            "[alice]\ntoken = \"a-secret\"\ngroups = [\"dev\"]\n[bob]\ntoken = \"b-secret\"\n",
        )
        .unwrap();
        let alice = users.authenticate("a-secret").unwrap();
        assert_eq!(alice.user, "alice");
        assert_eq!(alice.groups, ["dev"]);
        assert_eq!(users.authenticate("b-secret").unwrap().user, "bob");
        assert_eq!(users.authenticate("a-secre"), None);
    }
}
