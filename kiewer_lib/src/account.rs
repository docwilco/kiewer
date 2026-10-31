//! Accounts and handles: the person behind one or more provider
//! identities, named once by a handle (GLOSSARY). A first sign-in with an
//! unknown identity makes a new account; a second identity joins an
//! account only when its signed-in holder links it from settings
//! ([`link`]); identities are never merged by login or email
//! (docs/v1-scope.md, Accounts and access).

use std::fmt;
use std::str::FromStr;

use sqlx::PgPool;

use crate::auth::{Provider, ProviderUser};
use crate::error::{Error, Result};

/// An account's row id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, sqlx::Type)]
#[sqlx(transparent)]
pub struct AccountId(i64);

/// A person's unique name on kiewer: the first segment of a design URL.
/// Lowercase letters, digits and hyphens, 1 to 39 characters, no leading
/// or trailing hyphen, and not a word a top-level route uses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(String);

/// Words a handle may not be: the first segment of every top-level route
/// the server has or will have, and names that would read as the site
/// itself. A handle is the first URL segment, so a route and a handle
/// cannot share a word; the list is checked in code, not in the schema,
/// so it grows without a migration. Sorted, so a lookup is a binary
/// search and a test can check the order.
pub const RESERVED: &[&str] = &[
    "about", "admin", "api", "assets", "auth", "b", "blob", "blobs", "design", "designs", "docs",
    "explore", "f", "healthz", "help", "kiewer", "login", "logout", "me", "new", "r", "recent",
    "root", "search", "settings", "sign-in", "sign-out", "signin", "signout", "static", "status",
    "upload", "welcome", "www",
];

impl Handle {
    /// The longest handle: GitHub's limit, so a GitHub login always fits.
    pub const MAX_LEN: usize = 39;

    /// The handle as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A handle read back from its row, taken as it is: the row passed the
    /// rules when it was written, and a word added to [`RESERVED`] since
    /// must not lock its holder out.
    pub(crate) fn from_row(text: String) -> Self {
        Self(text)
    }

    /// The handle a provider's login suggests: lowercased, every run of
    /// other characters a hyphen, cut to the length, or `None` when
    /// nothing of it survives or what does is reserved.
    #[must_use]
    pub fn suggest(login: &str) -> Option<Self> {
        let mut text = String::with_capacity(login.len());
        for ch in login.chars().flat_map(char::to_lowercase) {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                text.push(ch);
            } else if !text.ends_with('-') {
                text.push('-');
            }
        }
        let mut text = text.trim_matches('-');
        if text.len() > Self::MAX_LEN {
            text = &text[..Self::MAX_LEN];
            text = text.trim_end_matches('-');
        }
        text.parse().ok()
    }

    /// Why `text` is not a handle, in the words the form shows, or `None`
    /// when it is one.
    fn fault(text: &str) -> Option<&'static str> {
        if text.is_empty() {
            return Some("is empty");
        }
        if text.len() > Self::MAX_LEN {
            return Some("is longer than 39 characters");
        }
        if !text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Some("may use only lowercase letters, digits and hyphens");
        }
        if text.starts_with('-') || text.ends_with('-') {
            return Some("may not start or end with a hyphen");
        }
        if RESERVED.binary_search(&text).is_ok() {
            return Some("is a reserved word");
        }
        None
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Handle {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        match Self::fault(text) {
            None => Ok(Self(text.to_owned())),
            Some(fault) => Err(Error::Malformed(format!("handle {text:?} {fault}"))),
        }
    }
}

/// What a sign-in found or made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub account_id: AccountId,
    /// `None` until the person picks one: send them to the welcome page.
    pub handle: Option<Handle>,
    /// The login the provider shows, kept on the identity; the welcome
    /// page's prefill.
    pub provider_login: String,
    /// Whether this sign-in made the account.
    pub is_new: bool,
}

