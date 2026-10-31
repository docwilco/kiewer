//! Sessions: a signed-in browser as a Postgres row behind a cookie
//! (docs/v1-scope.md, Accounts and access). The cookie holds a random id;
//! the row holds its SHA-256, the [`SessionKey`], so a read of the table
//! yields no usable cookie, and the key is what the sessions page names a
//! session by. Expiry slides: a session used with more than a day gone
//! since its last refresh gets its 30 days back; a use also marks the
//! row seen, at most once every few minutes. The clock is Postgres's
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

/// How long after the last mark a use marks the session seen again: the
/// sessions page's "last seen" is this coarse, and a busy session costs
/// one write per interval, not one per page.
pub const SEEN_EVERY_MINUTES: i32 = 5;

/// 32 bytes as 64 lowercase hex characters.
fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(64);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The 32 bytes 64 lowercase hex characters spell, and only that
/// spelling, so one value has one text form; `what` names the value in
/// the error.
fn from_hex(text: &str, what: &str) -> Result<[u8; 32]> {
    let malformed = || Error::Malformed(format!("not a {what}"));
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
    Ok(out)
}

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
    #[must_use]
    pub fn key(self) -> SessionKey {
        SessionKey(Sha256::digest(self.to_string().as_bytes()).into())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
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
        from_hex(text, "session id").map(Self)
    }
}

/// What the row stores for a session and what the sessions page names
/// one by: the SHA-256 of the cookie's id. Knowing a key opens nothing (a
/// preimage of SHA-256 is out of reach), so it may sit in a page and a
/// form; the session it names is still revoked only by its own account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionKey([u8; 32]);

impl SessionKey {
    /// The bytes, for a query parameter.
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl FromStr for SessionKey {
    type Err = Error;

    /// 64 lowercase hex digits, as [`Display`](fmt::Display) writes them.
    fn from_str(text: &str) -> Result<Self> {
        from_hex(text, "session key").map(Self)
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
    .bind(id.key().as_bytes())
    .bind(account)
    .bind(LIFETIME_DAYS)
    .execute(db)
    .await?;
    Ok(id)
}

/// The session behind `id` when it is live, refreshed when a day or more
/// of its lifetime has gone and marked seen when [`SEEN_EVERY_MINUTES`]
/// have; `None` for an unknown, ended or expired id.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn find(db: &PgPool, id: &SessionId) -> Result<Option<Session>> {
    // One statement for the read and the writes it decides: the refresh,
    // the mark and the lookup are one outcome, and the main query reads
    // the row as it was before the CTE's update, which is the same live
    // row. The self-join on `old` is how an UPDATE's RETURNING sees the
    // value before the SET (Postgres 16 has no RETURNING OLD), which says
    // whether the expiry moved and so whether the cookie is set again.
    let row: Option<(AccountId, Option<String>, bool)> = sqlx::query_as(
        "WITH touched AS (
             UPDATE session s
             SET expires_at = CASE
                     WHEN s.expires_at < now() + make_interval(days => $2 - $3)
                     THEN now() + make_interval(days => $2)
                     ELSE s.expires_at END,
                 last_seen_at = now()
             FROM session old
             WHERE old.id_hash = s.id_hash
               AND s.id_hash = $1 AND s.expires_at > now()
               AND (s.expires_at < now() + make_interval(days => $2 - $3)
                    OR s.last_seen_at < now() - make_interval(mins => $4))
             RETURNING (old.expires_at < now() + make_interval(days => $2 - $3)) AS refreshed
         )
         SELECT s.account_id, a.handle,
                COALESCE((SELECT refreshed FROM touched), false)
         FROM session s JOIN account a ON a.id = s.account_id
         WHERE s.id_hash = $1 AND s.expires_at > now()",
    )
    .bind(id.key().as_bytes())
    .bind(LIFETIME_DAYS)
    .bind(REFRESH_AFTER_DAYS)
    .bind(SEEN_EVERY_MINUTES)
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
        .bind(id.key().as_bytes())
        .execute(db)
        .await?;
    Ok(())
}

/// One live session of an account, as the sessions page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub key: SessionKey,
    /// When it started, UTC, ISO 8601 (`2026-10-31T12:00:00Z`).
    pub created_at: String,
    /// When it was last marked seen, the same form.
    pub last_seen_at: String,
    /// Whether this is the session asking.
    pub current: bool,
}

/// The time as the page shows it: UTC, ISO 8601 to the second, formatted
/// by Postgres so the code carries no time library.
const ISO_UTC: &str = r#"to_char(%s AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')"#;

/// `account`'s live sessions, most recently seen first, with the one
/// behind `current` marked.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row's
/// hash is not 32 bytes, which the schema forbids.
pub async fn list(
    db: &PgPool,
    account: AccountId,
    current: &SessionId,
) -> Result<Vec<ListedSession>> {
    let sql = format!(
        "SELECT id_hash, {created}, {seen} FROM session
         WHERE account_id = $1 AND expires_at > now()
         ORDER BY last_seen_at DESC, created_at DESC",
        created = ISO_UTC.replace("%s", "created_at"),
        seen = ISO_UTC.replace("%s", "last_seen_at"),
    );
    let rows: Vec<(Vec<u8>, String, String)> =
        sqlx::query_as(&sql).bind(account).fetch_all(db).await?;
    let current = current.key();
    rows.into_iter()
        .map(|(hash, created_at, last_seen_at)| {
            let key = SessionKey(
                hash.try_into()
                    .map_err(|_| Error::Malformed("a session hash is not 32 bytes".to_owned()))?,
            );
            Ok(ListedSession {
                key,
                created_at,
                last_seen_at,
                current: key == current,
            })
        })
        .collect()
}

/// End the session `key` names when it is `account`'s: a key alone, even
/// a real one, revokes nothing of anyone else's.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn revoke(db: &PgPool, account: AccountId, key: &SessionKey) -> Result<()> {
    sqlx::query("DELETE FROM session WHERE id_hash = $1 AND account_id = $2")
        .bind(key.as_bytes())
        .bind(account)
        .execute(db)
        .await?;
    Ok(())
}

/// End every session of `account` but the one behind `current`.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn revoke_others(db: &PgPool, account: AccountId, current: &SessionId) -> Result<()> {
    sqlx::query("DELETE FROM session WHERE account_id = $1 AND id_hash <> $2")
        .bind(account)
        .bind(current.key().as_bytes())
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

    #[test]
    fn the_key_is_the_hash_of_the_id_and_round_trips_as_hex() {
        let id = SessionId::random();
        let key = id.key();
        let text = key.to_string();
        assert_eq!(text.len(), 64);
        assert_ne!(text, id.to_string(), "the key is not the id");
        assert_eq!(text.parse::<SessionKey>().unwrap(), key);
        assert!(matches!(
            "not-a-key".parse::<SessionKey>(),
            Err(Error::Malformed(_))
        ));
    }
}
