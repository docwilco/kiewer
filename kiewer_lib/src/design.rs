//! Designs, branches and the rows that persist trees and revisions
//! (ADR 0002; GLOSSARY, Things people own). Every statement over the
//! history tables is here: the writes take the upload's transaction
//! (`upload::commit` is the one outcome they record), the reads take the
//! pool and serve a page. A revision is only ever read through a design
//! (`design_revision`), since a bare hash would be a capability.

use std::fmt;
use std::str::FromStr;

use sqlx::{PgConnection, PgPool};

use crate::account::{AccountId, Handle};
use crate::blob::BlobHash;
use crate::error::{Error, Refusal, Result};
use crate::revision::{
    Metadata, RevisionDraft, RevisionHash, TreeEntry, TreeHash, TreePath, revision_hash, tree_hash,
};

/// A design's row id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, sqlx::Type)]
#[sqlx(transparent)]
pub struct DesignId(i64);

/// The branch every design starts with (GLOSSARY, Default branch).
pub const DEFAULT_BRANCH: &str = "main";

/// A design's name in its URL: lowercase letters, digits and hyphens, 1
/// to 64 characters, no hyphen at either end, unique per owner. Picked
/// from the upload's name here; the full rules, renames and redirects are
/// #22.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slug(String);

impl Slug {
    /// The longest slug.
    pub const MAX_LEN: usize = 64;

    /// What a design with nothing usable in its name is called.
    pub const FALLBACK: &str = "design";

    /// The slug as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A slug read back from its row, taken as it is.
    pub(crate) fn from_row(text: String) -> Self {
        Self(text)
    }

    /// The slug a name suggests: lowercased, every run of other characters
    /// a hyphen, cut to the length; [`Slug::FALLBACK`] when nothing of the
    /// name survives.
    #[must_use]
    pub fn suggest(name: &str) -> Self {
        let mut text = String::with_capacity(name.len());
        for ch in name.chars().flat_map(char::to_lowercase) {
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
        text.parse()
            .unwrap_or_else(|_| Self(Self::FALLBACK.to_owned()))
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
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Some("may use only lowercase letters, digits and hyphens");
        }
        if text.starts_with('-') || text.ends_with('-') {
            return Some("may not start or end with a hyphen");
        }
        None
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Slug {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        match Self::fault(text) {
            None => Ok(Self(text.to_owned())),
            Some(fault) => Err(Error::Malformed(format!("slug {text:?} {fault}"))),
        }
    }
}

/// A design as its row has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Design {
    pub id: DesignId,
    pub owner_id: AccountId,
    pub slug: Slug,
    pub name: String,
    pub default_branch: String,
}

type DesignRow = (DesignId, AccountId, String, String, String);

fn design_from_row((id, owner_id, slug, name, default_branch): DesignRow) -> Design {
    Design {
        id,
        owner_id,
        slug: Slug::from_row(slug),
        name,
        default_branch,
    }
}

/// `owner`'s designs, newest first: what the upload form offers to add a
/// revision to.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn by_owner(db: &PgPool, owner: AccountId) -> Result<Vec<Design>> {
    // One read on the pool: a form's listing, part of no write.
    let rows: Vec<DesignRow> = sqlx::query_as(
        "SELECT id, owner_id, slug, name, default_branch FROM design
         WHERE owner_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(owner)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(design_from_row).collect())
}

/// `owner`'s design at `slug`, within the upload's transaction, locked
/// against another push to it until the transaction ends.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn find_owned(
    tx: &mut PgConnection,
    owner: AccountId,
    slug: &Slug,
) -> Result<Option<Design>> {
    let row: Option<DesignRow> = sqlx::query_as(
        "SELECT id, owner_id, slug, name, default_branch FROM design
         WHERE owner_id = $1 AND slug = $2 FOR UPDATE",
    )
    .bind(owner)
    .bind(slug.as_str())
    .fetch_optional(tx)
    .await?;
    Ok(row.map(design_from_row))
}