/// The account behind `provider`'s `user`, made on first sight; the
/// identity is found by the provider's id alone, never by the email or
/// the login, which both change hands. The login and the email are what
/// the provider shows now and replace what the identity kept.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn sign_in(db: &PgPool, provider: Provider, user: &ProviderUser) -> Result<SignedIn> {
    // One outcome, one transaction, and a write first: a fresh account is
    // made, the identity is inserted pointing at it or found pointing at
    // another, and in the second case the fresh account goes again. The
    // conflict path locks the identity's row, so two first sign-ins of one
    // person serialise and the second finds the first's account.
    let mut tx = db.begin().await?;
    let fresh: AccountId = sqlx::query_scalar("INSERT INTO account DEFAULT VALUES RETURNING id")
        .fetch_one(&mut *tx)
        .await?;
    let account_id: AccountId = sqlx::query_scalar(
        "INSERT INTO identity (provider, provider_user_id, account_id, provider_login, email)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (provider, provider_user_id)
         DO UPDATE SET provider_login = EXCLUDED.provider_login, email = EXCLUDED.email
         RETURNING account_id",
    )
    .bind(provider.as_str())
    .bind(&user.id)
    .bind(fresh)
    .bind(&user.login)
    .bind(&user.email)
    .fetch_one(&mut *tx)
    .await?;
    let is_new = account_id == fresh;
    let handle = if is_new {
        None
    } else {
        sqlx::query("DELETE FROM account WHERE id = $1")
            .bind(fresh)
            .execute(&mut *tx)
            .await?;
        sqlx::query_scalar::<_, Option<String>>("SELECT handle FROM account WHERE id = $1")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?
            .map(Handle::from_row)
    };
    tx.commit().await?;
    Ok(SignedIn {
        account_id,
        handle,
        provider_login: user.login.clone(),
        is_new,
    })
}

/// What linking an identity to an account came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    /// The identity is the account's from now on.
    Added,
    /// It was the account's already; the login and email were brought up
    /// to date.
    Already,
    /// It signs in to another account, which keeps it: no identity moves
    /// without that account's holder giving it up, and v1 has no way to.
    ToAnotherAccount,
}

/// Attach `provider`'s `user` to `account`, the signed-in person's own,
/// unless the identity signs in to another account.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn link(
    db: &PgPool,
    account: AccountId,
    provider: Provider,
    user: &ProviderUser,
) -> Result<Linked> {
    // One statement: the read (whose is this identity) is the write's own
    // condition. The conflict arm updates only a row that is already this
    // account's, so another account's row is untouched and the statement
    // returns nothing; xmax tells an insert from an update.
    let row: Option<(bool,)> = sqlx::query_as(
        "INSERT INTO identity (provider, provider_user_id, account_id, provider_login, email)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (provider, provider_user_id)
         DO UPDATE SET provider_login = EXCLUDED.provider_login, email = EXCLUDED.email
         WHERE identity.account_id = EXCLUDED.account_id
         RETURNING (xmax = 0) AS inserted",
    )
    .bind(provider.as_str())
    .bind(&user.id)
    .bind(account)
    .bind(&user.login)
    .bind(&user.email)
    .fetch_optional(db)
    .await?;
    Ok(match row {
        Some((true,)) => Linked::Added,
        Some((false,)) => Linked::Already,
        None => Linked::ToAnotherAccount,
    })
}

/// One sign-in method of an account, as settings lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub provider: Provider,
    /// What the provider showed for the person at the last sign-in.
    pub login: String,
    /// The email the provider vouched for then, when it gave one.
    pub email: Option<String>,
}

/// The identities that sign in to `account`, oldest first.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// names a provider the code no longer knows.
pub async fn identities(db: &PgPool, account: AccountId) -> Result<Vec<Identity>> {
    // One read on the pool: a page's listing, part of no write.
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT provider, provider_login, email FROM identity
         WHERE account_id = $1 ORDER BY created_at, provider",
    )
    .bind(account)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(provider, login, email)| {
            Ok(Identity {
                provider: provider.parse()?,
                login,
                email,
            })
        })
        .collect()
}

