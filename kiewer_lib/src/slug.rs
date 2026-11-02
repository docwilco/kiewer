//! A design's slug: its name in its URL (GLOSSARY, People, Slug). The
//! rules, the slug a name suggests at creation, the change of a slug by
//! its owner or an admin, and the redirect the old slug leaves until a design
//! reuses it (docs/v1-scope.md, Accounts and access, Q29). A redirect row
//! names the design, not the new slug, so a slug changed twice still
//! reaches the design from either old one.

use std::fmt;
use std::str::FromStr;

use sqlx::{PgConnection, PgPool};

use crate::account::{AccountId, slugify};
use crate::design::DesignId;
use crate::error::{Error, Refusal, Result};

/// A design's name in its URL: lowercase letters, digits and hyphens, 1
/// to 64 characters, no hyphen at either end, unique per owner. Picked
/// from the upload's name at creation ([`Slug::suggest`]) and changed by
/// the owner or an admin on the design page ([`change`]).
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
        slugify(name, Self::MAX_LEN)
            .parse()
            .unwrap_or_else(|_| Self(Self::FALLBACK.to_owned()))
    }

    /// Why `text` is not a slug, in the words the form shows, or `None`
    /// when it is one.
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

/// The slug a design of `handle` is at now, when `slug` is one it had
/// before: what `/{handle}/{slug}` redirects to. `None` when no design of
/// theirs was ever at `slug`, or one is there again.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub async fn redirect(db: &PgPool, handle: &str, slug: &str) -> Result<Option<Slug>> {
    // One read on the pool: a page's lookup after its own found nothing,
    // part of no write.
    let current: Option<String> = sqlx::query_scalar(
        "SELECT d.slug FROM slug_redirect sr
         JOIN account a ON a.id = sr.owner_id
         JOIN design d ON d.id = sr.design_id
         WHERE a.handle = $1 AND sr.slug = $2",
    )
    .bind(handle)
    .bind(slug)
    .fetch_optional(db)
    .await?;
    Ok(current.map(Slug::from_row))
}

/// `slug` is about to be a design of `owner`'s again: the redirect it
/// left, if any, goes. Called within the transaction that creates the
/// design or moves one there, before the write that takes the slug.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses.
pub(crate) async fn release(tx: &mut PgConnection, owner: AccountId, slug: &Slug) -> Result<()> {
    sqlx::query("DELETE FROM slug_redirect WHERE owner_id = $1 AND slug = $2")
        .bind(owner)
        .bind(slug.as_str())
        .execute(tx)
        .await?;
    Ok(())
}

/// Move `owner`'s design `design` from `old` to `new`, within the
/// transaction that saves the design's settings: the redirect at `new`
/// (if a design of theirs was there once) goes, the design moves, and
/// `old` becomes a redirect to it. The move is by compare-and-set on
/// `old`, the slug the form was built from.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::SlugTaken`] when a design of the
/// owner's is at `new`, with [`Refusal::SlugMoved`] when the design is
/// no longer at `old`; [`Error::Db`] when Postgres refuses otherwise.
pub async fn change(
    tx: &mut PgConnection,
    owner: AccountId,
    design: DesignId,
    old: &Slug,
    new: &Slug,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    release(tx, owner, new).await?;
    let moved = sqlx::query("UPDATE design SET slug = $3 WHERE id = $1 AND slug = $2")
        .bind(design)
        .bind(old.as_str())
        .bind(new.as_str())
        .execute(&mut *tx)
        .await;
    match moved {
        Ok(done) if done.rows_affected() == 1 => {}
        Ok(_) => {
            return Err(Error::Refused(Refusal::SlugMoved {
                slug: old.to_string(),
            }));
        }
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(Error::Refused(Refusal::SlugTaken {
                slug: new.to_string(),
            }));
        }
        Err(error) => return Err(error.into()),
    }
    // The old slug cannot be a redirect already (a slug is a design's or
    // a redirect, never both, since every take of one releases it), so
    // the conflict arm is for a row the invariant should never leave.
    sqlx::query(
        "INSERT INTO slug_redirect (owner_id, slug, design_id) VALUES ($1, $2, $3)
         ON CONFLICT (owner_id, slug) DO UPDATE SET design_id = EXCLUDED.design_id",
    )
    .bind(owner)
    .bind(old.as_str())
    .bind(design)
    .execute(tx)
    .await?;
    Ok(())
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
    #[test_case("a.b"; "a dot")]
    fn a_malformed_slug_is_refused_with_a_message(text: &str) {
        let err = text.parse::<Slug>().expect_err("refused");
        assert!(matches!(err, Error::Malformed(_)), "{err}");
        assert!(err.to_string().contains(&format!("{text:?}")), "{err}");
    }
}
