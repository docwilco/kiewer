//! Sign-in through an OAuth2 provider: the authorization-code exchange
//! with PKCE, then one call for who the person is. One module per
//! provider's particulars ([`github`], [`discord`], [`google`]); the flow
//! is shared, so a provider is a variant, an [`App`] and a user-parsing
//! arm, nothing else (docs/v1-scope.md, Accounts and access).

pub mod discord;
pub mod github;
pub mod google;

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
/// (`/auth/github`), the `identity.provider` column and the middle of its
/// environment variables (`OAUTH_GITHUB_CLIENT_ID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    GitHub,
    Discord,
    Google,
}

impl Provider {
    /// Every provider, in code order: the order of the sign-in links.
    pub const ALL: [Provider; 3] = [Provider::GitHub, Provider::Discord, Provider::Google];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::GitHub => "github",
            Provider::Discord => "discord",
            Provider::Google => "google",
        }
    }

    /// The name people know the provider by, for a page.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::Discord => "Discord",
            Provider::Google => "Google",
        }
    }

    /// The names of the two environment variables that hold this
    /// provider's [`Credentials`]: the client id, then the secret.
    #[must_use]
    pub fn env_names(self) -> (&'static str, &'static str) {
        match self {
            Provider::GitHub => ("OAUTH_GITHUB_CLIENT_ID", "OAUTH_GITHUB_CLIENT_SECRET"),
            Provider::Discord => ("OAUTH_DISCORD_CLIENT_ID", "OAUTH_DISCORD_CLIENT_SECRET"),
            Provider::Google => ("OAUTH_GOOGLE_CLIENT_ID", "OAUTH_GOOGLE_CLIENT_SECRET"),
        }
    }

    /// The app registered at this provider, on its real endpoints.
    #[must_use]
    pub fn app(self, credentials: Credentials) -> App {
        match self {
            Provider::GitHub => github::app(credentials),
            Provider::Discord => discord::app(credentials),
            Provider::Google => google::app(credentials),
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
            Provider::Discord => discord::parse_user(body),
            Provider::Google => google::parse_user(body),
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

/// What a provider issued when the app was registered there.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

impl fmt::Debug for Credentials {
    /// The id, never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// One provider's OAuth app registration and endpoints. The endpoints are
/// fields, not constants, so a test points them at a fake provider on
/// loopback.
#[derive(Debug, Clone)]
pub struct App {
    pub provider: Provider,
    pub credentials: Credentials,
    pub auth_url: String,
    pub token_url: String,
    /// The endpoint that says who the token's owner is.
    pub user_url: String,
    pub scopes: Vec<String>,
}

/// Who a provider says signed in: its own stable id for the person, the
/// login it shows, which prefills the handle and names the identity in
/// settings, and the email it vouches for, kept on the identity and never
/// used to find an account (docs/v1-scope.md: no merge by email).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUser {
    pub id: String,
    pub login: String,
    pub email: Option<String>,
}

/// A sign-in just started: where to send the browser, and what the
/// callback needs back from the browser's cookie to finish.
#[derive(Debug, Clone)]
pub struct SignInStart {
    /// The provider's authorization page, with the state and the PKCE
    /// challenge in its query.
    pub authorize_url: String,
    /// The CSRF state the provider echoes back; the callback refuses any
    /// other.
    pub state: String,
    /// The PKCE verifier the token exchange proves the challenge with.
    pub verifier: String,
}

/// An app with its oauth2 client, built once at startup so an endpoint
/// that is not a URL fails there, not at the first sign-in.
struct Registered {
    app: App,
    client: Client,
}

/// The sign-in flow over every configured provider.
pub struct Auth {
    registered: Vec<Registered>,
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
    /// [`Error::OAuth`] when an app's endpoint or the callback URL is not
    /// a URL; [`Error::Http`] when the HTTP client cannot be built.
    pub fn new(apps: Vec<App>, public_url: impl Into<String>) -> Result<Self> {
        let public_url = public_url.into();
        // No redirects: the token endpoint answers in place, and following
        // one would carry the client secret somewhere else (the oauth2
        // crate's own advice).
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("kiewer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let registered = apps
            .into_iter()
            .map(|app| {
                let client = client(&app, &callback_url(&public_url, app.provider))?;
                Ok(Registered { app, client })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            registered,
            public_url,
            http,
        })
    }

    /// The providers with an app registered, in [`Provider::ALL`] order.
    #[must_use]
    pub fn providers(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|provider| self.registered(*provider).is_some())
            .collect()
    }

    fn registered(&self, provider: Provider) -> Option<&Registered> {
        self.registered
            .iter()
            .find(|registered| registered.app.provider == provider)
    }

    /// Where the provider sends the browser back to.
    #[must_use]
    pub fn callback_url(&self, provider: Provider) -> String {
        callback_url(&self.public_url, provider)
    }

    /// Whether cookies carry `Secure`: yes under an `https` public URL,
    /// which is every deployment; no on a dev box over `http`, where
    /// Safari would drop a `Secure` cookie even from localhost.
    #[must_use]
    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    /// Start a sign-in with `provider`: a fresh state and PKCE pair and the
    /// URL to send the browser to. `None` when the provider has no app
    /// registered.
    #[must_use]
    pub fn begin(&self, provider: Provider) -> Option<SignInStart> {
        let registered = self.registered(provider)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = registered
            .client
            .authorize_url(CsrfToken::new_random)
            .add_scopes(registered.app.scopes.iter().cloned().map(Scope::new))
            .set_pkce_challenge(challenge)
            .url();
        Some(SignInStart {
            authorize_url: url.to_string(),
            state: state.into_secret(),
            verifier: verifier.into_secret(),
        })
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
        let Some(registered) = self.registered(provider) else {
            return Ok(None);
        };
        let token = registered
            .client
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
            .get(&registered.app.user_url)
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

fn callback_url(public_url: &str, provider: Provider) -> String {
    format!("{public_url}/auth/{provider}/callback")
}

fn client(app: &App, callback: &str) -> Result<Client> {
    let not_a_url = |what: &str, error: oauth2::url::ParseError| Error::OAuth {
        provider: app.provider,
        reason: format!("{what} is not a URL: {error}"),
    };
    Ok(
        BasicClient::new(ClientId::new(app.credentials.client_id.clone()))
            .set_client_secret(ClientSecret::new(app.credentials.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(app.auth_url.clone()).map_err(|e| not_a_url("auth_url", e))?)
            .set_token_uri(
                TokenUrl::new(app.token_url.clone()).map_err(|e| not_a_url("token_url", e))?,
            )
            .set_redirect_uri(
                RedirectUrl::new(callback.to_owned())
                    .map_err(|e| not_a_url("the callback URL", e))?,
            ),
    )
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
    use test_case::test_case;

    use super::*;

    #[test_case(Provider::GitHub, "github")]
    #[test_case(Provider::Discord, "discord")]
    #[test_case(Provider::Google, "google")]
    fn a_provider_round_trips_through_its_wire_name(provider: Provider, wire: &str) {
        assert_eq!(provider.as_str(), wire);
        assert_eq!(wire.parse::<Provider>().unwrap(), provider);
        assert_eq!(
            serde_json::to_string(&provider).unwrap(),
            format!("{wire:?}")
        );
    }

    #[test]
    fn a_wire_name_in_the_wrong_case_is_not_a_provider() {
        assert!(matches!(
            "GitHub".parse::<Provider>(),
            Err(Error::Malformed(_))
        ));
    }

    fn app(auth_url: &str) -> App {
        App {
            provider: Provider::GitHub,
            credentials: Credentials {
                client_id: "id".to_owned(),
                client_secret: "secret".to_owned(),
            },
            auth_url: auth_url.to_owned(),
            token_url: "https://provider.example/token".to_owned(),
            user_url: "https://provider.example/user".to_owned(),
            scopes: Vec::new(),
        }
    }

    #[test]
    fn the_callback_is_under_the_public_url() {
        let auth = Auth::new(Vec::new(), "https://kiewer.example").unwrap();
        assert_eq!(
            auth.callback_url(Provider::GitHub),
            "https://kiewer.example/auth/github/callback"
        );
        assert!(auth.providers().is_empty());
        assert!(auth.begin(Provider::GitHub).is_none());
    }

    #[test_case("https://kiewer.example", true; "https is secure")]
    #[test_case("http://localhost:8080", false; "http on a dev box is not")]
    fn cookies_are_secure_under_https(public_url: &str, secure: bool) {
        let auth = Auth::new(Vec::new(), public_url).unwrap();
        assert_eq!(auth.secure_cookies(), secure);
    }

    #[test]
    fn an_endpoint_that_is_not_a_url_fails_at_startup() {
        let err = Auth::new(vec![app("not a url")], "https://kiewer.example").unwrap_err();
        assert!(matches!(err, Error::OAuth { .. }), "{err}");
        assert!(err.to_string().contains("auth_url"), "{err}");
    }

    #[test]
    fn a_registered_app_starts_a_sign_in_with_state_and_pkce() {
        let auth = Auth::new(
            vec![app("https://provider.example/authorize")],
            "https://kiewer.example",
        )
        .unwrap();
        assert_eq!(auth.providers(), [Provider::GitHub]);
        let start = auth.begin(Provider::GitHub).unwrap();
        assert!(
            start
                .authorize_url
                .starts_with("https://provider.example/authorize?")
        );
        assert!(
            start
                .authorize_url
                .contains(&format!("state={}", start.state))
        );
        assert!(start.authorize_url.contains("code_challenge_method=S256"));
        assert!(!start.verifier.is_empty());
        assert!(
            !start.authorize_url.contains(&start.verifier),
            "the verifier stays home"
        );
    }

    #[test_case(Provider::GitHub, "https://api.github.com/user", &[]; "github asks no scope")]
    #[test_case(Provider::Discord, "https://discord.com/api/users/@me", &["identify", "email"]; "discord identify and email against users me")]
    #[test_case(Provider::Google, "https://openidconnect.googleapis.com/v1/userinfo", &["openid", "email", "profile"]; "google openid connect userinfo")]
    fn each_provider_has_its_user_endpoint_and_scopes(
        provider: Provider,
        user_url: &str,
        scopes: &[&str],
    ) {
        let app = provider.app(Credentials {
            client_id: "id".to_owned(),
            client_secret: "secret".to_owned(),
        });
        assert_eq!(app.provider, provider);
        assert_eq!(app.user_url, user_url);
        assert_eq!(app.scopes, scopes);
        assert!(app.auth_url.starts_with("https://"), "{}", app.auth_url);
        assert!(app.token_url.starts_with("https://"), "{}", app.token_url);
    }

    #[test_case(Provider::GitHub, "GitHub")]
    #[test_case(Provider::Discord, "Discord")]
    #[test_case(Provider::Google, "Google")]
    fn a_provider_has_the_name_people_know_it_by(provider: Provider, name: &str) {
        assert_eq!(provider.name(), name);
    }

    #[test]
    fn credentials_debug_hides_the_secret() {
        let text = format!(
            "{:?}",
            Credentials {
                client_id: "id".to_owned(),
                client_secret: "hunter2".to_owned()
            }
        );
        assert!(text.contains("id") && !text.contains("hunter2"), "{text}");
    }
}
