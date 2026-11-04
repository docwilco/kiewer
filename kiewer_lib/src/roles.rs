//! Roles on a design and its visibility (docs/v1-scope.md, Accounts and
//! access; GLOSSARY, People). The owner is the design row's `owner_id`,
//! one column and never null, which is how a design has exactly one
//! owner; admins and collaborators are `design_role` rows. [`require`] is
//! the one check every write route makes before it writes, so a route
//! never reads the roles itself. Authorisation is on writes only: a
//! design, listed or not, is read by anyone with its URL.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, PgPool};

use crate::account::{AccountId, Handle};
use crate::design::{Design, DesignId, design_row};
use crate::error::{Error, Result};
use crate::sql::statement;

/// What a person may do on a design, in rising order: a collaborator
/// pushes revisions and manages their own branches; an admin does that
/// on any branch and manages people, name and description; the owner
/// alone deletes, transfers and changes visibility. The wire name
/// (`admin`) is the `design_role.role` column and the people form's
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Collaborator,
    Admin,
    Owner,
}

impl Role {
    /// The two roles a person is given from the people page: the owner
    /// is made by a transfer, never granted.
    pub const GRANTABLE: [Role; 2] = [Role::Collaborator, Role::Admin];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Collaborator => "collaborator",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        [Role::Collaborator, Role::Admin, Role::Owner]
            .into_iter()
            .find(|role| role.as_str() == text)
            .ok_or_else(|| Error::Malformed(format!("not a role: {text:?}")))
    }
}

/// Whether a design is listed. Public is in every listing; unlisted
/// opens by its link and appears in none. The wire name is the
/// `design.visibility` column and the form's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Unlisted,
}

impl Visibility {
    /// Both, in the order a form offers them.
    pub const ALL: [Visibility; 2] = [Visibility::Public, Visibility::Unlisted];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Unlisted => "unlisted",
        }
    }

    /// The visibility read back from its row, which the schema's CHECK
    /// keeps to the two names.
    pub(crate) fn from_row(text: &str) -> Result<Self> {
        text.parse()
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Visibility {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        Visibility::ALL
            .into_iter()
            .find(|visibility| visibility.as_str() == text)
            .ok_or_else(|| Error::Malformed(format!("not a visibility: {text:?}")))
    }
}

/// `account`'s role on `design`, or `None` for no role or no such design.
/// Takes the pool for a page and a transaction's connection for a write
/// that checks inside its own transaction (the push).
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row's
/// role is not one, which the schema forbids.
pub async fn role_of<'c>(
    db: impl PgExecutor<'c>,
    design: DesignId,
    account: AccountId,
) -> Result<Option<Role>> {
    // One read: the design by its key, its role row for this account by
    // the primary key. Part of a write's transaction when the caller's
    // connection is one.
    let row: Option<Option<String>> = sqlx::query_scalar!(
        r#"SELECT CASE WHEN d.owner_id = $2 THEN 'owner' ELSE r.role END AS "role?"
         FROM design d
         LEFT JOIN design_role r ON r.design_id = d.id AND r.account_id = $2
         WHERE d.id = $1"#,
        design as DesignId,
        account as AccountId
    )
    .fetch_optional(db)
    .await?;
    row.flatten().as_deref().map(str::parse).transpose()
}

/// The check every write route makes: `account`'s role on `design` when
/// it is `at_least` the one the write needs. The role comes back so a
/// write that treats roles differently (a collaborator's own branch) can
/// go on from it.
///
/// # Errors
///
/// [`Error::Forbidden`] naming the role needed when the account's is
/// below it or it has none; [`Error::Db`] when Postgres refuses.
pub async fn require<'c>(
    db: impl PgExecutor<'c>,
    design: DesignId,
    account: AccountId,
    at_least: Role,
) -> Result<Role> {
    match role_of(db, design, account).await? {
        Some(role) if role >= at_least => Ok(role),
        _ => Err(Error::Forbidden { needed: at_least }),
    }
}

/// A design a person has a role on, as the upload form lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub design: Design,
    /// The owner's handle: the first segment of the design's URL.
    pub owner: Handle,
    pub role: Role,
}

impl Membership {
    /// `handle/slug`: the design's path, what the upload form's option
    /// carries.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.design.slug)
    }
}

