//! The collector (ADR 0002; docs/v1-scope.md, Storage and history): the
//! nightly job that removes what nothing names any more. A hard delete
//! takes a design's rows at once and leaves its revisions, trees and
//! blobs, since a fork may share them; [`run`] removes the ones no branch
//! reaches, in the order the references run, once
//! `crate::pending::expire` has taken the pending revisions past their
//! hour:
//!
//! 1. revisions, keeping every branch head, every revision younger than
//!    [`GRACE_HOURS`], and every ancestor of those: a parent stays while
//!    a child names it;
//! 2. trees no revision and no pending revision names, whatever their
//!    age, and with them their entries, so the blobs a pending revision
//!    holds stay;
//! 3. blobs no tree and no thumbnail names, as rows now and as objects
//!    in step 4;
//! 4. the objects of those blobs, queued in `blob_removal` by the same
//!    transaction that removed the row.
//!
//! Every step is a statement that finds its own work, so a second run
//! finds nothing, and a run cut short leaves work the next one finds: a
//! crash after a blob's row went leaves its `blob_removal` row, and the
//! next run deletes the object.
//!
//! Steps 1 to 3 each read a whole table that grows for the life of the
//! site, which no other statement here does: what nothing names has no
//! index to find it by. They run once a night, with no person waiting.
//!
//! # A concurrent upload
//!
//! An upload names blobs and a tree that may already exist, and the same
//! bytes may be going away at that moment. Two rules keep it safe.
//!
//! [`store`] claims a blob before it puts the bytes: one transaction
//! that writes the row, or sets `claimed_at` on the row there, and then
//! deletes any `blob_removal` row for the hash. The grace counts from
//! `claimed_at`, so a claimed blob is left alone for a day, far longer
//! than an upload lives. Between a claim and a collector the cases are:
//!
//! - the claim commits first: the collector's delete reads the new
//!   `claimed_at` and leaves the row;
//! - the collector's delete of the row commits first: the claim waited
//!   for it on the row, writes a fresh one, and its next statement sees
//!   the `blob_removal` row the collector just wrote and takes it, so the
//!   object stays;
//! - the collector is deleting the object: it holds that `blob_removal`
//!   row locked across the store's delete (`remove_object`), so the
//!   claim waits, commits after the object is gone, and the put writes
//!   it again.
//!
//! So a put's [`crate::blob::Put::Existed`] can be trusted: by the time
//! the put runs, no delete of that object is under way or still to come.
//!
//! [`crate::design::record_tree`] claims a tree the same way, on its one
//! row, and a push's revision is a new row, younger than the grace, on
//! a head the mark keeps. Should anything still name a row the collector
//! removes in the same instant (a new branch from a revision no branch
//! reaches), the foreign keys refuse one of the two: the loser's
//! transaction rolls back whole, and the next run tries again.
//!
//! The pages a run could change are few, since what it removes no branch
//! reaches: a fork's forked-from line loses a removed revision's hash. It
//! purges nothing; such a page catches up within the hour
//! ([`crate::cache::Policy::Page`]).

use std::fmt;
use std::time::Duration;

use sqlx::PgPool;

use crate::blob::{BlobHash, BlobStore, PutReceipt, object_name};
use crate::error::{Error, Result};
use crate::pending;

/// How long a row is left alone: long enough that nothing still in
/// flight can be holding it.
pub const GRACE_HOURS: i32 = 24;

/// How long the store gets to delete one object: `remove_object` holds a
/// row across the call, so the call has to end.
const DELETE_LIMIT: Duration = Duration::from_secs(30);

/// What one run removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Collected {
    /// Pending revisions past their hour.
    pub pending: u64,
    pub revisions: u64,
    pub trees: u64,
    /// Blob rows.
    pub blobs: u64,
    /// Objects deleted from the store, which may be from an earlier run
    /// that did not finish.
    pub objects: u64,
}

impl fmt::Display for Collected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            pending,
            revisions,
            trees,
            blobs,
            objects,
        } = self;
        write!(
            f,
            "{pending} pending revisions, {revisions} revisions, {trees} trees, {blobs} blobs, {objects} objects"
        )
    }
}

/// Store `bytes` as a blob a collector cannot take from under the
/// caller: the row claimed, then the bytes put. The one way a blob
/// reaches the store; the module doc says why the order matters.
///
/// # Errors
///
/// [`crate::error::Error::Db`] when Postgres refuses; the store's own
/// when the put fails, which leaves a row the collector removes.
pub async fn store(db: &PgPool, blobs: &dyn BlobStore, bytes: &[u8]) -> Result<PutReceipt> {
    claim(db, &BlobHash::of(bytes), bytes.len() as u64).await?;
    blobs.put(bytes).await
}

