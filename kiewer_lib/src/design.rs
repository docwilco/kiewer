//! Designs, branches and the rows that persist trees and revisions
//! (ADR 0002; GLOSSARY, Things people own). Every statement over the
//! history tables is here: the writes take the upload's transaction
//! (`upload::commit` is the one outcome they record), the reads take the
//! pool and serve a page. A revision is only ever read through a design
//! (`design_revision`), since a bare hash would be a capability.

use std::fmt;
use std::str::FromStr;

use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::account::{AccountId, Handle};
use crate::blob::BlobHash;
use crate::error::{Error, Refusal, Result};
use crate::revision::{
    Metadata, RevisionDraft, RevisionHash, TreeEntry, TreeHash, TreePath, revision_hash, tree_hash,
};
use crate::roles::Visibility;

/// The slug type, where the upload and the pages have always found it;
/// its rules and its redirects are `crate::slug`.
pub use crate::slug::Slug;

/// A design's row id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, sqlx::Type)]
#[sqlx(transparent)]
pub struct DesignId(i64);

impl DesignId {
    /// An id as a row would carry it, for a test with no row behind it.
    #[cfg(test)]
    pub(crate) fn for_test(id: i64) -> Self {
        Self(id)
    }
}

/// The number alone, as a cache key spells it (`crate::cache::Key`).
impl fmt::Display for DesignId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The branch every design starts with (GLOSSARY, Default branch).
pub const DEFAULT_BRANCH: &str = "main";

/// A branch's name (GLOSSARY, Branch): ASCII letters, digits, hyphens,
/// underscores and dots, 1 to 64 characters, not starting with a hyphen or
/// a dot. One URL segment (`/b/{branch}`), so no slash; typed, so a form
/// field never reaches a row or a URL unchecked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BranchName(String);

impl BranchName {
    /// The longest name.
    pub const MAX_LEN: usize = 64;

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A name read back from its row, taken as it is.
    pub(crate) fn from_row(text: String) -> Self {
        Self(text)
    }

    fn fault(text: &str) -> Option<&'static str> {
        if text.is_empty() {
            return Some("is empty");
        }
        if text.len() > Self::MAX_LEN {
            return Some("is longer than 64 characters");
        }
        if !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Some("may use only letters, digits, hyphens, underscores and dots");
        }
        if text.starts_with('-') || text.starts_with('.') {
            return Some("may not start with a hyphen or a dot");
        }
        None
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BranchName {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        match Self::fault(text) {
            None => Ok(Self(text.to_owned())),
            Some(fault) => Err(Error::Malformed(format!("branch name {text:?} {fault}"))),
        }
    }
}

/// A design as its row has it. The owner is the one person it belongs
/// to (`roles`: exactly one, since the column is never null); the other
/// roles are rows of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Design {
    pub id: DesignId,
    pub owner_id: AccountId,
    pub slug: Slug,
    pub name: String,
    pub default_branch: String,
    pub visibility: Visibility,
}

/// A design's row, read by column name so the columns can sit inside a
/// wider row (a page's, a listing's) unchanged.
#[derive(sqlx::FromRow)]
pub(crate) struct DesignRow {
    id: DesignId,
    owner_id: AccountId,
    slug: String,
    name: String,
    default_branch: String,
    visibility: String,
}

/// The columns every read of a design selects, under [`DesignRow`]'s
/// names, from a `design` aliased `d`.
pub(crate) const DESIGN_COLUMNS: &str =
    "d.id, d.owner_id, d.slug, d.name, d.default_branch, d.visibility";

impl TryFrom<DesignRow> for Design {
    type Error = Error;

    fn try_from(row: DesignRow) -> Result<Self> {
        Ok(Design {
            id: row.id,
            owner_id: row.owner_id,
            slug: Slug::from_row(row.slug),
            name: row.name,
            default_branch: row.default_branch,
            visibility: Visibility::from_row(&row.visibility)?,
        })
    }
}