/// The designs `account` may push to: its own and those it has a role
/// on, newest first.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a role or visibility that is not one, which the schema forbids.
pub async fn designs_of(db: &PgPool, account: AccountId) -> Result<Vec<Membership>> {
    // One read on the pool: a form's listing, part of no write. Two index
    // probes (design by owner, design_role by account) joined, not one
    // OR across both, which would scan design.
    //
    // The creation time is selected as fixed-width UTC text, which sorts
    // as the time does: the crate decodes no timestamps. The ordering
    // names the id by position, since the column's name carries its type
    // (`crate::sql`).
    let rows = statement!(query!(
        [
            "SELECT " DESIGN_COLUMNS r#", a.handle AS "handle!", 'owner' AS "role!",
                to_char(d.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US') AS created_at
         FROM design d JOIN account a ON a.id = d.owner_id
         WHERE d.owner_id = $1
         UNION ALL
         SELECT "# DESIGN_COLUMNS r#", a.handle, r.role,
                to_char(d.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US')
         FROM design_role r
         JOIN design d ON d.id = r.design_id
         JOIN account a ON a.id = d.owner_id
         WHERE r.account_id = $1
         ORDER BY created_at DESC, 1 DESC"#
        ],
        account as AccountId
    ))
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|row| {
            let owner = Handle::from_row(row.handle);
            let role = row.role.parse()?;
            Ok(Membership {
                design: design_row!(row).try_into()?,
                owner,
                role,
            })
        })
        .collect()
}

/// What a change to a design's people came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    /// Made.
    Done,
    /// No account has that handle.
    NoSuchPerson,
    /// The person named is the owner, whose role only a transfer moves.
    TheOwner,
    /// A transfer refused: the new owner already has a design at this
    /// slug, and a design's URL is its owner's handle and its slug.
    SlugTaken,
}

/// One person on a design, as the people page lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub account_id: AccountId,
    pub handle: Handle,
    pub role: Role,
}

