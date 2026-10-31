//! Sessions: a signed-in browser as a Postgres row behind a cookie
//! (docs/v1-scope.md, Accounts and access). The cookie holds a random id;
//! the row holds its SHA-256, so a read of the table yields no usable
//! cookie. Expiry slides: a session used with more than a day gone since
//! its last refresh gets its 30 days back. The clock is Postgres's
//! `now()`, so a test ages a row with SQL and the code needs no clock.

use std::fmt;
use std::str::FromStr;

use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::account::{AccountId, Handle};
use crate::error::{Error, Result};

/// How long a session lives after it starts or refreshes.
pub const LIFETIME_DAYS: i32 = 30;

/// How much of the lifetime has to pass before a use refreshes it: at
/// most one write per session per day, not one per page.
pub const REFRESH_AFTER_DAYS: i32 = 1;

/// The random id a session cookie carries: 32 bytes from the OS, shown as
/// 64 lowercase hex characters. Only its hash is stored.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; 32]);

impl SessionId {
    /// A fresh id from the operating system's randomness.
    #[must_use]
    pub fn random() -> Self {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// What the row stores: the SHA-256 of the id's text form.
    fn hash(self) -> Vec<u8> {
        Sha256::digest(self.to_string().as_bytes()).to_vec()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SessionId {
    /// Never the id: a debug log of a request must not hold a cookie.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionId(..)")
    }
}

impl FromStr for SessionId {
    type Err = Error;

    /// The canonical text only: 64 lowercase hex digits, so one id has one
    /// spelling (and one hash).
    fn from_str(text: &str) -> Result<Self> {
        let malformed = || Error::Malformed("not a session id".to_owned());
        let digit = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        if text.len() != 64 {
            return Err(malformed());
        }
        let mut out = [0u8; 32];
        for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let (high, low) = (digit(pair[0]), digit(pair[1]));
            *slot = match (high, low) {
                (Some(high), Some(low)) => (high << 4) | low,
                _ => return Err(malformed()),
            };
        }
        Ok(Self(out))
    }
}

/// A live session as [`find`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub account_id: AccountId,
    /// `None` until the person picks one on the welcome page.
    pub handle: Option<Handle>,
    /// Whether this use slid the expiry, so the cookie's Max-Age follows.
    pub refreshed: bool,
}

/// Start a session for `account`: a row with the full lifetime, and the
/// id for the cookie.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn start(db: &PgPool, account: AccountId) -> Result<SessionId> {
    // Not in the insert's transaction: the sweep of the account's dead
    // rows is independent of the new session and idempotent, and a crash
    // between the two leaves nothing a later sign-in does not finish.
    sqlx::query("DELETE FROM session WHERE account_id = $1 AND expires_at <= now()")
        .bind(account)
        .execute(db)
        .await?;
    let id = SessionId::random();
    sqlx::query(
        "INSERT INTO session (id_hash, account_id, expires_at)
         VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(id.hash())
    .bind(account)
    .bind(LIFETIME_DAYS)
    .execute(db)
    .await?;
    Ok(id)
}

/// The session behind `id` when it is live, refreshed when a day or more
/// of its lifetime has gone; `None` for an unknown, ended or expired id.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn find(db: &PgPool, id: &SessionId) -> Result<Option<Session>> {
    // One statement for the read and the write it decides: the refresh
    // and the lookup are one outcome, and the main query reads the row as
    // it was before the CTE's update, which is the same live row.
    let row: Option<(AccountId, Option<String>, bool)> = sqlx::query_as(
        "WITH refreshed AS (
             UPDATE session SET expires_at = now() + make_interval(days => $2)
             WHERE id_hash = $1 AND expires_at > now()
               AND expires_at < now() + make_interval(days => $2 - $3)
             RETURNING 1
         )
         SELECT s.account_id, a.handle, EXISTS (SELECT 1 FROM refreshed)
         FROM session s JOIN account a ON a.id = s.account_id
         WHERE s.id_hash = $1 AND s.expires_at > now()",
    )
    .bind(id.hash())
    .bind(LIFETIME_DAYS)
    .bind(REFRESH_AFTER_DAYS)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(account_id, handle, refreshed)| Session {
        account_id,
        handle: handle.map(Handle::from_row),
        refreshed,
    }))
}

/// End the session behind `id`: the row goes. Ending an unknown or
/// already ended session does nothing.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn end(db: &PgPool, id: &SessionId) -> Result<()> {
    sqlx::query("DELETE FROM session WHERE id_hash = $1")
        .bind(id.hash())
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test]
    fn an_id_is_64_lowercase_hex_characters_and_round_trips() {
        let id = SessionId::random();
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert!(text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(text.parse::<SessionId>().unwrap(), id);
        assert_ne!(SessionId::random(), id);
    }

    #[test_case(""; "empty")]
    #[test_case("abc"; "short")]
    #[test_case(&"zz".repeat(32); "the length but not hex")]
    #[test_case(&"AB".repeat(32); "uppercase hex is not the canonical spelling")]
    #[test_case(&"+f".repeat(32); "a sign is not a digit")]
    fn a_text_that_is_not_the_canonical_id_is_refused(text: &str) {
        assert!(matches!(
            text.parse::<SessionId>(),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn debug_hides_the_id() {
        assert_eq!(format!("{:?}", SessionId::random()), "SessionId(..)");
    }
}