/// `owner`'s designs, newest first.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row's
/// visibility is not one, which the schema forbids.
pub async fn by_owner(db: &PgPool, owner: AccountId) -> Result<Vec<Design>> {
    // One read on the pool: a listing, part of no write.
    let rows: Vec<DesignRow> = sqlx::query_as(&format!(
        "SELECT {DESIGN_COLUMNS} FROM design d
         WHERE d.owner_id = $1 ORDER BY d.created_at DESC, d.id DESC"
    ))
    .bind(owner)
    .fetch_all(db)
    .await?;
    rows.into_iter().map(Design::try_from).collect()
}

/// `owner`'s design at `slug`, within the upload's transaction. Not
/// locked: the head is read next and moved by compare-and-set
/// ([`set_head`]), so of two pushes at once the second is refused rather
/// than silently appended onto a head its uploader never saw.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when the
/// row's visibility is not one, which the schema forbids.
pub async fn find_owned(
    tx: &mut PgConnection,
    owner: AccountId,
    slug: &Slug,
) -> Result<Option<Design>> {
    let row: Option<DesignRow> = sqlx::query_as(&format!(
        "SELECT {DESIGN_COLUMNS} FROM design d WHERE d.owner_id = $1 AND d.slug = $2"
    ))
    .bind(owner)
    .bind(slug.as_str())
    .fetch_optional(tx)
    .await?;
    row.map(Design::try_from).transpose()
}

/// The design at `/{owner}/{slug}`: the one a push names, whoever pushes
/// (`roles::require` says whether they may), within the upload's
/// transaction; on the pool for a page about the design. Not locked, for
/// the reason [`find_owned`] gives.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when the
/// row's visibility is not one, which the schema forbids.
pub async fn find_at<'c>(
    db: impl PgExecutor<'c>,
    owner: &Handle,
    slug: &Slug,
) -> Result<Option<Design>> {
    let row: Option<DesignRow> = sqlx::query_as(&format!(
        "SELECT {DESIGN_COLUMNS} FROM design d
         JOIN account a ON a.id = d.owner_id
         WHERE a.handle = $1 AND d.slug = $2"
    ))
    .bind(owner.as_str())
    .bind(slug.as_str())
    .fetch_optional(db)
    .await?;
    row.map(Design::try_from).transpose()
}

/// A new design of `owner` at `slug`, within the upload's transaction. A
/// redirect an earlier design of theirs left at `slug` ends here: the
/// slug is this design's now.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::SlugTaken`] when the owner has one
/// there already; [`Error::Db`] when Postgres refuses otherwise.
pub async fn create(
    tx: &mut PgConnection,
    owner: AccountId,
    slug: &Slug,
    name: &str,
) -> Result<Design> {
    crate::slug::release(&mut *tx, owner, slug).await?;
    let inserted: std::result::Result<DesignId, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO design (owner_id, slug, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(owner)
    .bind(slug.as_str())
    .bind(name)
    .fetch_one(tx)
    .await;
    match inserted {
        Ok(id) => Ok(Design {
            id,
            owner_id: owner,
            slug: slug.clone(),
            name: name.to_owned(),
            default_branch: DEFAULT_BRANCH.to_owned(),
            visibility: Visibility::Public,
        }),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(Error::Refused(Refusal::SlugTaken {
                slug: slug.to_string(),
            }))
        }
        Err(error) => Err(error.into()),
    }
}

/// Postgres's `now()` as the text a revision's hash covers and its row
/// stores: UTC, ISO 8601 to the microsecond. Read within the upload's
/// transaction, so the server needs no clock of its own.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn now_text(tx: &mut PgConnection) -> Result<String> {
    Ok(sqlx::query_scalar(
        r#"SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')"#,
    )
    .fetch_one(tx)
    .await?)
}

