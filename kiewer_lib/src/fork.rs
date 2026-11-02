//! A fork: a signed-in person's own design made from another design's
//! branches (docs/v1-scope.md, Storage and history, Q33 and Q55;
//! GLOSSARY, Fork). It takes the source's default branch, or every
//! branch, at their heads; the fork writes design, branch and
//! `design_revision` rows only, pointing at the revision rows the source
//! has (ADR 0002: a fork copies nothing), and records the source design
//! and the revision it started at. Anyone signed in forks any design
//! they can open, which is every design with a link (visibility is
//! about listings); their own too, under the next free slug.

use std::fmt;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::account::{AccountId, Handle};
use crate::design::{self, Design, Slug};
use crate::error::{Error, Refusal, Result};

/// Which branches a fork takes. The wire name (`default`, `all`) is the
/// fork form's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Take {
    /// The source's default branch alone: what the form preselects.
    Default,
    /// Every branch of the source.
    All,
}

impl Take {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Take::Default => "default",
            Take::All => "all",
        }
    }
}

impl fmt::Display for Take {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The slug a fork of a design at `base` goes under: `base` itself when
/// the forker has no design there, else the first of `base-2`, `base-3`,
/// ... not among `taken` (the forker's slugs), cut so the suffix fits.
#[must_use]
pub fn next_free_slug(base: &Slug, taken: &[Slug]) -> Slug {
    if !taken.contains(base) {
        return base.clone();
    }
    // Ends: each number gives another slug, and `taken` is finite.
    let mut number = 2;
    loop {
        let candidate = numbered(base, number);
        if !taken.contains(&candidate) {
            return candidate;
        }
        number += 1;
    }
}

/// `base-n`, with the base cut (and a hyphen the cut leaves trimmed) so
/// the whole fits [`Slug::MAX_LEN`].
fn numbered(base: &Slug, n: u32) -> Slug {
    let suffix = format!("-{n}");
    let room = Slug::MAX_LEN - suffix.len();
    let stem = base.as_str();
    let stem = if stem.len() > room {
        stem[..room].trim_end_matches('-')
    } else {
        stem
    };
    format!("{stem}{suffix}")
        .parse()
        .expect("a slug cut at a byte of its own and a hyphen plus digits is a slug")
}

/// The design at `/{source_owner}/{source_slug}` forked into `forker`'s
/// account, taking `take`'s branches, in one transaction: the source's
/// row and branches are read and held, the design row is made at the
/// next free slug with the source's name, default branch and visibility
/// and the fork source recorded, the branches copied at their heads,
/// and the fork's index of its revisions built by walking their chains.
/// No revision, tree or blob row is written.
///
/// # Errors
///
/// [`Error::Refused`] with [`Refusal::NoSuchDesign`] when nothing is at
/// the path, with [`Refusal::NoSuchBranch`] when the source's default
/// branch has no head (a design with no revision yet has no page to
/// fork from either), with [`Refusal::SlugTaken`] when another fork took
/// the slug in the same instant; [`Error::Db`] when Postgres refuses.
pub async fn fork(
    db: &PgPool,
    forker: AccountId,
    source_owner: &Handle,
    source_slug: &Slug,
    take: Take,
) -> Result<Design> {
    let mut tx = db.begin().await?;
    // Reads first, against the usual write-first rule, because they are
    // locking ones: FOR SHARE on the source's row and its branch rows
    // makes a change to the design or a push that would move a head wait
    // for this commit, so what is inherited and copied is what was read.
    let source = design::lock_at(&mut tx, source_owner, source_slug)
        .await?
        .ok_or_else(|| {
            Error::Refused(Refusal::NoSuchDesign {
                slug: format!("{source_owner}/{source_slug}"),
            })
        })?;
    let branches = design::lock_branches(&mut tx, source.id).await?;
    let Some(head) = branches
        .iter()
        .find(|branch| branch.name.as_str() == source.default_branch)
        .map(|branch| branch.head)
    else {
        return Err(Error::Refused(Refusal::NoSuchBranch {
            branch: source.default_branch.clone(),
        }));
    };
    let taken = design::slugs_of(&mut tx, forker).await?;
    let slug = next_free_slug(&source.slug, &taken);
    let fork = design::create_fork(&mut tx, forker, &slug, &source, source_owner, head).await?;
    let names: Vec<&str> = match take {
        Take::Default => vec![source.default_branch.as_str()],
        Take::All => branches.iter().map(|branch| branch.name.as_str()).collect(),
    };
    let copied = design::copy_branches(&mut tx, source.id, fork.id, &names).await?;
    let revisions = design::rebuild_design_revisions(&mut tx, fork.id).await?;
    tx.commit().await?;
    tracing::info!(
        design = %fork.slug,
        source = %format_args!("{source_owner}/{source_slug}"),
        %take,
        branches = copied,
        revisions,
        "design forked"
    );
    Ok(fork)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn slug(text: &str) -> Slug {
        text.parse().expect("a slug")
    }

    #[test_case(Take::Default, "default")]
    #[test_case(Take::All, "all")]
    fn a_take_round_trips_through_its_wire_name(take: Take, wire: &str) {
        assert_eq!(take.as_str(), wire);
        assert_eq!(serde_json::to_string(&take).unwrap(), format!("{wire:?}"));
        assert_eq!(
            serde_json::from_str::<Take>(&format!("{wire:?}")).unwrap(),
            take
        );
    }

    #[test_case(&[], "board"; "nothing taken")]
    #[test_case(&["board"], "board-2"; "the base taken")]
    #[test_case(&["board", "board-2"], "board-3"; "the base and the second taken")]
    #[test_case(&["board", "board-3"], "board-2"; "a gap is filled")]
    #[test_case(&["board-2"], "board"; "the base free though a numbered one is not")]
    fn the_next_free_slug_numbers_from_two(taken: &[&str], expected: &str) {
        let taken: Vec<Slug> = taken.iter().map(|text| slug(text)).collect();
        assert_eq!(next_free_slug(&slug("board"), &taken), slug(expected));
    }

    #[test]
    fn a_long_slug_numbers_on_past_its_own_cut() {
        let long = slug(&"a".repeat(Slug::MAX_LEN));
        let second = next_free_slug(&long, std::slice::from_ref(&long));
        let third = next_free_slug(&long, &[long.clone(), second.clone()]);
        assert!(third.as_str().ends_with("-3"), "{third}");
    }

    #[test]
    fn a_long_slug_is_cut_so_the_number_fits() {
        let long = slug(&"a".repeat(Slug::MAX_LEN));
        let next = next_free_slug(&long, std::slice::from_ref(&long));
        assert_eq!(next.as_str().len(), Slug::MAX_LEN);
        assert!(next.as_str().ends_with("-2"), "{next}");
    }

    #[test]
    fn a_hyphen_the_cut_leaves_is_trimmed() {
        let hyphen_at_cut = slug(&format!("{}-x", "b".repeat(Slug::MAX_LEN - 3)));
        let next = next_free_slug(&hyphen_at_cut, std::slice::from_ref(&hyphen_at_cut));
        assert!(!next.as_str().contains("--"), "{next}");
    }
}
