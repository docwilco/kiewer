//! The hourly allowance on uploads (docs/v1-scope.md, Caps): an account
//! makes at most `per_hour` uploads in any hour, and the one over is
//! answered 429 with `Retry-After`. An upload counts whether it became a
//! revision or was refused, because a refusal costs the server the work
//! of reading it and a hostile upload would otherwise repeat freely.
//! Counted from the revision rows the account authored (`revision.author_id`,
//! `created_at`, through the index `revision_author_id`), the
//! `upload_refusal` rows and the `pending_revision` rows: a revision a
//! branch no longer reaches still counts until the collector takes it,
//! which is after the hour. A pending revision counts once: its row while
//! it waits, then the revision in its place, or nothing once its hour is
//! up, which is when the row would leave the window anyway. The count is a
//! bound on bursts, not a ledger: two uploads in flight at once may both
//! pass as the thirtieth.

use std::time::Duration;

use sqlx::PgExecutor;

use crate::account::AccountId;
use crate::error::{Refusal, Result};

/// The window the allowance is counted over.
pub const WINDOW: Duration = Duration::from_hours(1);

/// Whether `account` may upload once more under an allowance of
/// `per_hour` (at least one: a zero is taken as one), or the refusal that
/// says when it may.
///
/// `Retry-After` is when the oldest upload among the last `per_hour`
/// leaves the window, in whole seconds, at least one.
///
/// # Errors
///
/// [`Error::Refused`](crate::Error::Refused) with [`Refusal::RateLimited`]
/// when the allowance is used; [`Error::Db`](crate::Error::Db) when
/// Postgres refuses.
pub async fn check<'c>(db: impl PgExecutor<'c>, account: AccountId, per_hour: u32) -> Result<()> {
    let per_hour = per_hour.max(1);
    // The row at offset per_hour - 1, newest first, is the oldest of the
    // last per_hour uploads in the window; it exists only when the
    // allowance is used. Its leaving time, from now, is the wait.
    let wait: Option<i64> = sqlx::query_scalar!(
        r#"SELECT GREATEST(1, CEIL(EXTRACT(EPOCH FROM at + make_interval(secs => $3) - now())))::bigint
             AS "wait!"
         FROM (
             SELECT created_at AS at FROM revision
             WHERE author_id = $1 AND created_at > now() - make_interval(secs => $3)
             UNION ALL
             SELECT refused_at FROM upload_refusal
             WHERE account_id = $1 AND refused_at > now() - make_interval(secs => $3)
             UNION ALL
             SELECT created_at FROM pending_revision
             WHERE author_id = $1 AND created_at > now() - make_interval(secs => $3)
         ) uploads
         ORDER BY at DESC
         OFFSET $2 LIMIT 1"#,
        account as AccountId,
        i64::from(per_hour) - 1,
        WINDOW.as_secs_f64()
    )
    .fetch_optional(db)
    .await?;
    match wait {
        None => Ok(()),
        Some(seconds) => Err(Refusal::RateLimited {
            limit: per_hour,
            retry_after_secs: u64::try_from(seconds).unwrap_or(1),
        }
        .into()),
    }
}

/// An upload of `account`'s was refused: it spends the allowance like a
/// push. The account's expired rows go in the same statement, which is
/// what keeps the table small. A refusal that is the allowance's own is
/// not recorded, or being limited would extend the limit; the caller
/// sees to that.
///
/// # Errors
///
/// [`Error::Db`](crate::Error::Db) when Postgres refuses.
pub async fn record_refusal<'c>(db: impl PgExecutor<'c>, account: AccountId) -> Result<()> {
    sqlx::query!(
        "WITH expired AS (
             DELETE FROM upload_refusal
             WHERE account_id = $1 AND refused_at <= now() - make_interval(secs => $2)
         )
         INSERT INTO upload_refusal (account_id) VALUES ($1)",
        account as AccountId,
        WINDOW.as_secs_f64()
    )
    .execute(db)
    .await?;
    Ok(())
}