/// The blob row for `hash`, `size` uncompressed bytes; one already there
/// is left as it is.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn record_blob(tx: &mut PgConnection, hash: &BlobHash, size: u64) -> Result<()> {
    sqlx::query("INSERT INTO blob (hash, size) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING")
        .bind(hash.as_bytes())
        .bind(i64::try_from(size).unwrap_or(i64::MAX))
        .execute(tx)
        .await?;
    Ok(())
}

/// The tree of `entries` and its hash; a tree already there (the same
/// files uploaded again) is left as it is, since its entries are the same
/// by construction.
///
/// # Errors
///
/// [`Error::Malformed`] when two entries share a path; [`Error::Db`] when
/// Postgres refuses.
pub async fn record_tree(tx: &mut PgConnection, entries: &[TreeEntry]) -> Result<TreeHash> {
    let hash = tree_hash(entries)?;
    let inserted = sqlx::query("INSERT INTO tree (hash) VALUES ($1) ON CONFLICT (hash) DO NOTHING")
        .bind(hash.as_bytes())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted == 1 {
        for entry in entries {
            sqlx::query("INSERT INTO tree_entry (tree_hash, path, blob_hash) VALUES ($1, $2, $3)")
                .bind(hash.as_bytes())
                .bind(entry.path.as_str())
                .bind(entry.blob.as_bytes())
                .execute(&mut *tx)
                .await?;
        }
    }
    Ok(hash)
}

/// The revision `draft` describes, with `author` as its account and
/// `metadata` from its board, and its hash. A revision already there (the
/// same draft, which is the same files by the same person in the same
/// microsecond) is left as it is.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn record_revision(
    tx: &mut PgConnection,
    draft: &RevisionDraft<'_>,
    author: AccountId,
    metadata: Option<&Metadata>,
) -> Result<RevisionHash> {
    let hash = revision_hash(draft);
    let count = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO revision (hash, tree_hash, parent_hash, author_id, author_handle,
             created_at, message, format_version, layer_count, copper_layer_count,
             board_width_mm, board_height_mm, fonts, net_count, footprint_count)
         VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         ON CONFLICT (hash) DO NOTHING",
    )
    .bind(hash.as_bytes())
    .bind(draft.tree.as_bytes())
    .bind(draft.parent.as_ref().map(RevisionHash::as_bytes))
    .bind(author)
    .bind(draft.author.as_str())
    .bind(draft.time)
    .bind(draft.message)
    .bind(metadata.map(|m| count(m.format_version)))
    .bind(metadata.map(|m| count(m.layer_count)))
    .bind(metadata.map(|m| count(m.copper_layer_count)))
    .bind(metadata.and_then(|m| m.board_size).map(|(width, _)| width))
    .bind(
        metadata
            .and_then(|m| m.board_size)
            .map(|(_, height)| height),
    )
    .bind(metadata.map(|m| m.fonts.clone()))
    .bind(metadata.map(|m| count(m.net_count)))
    .bind(metadata.map(|m| count(m.footprint_count)))
    .execute(tx)
    .await?;
    Ok(hash)
}

/// The head of `branch` on `design`, or `None` for a branch that does not
/// exist yet.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when the row's
/// hash is not 32 bytes, which the schema forbids.
pub async fn head(
    tx: &mut PgConnection,
    design: DesignId,
    branch: &str,
) -> Result<Option<RevisionHash>> {
    let row: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT head_hash FROM branch WHERE design_id = $1 AND name = $2")
            .bind(design)
            .bind(branch)
            .fetch_optional(tx)
            .await?;
    row.as_deref().map(RevisionHash::from_bytes).transpose()
}

