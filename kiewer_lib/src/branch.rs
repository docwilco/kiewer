//! Managing a design's branches (docs/v1-scope.md, Storage and history;
//! GLOSSARY, Branch, Default branch): the branches page's list, and
//! rename, delete, set-default and new-branch-here, each one outcome in
//! one transaction. Pushes are `crate::upload`'s; `crate::design` holds
//! the reads every page makes.
//!
//! Who may: a collaborator manages their own branches, the ones they
//! made; an admin or the owner any branch; the default branch is renamed
//! by an admin or the owner, and only they change which branch it is
//! ([`Allowed`]). The default branch cannot be deleted while it is the
//! default, whoever asks. A new branch at a revision takes the
//! collaborator role and writes one branch row: no revision.
//!
//! Each of the three changes holds the design's row exclusively before
//! it reads anything, which waits out every push and every new branch
//! (they hold it shared, `design::Hold`): so the default branch cannot
//! move onto a branch being deleted, and a new branch cannot start at a
//! revision a delete is taking out of the index.

use sqlx::{PgConnection, PgPool};

use crate::account::AccountId;
use crate::design::{self, BranchName, CHAIN, DesignId, Hold};
use crate::error::{Error, Refusal, Result};
use crate::revision::RevisionHash;
use crate::roles::{self, Role};

/// One branch as the branches page lists it: head, ordinal, age, author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: BranchName,
    pub head: RevisionHash,
    /// The head's ordinal: how many revisions long the branch is.
    pub ordinal: u32,
    /// How long ago the head was pushed, in seconds.
    pub age_secs: u64,
    /// Who pushed the head.
    pub author: String,
    /// Who made the branch, when it is known.
    pub created_by: Option<AccountId>,
}

impl Listed {
    /// The head's age in the largest whole unit, for the page.
    #[must_use]
    pub fn age(&self) -> String {
        age_in_words(self.age_secs)
    }
}

/// `seconds` in the largest whole unit (`3 days ago`, `1 hour ago`), or
/// `just now` under a minute.
fn age_in_words(seconds: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(86_400, "day"), (3_600, "hour"), (60, "minute")];
    for (size, unit) in UNITS {
        let count = seconds / size;
        if count > 0 {
            let plural = if count == 1 { "" } else { "s" };
            return format!("{count} {unit}{plural} ago");
        }
    }
    "just now".to_owned()
}

/// What a person may do to one branch, as the page offers it and each
/// write checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowed {
    pub rename: bool,
    pub delete: bool,
    pub set_default: bool,
}

impl Allowed {
    /// What `role` may do to a branch that is `mine` or not and the
    /// design's default or not. Delete and set-default are false on the
    /// default branch for everyone: there is nothing to do, or the
    /// default-branch refusal says why not.
    #[must_use]
    pub fn of(role: Option<Role>, mine: bool, is_default: bool) -> Self {
        let admin = role.is_some_and(|role| role >= Role::Admin);
        let collaborator = role.is_some();
        Self {
            rename: admin || (collaborator && mine && !is_default),
            delete: !is_default && (admin || (collaborator && mine)),
            set_default: admin && !is_default,
        }
    }
}

/// Every branch of `design` with what its page lists, by name. One read
/// on the pool: a page's listing, part of no write.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn list(db: &PgPool, design: DesignId) -> Result<Vec<Listed>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        name: String,
        head_hash: Vec<u8>,
        length: i64,
        age_secs: i64,
        author_handle: String,
        created_by: Option<AccountId>,
    }
    // One walk over the design's chains, whose cost is their length
    // (`design::CHAIN`); the heads join revision by its key.
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "WITH RECURSIVE {CHAIN}
         SELECT b.name, b.head_hash, l.length, r.author_handle, b.created_by,
                EXTRACT(EPOCH FROM now() - r.created_at)::bigint AS age_secs
         FROM branch b
         JOIN revision r ON r.hash = b.head_hash
         JOIN (SELECT name, count(*) AS length FROM chain GROUP BY name) l ON l.name = b.name
         WHERE b.design_id = $1
         ORDER BY b.name"
    ))
    .bind(design)
    .bind(None::<&str>)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Listed {
                name: BranchName::from_row(row.name),
                head: RevisionHash::from_bytes(&row.head_hash)?,
                ordinal: u32::try_from(row.length).unwrap_or(u32::MAX),
                age_secs: u64::try_from(row.age_secs).unwrap_or(0),
                author: row.author_handle,
                created_by: row.created_by,
            })
        })
        .collect()
}

