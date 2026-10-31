//! GitHub's particulars: its endpoints and the shape of `GET /user`.

use super::{App, Credentials, Provider, ProviderUser};
use crate::error::{Error, Result};

/// The app registered at GitHub (README, Development), on GitHub's own
/// endpoints. No scope: `/user` answers with the public profile, which has
/// the id and the login, and kiewer wants nothing else from GitHub.
#[must_use]
pub fn app(credentials: Credentials) -> App {
    App {
        provider: Provider::GitHub,
        credentials,
        auth_url: "https://github.com/login/oauth/authorize".to_owned(),
        token_url: "https://github.com/login/oauth/access_token".to_owned(),
        user_url: "https://api.github.com/user".to_owned(),
        scopes: Vec::new(),
    }
}

/// The person behind `GET /user`: the numeric `id`, which survives a
/// rename, the `login`, and the public profile's `email` when there is
/// one (GitHub shows only an address it has verified).
pub(super) fn parse_user(body: &serde_json::Value) -> Result<ProviderUser> {
    let missing = |field: &str| Error::OAuth {
        provider: Provider::GitHub,
        reason: format!("the user endpoint answered without {field}"),
    };
    let id = body
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| missing("a numeric id"))?;
    let login = body
        .get("login")
        .and_then(serde_json::Value::as_str)
        .filter(|login| !login.is_empty())
        .ok_or_else(|| missing("a login"))?;
    let email = body
        .get("email")
        .and_then(serde_json::Value::as_str)
        .filter(|email| !email.is_empty())
        .map(ToOwned::to_owned);
    Ok(ProviderUser {
        id: id.to_string(),
        login: login.to_owned(),
        email,
    })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(&serde_json::json!({"login": "octocat", "id": 583_231, "name": "The Octocat", "email": null}), None; "no public email")]
    #[test_case(&serde_json::json!({"login": "octocat", "id": 583_231, "email": "octocat@github.com"}), Some("octocat@github.com"); "a public email")]
    fn the_user_is_the_id_the_login_and_the_public_email(
        body: &serde_json::Value,
        email: Option<&str>,
    ) {
        assert_eq!(
            parse_user(body).unwrap(),
            ProviderUser {
                id: "583231".to_owned(),
                login: "octocat".to_owned(),
                email: email.map(ToOwned::to_owned),
            }
        );
    }

    #[test]
    fn a_body_without_an_id_is_refused() {
        let err = parse_user(&serde_json::json!({"login": "octocat"})).unwrap_err();
        assert!(matches!(err, Error::OAuth { .. }), "{err}");
    }
}
