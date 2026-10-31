//! GitHub's particulars: its endpoints and the shape of `GET /user`.

use super::{App, Provider, ProviderUser};
use crate::error::{Error, Result};

/// The app registered at GitHub (README, Development), on GitHub's own
/// endpoints. No scope: `/user` answers with the public profile, which has
/// the id and the login, and kiewer wants nothing else from GitHub.
#[must_use]
pub fn app(client_id: String, client_secret: String) -> App {
    App {
        provider: Provider::GitHub,
        client_id,
        client_secret,
        auth_url: "https://github.com/login/oauth/authorize".to_owned(),
        token_url: "https://github.com/login/oauth/access_token".to_owned(),
        user_url: "https://api.github.com/user".to_owned(),
        scopes: Vec::new(),
    }
}

/// The person behind `GET /user`: the numeric `id`, which survives a
/// rename, and the `login`.
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
    Ok(ProviderUser {
        id: id.to_string(),
        login: login.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_is_the_id_and_the_login() {
        let body = serde_json::json!({"login": "octocat", "id": 583_231, "name": "The Octocat"});
        assert_eq!(
            parse_user(&body).unwrap(),
            ProviderUser {
                id: "583231".to_owned(),
                login: "octocat".to_owned()
            }
        );
    }

    #[test]
    fn a_body_without_an_id_is_refused() {
        let err = parse_user(&serde_json::json!({"login": "octocat"})).unwrap_err();
        assert!(matches!(err, Error::OAuth { .. }), "{err}");
    }
}
