//! Takedowns: the `blocked_blob` table is how the operator refuses a
//! hash site-wide (ADR 0002). A row in it stops an upload that holds the
//! bytes, makes the two routes that serve them answer 410 and keeps a
//! thumbnail off every page.

use sqlx::PgExecutor;

use crate::error::{Error, Refusal, Result};
use crate::revision::{TreeHash, TreePath};

/// The path of a blocked blob in `tree`, the first by path when there
/// are several, or `None` when none is blocked: what a write asks of a
/// tree it is about to name, so a block made after the file was
/// accepted still stops it, and the refusal can name the file. On any
/// executor, so a write asks inside its own transaction.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a path that is not one, which the upload forbids.
pub async fn blocked_in<'c>(db: impl PgExecutor<'c>, tree: TreeHash) -> Result<Option<TreePath>> {
    // The tree's entries by primary key, one probe of blocked_blob each.
    let path: Option<String> = sqlx::query_scalar(
        "SELECT te.path FROM tree_entry te
         JOIN blocked_blob b ON b.hash = te.blob_hash
         WHERE te.tree_hash = $1
         ORDER BY te.path LIMIT 1",
    )
    .bind(tree.as_bytes())
    .fetch_optional(db)
    .await?;
    path.map(|path| {
        path.parse()
            .map_err(|error: Error| Error::Malformed(format!("a tree's path: {error}")))
    })
    .transpose()
}

/// [`blocked_in`] as a refusal, for a write that stops there.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::Blocked`] naming the file, and
/// [`blocked_in`]'s own.
pub async fn refuse_blocked<'c>(db: impl PgExecutor<'c>, tree: TreeHash) -> Result<()> {
    match blocked_in(db, tree).await? {
        Some(path) => Err(Refusal::Blocked {
            path: path.to_string(),
        }
        .into()),
        None => Ok(()),
    }
}