/// Move `branch` on `design` from `expected` (its head when the upload
/// read it; `None` makes the branch) to `new`, by compare-and-set: the
/// read that decided the write is the write's own condition.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::HeadMoved`] when the head is no
/// longer `expected`, or with [`Refusal::BranchTaken`] when `expected` is
/// `None` and the branch exists; [`Error::Db`] when Postgres refuses.
pub async fn set_head(
    tx: &mut PgConnection,
    design: DesignId,
    branch: &str,
    expected: Option<RevisionHash>,
    new: RevisionHash,
) -> Result<()> {
    let (moved, refusal) = match expected {
        None => (
            sqlx::query(
                "INSERT INTO branch (design_id, name, head_hash) VALUES ($1, $2, $3)
                 ON CONFLICT (design_id, name) DO NOTHING",
            )
            .bind(design)
            .bind(branch)
            .bind(new.as_bytes())
            .execute(tx)
            .await?,
            Refusal::BranchTaken {
                branch: branch.to_owned(),
            },
        ),
        Some(expected) => (
            sqlx::query(
                "UPDATE branch SET head_hash = $3, updated_at = now()
                 WHERE design_id = $1 AND name = $2 AND head_hash = $4",
            )
            .bind(design)
            .bind(branch)
            .bind(new.as_bytes())
            .bind(expected.as_bytes())
            .execute(tx)
            .await?,
            Refusal::HeadMoved {
                branch: branch.to_owned(),
            },
        ),
    };
    if moved.rows_affected() == 1 {
        Ok(())
    } else {
        Err(Error::Refused(refusal))
    }
}

/// Whether `design` reaches `revision`: whether `design_revision` lists
/// it, the index every read of a revision is scoped by. On a connection
/// rather than the pool, so a push checks the revision a new branch
/// starts at inside its own transaction and a page checks on the pool.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn reaches<'c, E>(db: E, design: DesignId, revision: RevisionHash) -> Result<bool>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM design_revision WHERE design_id = $1 AND revision_hash = $2)",
    )
    .bind(design)
    .bind(revision.as_bytes())
    .fetch_one(db)
    .await?)
}

/// `revision` becomes one `design` may show.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn record_design_revision(
    tx: &mut PgConnection,
    design: DesignId,
    revision: RevisionHash,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO design_revision (design_id, revision_hash) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(design)
    .bind(revision.as_bytes())
    .execute(tx)
    .await?;
    Ok(())
}

/// How many bytes `owner`'s designs hold (every distinct blob a revision
/// of theirs names, uncompressed), and how many `adding` these blobs would
/// add: the ones among them no design of theirs names yet. Read within
/// the upload's transaction after [`record_blob`], so the sizes are there.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn account_bytes(
    tx: &mut PgConnection,
    owner: AccountId,
    adding: &[BlobHash],
) -> Result<(u64, u64)> {
    let adding: Vec<Vec<u8>> = adding.iter().map(|hash| hash.as_bytes().to_vec()).collect();
    // The reachable set is one index walk per design of the owner: design
    // by owner, design_revision by design, revision and tree_entry by
    // primary key; the sums join blob by its primary key from that set
    // and from the given hashes, so neither scans blob.
    let (used, added): (i64, i64) = sqlx::query_as(
        "WITH reachable AS (
             SELECT DISTINCT te.blob_hash
             FROM design d
             JOIN design_revision dr ON dr.design_id = d.id
             JOIN revision r ON r.hash = dr.revision_hash
             JOIN tree_entry te ON te.tree_hash = r.tree_hash
             WHERE d.owner_id = $1
         ),
         used AS (
             SELECT COALESCE(SUM(b.size), 0)::bigint AS bytes
             FROM reachable x JOIN blob b ON b.hash = x.blob_hash
         ),
         added AS (
             SELECT COALESCE(SUM(b.size), 0)::bigint AS bytes
             FROM blob b
             WHERE b.hash = ANY($2)
               AND NOT EXISTS (SELECT 1 FROM reachable x WHERE x.blob_hash = b.hash)
         )
         SELECT used.bytes, added.bytes FROM used, added",
    )
    .bind(owner)
    .bind(&adding)
    .fetch_one(tx)
    .await?;
    Ok((
        u64::try_from(used).unwrap_or(0),
        u64::try_from(added).unwrap_or(0),
    ))
}

