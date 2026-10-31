//! Discord's particulars: its endpoints and the shape of `GET /users/@me`
//! (docs/v1-scope.md, Accounts and access: the `identify` and `email`
//! scopes against `/users/@me`).

use super::{App, Credentials, Provider, ProviderUser};
use crate::error::{Error, Result};

/// The app registered at Discord (README, Development), on Discord's own
/// endpoints. `identify` opens `/users/@me`; `email` adds the address.
#[must_use]
pub fn app(credentials: Credentials) -> App {
    App {
        provider: Provider::Discord,
        credentials,
        auth_url: "https://discord.com/oauth2/authorize".to_owned(),
        token_url: "https://discord.com/api/oauth2/token".to_owned(),
        user_url: "https://discord.com/api/users/@me".to_owned(),
        scopes: vec!["identify".to_owned(), "email".to_owned()],
    }
}

/// The person behind `GET /users/@me`: the snowflake `id`, a string that
/// survives a rename, the `username`, and the `email` when Discord has
/// verified it (`verified`); an unverified one is nobody's yet.
pub(super) fn parse_user(body: &serde_json::Value) -> Result<ProviderUser> {
    let missing = |field: &str| Error::OAuth {
        provider: Provider::Discord,
        reason: format!("the user endpoint answered without {field}"),
    };
    let text = |field: &str| {
        body.get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let id = text("id").ok_or_else(|| missing("an id"))?;
    let login = text("username").ok_or_else(|| missing("a username"))?;
    let verified = body
        .get("verified")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(ProviderUser {
        id: id.to_owned(),
        login: login.to_owned(),
        email: text("email").filter(|_| verified).map(ToOwned::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_is_the_snowflake_the_username_and_the_verified_email() {
        let body = serde_json::json!({
            "id": "80351110224678912",
            "username": "Nelly",
            "discriminator": "0",
            "global_name": "Nelly the Great",
            "verified": true,
            "email": "nelly@discord.example"
        });
        assert_eq!(
            parse_user(&body).unwrap(),
            ProviderUser {
                id: "80351110224678912".to_owned(),
                login: "Nelly".to_owned(),
                email: Some("nelly@discord.example".to_owned()),
            }
        );
    }

    #[test]
    fn an_unverified_email_is_not_kept() {
        let body = serde_json::json!({
            "id": "80351110224678912",
            "username": "Nelly",
            "verified": false,
            "email": "nelly@discord.example"
        });
        assert_eq!(parse_user(&body).unwrap().email, None);
    }

    #[test]
    fn a_numeric_id_is_not_a_snowflake() {
        // Discord sends snowflakes as strings so JavaScript keeps every
        // digit; a number here is some other service's answer.
        let err =
            parse_user(&serde_json::json!({"id": 80_351_110, "username": "Nelly"})).unwrap_err();
        assert!(matches!(err, Error::OAuth { .. }), "{err}");
    }
}
