//! Sign-in through an OAuth2 provider: the authorization-code exchange
//! with PKCE, then one call for who the person is. One module per
//! provider's particulars ([`github`]); the flow is shared, so Discord and
//! Google (docs/v1-scope.md, Accounts and access) add a variant, an
//! [`App`] and a user-parsing arm, nothing else.

pub mod github;

use std::fmt;
use std::str::FromStr;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A sign-in provider. The wire name (`github`) is the URL segment
/// (`/auth/github`) and the `identity.provider` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    GitHub,
}

impl Provider {
    /// Every provider, in code order.
    pub const ALL: [Provider; 1] = [Provider::GitHub];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::GitHub => "github",
        }
    }

    /// The name people see.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
        }
    }

    /// The person the provider's user endpoint describes, from its JSON.
    ///
    /// # Errors
    ///
    /// [`Error::OAuth`] when the body lacks the id or the login.
    pub fn parse_user(self, body: &serde_json::Value) -> Result<ProviderUser> {
        match self {
            Provider::GitHub => github::parse_user(body),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        Provider::ALL
            .into_iter()
            .find(|provider| provider.as_str() == text)
            .ok_or_else(|| Error::Malformed(format!("not a sign-in provider: {text:?}")))
    }
}

/// One provider's OAuth app registration and endpoints. The endpoints are
/// fields, not constants, so a test points them at a fake provider on
/// loopback.
#[derive(Debug, Clone)]
pub struct App {
    pub provider: Provider,
    pub client_id: String,
    pub client_secret: String,
    pub auth_url: String,
    pub token_url: String,
    /// The endpoint that says who the token's owner is.
    pub user_url: String,
    pub scopes: Vec<String>,
}

/// Who a provider says signed in: its own stable id for the person, and
/// the login it shows, which prefills the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUser {
    pub id: String,
    pub login: String,
}

/// A sign-in under way: where to send the browser, and what the callback
/// needs back from the browser's cookie to finish.
#[derive(Debug, Clone)]
pub struct Begun {
    /// The provider's authorization page, with the state and the PKCE
    /// challenge in its query.
    pub authorize_url: String,
    /// The CSRF state the provider echoes back; the callback refuses any
    /// other.
    pub state: String,
    /// The PKCE verifier the token exchange proves the challenge with.
    pub verifier: String,
}

/// The sign-in flow over every configured provider.
pub struct Auth {
    apps: Vec<App>,
    /// The origin browsers reach the server at, for the redirect URL the
    /// provider sends the browser back to.
    public_url: String,
    http: reqwest::Client,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("providers", &self.providers())
            .field("public_url", &self.public_url)
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// The flow for `apps`, with callbacks under `public_url` (no trailing
    /// slash).
    ///
    /// # Errors
    ///
    /// [`Error::Http`] when the HTTP client cannot be built.
    pub fn new(apps: Vec<App>, public_url: impl Into<String>) -> Result<Self> {
        // No redirects: the token endpoint answers in place, and following
        // one would carry the client secret somewhere else (the oauth2
        // crate's own advice).
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("kiewer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            apps,
            public_url: public_url.into(),
            http,
        })
    }

    /// The providers with an app registered, in [`Provider::ALL`] order.
    #[must_use]
    pub fn providers(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|provider| self.app(*provider).is_some())
            .collect()
    }

    fn app(&self, provider: Provider) -> Option<&App> {
        self.apps.iter().find(|app| app.provider == provider)
    }

    /// Where the provider sends the browser back to.
    #[must_use]
    pub fn callback_url(&self, provider: Provider) -> String {
        format!("{}/auth/{provider}/callback", self.public_url)
    }

    fn client(&self, app: &App) -> Result<Client> {
        let not_a_url = |what: &str, error: oauth2::url::ParseError| Error::OAuth {
            provider: app.provider,
            reason: format!("{what} is not a URL: {error}"),
        };
        Ok(BasicClient::new(ClientId::new(app.client_id.clone()))
            .set_client_secret(ClientSecret::new(app.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(app.auth_url.clone()).map_err(|e| not_a_url("auth_url", e))?)
            .set_token_uri(
                TokenUrl::new(app.token_url.clone()).map_err(|e| not_a_url("token_url", e))?,
            )
            .set_redirect_uri(
                RedirectUrl::new(self.callback_url(app.provider))
                    .map_err(|e| not_a_url("the callback URL", e))?,
            ))
    }

    /// Start a sign-in with `provider`: a fresh state and PKCE pair and the
    /// URL to send the browser to. `None` when the provider has no app
    /// registered.
    ///
    /// # Errors
    ///
    /// [`Error::OAuth`] when an endpoint or the public URL is not a URL.
    pub fn begin(&self, provider: Provider) -> Result<Option<Begun>> {
        let Some(app) = self.app(provider) else {
            return Ok(None);
        };
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = self
            .client(app)?
            .authorize_url(CsrfToken::new_random)
            .add_scopes(app.scopes.iter().cloned().map(Scope::new))
            .set_pkce_challenge(challenge)
            .url();
        Ok(Some(Begun {
            authorize_url: url.to_string(),
            state: state.into_secret(),
            verifier: verifier.into_secret(),
        }))
    }

    /// Finish a sign-in: trade `code` for a token with `verifier`, then ask
    /// the provider who the person is. `None` when the provider has no app
    /// registered.
    ///
    /// # Errors
    ///
    /// [`Error::OAuth`] when the provider refuses the code or answers
    /// without the person's id; [`Error::Http`] when it cannot be reached.
    pub async fn finish(
        &self,
        provider: Provider,
        code: String,
        verifier: String,
    ) -> Result<Option<ProviderUser>> {
        let Some(app) = self.app(provider) else {
            return Ok(None);
        };
        let token = self
            .client(app)?
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(PkceCodeVerifier::new(verifier))
            .request_async(&self.http)
            .await
            .map_err(|error| Error::OAuth {
                provider,
                reason: format!("the token exchange failed: {error}"),
            })?;
        let body: serde_json::Value = self
            .http
            .get(&app.user_url)
            .bearer_auth(token.access_token().secret())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        provider.parse_user(&body).map(Some)
    }
}

type Client = oauth2::Client<
    oauth2::basic::BasicErrorResponse,
    oauth2::basic::BasicTokenResponse,
    oauth2::basic::BasicTokenIntrospectionResponse,
    oauth2::StandardRevocableToken,
    oauth2::basic::BasicRevocationErrorResponse,
    oauth2::EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointSet,
>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_round_trips_through_its_wire_name() {
        for provider in Provider::ALL {
            assert_eq!(provider.as_str().parse::<Provider>().unwrap(), provider);
            assert_eq!(
                serde_json::to_string(&provider).unwrap(),
                format!("{:?}", provider.as_str())
            );
        }
        assert!(matches!(
            "GitHub".parse::<Provider>(),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn the_callback_is_under_the_public_url() {
        let auth = Auth::new(Vec::new(), "https://kiewer.example").unwrap();
        assert_eq!(
            auth.callback_url(Provider::GitHub),
            "https://kiewer.example/auth/github/callback"
        );
        assert!(auth.providers().is_empty());
        assert!(auth.begin(Provider::GitHub).unwrap().is_none());
    }
}
