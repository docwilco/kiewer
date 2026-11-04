//! A pending revision (ADR 0002; docs/v1-scope.md, Push; GLOSSARY): an
//! upload whose branch head moved while it was on its way. `upload::push`
//! has stored the blobs and the tree by then, so what it writes here
//! (`hold`) is everything else the revision will be; there is no
//! revision row until the uploader decides ([`decide`]), since the hash
//! covers the parent and the parent is what they decide: re-append
//! rewrites it to the new head, branch keeps the old head under a new
//! branch. Either way the revision's time is the decision's, so it never
//! sits older than its parent.
//!
//! Only the uploader sees or decides a pending revision, and only for its
//! hour; after that it is gone for them, whether or not the collector has
//! swept the row yet ([`expire`], which the collector calls first; a row
//! still there keeps its tree). The upload counted against the hourly
//! allowance when it arrived, and a decision is that same upload, not
//! another: `rate_limit` counts the row while it waits, and the revision
//! row in its place afterwards; a branch delete that takes the row hands
//! its count to `upload_refusal` (`spend`). A decision meets the account
//! cap and the block list again, since the bytes join the account and a
//! revision only then.
//!
//! A branch rename or delete reaches this table through its foreign key
//! with the design's row locked exclusively, so [`decide`] locks the
//! design before it touches its row (`crate::branch`, on the locks).

use std::fmt;
use std::str::FromStr;

use sqlx::{PgConnection, PgPool};

use crate::account::{AccountId, Handle};
use crate::design::{
    self, BranchName, Design, DesignId, Lock, MetadataColumns, design_row, metadata_row,
};
use crate::error::{Error, Refusal, Result};
use crate::revision::{Metadata, RevisionHash, TreeHash};
use crate::roles::{self, Role};
use crate::sql::statement;
use crate::takedown;
use crate::upload::{self, Author, Landing, Limits};

/// A pending revision's row id: its page's URL segment, which opens for
/// its uploader alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, sqlx::Type)]
#[sqlx(transparent)]
pub struct PendingId(i64);

impl fmt::Display for PendingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for PendingId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        text.parse()
            .map(Self)
            .map_err(|_| Error::Malformed(format!("not a pending revision: {text:?}")))
    }
}

/// What an upload held here is, as [`hold`] writes it.
pub(crate) struct Held<'a> {
    pub design: DesignId,
    pub branch: &'a BranchName,
    pub author: AccountId,
    /// The head the upload was built on.
    pub base: RevisionHash,
    pub tree: TreeHash,
    pub message: Option<&'a str>,
    pub metadata: Option<&'a Metadata>,
    /// As `upload::Pushed::warnings`, kept for the choice page.
    pub warnings: &'a [String],
}

/// The upload is held rather than landed, within its transaction.
pub(crate) async fn hold(tx: &mut PgConnection, held: &Held<'_>) -> Result<PendingId> {
    let m = MetadataColumns::of(held.metadata);
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO pending_revision (design_id, branch, author_id, base_hash, tree_hash,
             message, warnings, format_version, layer_count, copper_layer_count,
             board_width_mm, board_height_mm, fonts, net_count, footprint_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         RETURNING id AS "id: PendingId""#,
        held.design as DesignId,
        held.branch.as_str(),
        held.author as AccountId,
        held.base.as_bytes(),
        held.tree.as_bytes(),
        held.message,
        held.warnings,
        m.format_version,
        m.layer_count,
        m.copper_layer_count,
        m.board_width_mm,
        m.board_height_mm,
        m.fonts.as_deref(),
        m.net_count,
        m.footprint_count
    )
    .fetch_one(tx)
    .await?)
}

/// A revision along the branch: one end of the gap the choice page
/// shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlongBranch {
    pub hash: RevisionHash,
    /// Its ordinal along the branch.
    pub ordinal: u32,
    /// Who pushed it.
    pub author: String,
}

