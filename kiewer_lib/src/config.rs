//! What the server reads from its environment. Every variable in one place,
//! read once at startup, so a missing or malformed one fails before the
//! server listens, naming the variable.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::auth::{Credentials, Provider};
use crate::cache::KeyPrefix;
use crate::error::{Error, Result};

/// The port when `PORT` is unset: Cloud Run's default, so a dev box and
/// the container agree.
pub const DEFAULT_PORT: u16 = 8090;

/// Where the filesystem blob store writes when `KIEWER_BLOB_DIR` is unset:
/// under the working directory, git-ignored.
pub const DEFAULT_BLOB_DIR: &str = "blobs";

/// Where blobs live, as the environment chooses it. `server::run` builds
/// the matching `BlobStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobStoreConfig {
    /// The filesystem store under `KIEWER_BLOB_DIR` (default
    /// [`DEFAULT_BLOB_DIR`]): development and tests.
    Filesystem { root: PathBuf },
    /// The GCS store on the bucket `BLOB_BUCKET` names, the variable the
    /// Terraform in `infra/` gives the Cloud Run services; with
    /// `KIEWER_GCS_EMULATOR` beside it, on that emulator (its base URL)
    /// with no credentials. The emulator variable alone changes nothing.
    Gcs {
        bucket: String,
        emulator: Option<String>,
    },
}

impl fmt::Display for BlobStoreConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobStoreConfig::Filesystem { root } => write!(f, "directory {}", root.display()),
            BlobStoreConfig::Gcs {
                bucket,
                emulator: None,
            } => write!(f, "bucket {bucket}"),
            BlobStoreConfig::Gcs {
                bucket,
                emulator: Some(endpoint),
            } => write!(f, "bucket {bucket} on the emulator at {endpoint}"),
        }
    }
}

/// The Fastly service the server purges on and the token it purges
/// with: `FASTLY_SERVICE_ID` and `FASTLY_API_TOKEN`, the names the
/// Terraform in `infra/` gives the Cloud Run services (its README lists
/// them). Absent on a dev box, where nothing is in front of the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastlyConfig {
    pub service_id: String,
    pub api_token: String,
}

/// The Cloud Tasks queue thumbnail retries go on (ADR 0004) and the
/// service account its tasks call back as: `THUMBNAIL_QUEUE` and
/// `THUMBNAIL_TASK_ACCOUNT`, the variables the Terraform in `infra/`
/// gives the Cloud Run services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskConfig {
    /// The queue, `projects/…/locations/…/queues/…`.
    pub queue: String,
    /// The runtime service account's email: what the task's OIDC token
    /// is minted as and the retry path checks for.
    pub account: String,
}

/// The server's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `PORT`: the TCP port to listen on, on every interface.
    pub port: u16,
    /// `DATABASE_URL`: the Postgres connection string, as sqlx reads it.
    pub database_url: String,
    /// `BLOB_BUCKET`, `KIEWER_GCS_EMULATOR`, `KIEWER_BLOB_DIR`: where blobs
    /// live.
    pub blobs: BlobStoreConfig,
    /// `PUBLIC_URL`: the origin browsers reach the server at, with no
    /// trailing slash; the sign-in callback URL is under it and cookies
    /// are `Secure` when it is `https`. Defaults to
    /// `http://localhost:<port>`, a dev box. The names of this and the
    /// OAuth variables are the deploy contract in `infra/` (its README
    /// lists them; a retired one is a breaking Release).
    pub public_url: String,
    /// `OAUTH_<PROVIDER>_CLIENT_ID` and `OAUTH_<PROVIDER>_CLIENT_SECRET`
    /// for PROVIDER in GITHUB, DISCORD, GOOGLE
    /// ([`Provider::env_names`]): the OAuth app registered at each
    /// provider (README, Development). A provider with both unset is
    /// absent here and from the sign-in links, never an error; one of a
    /// pair alone is.
    pub oauth: BTreeMap<Provider, Credentials>,
    /// `FASTLY_SERVICE_ID` and `FASTLY_API_TOKEN`: the edge to purge on
    /// after a write (`crate::cache`). The service id alone, which a
    /// deployment has before its token is given, means no purges, never
    /// an error; a token without the service id is an error naming it.
    pub fastly: Option<FastlyConfig>,
    /// `SURROGATE_KEY_PREFIX`: what goes in front of every surrogate key
    /// the server names, in its pages' headers and its purges
    /// ([`KeyPrefix`]). The Terraform in `infra/` sets `staging-` on
    /// kiewer-staging alone, since both environments share one Fastly
    /// service (ADR 0003); unset is no prefix, production's and a dev
    /// box's.
    pub surrogate_key_prefix: KeyPrefix,
    /// `THUMBNAIL_QUEUE` and `THUMBNAIL_TASK_ACCOUNT`: where a thumbnail
    /// render past its budget is retried from. The queue alone, which a
    /// deployment has until the account's variable is applied, means no
    /// retries, never an error, as does neither (a dev box); the log says
    /// so once. An account without the queue is an error naming it.
    pub thumbnail_tasks: Option<TaskConfig>,
}

