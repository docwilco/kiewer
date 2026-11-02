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
//! row in its place afterwards. A decision meets the account cap again,
//! since the bytes join the account only then.
//!
//! Locks, in the order every writer here and in `branch` and `upload`
//! takes them: the design row, then the branch row, then this table's.
//! The rename and the delete that hold the design row exclusively reach
//! this table through its foreign key, so [`decide`] takes the design
//! before it touches its row.

use std::fmt;
use std::str::FromStr;

use sqlx::{PgConnection, PgPool, Row as _};

use crate::account::{AccountId, Handle};
use crate::design::{self, BranchName, Design, DesignId, Hold, MetadataRow, bind_metadata};
use crate::error::{Error, Refusal, Result};
use crate::revision::{Metadata, RevisionHash, TreeHash};
use crate::roles::{self, Role};
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
    let query = sqlx::query(
        "INSERT INTO pending_revision (design_id, branch, author_id, base_hash, tree_hash,
             message, warnings, format_version, layer_count, copper_layer_count,
             board_width_mm, board_height_mm, fonts, net_count, footprint_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         RETURNING id",
    )
    .bind(held.design)
    .bind(held.branch.as_str())
    .bind(held.author)
    .bind(held.base.as_bytes())
    .bind(held.tree.as_bytes())
    .bind(held.message)
    .bind(held.warnings);
    let row = bind_metadata(query, held.metadata).fetch_one(tx).await?;
    Ok(row.try_get("id")?)
}

/// One end of the gap the choice page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
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
    pub base: Point,
    /// The branch's head now.
    pub head: Point,
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
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        design: design::DesignRow,
        handle: String,
        branch: String,
        base_hash: Vec<u8>,
        message: Option<String>,
        warnings: Vec<String>,
        minutes_left: i64,
    }
    // The row by its key, its design and owner by theirs.
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {columns}, a.handle, p.branch, p.base_hash, p.message, p.warnings,
                CEIL(EXTRACT(EPOCH FROM p.expires_at - now()) / 60)::bigint AS minutes_left
         FROM pending_revision p
         JOIN design d ON d.id = p.design_id
         JOIN account a ON a.id = d.owner_id
         WHERE p.id = $1 AND p.author_id = $2 AND p.expires_at > now()",
        columns = design::DESIGN_COLUMNS
    ))
    .bind(id)
    .bind(account)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let branch = BranchName::from_row(row.branch);
    let design: Design = row.design.try_into()?;
    let base = RevisionHash::from_bytes(&row.base_hash)?;
    let entries = design::history(db, design.id, &branch, None).await?;
    let point = |hash: RevisionHash| {
        entries
            .iter()
            .find(|entry| entry.revision.hash == hash)
            .map(|entry| Point {
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
        owner: Handle::from_row(row.handle),
        branch,
        base,
        head,
        message: row.message,
        warnings: row.warnings,
        minutes_left: u32::try_from(row.minutes_left).unwrap_or(1),
    }))
}

/// What the uploader chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Onto the branch's new head: the parent rewritten, the hash
    /// recomputed.
    Reappend,
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
/// new branch's name, `AccountFull` for the cap, `Blocked` for a file
/// taken down in the meantime. [`Error::Forbidden`] when their role is
/// gone. [`Error::Db`] when Postgres refuses.
pub async fn decide(
    db: &PgPool,
    limits: &Limits,
    author: &Author,
    id: PendingId,
    choice: Choice,
) -> Result<Option<Decided>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        branch: String,
        base_hash: Vec<u8>,
        tree_hash: Vec<u8>,
        message: Option<String>,
        #[sqlx(flatten)]
        metadata: MetadataRow,
    }
    let mut tx = db.begin().await?;
    // Unlocked, for the design to lock: see the module's lock order.
    let design_id: Option<DesignId> = sqlx::query_scalar(
        "SELECT design_id FROM pending_revision
         WHERE id = $1 AND author_id = $2 AND expires_at > now()",
    )
    .bind(id)
    .bind(author.account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(design_id) = design_id else {
        return Ok(None);
    };
    let Some(design) = design::lock(&mut tx, design_id, Hold::Shared).await? else {
        return Ok(None);
    };
    roles::require(&mut *tx, design.id, author.account_id, Role::Collaborator).await?;
    // The delete is the claim: of two decisions at once the second finds
    // no row. A refusal below rolls it back, so the upload stays pending.
    let row: Option<Row> = sqlx::query_as(
        "DELETE FROM pending_revision
         WHERE id = $1 AND author_id = $2 AND expires_at > now()
         RETURNING branch, base_hash, tree_hash, message, format_version, layer_count,
             copper_layer_count, board_width_mm, board_height_mm, fonts, net_count,
             footprint_count",
    )
    .bind(id)
    .bind(author.account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let tree = TreeHash::from_bytes(&row.tree_hash)?;
    let base = RevisionHash::from_bytes(&row.base_hash)?;
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
        Choice::Reappend => {
            let branch = BranchName::from_row(row.branch);
            // The foreign key kept the branch while the row lived, and the
            // design held keeps it now.
            let head = design::lock_head(&mut tx, design.id, branch.as_str())
                .await?
                .ok_or_else(|| {
                    Error::Refused(Refusal::NoSuchBranch {
                        branch: branch.to_string(),
                    })
                })?;
            (branch, Some(head), Some(head))
        }
        Choice::Branch(name) => (name, Some(base), None),
    };
    let revision = upload::land(
        &mut tx,
        &Landing {
            design: design.id,
            branch: branch.as_str(),
            parent,
            expected,
            tree,
            author,
            message: row.message.as_deref(),
            metadata: row.metadata.into_metadata().as_ref(),
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
        sqlx::query("DELETE FROM pending_revision WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}