/// Whether the operator has blocked this blob site-wide. One read on the
/// pool: a gate before an upload stores or a route serves, part of no
/// write.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn is_blocked(db: &PgPool, hash: &BlobHash) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blocked_blob WHERE hash = $1)")
            .bind(hash.as_bytes())
            .fetch_one(db)
            .await?,
    )
}

/// A revision as a page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    pub hash: RevisionHash,
    pub tree: TreeHash,
    pub parent: Option<RevisionHash>,
    /// The handle the hash covered, as text: it outlives the account.
    pub author: String,
    /// When, UTC, ISO 8601 to the second (`2026-11-02T12:00:00Z`).
    pub created_at: String,
    pub message: Option<String>,
    /// What the board said, or `None` for a revision without one.
    pub metadata: Option<Metadata>,
}

/// A revision's row, read by column name so the revision columns can sit
/// inside a wider row ([`DesignHead`]'s) unchanged.
#[derive(sqlx::FromRow)]
struct RevisionRow {
    hash: Vec<u8>,
    tree_hash: Vec<u8>,
    parent_hash: Option<Vec<u8>>,
    author_handle: String,
    /// Formatted by Postgres: UTC, ISO 8601 to the second.
    created_at: String,
    message: Option<String>,
    format_version: Option<i32>,
    layer_count: Option<i32>,
    copper_layer_count: Option<i32>,
    board_width_mm: Option<f64>,
    board_height_mm: Option<f64>,
    fonts: Option<Vec<String>>,
    net_count: Option<i32>,
    footprint_count: Option<i32>,
}

/// The columns every read of a revision selects, under [`RevisionRow`]'s
/// names, from a `revision` aliased `r`.
const REVISION_COLUMNS: &str = r#"r.hash, r.tree_hash, r.parent_hash, r.author_handle,
    to_char(r.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS created_at,
    r.message, r.format_version, r.layer_count, r.copper_layer_count,
    r.board_width_mm, r.board_height_mm, r.fonts, r.net_count, r.footprint_count"#;

impl TryFrom<RevisionRow> for Revision {
    type Error = Error;

    fn try_from(row: RevisionRow) -> Result<Self> {
        let count = |n: Option<i32>| n.and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
        let metadata = row.format_version.map(|version| Metadata {
            format_version: u32::try_from(version).unwrap_or(0),
            layer_count: count(row.layer_count),
            copper_layer_count: count(row.copper_layer_count),
            board_size: row.board_width_mm.zip(row.board_height_mm),
            fonts: row.fonts.unwrap_or_default(),
            net_count: count(row.net_count),
            footprint_count: count(row.footprint_count),
        });
        Ok(Revision {
            hash: RevisionHash::from_bytes(&row.hash)?,
            tree: TreeHash::from_bytes(&row.tree_hash)?,
            parent: row
                .parent_hash
                .as_deref()
                .map(RevisionHash::from_bytes)
                .transpose()?,
            author: row.author_handle,
            created_at: row.created_at,
            message: row.message,
            metadata,
        })
    }
}

/// A branch of a design: its name and its head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: BranchName,
    pub head: RevisionHash,
}

/// One revision along a branch: its ordinal (GLOSSARY, Ordinal) and the
/// revision. What a history page lists.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// How many revisions deep it sits along the branch, from 1 at the
    /// root.
    pub ordinal: u32,
    pub revision: Revision,
}

/// Which revision of a design a URL names (docs/v1-scope.md, URLs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At<'a> {
    /// `/{handle}/{slug}`: the head of the default branch.
    Default,
    /// `/{handle}/{slug}/b/{branch}`: the head of this branch.
    Branch(&'a BranchName),
    /// `/{handle}/{slug}/r/{hash}`: this revision, when the design
    /// reaches it, in the context of `branch` when a link from that
    /// branch's page says so and the branch reaches the revision, else
    /// of the default branch when it does, else of the first branch by
    /// name that does.
    Revision {
        hash: RevisionHash,
        branch: Option<&'a BranchName>,
    },
}