/// The Fastly pair: both, the service id alone (no purges), or neither.
///
/// # Errors
///
/// [`Error::Env`] naming `FASTLY_SERVICE_ID` when the token is set
/// without it.
fn fastly(var: &impl Fn(&str) -> Option<String>) -> Result<Option<FastlyConfig>> {
    let read = |name: &str| var(name).filter(|value| !value.is_empty());
    match (read("FASTLY_SERVICE_ID"), read("FASTLY_API_TOKEN")) {
        (Some(service_id), Some(api_token)) => Ok(Some(FastlyConfig {
            service_id,
            api_token,
        })),
        (Some(_) | None, None) => Ok(None),
        (None, Some(_)) => Err(Error::Env {
            name: "FASTLY_SERVICE_ID",
            reason: "is not set while FASTLY_API_TOKEN is; see infra/README.md".to_owned(),
        }),
    }
}

/// The task queue pair: both, the queue alone (no retries), or neither.
///
/// # Errors
///
/// [`Error::Env`] naming `THUMBNAIL_QUEUE` when the account is set
/// without it.
fn thumbnail_tasks(var: &impl Fn(&str) -> Option<String>) -> Result<Option<TaskConfig>> {
    let read = |name: &str| var(name).filter(|value| !value.is_empty());
    match (read("THUMBNAIL_QUEUE"), read("THUMBNAIL_TASK_ACCOUNT")) {
        (Some(queue), Some(account)) => Ok(Some(TaskConfig { queue, account })),
        (Some(_) | None, None) => Ok(None),
        (None, Some(_)) => Err(Error::Env {
            name: "THUMBNAIL_QUEUE",
            reason: "is not set while THUMBNAIL_TASK_ACCOUNT is; see infra/README.md".to_owned(),
        }),
    }
}

