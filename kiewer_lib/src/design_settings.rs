//! The settings a design's owner or admin edits on its page: the name,
//! the slug and the description (docs/v1-scope.md, Accounts and access,
//! Q29 and Q34; Pages and URLs, Q48). One form, one outcome, one
//! transaction: the description renders and lands with the name, and the
//! slug moves with its redirect (`slug::change`), or nothing changes.

use sqlx::PgPool;

use crate::account::AccountId;
use crate::design::{Design, Slug};
use crate::error::{Error, Result};
use crate::roles::{self, Role};
use crate::{description, slug};

/// The longest name, in characters.
pub const MAX_NAME_LEN: usize = 100;

/// What the form says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The name, as the person typed it; trimmed and checked by
    /// [`Settings::name`].
    pub name: String,
    pub slug: Slug,
    /// The description's Markdown, at most [`description::MAX_LEN`]
    /// bytes.
    pub description: String,
}

impl Settings {
    /// The settings a design has now: what its form starts with.
    #[must_use]
    pub fn of(design: &Design, description: &description::Description) -> Self {
        Self {
            name: design.name.clone(),
            slug: design.slug.clone(),
            description: description.markdown.clone(),
        }
    }

    /// The name as the row stores it: trimmed, not empty, at most
    /// [`MAX_NAME_LEN`] characters.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] with the words the form shows.
    pub fn name(&self) -> Result<&str> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(Error::Malformed("the name is empty".to_owned()));
        }
        if name.chars().count() > MAX_NAME_LEN {
            return Err(Error::Malformed(format!(
                "the name is longer than {MAX_NAME_LEN} characters"
            )));
        }
        Ok(name)
    }

    /// Why the settings cannot be saved as they are, or `Ok` when they
    /// can: the name's rules and the description's length.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] with the words the form shows.
    pub fn check(&self) -> Result<()> {
        self.name()?;
        if self.description.len() > description::MAX_LEN {
            return Err(Error::Malformed(format!(
                "the description is longer than {} KB",
                description::MAX_LEN / 1024
            )));
        }
        Ok(())
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
    settings.check()?;
    let name = settings.name()?;
    let mut tx = db.begin().await?;
    // The role is read inside the transaction, as the gate of the writes
    // that follow it, so a role revoked in the same instant refuses the
    // whole save rather than half of it.
    roles::require(&mut *tx, design.id, editor, Role::Admin).await?;
    description::set(&mut tx, design.id, &settings.description).await?;
    sqlx::query("UPDATE design SET name = $2 WHERE id = $1")
        .bind(design.id)
        .bind(name)
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
        assert_eq!(settings(typed, "").name().ok(), name);
    }

    #[test]
    fn the_name_and_the_description_have_a_longest() {
        let long = "ä".repeat(MAX_NAME_LEN);
        assert_eq!(
            settings(&long, "").name().unwrap(),
            long,
            "characters, not bytes"
        );
        let over = "a".repeat(MAX_NAME_LEN + 1);
        let err = settings(&over, "").check().expect_err("too long");
        assert!(err.to_string().contains("100 characters"), "{err}");
        let err = settings("ok", &"m".repeat(description::MAX_LEN + 1))
            .check()
            .expect_err("too long");
        assert!(err.to_string().contains("64 KB"), "{err}");
        settings("ok", &"m".repeat(description::MAX_LEN))
            .check()
            .expect("at the cap");
    }
}