/// How many revisions a design page's revision dropdown lists.
pub const RECENT: u32 = 10;

/// What a design page shows: the design, its owner's handle, the
/// revision with its files, and the branch in context: the one named, or
/// for a revision named by hash the default branch when it reaches the
/// revision, else the first by name that does.
#[derive(Debug, Clone, PartialEq)]
pub struct DesignHead {
    pub design: Design,
    pub owner: Handle,
    /// The branch in context.
    pub branch: BranchName,
    /// Its head: the revision shown, or the one `behind` revisions ahead.
    pub head: RevisionHash,
    pub revision: Revision,
    /// The revision's ordinal along the branch in context.
    pub ordinal: u32,
    /// How many revisions the branch's head is ahead of the one shown: 0
    /// at the head.
    pub behind: u32,
    /// The tree's entries, in path order.
    pub files: Vec<TreeEntry>,
    /// Every branch of the design, by name.
    pub branches: Vec<Branch>,
    /// The last [`RECENT`] revisions along the branch in context, newest
    /// first.
    pub recent: Vec<HistoryEntry>,
}

impl DesignHead {
    /// The board the viewer loads: the shallowest `.kicad_pcb`, first by
    /// name among equals, or `None` for a revision without one.
    #[must_use]
    pub fn board(&self) -> Option<&TreeEntry> {
        self.shallowest("kicad_pcb")
    }

    /// The files with `extension`, in path order.
    fn with_extension<'a>(&'a self, extension: &'a str) -> impl Iterator<Item = &'a TreeEntry> {
        self.files
            .iter()
            .filter(move |entry| entry.path.extension().as_deref() == Some(extension))
    }

    /// The shallowest file with `extension`, first by name among equals.
    fn shallowest<'a>(&'a self, extension: &'a str) -> Option<&'a TreeEntry> {
        self.with_extension(extension)
            .min_by_key(|entry| (entry.path.depth(), &entry.path))
    }

    /// The root sheet the schematic page loads: the `.kicad_sch` named
    /// like the project file (KiCad names the root sheet after the
    /// project), else like the board, else the shallowest, first by name
    /// among equals; `None` for a revision without one.
    #[must_use]
    pub fn schematic(&self) -> Option<&TreeEntry> {
        let stem = |entry: &TreeEntry| {
            entry
                .path
                .file_name()
                .rsplit_once('.')
                .map(|(stem, _)| stem.to_owned())
        };
        let named_like = |extension: &str| {
            let anchor = self.shallowest(extension)?;
            let wanted = stem(anchor)?;
            self.with_extension("kicad_sch")
                .filter(|entry| entry.path.depth() == anchor.path.depth())
                .find(|entry| stem(entry).as_deref() == Some(wanted.as_str()))
        };
        named_like("kicad_pro")
            .or_else(|| named_like("kicad_pcb"))
            .or_else(|| self.shallowest("kicad_sch"))
    }

    /// Whether the revision opens on its schematic: it has no board for
    /// the viewer and a root sheet to show instead (docs/v1-scope.md,
    /// Schematic). Every page that shows a revision (the design page,
    /// its branch and revision forms) sends the browser to the schematic
    /// page when this holds.
    #[must_use]
    pub fn opens_on_schematic(&self) -> bool {
        self.board().is_none() && self.schematic().is_some()
    }
}

/// The design at `/{handle}/{slug}` with the head of its default branch,
/// or `None` when there is no such design or the branch has no head yet.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn show(db: &PgPool, handle: &str, slug: &str) -> Result<Option<DesignHead>> {
    show_at(db, handle, slug, At::Default).await
}

