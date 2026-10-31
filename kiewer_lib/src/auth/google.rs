//! Google's particulars: its endpoints and the shape of the OpenID
//! Connect userinfo answer. Plain OAuth2 with a userinfo call, as the
//! other providers: the id token is not read, so no JWT validation.

use super::{App, Credentials, Provider, ProviderUser};
use crate::error::{Error, Result};

/// The app registered at Google (README, Development), on Google's own
/// endpoints. `openid` and `profile` give `sub` and the name, `email` the
/// address.
#[must_use]
pub fn app(credentials: Credentials) -> App {
    App {
        provider: Provider::Google,
        credentials,
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
        token_url: "https://oauth2.googleapis.com/token".to_owned(),
        user_url: "https://openidconnect.googleapis.com/v1/userinfo".to_owned(),
        scopes: vec![
            "openid".to_owned(),
            "email".to_owned(),
            "profile".to_owned(),
        ],
    }
}

/// The person behind the userinfo endpoint: `sub`, Google's stable id,
/// never the email, which changes hands; the `name` as the login, or the
/// email's local part when the profile has no name; the `email` when
/// `email_verified` says Google checked it.
pub(super) fn parse_user(body: &serde_json::Value) -> Result<ProviderUser> {
    let missing = |field: &str| Error::OAuth {
        provider: Provider::Google,
        reason: format!("the userinfo endpoint answered without {field}"),
    };
    let text = |field: &str| {
        body.get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let id = text("sub").ok_or_else(|| missing("a sub"))?;
    let email = text("email");
    let login = text("name")
        .or_else(|| email.and_then(|email| email.split('@').next()))
        .filter(|login| !login.is_empty())
        .ok_or_else(|| missing("a name or an email"))?;
    let verified = body
        .get("email_verified")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(ProviderUser {
        id: id.to_owned(),
        login: login.to_owned(),
        email: email.filter(|_| verified).map(ToOwned::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_is_sub_the_name_and_the_verified_email() {
        let body = serde_json::json!({
            "sub": "110248495921238986420",
            "name": "Jane Doe",
            "given_name": "Jane",
            "picture": "https://lh3.googleusercontent.com/a/photo",
            "email": "jane@example.com",
            "email_verified": true
        });
        assert_eq!(
            parse_user(&body).unwrap(),
            ProviderUser {
                id: "110248495921238986420".to_owned(),
                login: "Jane Doe".to_owned(),
                email: Some("jane@example.com".to_owned()),
            }
        );
    }

    #[test]
    fn the_id_is_never_the_email() {
        let body = serde_json::json!({"email": "jane@example.com", "email_verified": true});
        let err = parse_user(&body).unwrap_err();
        assert!(matches!(err, Error::OAuth { .. }), "{err}");
        assert!(err.to_string().contains("sub"), "{err}");
    }

    #[test]
    fn without_a_name_the_emails_local_part_is_the_login() {
        let body = serde_json::json!({
            "sub": "1",
            "email": "jane.doe@example.com",
            "email_verified": false
        });
        let user = parse_user(&body).unwrap();
        assert_eq!(user.login, "jane.doe");
        assert_eq!(user.email, None, "unverified: not kept");
    }
}