/// A new design of `owner` at `slug`, within the upload's transaction.
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
/// longer `expected`; [`Error::Db`] when Postgres refuses.
pub async fn set_head(
    tx: &mut PgConnection,
    design: DesignId,
    branch: &str,
    expected: Option<RevisionHash>,
    new: RevisionHash,
) -> Result<()> {
    let moved = match expected {
        None => {
            sqlx::query(
                "INSERT INTO branch (design_id, name, head_hash) VALUES ($1, $2, $3)
                 ON CONFLICT (design_id, name) DO NOTHING",
            )
            .bind(design)
            .bind(branch)
            .bind(new.as_bytes())
            .execute(tx)
            .await?
        }
        Some(expected) => {
            sqlx::query(
                "UPDATE branch SET head_hash = $3, updated_at = now()
                 WHERE design_id = $1 AND name = $2 AND head_hash = $4",
            )
            .bind(design)
            .bind(branch)
            .bind(new.as_bytes())
            .bind(expected.as_bytes())
            .execute(tx)
            .await?
        }
    };
    if moved.rows_affected() == 1 {
        Ok(())
    } else {
        Err(Error::Refused(Refusal::HeadMoved {
            branch: branch.to_owned(),
        }))
    }
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
    // primary key. Nothing here scans a whole table.
    let (used, added): (i64, i64) = sqlx::query_as(
        "WITH reachable AS (
             SELECT DISTINCT te.blob_hash
             FROM design d
             JOIN design_revision dr ON dr.design_id = d.id
             JOIN revision r ON r.hash = dr.revision_hash
             JOIN tree_entry te ON te.tree_hash = r.tree_hash
             WHERE d.owner_id = $1
         )
         SELECT
             (SELECT COALESCE(SUM(b.size), 0)::bigint FROM blob b
              WHERE b.hash IN (SELECT blob_hash FROM reachable)),
             (SELECT COALESCE(SUM(b.size), 0)::bigint FROM blob b
              WHERE b.hash = ANY($2) AND b.hash NOT IN (SELECT blob_hash FROM reachable))",
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
/// inside a wider row ([`Shown`]'s) unchanged.
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

/// What a design page shows: the design, its owner's handle, the branch
/// and the revision at its head, with the revision's files.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub design: Design,
    pub owner: Handle,
    pub branch: String,
    pub revision: Revision,
    /// The tree's entries, in path order.
    pub files: Vec<TreeEntry>,
}

impl Shown {
    /// The board the viewer loads: the shallowest `.kicad_pcb`, first by
    /// name among equals, or `None` for a revision without one.
    #[must_use]
    pub fn board(&self) -> Option<&TreeEntry> {
        self.files
            .iter()
            .filter(|entry| entry.path.extension().as_deref() == Some("kicad_pcb"))
            .min_by_key(|entry| (entry.path.as_str().matches('/').count(), &entry.path))
    }
}

/// The design at `/{handle}/{slug}` with the head of its default branch,
/// or `None` when there is no such design or the branch has no head yet.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn show(db: &PgPool, handle: &str, slug: &str) -> Result<Option<Shown>> {
    // Two reads on the pool for one page: the head and its files. A push
    // between them could show a head with the files of the same head only,
    // since both are keyed by hashes the first read returned; nothing is
    // written.
    #[derive(sqlx::FromRow)]
    struct HeadRow {
        id: DesignId,
        owner_id: AccountId,
        slug: String,
        name: String,
        default_branch: String,
        handle: String,
        #[sqlx(flatten)]
        revision: RevisionRow,
    }
    let row: Option<HeadRow> = sqlx::query_as(&format!(
        "SELECT d.id, d.owner_id, d.slug, d.name, d.default_branch, a.handle, {REVISION_COLUMNS}
         FROM design d
         JOIN account a ON a.id = d.owner_id
         JOIN branch b ON b.design_id = d.id AND b.name = d.default_branch
         JOIN revision r ON r.hash = b.head_hash
         JOIN design_revision dr ON dr.design_id = d.id AND dr.revision_hash = r.hash
         WHERE a.handle = $1 AND d.slug = $2"
    ))
    .bind(handle)
    .bind(slug)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let revision = Revision::try_from(row.revision)?;
    let files = entries(db, revision.tree).await?;
    Ok(Some(Shown {
        design: Design {
            id: row.id,
            owner_id: row.owner_id,
            slug: Slug::from_row(row.slug),
            name: row.name,
            default_branch: row.default_branch.clone(),
        },
        owner: Handle::from_row(row.handle),
        branch: row.default_branch,
        revision,
        files,
    }))
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

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("midi8knob", "midi8knob"; "a name that is a slug")]
    #[test_case("My Board v2", "my-board-v2"; "case and spaces")]
    #[test_case("--Odd__Name--", "odd-name"; "runs collapsed and ends trimmed")]
    #[test_case("***", "design"; "nothing survives")]
    #[test_case("", "design"; "empty")]
    fn a_name_suggests_a_slug(name: &str, slug: &str) {
        assert_eq!(Slug::suggest(name).as_str(), slug);
    }

    #[test]
    fn a_long_name_is_cut_without_a_trailing_hyphen() {
        let name = format!("{}-x", "a".repeat(Slug::MAX_LEN - 1));
        let slug = Slug::suggest(&name);
        assert_eq!(slug.as_str(), "a".repeat(Slug::MAX_LEN - 1));
    }

    #[test_case(""; "empty")]
    #[test_case("Upper"; "an uppercase letter")]
    #[test_case("-x"; "a leading hyphen")]
    #[test_case("x-"; "a trailing hyphen")]
    #[test_case("a b"; "a space")]
    fn a_malformed_slug_is_refused(text: &str) {
        assert!(matches!(text.parse::<Slug>(), Err(Error::Malformed(_))));
    }
}