/// The design at `/{handle}/{slug}` shown at `at`, or `None` when there
/// is no such design, no such branch, or the design does not reach the
/// revision named (a revision no branch of the design has in its chain is
/// not shown either, whatever `design_revision` still lists: the index
/// is maintained on branch delete, so such a row is the collector's).
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn show_at(
    db: &PgPool,
    handle: &str,
    slug: &str,
    at: At<'_>,
) -> Result<Option<DesignHead>> {
    // Several reads on the pool for one page, nothing written. A push
    // between two of them moves a head the page already read: the page
    // then shows the revision it resolved with that revision's files and
    // a chain read a moment later, which is at worst one revision ahead
    // of the head it named. Nothing shown is keyed by anything but the
    // hashes the earlier reads returned.
    let Some((design, owner)) = find(db, handle, slug).await? else {
        return Ok(None);
    };
    let branches = branches(db, design.id).await?;
    let hash = match at {
        At::Default | At::Branch(_) => {
            let name = match at {
                At::Branch(name) => name.as_str(),
                _ => design.default_branch.as_str(),
            };
            match branches.iter().find(|branch| branch.name.as_str() == name) {
                Some(branch) => branch.head,
                None => return Ok(None),
            }
        }
        At::Revision { hash, .. } => {
            if !reaches(db, design.id, hash).await? {
                return Ok(None);
            }
            hash
        }
    };
    let positions = positions(db, design.id, hash).await?;
    let on = |name: &str| positions.iter().find(|p| p.branch.as_str() == name);
    let position = match at {
        At::Branch(name) => on(name.as_str()),
        At::Default => on(&design.default_branch).or_else(|| positions.first()),
        At::Revision { branch, .. } => branch
            .and_then(|name| on(name.as_str()))
            .or_else(|| on(&design.default_branch))
            .or_else(|| positions.first()),
    };
    let Some(position) = position.cloned() else {
        return Ok(None);
    };
    let Some(revision) = revision(db, hash).await? else {
        return Ok(None);
    };
    let files = entries(db, revision.tree).await?;
    let recent = history(db, design.id, &position.branch, Some(RECENT)).await?;
    let head = branches
        .iter()
        .find(|branch| branch.name == position.branch)
        .map_or(hash, |branch| branch.head);
    Ok(Some(DesignHead {
        design,
        owner,
        branch: position.branch,
        head,
        revision,
        ordinal: position.ordinal,
        behind: position.behind,
        files,
        branches,
        recent,
    }))
}

/// The design at `/{handle}/{slug}` and its owner's handle, or `None`
/// without one. One read on the pool: a page's lookup, part of no write.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn find(db: &PgPool, handle: &str, slug: &str) -> Result<Option<(Design, Handle)>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        design: DesignRow,
        handle: String,
    }
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {DESIGN_COLUMNS}, a.handle
         FROM design d JOIN account a ON a.id = d.owner_id
         WHERE a.handle = $1 AND d.slug = $2"
    ))
    .bind(handle)
    .bind(slug)
    .fetch_optional(db)
    .await?;
    row.map(|row| Ok((Design::try_from(row.design)?, Handle::from_row(row.handle))))
        .transpose()
}

/// Every branch of `design` with its head, by name. One read on the pool:
/// a page's listing, part of no write.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn branches(db: &PgPool, design: DesignId) -> Result<Vec<Branch>> {
    let rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT name, head_hash FROM branch WHERE design_id = $1 ORDER BY name")
            .bind(design)
            .fetch_all(db)
            .await?;
    rows.into_iter()
        .map(|(name, head)| {
            Ok(Branch {
                name: BranchName::from_row(name),
                head: RevisionHash::from_bytes(&head)?,
            })
        })
        .collect()
}

/// Where a revision sits on one branch of a design.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Position {
    branch: BranchName,
    ordinal: u32,
    behind: u32,
}

