//! What the server reads from its environment. Every variable in one place,
//! read once at startup, so a missing or malformed one fails before the
//! server listens, naming the variable.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// The port when `PORT` is unset: Cloud Run's default, so a dev box and
/// the container agree.
pub const DEFAULT_PORT: u16 = 8080;

/// Where the filesystem blob store writes when `KIEWER_BLOB_DIR` is unset:
/// under the working directory, git-ignored.
pub const DEFAULT_BLOB_DIR: &str = "blobs";

/// The server's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `PORT`: the TCP port to listen on, on every interface.
    pub port: u16,
    /// `DATABASE_URL`: the Postgres connection string, as sqlx reads it.
    pub database_url: String,
    /// `KIEWER_BLOB_DIR`: the filesystem blob store's root.
    pub blob_dir: PathBuf,
    /// `KIEWER_PUBLIC_URL`: the origin browsers reach the server at, with
    /// no trailing slash; the sign-in callback URL is under it. Defaults
    /// to `http://localhost:<port>`, a dev box.
    pub public_url: String,
    /// `GITHUB_CLIENT_ID` and `GITHUB_CLIENT_SECRET`: the GitHub OAuth app
    /// (README, Development), or `None` with both unset, which turns
    /// GitHub sign-in off.
    pub github: Option<OAuthApp>,
}

/// One provider's OAuth app registration, as the environment gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: String,
}

/// The pair of variables for one provider's app: both, or neither.
///
/// # Errors
///
/// [`Error::Env`] naming the one of the pair that is unset.
fn oauth_app(
    var: &impl Fn(&str) -> Option<String>,
    id_name: &'static str,
    secret_name: &'static str,
) -> Result<Option<OAuthApp>> {
    let read = |name: &str| var(name).filter(|value| !value.is_empty());
    match (read(id_name), read(secret_name)) {
        (None, None) => Ok(None),
        (Some(client_id), Some(client_secret)) => Ok(Some(OAuthApp {
            client_id,
            client_secret,
        })),
        (Some(_), None) => Err(Error::Env {
            name: secret_name,
            reason: format!("is not set while {id_name} is; see README, Development"),
        }),
        (None, Some(_)) => Err(Error::Env {
            name: id_name,
            reason: format!("is not set while {secret_name} is; see README, Development"),
        }),
    }
}

impl Config {
    /// The configuration from the process environment.
    ///
    /// # Errors
    ///
    /// [`Error::Env`] naming the variable that is missing or malformed.
    pub fn from_env() -> Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// The configuration from any source of variables, so a test needs no
    /// process environment.
    ///
    /// # Errors
    ///
    /// [`Error::Env`] naming the variable that is missing or malformed.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let port = match var("PORT") {
            None => DEFAULT_PORT,
            Some(text) => text.parse().map_err(|_| Error::Env {
                name: "PORT",
                reason: format!("{text:?} is not a port number"),
            })?,
        };
        let database_url = var("DATABASE_URL")
            .filter(|url| !url.is_empty())
            .ok_or(Error::Env {
                name: "DATABASE_URL",
                reason: "is not set; see README, Development".to_owned(),
            })?;
        let blob_dir = var("KIEWER_BLOB_DIR")
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| PathBuf::from(DEFAULT_BLOB_DIR), PathBuf::from);
        let public_url = var("KIEWER_PUBLIC_URL")
            .filter(|url| !url.is_empty())
            .map_or_else(
                || format!("http://localhost:{port}"),
                |url| url.trim_end_matches('/').to_owned(),
            );
        let github = oauth_app(&var, "GITHUB_CLIENT_ID", "GITHUB_CLIENT_SECRET")?;
        Ok(Self {
            port,
            database_url,
            blob_dir,
            public_url,
            github,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn defaults_fill_what_is_unset() {
        let config = Config::from_vars(vars(&[("DATABASE_URL", "postgres://x")])).expect("valid");
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.blob_dir, PathBuf::from(DEFAULT_BLOB_DIR));
        assert_eq!(config.database_url, "postgres://x");
        assert_eq!(config.public_url, "http://localhost:8080");
        assert_eq!(config.github, None, "sign-in is off without the app");
    }

    #[test]
    fn the_github_app_is_both_variables_or_neither() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("KIEWER_PUBLIC_URL", "https://kiewer.example/"),
            ("GITHUB_CLIENT_ID", "id"),
            ("GITHUB_CLIENT_SECRET", "secret"),
        ]))
        .expect("valid");
        assert_eq!(
            config.public_url, "https://kiewer.example",
            "no trailing slash"
        );
        assert_eq!(
            config.github,
            Some(OAuthApp {
                client_id: "id".to_owned(),
                client_secret: "secret".to_owned()
            })
        );
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("GITHUB_CLIENT_ID", "id"),
        ]))
        .expect_err("half an app");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "GITHUB_CLIENT_SECRET",
                    ..
                }
            ),
            "{err}"
        );
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("GITHUB_CLIENT_SECRET", "secret"),
        ]))
        .expect_err("half an app");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "GITHUB_CLIENT_ID",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn every_variable_is_read() {
        let config = Config::from_vars(vars(&[
            ("PORT", "3000"),
            ("DATABASE_URL", "postgres://y"),
            ("KIEWER_BLOB_DIR", "/var/blobs"),
        ]))
        .expect("valid");
        assert_eq!(config.port, 3000);
        assert_eq!(config.blob_dir, PathBuf::from("/var/blobs"));
    }

    #[test]
    fn a_missing_database_url_names_itself() {
        let err = Config::from_vars(vars(&[])).expect_err("DATABASE_URL is required");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "DATABASE_URL",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_bad_port_names_itself() {
        let err = Config::from_vars(vars(&[
            ("PORT", "eighty"),
            ("DATABASE_URL", "postgres://x"),
        ]))
        .expect_err("not a port");
        assert!(matches!(err, Error::Env { name: "PORT", .. }), "{err}");
    }
}