/// A pending revision as its uploader's page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: PendingId,
    pub design: Design,
    /// The design's owner's handle: the first segment of its URL.
    pub owner: Handle,
    pub branch: BranchName,
    /// The head the upload was built on.
    pub base: AlongBranch,
    /// The branch's head now.
    pub head: AlongBranch,
    pub message: Option<String>,
    pub warnings: Vec<String>,
    /// Whole minutes of its hour left, at least one.
    pub minutes_left: u32,
}

/// `account`'s pending revision `id`, or `None` when it is someone
/// else's, past its hour, decided, or never was: to anyone but its
/// uploader, it does not exist. A page's reads on the pool, nothing
/// written; a push between them moves the head the page shows, and the
/// decision reads the head again.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn find(db: &PgPool, id: PendingId, account: AccountId) -> Result<Option<Pending>> {
    // The row by its key, its design and owner by theirs.
    let row = statement!(query!(
        [
            "SELECT " DESIGN_COLUMNS r#", a.handle AS "handle!", p.branch, p.base_hash, p.message,
                p.warnings,
                CEIL(EXTRACT(EPOCH FROM p.expires_at - now()) / 60)::bigint AS "minutes_left!"
         FROM pending_revision p
         JOIN design d ON d.id = p.design_id
         JOIN account a ON a.id = d.owner_id
         WHERE p.id = $1 AND p.author_id = $2 AND p.expires_at > now()"#
        ],
        id as PendingId,
        account as AccountId
    ))
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let branch = BranchName::from_row(row.branch);
    let base = RevisionHash::from_bytes(&row.base_hash)?;
    let owner = Handle::from_row(row.handle);
    let message = row.message;
    let warnings = row.warnings;
    let minutes_left = row.minutes_left;
    let design: Design = design_row!(row).try_into()?;
    let entries = design::history(db, design.id, &branch, None).await?;
    let point = |hash: RevisionHash| {
        entries
            .iter()
            .find(|entry| entry.revision.hash == hash)
            .map(|entry| AlongBranch {
                hash,
                ordinal: entry.ordinal,
                author: entry.revision.author.clone(),
            })
    };
    // Both are on the chain the foreign key keeps; a delete between the
    // reads leaves none, which is the row gone.
    let (Some(base), Some(head)) = (
        point(base),
        entries.first().and_then(|entry| point(entry.revision.hash)),
    ) else {
        return Ok(None);
    };
    Ok(Some(Pending {
        id,
        design,
        owner,
        branch,
        base,
        head,
        message,
        warnings,
        minutes_left: u32::try_from(minutes_left).unwrap_or(1),
    }))
}

/// What the uploader chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Onto the branch's new head: the parent rewritten, the hash
    /// recomputed. `onto` is the head the page showed: a head that has
    /// moved again is refused, or these files would land on a revision
    /// their uploader never saw, which is what holding them prevents.
    Reappend { onto: RevisionHash },
    /// A new branch at the old head, with the revision on top.
    Branch(BranchName),
}

/// What a decision made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub design: Design,
    /// The design's owner's handle: the first segment of its URL.
    pub owner: Handle,
    /// The branch the revision is the head of.
    pub branch: BranchName,
    pub revision: RevisionHash,
}