/// The claim [`store`] makes: `hash`'s row made or freshened, then any
/// removal of it taken back or waited out. Two statements in this order
/// and not one: the second needs a snapshot taken after the first
/// finished waiting, which the module doc's second case relies on.
async fn claim(db: &PgPool, hash: &BlobHash, size: u64) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO blob (hash, size) VALUES ($1, $2)
         ON CONFLICT (hash) DO UPDATE SET claimed_at = now()",
    )
    .bind(hash.as_bytes())
    .bind(i64::try_from(size).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM blob_removal WHERE hash = $1")
        .bind(hash.as_bytes())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// One night's work: everything nothing names and the grace no longer
/// covers, and what a run before it did not finish. Each step commits
/// on its own, since each is a whole outcome and the next run redoes
/// nothing of it.
///
/// # Errors
///
/// [`crate::error::Error::Db`] when Postgres refuses, a row someone began
/// to use in that instant among the reasons, and the store's own when an
/// object will not go. What was done stays done; the next run finds the
/// rest.
pub async fn run(db: &PgPool, blobs: &dyn BlobStore) -> Result<Collected> {
    let expired = pending::expire(db).await?;
    let revisions = remove_revisions(db).await?;
    let trees = remove_trees(db).await?;
    let removed_blobs = remove_blobs(db).await?;
    let objects = remove_objects(db, blobs).await?;
    Ok(Collected {
        pending: expired,
        revisions,
        trees,
        blobs: removed_blobs,
        objects,
    })
}

/// Step 1. The mark and the sweep are one statement, so one snapshot:
/// `kept` is every branch head and every young revision with their
/// chains, and the rest go with their `design_revision` rows, which
/// name them and would refuse the delete. A pending revision's base
/// needs no place here: it sits on the chain of a branch that exists. A thumbnail goes with its
/// revision by cascade, and a fork's `forked_from_revision_hash` is set
/// null by its action.
async fn remove_revisions(db: &PgPool) -> Result<u64> {
    let removed = sqlx::query(
        "WITH RECURSIVE kept (hash) AS (
             SELECT head_hash FROM branch
             UNION
             SELECT hash FROM revision WHERE created_at > now() - make_interval(hours => $1)
             UNION
             SELECT r.parent_hash FROM kept k
             JOIN revision r ON r.hash = k.hash
             WHERE r.parent_hash IS NOT NULL
         ),
         doomed AS (
             SELECT r.hash FROM revision r
             WHERE NOT EXISTS (SELECT 1 FROM kept k WHERE k.hash = r.hash)
         ),
         unlisted AS (
             DELETE FROM design_revision dr USING doomed d WHERE dr.revision_hash = d.hash
         )
         DELETE FROM revision r USING doomed d WHERE r.hash = d.hash",
    )
    .bind(GRACE_HOURS)
    .execute(db)
    .await?
    .rows_affected();
    Ok(removed)
}

/// Step 2. Its entries go by cascade, which is what frees the blobs for
/// step 3.
async fn remove_trees(db: &PgPool) -> Result<u64> {
    let removed = sqlx::query(
        "DELETE FROM tree t
         WHERE t.claimed_at <= now() - make_interval(hours => $1)
           AND NOT EXISTS (SELECT 1 FROM revision r WHERE r.tree_hash = t.hash)
           AND NOT EXISTS (SELECT 1 FROM pending_revision p WHERE p.tree_hash = t.hash)",
    )
    .bind(GRACE_HOURS)
    .execute(db)
    .await?
    .rows_affected();
    Ok(removed)
}

/// Step 3: the rows, and in the same statement the record that the
/// objects are still to go, so a crash leaves neither half alone.
async fn remove_blobs(db: &PgPool) -> Result<u64> {
    let removed = sqlx::query(
        "WITH gone AS (
             DELETE FROM blob b
             WHERE b.claimed_at <= now() - make_interval(hours => $1)
               AND NOT EXISTS (SELECT 1 FROM tree_entry te WHERE te.blob_hash = b.hash)
               AND NOT EXISTS (
                   SELECT 1 FROM thumbnail t WHERE b.hash IN (t.large_hash, t.small_hash)
               )
             RETURNING b.hash
         )
         INSERT INTO blob_removal (hash) SELECT hash FROM gone
         ON CONFLICT (hash) DO NOTHING",
    )
    .bind(GRACE_HOURS)
    .execute(db)
    .await?
    .rows_affected();
    Ok(removed)
}

/// Step 4: every object queued for removal, this run's and any an
/// earlier run left. Stops at the first the store will not delete, so a
/// store that is down costs one failed call.
async fn remove_objects(db: &PgPool, blobs: &dyn BlobStore) -> Result<u64> {
    // A work queue that is empty between runs, read whole.
    let queued: Vec<Vec<u8>> = sqlx::query_scalar("SELECT hash FROM blob_removal")
        .fetch_all(db)
        .await?;
    let mut removed = 0;
    for bytes in queued {
        if remove_object(db, blobs, &BlobHash::from_bytes(&bytes)?).await? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// One object out of the store, and whether it went. The transaction
/// stays open across the store's delete, the one here that does and the
/// exception to the rule against that: the `blob_removal` row it holds
/// locked is what makes a claim of the same bytes wait (the module doc's
/// third case), so the wait is the point, and a claim of these bytes is
/// the only thing that can wait on it; [`DELETE_LIMIT`] bounds the call.
/// A failure or a crash rolls back, the row stays and the next run
/// deletes again, which is harmless for an object already gone.
async fn remove_object(db: &PgPool, blobs: &dyn BlobStore, hash: &BlobHash) -> Result<bool> {
    let mut tx = db.begin().await?;
    // `Some(false)` is a queued object whose blob stayed gone. A blob that
    // is back (`Some(true)`) keeps its object, and no row (`None`) means
    // an upload's claim took the removal back first.
    let blob_is_back: Option<bool> = sqlx::query_scalar(
        "DELETE FROM blob_removal r WHERE r.hash = $1
         RETURNING EXISTS (SELECT 1 FROM blob b WHERE b.hash = r.hash)",
    )
    .bind(hash.as_bytes())
    .fetch_optional(&mut *tx)
    .await?;
    let removes = blob_is_back == Some(false);
    if removes {
        tokio::time::timeout(DELETE_LIMIT, blobs.delete(hash))
            .await
            .map_err(|_| Error::DeleteBlob {
                hash: *hash,
                path: object_name(hash).into(),
                reason: format!("no answer in {} s", DELETE_LIMIT.as_secs()),
            })??;
    }
    tx.commit().await?;
    Ok(removes)
}
