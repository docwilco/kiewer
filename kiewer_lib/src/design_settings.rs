//! The settings a design's owner or admin edits on its page: the name,
//! the slug and the description (docs/v1-scope.md, Accounts and access,
//! Q29 and Q34; Pages and URLs, Q48). One form, one outcome, one
//! transaction: the description renders and lands with the name, and the
//! slug moves with its redirect (`slug::change`), or nothing changes.

use sqlx::PgPool;

use crate::account::AccountId;
use crate::design::{Design, DesignId, Slug};
use crate::error::{Error, Result};
use crate::roles::{self, Role};
use crate::{description, slug};

/// The longest name, in characters.
pub const MAX_NAME_LEN: usize = 100;

/// What the form says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The name, as the person typed it; trimmed and checked by
    /// [`Settings::check`].
    pub name: String,
    /// The slug the design is to be at: its own, or a new one, which
    /// moves it.
    pub slug: Slug,
    /// The description's Markdown, at most [`description::MAX_LEN`]
    /// bytes.
    pub description: String,
}

impl Settings {
    /// The name as the row stores it (trimmed, not empty, at most
    /// [`MAX_NAME_LEN`] characters), once the description is within its
    /// length: the one check a save makes before it writes.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] with the words the form shows.
    pub fn check(&self) -> Result<&str> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(Error::Malformed("the name is empty".to_owned()));
        }
        if name.chars().count() > MAX_NAME_LEN {
            return Err(Error::Malformed(format!(
                "the name is longer than {MAX_NAME_LEN} characters"
            )));
        }
        if self.description.len() > description::MAX_LEN {
            return Err(Error::Malformed(format!(
                "the description is longer than {} KB",
                description::MAX_LEN / 1024
            )));
        }
        Ok(name)
    }
}

/// `settings` become `design`'s, by `editor`, an admin or the owner
/// (docs/v1-scope.md, Accounts and access: both edit name, slug and
/// description): the name and the description as given, the slug moved
/// when it differs, with the redirect under the design's owner whoever
/// edits. What comes back is the design's slug after the save, for the
/// page to go to.
///
/// # Errors
///
/// [`Error::Malformed`] when [`Settings::check`] refuses;
/// [`Error::Forbidden`] when `editor` is below admin on the design;
/// [`Error::Refused`] when the slug is taken or the design is no longer
/// at its slug ([`slug::change`]); [`Error::Db`] when Postgres refuses.
pub async fn save(
    db: &PgPool,
    editor: AccountId,
    design: &Design,
    settings: &Settings,
) -> Result<Slug> {
    let name = settings.check()?;
    let mut tx = db.begin().await?;
    // The role is read inside the transaction, as the gate of the writes
    // that follow it, so a role revoked in the same instant refuses the
    // whole save rather than half of it. A read first, against the usual
    // write-first rule: the roles seam is the one place roles are read,
    // so the gate cannot fold into the first UPDATE, and Postgres takes no
    // lock on a read that a later write would have to upgrade.
    roles::require(&mut *tx, design.id, editor, Role::Admin).await?;
    description::set(&mut tx, design.id, &settings.description).await?;
    sqlx::query!(
        "UPDATE design SET name = $2 WHERE id = $1",
        design.id as DesignId,
        name
    )
    .execute(&mut *tx)
    .await?;
    slug::change(
        &mut tx,
        design.owner_id,
        design.id,
        &design.slug,
        &settings.slug,
    )
    .await?;
    tx.commit().await?;
    tracing::info!(
        design = %settings.slug,
        moved = design.slug != settings.slug,
        "design settings saved"
    );
    Ok(settings.slug.clone())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn settings(name: &str, description: &str) -> Settings {
        Settings {
            name: name.to_owned(),
            slug: "x".parse().unwrap(),
            description: description.to_owned(),
        }
    }

    #[test_case("  Demo board ", Some("Demo board"); "trimmed")]
    #[test_case("", None; "empty")]
    #[test_case("   ", None; "blank")]
    fn the_name_is_trimmed_and_not_empty(typed: &str, name: Option<&str>) {
        assert_eq!(settings(typed, "").check().ok(), name);
    }

    #[test_case("ä".repeat(MAX_NAME_LEN), String::new(), Ok(()); "a name at the cap, in characters not bytes")]
    #[test_case("a".repeat(MAX_NAME_LEN + 1), String::new(), Err("100 characters"); "a name one over")]
    #[test_case("ok".to_owned(), "m".repeat(description::MAX_LEN), Ok(()); "a description at the cap")]
    #[test_case("ok".to_owned(), "m".repeat(description::MAX_LEN + 1), Err("64 KB"); "a description one over")]
    fn the_name_and_the_description_have_a_longest(
        name: String,
        description: String,
        outcome: std::result::Result<(), &str>,
    ) {
        let settings = Settings {
            name,
            slug: "x".parse().unwrap(),
            description,
        };
        let checked = settings.check();
        match outcome {
            Ok(()) => assert_eq!(checked.expect("within the caps"), settings.name.trim()),
            Err(words) => {
                let err = checked.expect_err("over a cap");
                assert!(err.to_string().contains(words), "{err}");
            }
        }
    }
}
