//! Deleting a design and deleting an account (docs/v1-scope.md, Storage
//! and history, Q35 and Q43): a hard delete of the rows at once. The
//! schema's cascades do the work (a design takes its branches, its index
//! of revisions, its roles and its slug redirects; an account takes its
//! designs, identities, sessions and roles), and the revisions, trees
//! and blobs stay for the collector, since a fork may share them
//! (`crate::collector`). A fork of a deleted design keeps working: its
//! source reference goes null and its text columns name what is gone
//! (`design::fork_source`).

use sqlx::PgPool;

use crate::account::{self, AccountId, Handle};
use crate::cache::{self, Key};
use crate::design::DesignId;
use crate::error::Result;
use crate::roles::{self, Role, Visibility};

/// What asking to delete came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deleted {
    /// The rows are gone; these are the keys of the pages that showed
    /// them.
    Done(Vec<Key>),
    /// What was typed is not the design's name or the account's handle,
    /// so nothing changed.
    NotConfirmed,
    /// There is no such design: someone deleted it first.
    Gone,
}

/// Delete `design` for `by`, who has typed `typed` to confirm: the
/// owner alone may ([`Role::Owner`]), and the name must be the design's,
/// surrounding space aside.
///
/// # Errors
///
/// [`crate::error::Error::Forbidden`] when `by` is not the owner;
/// [`crate::error::Error::Db`] when Postgres refuses;
/// [`crate::error::Error::Malformed`] when the row's visibility is not
/// one, which the schema forbids.
pub async fn design(db: &PgPool, by: AccountId, design: DesignId, typed: &str) -> Result<Deleted> {
    // One outcome, one transaction, the row locked first, in the order
    // every writer takes them (`crate::pending`): a transfer or a rename
    // waits and cannot change what the checks below read, and a fork or a
    // push in flight, which holds the row, finishes first.
    let mut tx = db.begin().await?;
    let row = sqlx::query!(
        r#"SELECT d.name, d.visibility, a.handle AS "owner!" FROM design d
         JOIN account a ON a.id = d.owner_id
         WHERE d.id = $1
         FOR UPDATE OF d"#,
        design as DesignId
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(Deleted::Gone);
    };
    let (name, visibility, owner) = (row.name, row.visibility, row.owner);
    roles::require(&mut *tx, design, by, Role::Owner).await?;
    if typed.trim() != name {
        return Ok(Deleted::NotConfirmed);
    }
    let keys = cache::of_design(
        design,
        &Handle::from_row(owner),
        Visibility::from_row(&visibility)?,
    );
    sqlx::query!("DELETE FROM design WHERE id = $1", design as DesignId)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Deleted::Done(keys))
}

/// The names of `account`'s designs that have other people on them:
/// what the settings page lists before an account delete, since those
/// people lose the design too. Newest first.
///
/// # Errors
///
/// [`crate::error::Error::Db`] when Postgres refuses.
pub async fn shared_designs(db: &PgPool, account: AccountId) -> Result<Vec<String>> {
    // One read on the pool: a page's listing, part of no write. The
    // account's designs by the (owner_id, slug) index, a role row of
    // each by design_role's primary key.
    Ok(sqlx::query_scalar!(
        "SELECT d.name FROM design d
         WHERE d.owner_id = $1
           AND EXISTS (SELECT 1 FROM design_role r WHERE r.design_id = d.id)
         ORDER BY d.created_at DESC, d.id DESC",
        account as AccountId
    )
    .fetch_all(db)
    .await?)
}

/// Delete `account`, whose handle is `handle`, and reserve the handle,
/// once `typed` is that handle, surrounding space aside: its designs,
/// identities, sessions and roles go by cascade, and the revisions it
/// authored stay under the handle as text. The keys that come back are
/// every design it owned, its handle, and the home page when one of the
/// designs was public.
///
/// # Errors
///
/// [`crate::error::Error::Db`] when Postgres refuses;
/// [`crate::error::Error::Malformed`] when a row's visibility is not
/// one, which the schema forbids.
pub async fn account(
    db: &PgPool,
    account: AccountId,
    handle: &Handle,
    typed: &str,
) -> Result<Deleted> {
    if typed.trim() != handle.as_str() {
        return Ok(Deleted::NotConfirmed);
    }
    // One outcome, one transaction: the reservation goes in with the
    // account's end, so the handle is never free in between. The designs
    // are read and held before the cascade takes them, for the purge.
    let mut tx = db.begin().await?;
    account::reserve(&mut tx, handle).await?;
    let designs = sqlx::query!(
        r#"SELECT id AS "id: DesignId", visibility FROM design WHERE owner_id = $1 FOR UPDATE"#,
        account as AccountId
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut keys = vec![Key::Handle(handle.clone())];
    let mut public = false;
    for row in designs {
        keys.push(Key::Design(row.id));
        public |= Visibility::from_row(&row.visibility)? == Visibility::Public;
    }
    if public {
        keys.push(Key::Home);
    }
    sqlx::query!("DELETE FROM account WHERE id = $1", account as AccountId)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Deleted::Done(keys))
}