/// The pair of variables for one provider's app: both, or neither.
///
/// # Errors
///
/// [`Error::Env`] naming the one of the pair that is unset.
fn oauth_app(
    var: &impl Fn(&str) -> Option<String>,
    provider: Provider,
) -> Result<Option<Credentials>> {
    let (id_name, secret_name) = provider.env_names();
    let read = |name: &str| var(name).filter(|value| !value.is_empty());
    match (read(id_name), read(secret_name)) {
        (None, None) => Ok(None),
        (Some(client_id), Some(client_secret)) => Ok(Some(Credentials {
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
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        // The emulator variable means something only beside a bucket: a dev
        // box exports it for the GCS store test and still runs the server
        // on the directory.
        let blobs = match set("BLOB_BUCKET") {
            Some(bucket) => BlobStoreConfig::Gcs {
                bucket,
                emulator: set("KIEWER_GCS_EMULATOR"),
            },
            None => BlobStoreConfig::Filesystem {
                root: set("KIEWER_BLOB_DIR")
                    .map_or_else(|| PathBuf::from(DEFAULT_BLOB_DIR), PathBuf::from),
            },
        };
        let public_url = set("PUBLIC_URL").map_or_else(
            || format!("http://localhost:{port}"),
            |url| url.trim_end_matches('/').to_owned(),
        );
        let mut oauth = BTreeMap::new();
        for provider in Provider::ALL {
            if let Some(credentials) = oauth_app(&var, provider)? {
                oauth.insert(provider, credentials);
            }
        }
        Ok(Self {
            port,
            database_url,
            blobs,
            public_url,
            oauth,
            fastly: fastly(&var)?,
            surrogate_key_prefix: set("SURROGATE_KEY_PREFIX")
                .map(|text| {
                    text.parse().map_err(|_| Error::Env {
                        name: "SURROGATE_KEY_PREFIX",
                        reason: format!("is {text:?}, not visible ASCII without spaces"),
                    })
                })
                .transpose()?
                .unwrap_or_default(),
            thumbnail_tasks: thumbnail_tasks(&var)?,
        })
    }
}

/// The log's shape, as `LOG_FORMAT` chooses it (`json` or `text`) and,
/// unset, as stdout does: a terminal reads text, anything else (a pipe,
/// Cloud Run's log agent) gets JSON. The executable applies it; the
/// library only names it, so the variable is listed with the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per event in the shape Cloud Logging reads, its
    /// `severity` and `time` included, with the event's fields as
    /// camelCased top-level keys: what Cloud Logging reads into
    /// `jsonPayload` and the renderer beacon's metric counts
    /// (`infra/logging.tf`).
    Json,
    /// One line per event for a person reading a terminal.
    Text,
}

impl FromStr for LogFormat {
    type Err = Error;

    /// `json` or `text`, the values `LOG_FORMAT` takes.
    fn from_str(text: &str) -> Result<Self> {
        match text {
            "json" => Ok(LogFormat::Json),
            "text" => Ok(LogFormat::Text),
            other => Err(Error::Env {
                name: "LOG_FORMAT",
                reason: format!("is {other:?}, not json or text"),
            }),
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LogFormat::Json => "json",
            LogFormat::Text => "text",
        })
    }
}

impl LogFormat {
    /// The format from the process environment, with `terminal` saying
    /// whether stdout is one.
    ///
    /// # Errors
    ///
    /// [`Error::Env`] when `LOG_FORMAT` is set to anything but `json` or
    /// `text`.
    pub fn from_env(terminal: bool) -> Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok(), terminal)
    }

    /// The format from any source of variables.
    ///
    /// # Errors
    ///
    /// As [`LogFormat::from_env`].
    pub fn from_vars(var: impl Fn(&str) -> Option<String>, terminal: bool) -> Result<Self> {
        match var("LOG_FORMAT").filter(|value| !value.is_empty()) {
            Some(value) => value.parse(),
            None if terminal => Ok(LogFormat::Text),
            None => Ok(LogFormat::Json),
        }
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
        assert_eq!(
            config.blobs,
            BlobStoreConfig::Filesystem {
                root: PathBuf::from(DEFAULT_BLOB_DIR)
            }
        );
        assert_eq!(config.database_url, "postgres://x");
        assert_eq!(config.public_url, "http://localhost:8090");
        assert!(config.oauth.is_empty(), "sign-in is off without an app");
        assert_eq!(config.fastly, None, "no edge to purge without the pair");
        assert_eq!(config.thumbnail_tasks, None, "no retries without a queue");
        assert_eq!(
            config.surrogate_key_prefix,
            KeyPrefix::default(),
            "production's keys carry no prefix"
        );
    }

    #[test]
    fn the_surrogate_key_prefix_is_read_and_refused_with_a_space() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("SURROGATE_KEY_PREFIX", "staging-"),
        ]))
        .expect("valid");
        assert_eq!(
            config.surrogate_key_prefix,
            "staging-".parse().expect("a prefix")
        );
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("SURROGATE_KEY_PREFIX", "staging "),
        ]))
        .expect_err("a space would split every key in two");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "SURROGATE_KEY_PREFIX",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn the_fastly_pair_is_both_or_the_service_alone() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("FASTLY_SERVICE_ID", "SU1Z0isxPaozGVKXdv0eY"),
            ("FASTLY_API_TOKEN", "token"),
        ]))
        .expect("valid");
        assert_eq!(
            config.fastly,
            Some(FastlyConfig {
                service_id: "SU1Z0isxPaozGVKXdv0eY".to_owned(),
                api_token: "token".to_owned(),
            })
        );
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("FASTLY_SERVICE_ID", "SU1Z0isxPaozGVKXdv0eY"),
        ]))
        .expect("the service before its token is given: no purges");
        assert_eq!(config.fastly, None);
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("FASTLY_API_TOKEN", "token"),
        ]))
        .expect_err("a token with nothing to purge on");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "FASTLY_SERVICE_ID",
                    ..
                }
            ),
            "{err}"
        );
    }

    const QUEUE: &str = "projects/p/locations/l/queues/q";
    const ACCOUNT: &str = "kiewer-run@p.iam.gserviceaccount.com";

    #[test]
    fn the_task_queue_with_its_account_turns_retries_on() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("THUMBNAIL_QUEUE", QUEUE),
            ("THUMBNAIL_TASK_ACCOUNT", ACCOUNT),
        ]))
        .expect("valid");
        assert_eq!(
            config.thumbnail_tasks,
            Some(TaskConfig {
                queue: QUEUE.to_owned(),
                account: ACCOUNT.to_owned(),
            })
        );
    }

    /// The Terraform has given Cloud Run the queue since #27: a server
    /// that refused to start without the account would crash-loop every
    /// deployment between a deploy and the apply that adds the account.
    #[test]
    fn the_task_queue_alone_starts_the_server_without_retries() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("THUMBNAIL_QUEUE", QUEUE),
        ]))
        .expect("a deployment before the account's apply starts");
        assert_eq!(config.thumbnail_tasks, None);
    }

    #[test]
    fn an_account_without_a_task_queue_names_the_queue() {
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("THUMBNAIL_TASK_ACCOUNT", ACCOUNT),
        ]))
        .expect_err("half a pair");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "THUMBNAIL_QUEUE",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_provider_app_is_both_variables_or_neither() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("PUBLIC_URL", "https://kiewer.example/"),
            ("OAUTH_GITHUB_CLIENT_ID", "id"),
            ("OAUTH_GITHUB_CLIENT_SECRET", "secret"),
        ]))
        .expect("valid");
        assert_eq!(
            config.public_url, "https://kiewer.example",
            "no trailing slash"
        );
        assert_eq!(
            config.oauth.get(&Provider::GitHub),
            Some(&Credentials {
                client_id: "id".to_owned(),
                client_secret: "secret".to_owned()
            })
        );
        assert_eq!(
            config.oauth.len(),
            1,
            "a provider without variables is absent, not an error"
        );
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("OAUTH_GITHUB_CLIENT_ID", "id"),
        ]))
        .expect_err("half an app");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "OAUTH_GITHUB_CLIENT_SECRET",
                    ..
                }
            ),
            "{err}"
        );
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("OAUTH_GITHUB_CLIENT_SECRET", "secret"),
        ]))
        .expect_err("half an app");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "OAUTH_GITHUB_CLIENT_ID",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn every_provider_reads_its_own_pair() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("OAUTH_DISCORD_CLIENT_ID", "d-id"),
            ("OAUTH_DISCORD_CLIENT_SECRET", "d-secret"),
            ("OAUTH_GOOGLE_CLIENT_ID", "g-id"),
            ("OAUTH_GOOGLE_CLIENT_SECRET", "g-secret"),
        ]))
        .expect("valid");
        assert_eq!(
            config.oauth.keys().copied().collect::<Vec<_>>(),
            [Provider::Discord, Provider::Google],
            "GitHub unset: absent"
        );
        assert_eq!(config.oauth[&Provider::Discord].client_id, "d-id");
        assert_eq!(config.oauth[&Provider::Google].client_secret, "g-secret");
        let err = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("OAUTH_GOOGLE_CLIENT_SECRET", "g-secret"),
        ]))
        .expect_err("half an app");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "OAUTH_GOOGLE_CLIENT_ID",
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
        assert_eq!(
            config.blobs,
            BlobStoreConfig::Filesystem {
                root: PathBuf::from("/var/blobs")
            }
        );
    }

    #[test]
    fn a_bucket_means_the_gcs_store() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("BLOB_BUCKET", "my-project-kiewer-blobs"),
        ]))
        .expect("valid");
        assert_eq!(
            config.blobs,
            BlobStoreConfig::Gcs {
                bucket: "my-project-kiewer-blobs".to_owned(),
                emulator: None,
            }
        );
    }

    #[test]
    fn an_emulator_beside_the_bucket_points_the_gcs_store_at_it() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("BLOB_BUCKET", "kiewer-dev"),
            ("KIEWER_GCS_EMULATOR", "http://127.0.0.1:8092"),
        ]))
        .expect("valid");
        assert_eq!(
            config.blobs,
            BlobStoreConfig::Gcs {
                bucket: "kiewer-dev".to_owned(),
                emulator: Some("http://127.0.0.1:8092".to_owned()),
            }
        );
    }

    #[test]
    fn an_emulator_without_a_bucket_leaves_the_filesystem_store() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://x"),
            ("KIEWER_GCS_EMULATOR", "http://127.0.0.1:8092"),
        ]))
        .expect("the test's export does not change the server's store");
        assert_eq!(
            config.blobs,
            BlobStoreConfig::Filesystem {
                root: PathBuf::from(DEFAULT_BLOB_DIR)
            }
        );
    }

    #[test]
    fn the_choice_names_itself() {
        let fs = BlobStoreConfig::Filesystem {
            root: PathBuf::from("blobs"),
        };
        assert_eq!(fs.to_string(), "directory blobs");
        let gcs = BlobStoreConfig::Gcs {
            bucket: "b".to_owned(),
            emulator: Some("http://127.0.0.1:8092".to_owned()),
        };
        assert_eq!(
            gcs.to_string(),
            "bucket b on the emulator at http://127.0.0.1:8092"
        );
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

    #[test]
    fn the_log_format_follows_the_variable_then_the_terminal() {
        assert_eq!(
            LogFormat::from_vars(vars(&[("LOG_FORMAT", "json")]), true).expect("valid"),
            LogFormat::Json
        );
        assert_eq!(
            LogFormat::from_vars(vars(&[("LOG_FORMAT", "text")]), false).expect("valid"),
            LogFormat::Text
        );
        assert_eq!(
            LogFormat::from_vars(vars(&[]), true).expect("valid"),
            LogFormat::Text,
            "a terminal reads text"
        );
        assert_eq!(
            LogFormat::from_vars(vars(&[]), false).expect("valid"),
            LogFormat::Json,
            "a pipe or Cloud Run gets JSON"
        );
        assert_eq!(
            LogFormat::from_vars(vars(&[("LOG_FORMAT", "")]), false).expect("valid"),
            LogFormat::Json,
            "empty is unset"
        );
        let err =
            LogFormat::from_vars(vars(&[("LOG_FORMAT", "yaml")]), true).expect_err("not a format");
        assert!(
            matches!(
                err,
                Error::Env {
                    name: "LOG_FORMAT",
                    ..
                }
            ),
            "{err}"
        );
        for format in [LogFormat::Json, LogFormat::Text] {
            assert_eq!(
                format
                    .to_string()
                    .parse::<LogFormat>()
                    .expect("its own name"),
                format
            );
        }
    }
}