/// The handle to prefill the welcome page with: what the account's most
/// recent identity's login suggests, or `None` when nothing of it makes a
/// handle.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn suggested_handle(db: &PgPool, account: AccountId) -> Result<Option<Handle>> {
    let login: Option<String> = sqlx::query_scalar(
        "SELECT provider_login FROM identity WHERE account_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account)
    .fetch_optional(db)
    .await?;
    Ok(login.as_deref().and_then(Handle::suggest))
}

/// Give `account` its handle, once: the first sign-in's choice.
///
/// # Errors
///
/// [`Error::HandleTaken`] when another account has it; [`Error::Failed`]
/// when the account already has one (no rename in v1); [`Error::Db`] when
/// Postgres refuses otherwise.
pub async fn choose_handle(db: &PgPool, account: AccountId, handle: &Handle) -> Result<()> {
    // One statement: the read (is the handle still free, is this account
    // still without one) is the write's own condition and constraint.
    let result = sqlx::query("UPDATE account SET handle = $2 WHERE id = $1 AND handle IS NULL")
        .bind(account)
        .bind(handle.as_str())
        .execute(db)
        .await;
    match result {
        Ok(done) if done.rows_affected() == 1 => Ok(()),
        Ok(_) => Err(Error::Failed {
            reason: format!("account {} already has a handle", account.0),
        }),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(Error::HandleTaken(handle.clone()))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("alice"; "letters")]
    #[test_case("alice-2"; "letters digits and a hyphen")]
    #[test_case("a"; "one character")]
    #[test_case("0"; "one digit")]
    #[test_case("a23456789b23456789c23456789d23456789e23"; "thirty-nine characters")]
    #[test_case("api-v2"; "a reserved word as a prefix is fine")]
    fn a_valid_handle_parses(text: &str) {
        let handle: Handle = text.parse().expect("valid");
        assert_eq!(handle.as_str(), text);
    }

    #[test_case("octocat", Some("octocat"); "a handle already")]
    #[test_case("Octo_Cat", Some("octo-cat"); "case folded and an underscore made a hyphen")]
    #[test_case("--Octo..Cat--", Some("octo-cat"); "runs collapsed and the ends trimmed")]
    #[test_case("api", None; "a reserved word")]
    #[test_case("___", None; "nothing survives")]
    #[test_case("a23456789b23456789c23456789d23456789e2-x", Some("a23456789b23456789c23456789d23456789e2"); "cut to the length without a trailing hyphen")]
    fn a_login_suggests_a_handle(login: &str, suggested: Option<&str>) {
        assert_eq!(
            Handle::suggest(login).as_ref().map(Handle::as_str),
            suggested
        );
    }

    #[test]
    fn the_reserved_list_is_sorted_and_made_of_handles() {
        assert!(
            RESERVED.windows(2).all(|pair| pair[0] < pair[1]),
            "RESERVED is binary-searched: keep it sorted and unique"
        );
        for word in RESERVED {
            assert_eq!(Handle::fault(word), Some("is a reserved word"), "{word}");
        }
    }

    #[test_case(""; "empty")]
    #[test_case("Alice"; "an uppercase letter")]
    #[test_case("-alice"; "a leading hyphen")]
    #[test_case("alice-"; "a trailing hyphen")]
    #[test_case("al ice"; "a space")]
    #[test_case("al_ice"; "an underscore")]
    #[test_case("al.ice"; "a dot")]
    #[test_case("älice"; "a letter outside ascii")]
    #[test_case("a23456789b23456789c23456789d23456789e234"; "forty characters")]
    #[test_case("api"; "the api route")]
    #[test_case("auth"; "the auth route")]
    #[test_case("healthz"; "the health route")]
    #[test_case("blob"; "the blob route")]
    #[test_case("welcome"; "the welcome page")]
    #[test_case("settings"; "the settings page")]
    #[test_case("admin"; "admin")]
    #[test_case("kiewer"; "the site's own name")]
    fn a_malformed_or_reserved_handle_is_refused_with_a_message(text: &str) {
        let err = text.parse::<Handle>().expect_err("refused");
        assert!(matches!(err, Error::Malformed(_)), "{err}");
        assert!(err.to_string().contains(&format!("{text:?}")), "{err}");
    }
}