/// Everyone with a role on `design`: the owner first, then admins, then
/// collaborators, each group by handle.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row's
/// role is not one or its account has no handle, which the code paths
/// that grant a role forbid.
pub async fn people(db: &PgPool, design: DesignId) -> Result<Vec<Person>> {
    // One read on the pool: a page's listing, part of no write.
    let rows = sqlx::query!(
        r#"SELECT a.id AS "account_id!: AccountId", a.handle AS "handle?", 'owner' AS "role!"
         FROM design d JOIN account a ON a.id = d.owner_id WHERE d.id = $1
         UNION ALL
         SELECT a.id, a.handle, r.role
         FROM design_role r JOIN account a ON a.id = r.account_id WHERE r.design_id = $1"#,
        design as DesignId
    )
    .fetch_all(db)
    .await?;
    let mut people = rows
        .into_iter()
        .map(|row| {
            let handle = row
                .handle
                .ok_or_else(|| Error::Malformed("an account without a handle has a role".into()))?;
            Ok(Person {
                account_id: row.account_id,
                handle: Handle::from_row(handle),
                role: row.role.parse()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    people.sort_by(|a, b| b.role.cmp(&a.role).then_with(|| a.handle.cmp(&b.handle)));
    Ok(people)
}

/// Give the account at `handle` the role `role` on `design`, or change
/// the one it has. The owner's role is never set this way.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
///
/// # Panics
///
/// When `role` is [`Role::Owner`]: the owner is made by [`transfer`].
pub async fn set_role(
    db: &PgPool,
    design: DesignId,
    handle: &Handle,
    role: Role,
) -> Result<Changed> {
    assert!(
        Role::GRANTABLE.contains(&role),
        "the owner is made by a transfer, never granted"
    );
    // One statement: the reads that decide the write (is there such an
    // account, is it the owner) are the write's own conditions. Nothing
    // written is the whole outcome too; the read after only names why.
    // FOR SHARE on the design row: a transfer in flight holds it, so this
    // waits and then sees the new owner, rather than giving the owner a
    // row beside the column.
    let written = sqlx::query!(
        "INSERT INTO design_role (design_id, account_id, role)
         SELECT d.id, a.id, $3 FROM design d, account a
         WHERE d.id = $1 AND a.handle = $2 AND d.owner_id <> a.id
         FOR SHARE OF d
         ON CONFLICT (design_id, account_id) DO UPDATE SET role = EXCLUDED.role",
        design as DesignId,
        handle.as_str(),
        role.as_str()
    )
    .execute(db)
    .await?
    .rows_affected();
    if written == 1 {
        return Ok(Changed::Done);
    }
    why_not(db, design, handle).await
}

/// Take `handle`'s role on `design` away. A person with no role is left
/// as they are; the owner is never removed.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn remove_role(db: &PgPool, design: DesignId, handle: &Handle) -> Result<Changed> {
    // One statement: the removal is the whole outcome; the read after
    // only names why nothing went.
    let removed = sqlx::query!(
        "DELETE FROM design_role r USING account a
         WHERE r.design_id = $1 AND r.account_id = a.id AND a.handle = $2",
        design as DesignId,
        handle.as_str()
    )
    .execute(db)
    .await?
    .rows_affected();
    if removed == 1 {
        return Ok(Changed::Done);
    }
    match why_not(db, design, handle).await? {
        // Not on the design: removing them is already done.
        Changed::Done => Ok(Changed::Done),
        other => Ok(other),
    }
}

/// Why a change to `handle` on `design` wrote nothing: no such account,
/// or it is the owner. `Done` when neither, which a race can leave.
async fn why_not(db: &PgPool, design: DesignId, handle: &Handle) -> Result<Changed> {
    let is_owner: Option<bool> = sqlx::query_scalar!(
        r#"SELECT d.owner_id = a.id AS "is_owner!" FROM account a, design d WHERE a.handle = $2 AND d.id = $1"#,
        design as DesignId,
        handle.as_str()
    )
    .fetch_optional(db)
    .await?;
    Ok(match is_owner {
        None => Changed::NoSuchPerson,
        Some(true) => Changed::TheOwner,
        Some(false) => Changed::Done,
    })
}

/// Make the account at `handle` the owner of `design`: the owner column
/// moves, the new owner's role row goes, and the previous owner becomes
/// an admin, in one transaction.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn transfer(db: &PgPool, design: DesignId, handle: &Handle) -> Result<Changed> {
    // One outcome, one transaction, a write first: the update's conditions
    // are the reads (such an account, not the owner already), and its
    // RETURNING gives the previous owner through the self-join (Postgres
    // 16 has no RETURNING OLD). The unique (owner_id, slug) refuses a slug
    // the new owner has.
    let mut tx = db.begin().await?;
    let moved = sqlx::query!(
        r#"UPDATE design d SET owner_id = a.id
         FROM design old, account a
         WHERE old.id = d.id AND d.id = $1 AND a.handle = $2 AND d.owner_id <> a.id
         RETURNING old.owner_id AS "previous: AccountId", a.id AS "new: AccountId""#,
        design as DesignId,
        handle.as_str()
    )
    .fetch_optional(&mut *tx)
    .await;
    let (previous, new) = match moved {
        Ok(Some(row)) => (row.previous, row.new),
        Ok(None) => {
            drop(tx);
            return why_not(db, design, handle).await;
        }
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Ok(Changed::SlugTaken);
        }
        Err(error) => return Err(error.into()),
    };
    sqlx::query!(
        "DELETE FROM design_role WHERE design_id = $1 AND account_id = $2",
        design as DesignId,
        new as AccountId
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO design_role (design_id, account_id, role) VALUES ($1, $2, 'admin')
         ON CONFLICT (design_id, account_id) DO UPDATE SET role = 'admin'",
        design as DesignId,
        previous as AccountId
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Changed::Done)
}

/// Make `design` `visibility`.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn set_visibility(db: &PgPool, design: DesignId, visibility: Visibility) -> Result<()> {
    // One statement on the pool: the change is the whole outcome.
    sqlx::query!(
        "UPDATE design SET visibility = $2 WHERE id = $1",
        design as DesignId,
        visibility.as_str()
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(Role::Collaborator, "collaborator")]
    #[test_case(Role::Admin, "admin")]
    #[test_case(Role::Owner, "owner")]
    fn a_role_round_trips_through_its_wire_name(role: Role, wire: &str) {
        assert_eq!(role.as_str(), wire);
        assert_eq!(wire.parse::<Role>().unwrap(), role);
        assert_eq!(serde_json::to_string(&role).unwrap(), format!("{wire:?}"));
    }

    #[test]
    fn roles_rise_from_collaborator_to_owner() {
        assert!(Role::Collaborator < Role::Admin && Role::Admin < Role::Owner);
        assert!(!Role::GRANTABLE.contains(&Role::Owner));
    }

    #[test_case(Visibility::Public, "public")]
    #[test_case(Visibility::Unlisted, "unlisted")]
    fn a_visibility_round_trips_through_its_wire_name(visibility: Visibility, wire: &str) {
        assert_eq!(visibility.as_str(), wire);
        assert_eq!(wire.parse::<Visibility>().unwrap(), visibility);
        assert_eq!(
            serde_json::to_string(&visibility).unwrap(),
            format!("{wire:?}")
        );
    }

    #[test_case("Owner"; "the wrong case")]
    #[test_case("root"; "a word that is no role")]
    fn a_text_that_is_no_role_or_visibility_is_refused(text: &str) {
        assert!(matches!(text.parse::<Role>(), Err(Error::Malformed(_))));
        assert!(matches!(
            text.parse::<Visibility>(),
            Err(Error::Malformed(_))
        ));
    }
}
