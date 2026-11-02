//! The listings (docs/v1-scope.md, Pages and URLs): the home page's
//! recent public designs and a person's public designs. Only a public
//! design with a head is listed: an unlisted one opens by its link and
//! appears here never, and a design without a revision has no page to
//! open. Both reads are the same for everyone, so the pages that show
//! them stay cacheable (ADR 0003).

use sqlx::PgPool;

use crate::account::AccountId;
use crate::error::Result;
use crate::revision::RevisionHash;
use crate::thumbnail::{self, SHOWABLE, Size};

/// How many designs the home page lists.
pub const RECENT: i64 = 20;

/// One design as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Listed {
    /// The owner's handle: the first segment of the design's URL.
    pub handle: String,
    pub slug: String,
    pub name: String,
    /// When the default branch's head last moved, UTC, ISO 8601 to the
    /// second.
    pub updated_at: String,
    /// The head's hash, 64 hex characters: what its thumbnail's URL is
    /// under.
    pub head: String,
    /// Whether the head has a thumbnail to show (ADR 0004).
    pub thumbnail: bool,
}

impl Listed {
    /// The design's page.
    #[must_use]
    pub fn url(&self) -> String {
        format!("/{}/{}", self.handle, self.slug)
    }

    /// The head's small thumbnail, once it is rendered and while it is
    /// not blocked; the placeholder otherwise.
    #[must_use]
    pub fn thumbnail_url(&self) -> Option<String> {
        if !self.thumbnail {
            return None;
        }
        let hash: RevisionHash = self.head.parse().ok()?;
        Some(thumbnail::url(&self.handle, &self.slug, hash, Size::Small))
    }
}

/// The columns a listing selects, under [`Listed`]'s names, from a
/// `design d`, its owner `a` and its default branch `b`; the thumbnail
/// flag is one probe of `thumbnail` by its key per row, with the block
/// list's own probes ([`SHOWABLE`]).
fn listed_columns() -> String {
    format!(
        r#"a.handle, d.slug, d.name,
    to_char(b.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS updated_at,
    encode(b.head_hash, 'hex') AS head,
    EXISTS (
        SELECT 1 FROM thumbnail t WHERE t.revision_hash = b.head_hash AND {SHOWABLE}
    ) AS thumbnail"#
    )
}

/// The public designs whose default branch moved most recently, newest
/// first, at most [`RECENT`].
///
/// # Errors
///
/// [`Error::Db`](crate::Error::Db) when Postgres refuses.
pub async fn recent_public(db: &PgPool) -> Result<Vec<Listed>> {
    // One read on the pool: a page's listing, part of no write. Walks
    // branch by its updated_at index, newest first, and stops at the
    // limit; the joins are by primary key. The walk runs past the limit
    // by however many of the newest branches are not a default branch or
    // belong to an unlisted design, which is a few per page while most
    // pushes go to main on a public design; a partial index over the
    // join is the answer if that stops being true.
    Ok(sqlx::query_as(&format!(
        "SELECT {}
         FROM branch b
         JOIN design d ON d.id = b.design_id AND d.default_branch = b.name
         JOIN account a ON a.id = d.owner_id
         WHERE d.visibility = 'public'
         ORDER BY b.updated_at DESC, d.id DESC
         LIMIT $1",
        listed_columns()
    ))
    .bind(RECENT)
    .fetch_all(db)
    .await?)
}

/// The account behind `handle`, or `None`: whether a person's page
/// exists.
///
/// # Errors
///
/// [`Error::Db`](crate::Error::Db) when Postgres refuses.
pub async fn account_at(db: &PgPool, handle: &str) -> Result<Option<AccountId>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM account WHERE handle = $1")
            .bind(handle)
            .fetch_optional(db)
            .await?,
    )
}

/// `owner`'s public designs, the most recently pushed first.
///
/// # Errors
///
/// [`Error::Db`](crate::Error::Db) when Postgres refuses.
pub async fn public_of(db: &PgPool, owner: AccountId) -> Result<Vec<Listed>> {
    // One read on the pool: a page's listing, part of no write. The
    // owner's designs by the (owner_id, slug) index, each's default
    // branch by primary key.
    Ok(sqlx::query_as(&format!(
        "SELECT {}
         FROM design d
         JOIN account a ON a.id = d.owner_id
         JOIN branch b ON b.design_id = d.id AND b.name = d.default_branch
         WHERE d.owner_id = $1 AND d.visibility = 'public'
         ORDER BY b.updated_at DESC, d.id DESC",
        listed_columns()
    ))
    .bind(owner)
    .fetch_all(db)
    .await?)
}