/// The upload `id` of `author` lands as `choice` says: the revision, the
/// head moved, the design's index and the pending row gone, in one
/// transaction, or `None` when there is nothing to decide (as [`find`]).
/// No allowance is checked: the upload spent it when it arrived.
///
/// # Errors
///
/// [`Error::Refused`] for what the uploader can act on, with the upload
/// still pending: [`Refusal::BranchTaken`] for a
/// new branch's name, [`Refusal::HeadMoved`] for a head past `onto`,
/// `AccountFull` for the cap, `Blocked` for a file taken down in the
/// meantime. [`Error::Forbidden`] when their role is
/// gone. [`Error::Db`] when Postgres refuses.
pub async fn decide(
    db: &PgPool,
    limits: &Limits,
    author: &Author,
    id: PendingId,
    choice: Choice,
) -> Result<Option<Decided>> {
    // On the pool, before the transaction: it only says which design to
    // lock first, and the claim below decides.
    let design_id: Option<DesignId> = sqlx::query_scalar!(
        r#"SELECT design_id AS "design_id: DesignId" FROM pending_revision
         WHERE id = $1 AND author_id = $2 AND expires_at > now()"#,
        id as PendingId,
        author.account_id as AccountId
    )
    .fetch_optional(db)
    .await?;
    let Some(design_id) = design_id else {
        return Ok(None);
    };
    let mut tx = db.begin().await?;
    let Some(design) = design::lock(&mut tx, design_id, Lock::Shared).await? else {
        return Ok(None);
    };
    roles::require(&mut *tx, design.id, author.account_id, Role::Collaborator).await?;
    // The delete is the claim: of two decisions at once the second finds
    // no row. A refusal below rolls it back, so the upload stays pending.
    let row = sqlx::query!(
        "DELETE FROM pending_revision
         WHERE id = $1 AND author_id = $2 AND expires_at > now()
         RETURNING branch, base_hash, tree_hash, message, format_version, layer_count,
             copper_layer_count, board_width_mm, board_height_mm, fonts, net_count,
             footprint_count",
        id as PendingId,
        author.account_id as AccountId
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let tree = TreeHash::from_bytes(&row.tree_hash)?;
    let base = RevisionHash::from_bytes(&row.base_hash)?;
    let pending_branch = row.branch;
    let message = row.message;
    let metadata = metadata_row!(row).into_metadata();
    // A file taken down during the hour: the decision is the upload
    // landing, so the block list has the last word here too.
    if let Some(path) = takedown::blocked_in(&mut *tx, tree).await? {
        return Err(Refusal::Blocked {
            path: path.to_string(),
        }
        .into());
    }
    let owner = design::owner_handle(&mut tx, &design).await?;
    let hashes = design::tree_blobs(&mut tx, tree).await?;
    upload::check_account_cap(&mut tx, limits, author.account_id, &hashes).await?;
    let (branch, parent, expected) = match choice {
        Choice::Reappend { onto } => {
            let branch = BranchName::from_row(pending_branch);
            // The foreign key kept the branch while the row lived, and the
            // design held keeps it now.
            let head = design::lock_head(&mut tx, design.id, branch.as_str())
                .await?
                .ok_or_else(|| {
                    Error::Refused(Refusal::NoSuchBranch {
                        branch: branch.to_string(),
                    })
                })?;
            if head != onto {
                return Err(Refusal::HeadMoved {
                    branch: branch.to_string(),
                }
                .into());
            }
            (branch, Some(head), Some(head))
        }
        Choice::Branch(name) => (name, Some(base), None),
    };
    let revision = upload::land(
        &mut tx,
        &Landing {
            design: design.id,
            branch: &branch,
            parent,
            expected,
            tree,
            author,
            message: message.as_deref(),
            metadata: metadata.as_ref(),
        },
    )
    .await?;
    tx.commit().await?;
    tracing::info!(design = %design.slug, %branch, %revision, "pending revision decided");
    Ok(Some(Decided {
        design,
        owner,
        branch,
        revision,
    }))
}

/// The pending revisions aimed at `branch` keep counting against their
/// uploaders' allowance once a delete takes them: a revision a branch no
/// longer reaches still counts, and a delete must not be how an upload
/// stops counting. Each becomes the `upload_refusal` row of its time,
/// before the delete in the same transaction.
pub(crate) async fn spend(
    tx: &mut PgConnection,
    design: DesignId,
    branch: &BranchName,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO upload_refusal (account_id, refused_at)
         SELECT author_id, created_at FROM pending_revision
         WHERE design_id = $1 AND branch = $2",
        design as DesignId,
        branch.as_str()
    )
    .execute(tx)
    .await?;
    Ok(())
}

/// Every pending revision past its hour goes, and how many did: what the
/// collector does before it looks at trees, so a tree only a lapsed row
/// named is reclaimed in the same run. One statement, through the
/// `expires_at` index; each row is independent of the others.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn expire(db: &PgPool) -> Result<u64> {
    Ok(
        sqlx::query!("DELETE FROM pending_revision WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}