/// The chain from a branch's head down through the parents, as SQL: one
/// row per revision with how far behind the head it is. Over every branch
/// of the design (`$1`) with `$2` null, or the one named `$2`. Each step
/// is a primary-key probe of `revision`, so the walk costs the chains'
/// length, never a scan; a chain cannot loop, since a revision's hash
/// covers its parent's.
const CHAIN: &str = "chain AS (
    SELECT b.name, b.head_hash AS hash, 0 AS behind
    FROM branch b WHERE b.design_id = $1 AND ($2::text IS NULL OR b.name = $2)
    UNION ALL
    SELECT c.name, r.parent_hash, c.behind + 1
    FROM chain c JOIN revision r ON r.hash = c.hash WHERE r.parent_hash IS NOT NULL
)";

/// The ordinal of a row of the chain: the chain's length less how far
/// behind the head the row is.
fn ordinal(length: i64, behind: i64) -> u32 {
    u32::try_from(length - behind).unwrap_or(0)
}

/// On which branches of `design` the revision `hash` sits, by branch
/// name, with its ordinal and distance behind the head on each. One read
/// on the pool: a page's lookup, part of no write.
async fn positions(db: &PgPool, design: DesignId, hash: RevisionHash) -> Result<Vec<Position>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(&format!(
        "WITH RECURSIVE {CHAIN}
         SELECT name, behind::bigint, length FROM (
             SELECT name, hash, behind, count(*) OVER (PARTITION BY name) AS length FROM chain
         ) x WHERE x.hash = $3 ORDER BY name"
    ))
    .bind(design)
    .bind(None::<&str>)
    .bind(hash.as_bytes())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, behind, length)| Position {
            branch: BranchName::from_row(name),
            ordinal: ordinal(length, behind),
            behind: u32::try_from(behind).unwrap_or(u32::MAX),
        })
        .collect())
}

/// The revisions along `branch` of `design`, newest first, each with its
/// ordinal; at most `limit` of them, every one without a limit; empty for
/// a branch that does not exist. One read on the pool: a page's listing,
/// part of no write.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn history(
    db: &PgPool,
    design: DesignId,
    branch: &BranchName,
    limit: Option<u32>,
) -> Result<Vec<HistoryEntry>> {
    #[derive(sqlx::FromRow)]
    struct EntryRow {
        behind: i64,
        length: i64,
        #[sqlx(flatten)]
        revision: RevisionRow,
    }
    let rows: Vec<EntryRow> = sqlx::query_as(&format!(
        "WITH RECURSIVE {CHAIN}
         SELECT c.behind::bigint AS behind, count(*) OVER () AS length, {REVISION_COLUMNS}
         FROM chain c JOIN revision r ON r.hash = c.hash
         ORDER BY c.behind LIMIT $3"
    ))
    .bind(design)
    .bind(Some(branch.as_str()))
    .bind(limit.map(i64::from))
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(HistoryEntry {
                ordinal: ordinal(row.length, row.behind),
                revision: Revision::try_from(row.revision)?,
            })
        })
        .collect()
}

/// The revision `hash`, or `None` without one. Called after the design
/// was checked to reach it: the row is global, the check is the scope.
async fn revision(db: &PgPool, hash: RevisionHash) -> Result<Option<Revision>> {
    let row: Option<RevisionRow> = sqlx::query_as(&format!(
        "SELECT {REVISION_COLUMNS} FROM revision r WHERE r.hash = $1"
    ))
    .bind(hash.as_bytes())
    .fetch_optional(db)
    .await?;
    row.map(Revision::try_from).transpose()
}

/// The entries of `tree`, in path order.
async fn entries(db: &PgPool, tree: TreeHash) -> Result<Vec<TreeEntry>> {
    let rows: Vec<(String, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT te.path, te.blob_hash, b.size FROM tree_entry te
         JOIN blob b ON b.hash = te.blob_hash
         WHERE te.tree_hash = $1 ORDER BY te.path",
    )
    .bind(tree.as_bytes())
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(path, blob, size)| {
            Ok(TreeEntry {
                path: TreePath::from_str(&path)?,
                blob: BlobHash::from_bytes(&blob)?,
                size: u64::try_from(size).unwrap_or(0),
            })
        })
        .collect()
}