/// What a change found, with the design held and the branch's row held.
struct Found {
    role: Role,
    is_default: bool,
    allowed: Allowed,
}

/// The first steps of a change to `name` on `design` by `account`: the
/// design held exclusively, the account's role (none is refused), the
/// branch's row held (none is refused), and what the account may do.
async fn find(
    tx: &mut PgConnection,
    design: DesignId,
    account: AccountId,
    name: &BranchName,
) -> Result<Found> {
    let no_branch = || {
        Error::Refused(Refusal::NoSuchBranch {
            branch: name.to_string(),
        })
    };
    let design = design::lock(tx, design, Hold::Exclusive)
        .await?
        .ok_or_else(no_branch)?;
    let role = roles::require(&mut *tx, design.id, account, Role::Collaborator).await?;
    let creator: Option<Option<AccountId>> = sqlx::query_scalar(
        "SELECT created_by FROM branch WHERE design_id = $1 AND name = $2 FOR UPDATE",
    )
    .bind(design.id)
    .bind(name.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let creator = creator.ok_or_else(no_branch)?;
    let is_default = name.as_str() == design.default_branch;
    let allowed = Allowed::of(Some(role), creator == Some(account), is_default);
    Ok(Found {
        role,
        is_default,
        allowed,
    })
}

/// The refusal of a change an account's role does not cover: only an
/// admin or the owner goes further than a collaborator here.
fn forbidden() -> Error {
    Error::Forbidden {
        needed: Role::Admin,
    }
}

/// `from` on `design` becomes `to`, the design's default branch with it
/// when it is that; every URL of the branch moves with the name, and a
/// held upload aimed at it follows through its foreign key.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::NoSuchBranch`] or
/// [`Refusal::BranchTaken`]; [`Error::Forbidden`] when `account` may not;
/// [`Error::Db`] when Postgres refuses.
pub async fn rename(
    db: &PgPool,
    account: AccountId,
    design: DesignId,
    from: &BranchName,
    to: &BranchName,
) -> Result<()> {
    let mut tx = db.begin().await?;
    let found = find(&mut tx, design, account, from).await?;
    if !found.allowed.rename {
        return Err(forbidden());
    }
    if from == to {
        return Ok(());
    }
    let renamed = sqlx::query("UPDATE branch SET name = $3 WHERE design_id = $1 AND name = $2")
        .bind(design)
        .bind(from.as_str())
        .bind(to.as_str())
        .execute(&mut *tx)
        .await;
    match renamed {
        Ok(_) => {}
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(Refusal::BranchTaken {
                branch: to.to_string(),
            }
            .into());
        }
        Err(error) => return Err(error.into()),
    }
    if found.is_default {
        write_default(&mut tx, design, to).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `design`'s row says `name` is its default branch.
async fn write_default(tx: &mut PgConnection, design: DesignId, name: &BranchName) -> Result<()> {
    sqlx::query("UPDATE design SET default_branch = $2 WHERE id = $1")
        .bind(design)
        .bind(name.as_str())
        .execute(tx)
        .await?;
    Ok(())
}

/// `name` on `design` goes, and with it the index rows of revisions no
/// other branch of the design reaches (ADR 0002): those stop being the
/// design's at once, and the collector takes the rows themselves later.
/// A held upload aimed at the branch goes through its foreign key.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::DefaultBranch`] for the default
/// branch, whoever asks, or [`Refusal::NoSuchBranch`];
/// [`Error::Forbidden`] when `account` may not; [`Error::Db`] when
/// Postgres refuses.
pub async fn delete(
    db: &PgPool,
    account: AccountId,
    design: DesignId,
    name: &BranchName,
) -> Result<()> {
    let mut tx = db.begin().await?;
    let found = find(&mut tx, design, account, name).await?;
    if found.is_default {
        return Err(Refusal::DefaultBranch {
            branch: name.to_string(),
        }
        .into());
    }
    if !found.allowed.delete {
        return Err(forbidden());
    }
    sqlx::query("DELETE FROM branch WHERE design_id = $1 AND name = $2")
        .bind(design)
        .bind(name.as_str())
        .execute(&mut *tx)
        .await?;
    // Bounded by the design's own index rows and its chains: the primary
    // key's prefix, and `design::CHAIN`'s walk.
    sqlx::query(&format!(
        "WITH RECURSIVE {CHAIN}
         DELETE FROM design_revision dr
         WHERE dr.design_id = $1
           AND NOT EXISTS (SELECT 1 FROM chain c WHERE c.hash = dr.revision_hash)"
    ))
    .bind(design)
    .bind(None::<&str>)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `name` becomes `design`'s default branch: what its bare URL shows.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::NoSuchBranch`];
/// [`Error::Forbidden`] below admin; [`Error::Db`] when Postgres refuses.
pub async fn set_default(
    db: &PgPool,
    account: AccountId,
    design: DesignId,
    name: &BranchName,
) -> Result<()> {
    let mut tx = db.begin().await?;
    let found = find(&mut tx, design, account, name).await?;
    if found.role < Role::Admin {
        return Err(forbidden());
    }
    if !found.is_default {
        write_default(&mut tx, design, name).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// A new branch `name` on `design` pointing at `at`, a revision the
/// design reaches, made by `account`: one branch row, no revision. It
/// moves no head, so its `updated_at` is the revision's own time, and
/// the design stays where its last push put it in the listings.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::NoSuchRevision`] or
/// [`Refusal::BranchTaken`]; [`Error::Forbidden`] without the
/// collaborator role; [`Error::Db`] when Postgres refuses.
pub async fn create_at(
    db: &PgPool,
    account: AccountId,
    design: DesignId,
    name: &BranchName,
    at: RevisionHash,
) -> Result<()> {
    let mut tx = db.begin().await?;
    design::lock(&mut tx, design, Hold::Shared).await?;
    roles::require(&mut *tx, design, account, Role::Collaborator).await?;
    if !design::reaches(&mut *tx, design, at).await? {
        return Err(Refusal::NoSuchRevision {
            hash: at.to_string(),
        }
        .into());
    }
    let made = sqlx::query(
        "INSERT INTO branch (design_id, name, head_hash, created_by, updated_at)
         SELECT $1, $2, r.hash, $4, r.created_at FROM revision r WHERE r.hash = $3
         ON CONFLICT (design_id, name) DO NOTHING",
    )
    .bind(design)
    .bind(name.as_str())
    .bind(at.as_bytes())
    .bind(account)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if made == 0 {
        return Err(Refusal::BranchTaken {
            branch: name.to_string(),
        }
        .into());
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const fn allowed(rename: bool, delete: bool, set_default: bool) -> Allowed {
        Allowed {
            rename,
            delete,
            set_default,
        }
    }

    #[test_case(0, "just now")]
    #[test_case(59, "just now")]
    #[test_case(60, "1 minute ago")]
    #[test_case(7_200, "2 hours ago")]
    #[test_case(86_400, "1 day ago")]
    #[test_case(864_000, "10 days ago")]
    fn an_age_is_said_in_its_largest_unit(seconds: u64, words: &str) {
        assert_eq!(age_in_words(seconds), words);
    }

    #[test_case(None, true, false, allowed(false, false, false); "no role, even on a branch they made")]
    #[test_case(Some(Role::Collaborator), false, false, allowed(false, false, false); "a collaborator on another's branch")]
    #[test_case(Some(Role::Collaborator), true, false, allowed(true, true, false); "a collaborator on their own")]
    #[test_case(Some(Role::Collaborator), true, true, allowed(false, false, false); "a collaborator on their own default")]
    #[test_case(Some(Role::Admin), false, false, allowed(true, true, true); "an admin on any branch")]
    #[test_case(Some(Role::Admin), false, true, allowed(true, false, false); "an admin on the default")]
    #[test_case(Some(Role::Owner), false, false, allowed(true, true, true); "the owner on any branch")]
    #[test_case(Some(Role::Owner), false, true, allowed(true, false, false); "the owner on the default")]
    fn the_role_decides_what_is_offered(
        role: Option<Role>,
        mine: bool,
        is_default: bool,
        expected: Allowed,
    ) {
        assert_eq!(Allowed::of(role, mine, is_default), expected);
    }
}
