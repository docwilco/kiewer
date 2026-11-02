//! Slugs, descriptions and the settings save through the library (#22):
//! a slug changed leaves a redirect, a new design at the old slug ends
//! it, a taken slug is refused, and the description's HTML is kept in the
//! row and rendered again at every edit. Gated on `DATABASE_URL` like
//! `tests/db.rs`; rows persist between runs on a dev box, so every test
//! picks its own handle.

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::design::{self, Design, Slug};
use kiewer_lib::design_settings::{self, Settings};
use kiewer_lib::error::Refusal;
use kiewer_lib::roles::Role;
use kiewer_lib::{Error, description, slug};
use kiewer_test_support::db::{fresh, pool_from_env};
use sqlx::PgPool;

/// A person with a handle: the owner of what the test writes.
async fn person(db: &PgPool) -> (AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "owner".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("o").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    (signed_in.account_id, handle)
}

/// A new design of `owner` at `slug`, committed.
async fn new_design(db: &PgPool, owner: AccountId, slug: &str, name: &str) -> Design {
    let mut tx = db.begin().await.unwrap();
    let design = design::create(&mut tx, owner, &slug.parse().unwrap(), name)
        .await
        .expect("a new design");
    tx.commit().await.unwrap();
    design
}

fn settings(design: &Design, slug: &str, description: &str) -> Settings {
    Settings {
        name: design.name.clone(),
        slug: slug.parse().unwrap(),
        description: description.to_owned(),
    }
}

#[tokio::test]
async fn the_description_is_rendered_into_the_row_and_again_on_edit() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, _) = person(&db).await;
    let design = new_design(&db, owner, "described", "Described").await;
    assert_eq!(
        description::of(&db, design.id).await.unwrap(),
        description::Description::default(),
        "a new design has an empty description"
    );

    // The script line is an HTML block of its own (CommonMark ends one at
    // the closing tag's line), so the links get a paragraph of their own.
    let first = "# Hello\n\n<script>x</script>\n\n[go](javascript:1) [ok](https://a.b)";
    design_settings::save(&db, owner, &design, &settings(&design, "described", first))
        .await
        .expect("saved");
    let stored = description::of(&db, design.id).await.unwrap();
    assert_eq!(stored.markdown, first, "the Markdown comes back as written");
    assert_eq!(stored.html, description::render(first));
    assert!(
        stored.html.contains("<h1>Hello</h1>")
            && stored
                .html
                .contains(r#"<a href="https://a.b" rel="nofollow noopener">ok</a>"#)
            && !stored.html.contains("script")
            && !stored.html.contains("javascript"),
        "{}",
        stored.html
    );

    let second = "plain now";
    design_settings::save(&db, owner, &design, &settings(&design, "described", second))
        .await
        .expect("saved again");
    let stored = description::of(&db, design.id).await.unwrap();
    assert_eq!(stored.markdown, second);
    assert_eq!(
        stored.html, "<p>plain now</p>\n",
        "rendered again at the edit"
    );
}

#[tokio::test]
async fn a_changed_slug_redirects_until_a_new_design_reuses_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "first", "First").await;
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "first").await.unwrap(),
        None
    );

    let moved = design_settings::save(&db, owner, &design, &settings(&design, "second", ""))
        .await
        .expect("moved");
    assert_eq!(moved.as_str(), "second");
    let listed = design::by_owner(&db, owner).await.unwrap();
    assert_eq!(
        listed[0].slug.as_str(),
        "second",
        "the design is at its new slug"
    );
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "first")
            .await
            .unwrap()
            .as_ref()
            .map(Slug::as_str),
        Some("second"),
        "the old slug redirects"
    );
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "second")
            .await
            .unwrap(),
        None,
        "the slug a design is at redirects nowhere"
    );

    // Moved again: both old slugs reach the design where it is now.
    let design = Design {
        slug: moved,
        ..design
    };
    design_settings::save(&db, owner, &design, &settings(&design, "third", ""))
        .await
        .expect("moved again");
    for old in ["first", "second"] {
        assert_eq!(
            slug::redirect(&db, handle.as_str(), old)
                .await
                .unwrap()
                .as_ref()
                .map(Slug::as_str),
            Some("third"),
            "{old}"
        );
    }

    // A new design at the first slug ends that redirect and no other.
    let reused = new_design(&db, owner, "first", "First again").await;
    assert_ne!(reused.id, design.id);
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "first").await.unwrap(),
        None
    );
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "second")
            .await
            .unwrap()
            .as_ref()
            .map(Slug::as_str),
        Some("third")
    );

    // Moving the design back onto a slug it left ends that redirect too.
    let design = Design {
        slug: "third".parse().unwrap(),
        ..design
    };
    design_settings::save(&db, owner, &design, &settings(&design, "second", ""))
        .await
        .expect("moved back");
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "second")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "third")
            .await
            .unwrap()
            .as_ref()
            .map(Slug::as_str),
        Some("second")
    );
}

#[tokio::test]
async fn a_slug_another_design_of_the_owner_has_is_refused_and_nothing_moves() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let (other, _) = person(&db).await;
    let design = new_design(&db, owner, "mine", "Mine").await;
    new_design(&db, owner, "taken", "Taken").await;
    new_design(&db, other, "theirs", "Theirs").await;

    let forbidden = design_settings::save(
        &db,
        other,
        &design,
        &settings(&design, "mine", "not theirs to write"),
    )
    .await
    .expect_err("another person has no role on the design");
    assert!(
        matches!(
            forbidden,
            Error::Forbidden {
                needed: Role::Admin
            }
        ),
        "{forbidden}"
    );
    assert_eq!(
        description::of(&db, design.id).await.unwrap().markdown,
        "",
        "the refused editor wrote nothing"
    );

    let refused = design_settings::save(
        &db,
        owner,
        &design,
        &settings(&design, "taken", "described anyway"),
    )
    .await
    .expect_err("the owner has a design there");
    assert!(
        matches!(refused, Error::Refused(Refusal::SlugTaken { ref slug }) if slug == "taken"),
        "{refused}"
    );
    assert_eq!(
        description::of(&db, design.id).await.unwrap().markdown,
        "",
        "the refused save wrote nothing"
    );
    assert_eq!(
        slug::redirect(&db, handle.as_str(), "mine").await.unwrap(),
        None
    );

    design_settings::save(&db, owner, &design, &settings(&design, "theirs", ""))
        .await
        .expect("another owner's slug is free for this owner");

    let stale = design_settings::save(&db, owner, &design, &settings(&design, "elsewhere", ""))
        .await
        .expect_err("the design is no longer at the slug the form was built from");
    assert!(
        matches!(stale, Error::Refused(Refusal::NoSuchDesign { ref slug }) if slug == "mine"),
        "{stale}"
    );
}

#[tokio::test]
async fn the_name_is_saved_trimmed_and_a_malformed_one_is_refused() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, _) = person(&db).await;
    let design = new_design(&db, owner, "named", "Named").await;
    let renamed = Settings {
        name: "  Renamed board  ".to_owned(),
        ..settings(&design, "named", "")
    };
    design_settings::save(&db, owner, &design, &renamed)
        .await
        .expect("saved");
    let listed = design::by_owner(&db, owner).await.unwrap();
    assert_eq!(listed[0].name, "Renamed board");

    let blank = Settings {
        name: "   ".to_owned(),
        ..settings(&design, "named", "")
    };
    let refused = design_settings::save(&db, owner, &design, &blank)
        .await
        .expect_err("a blank name");
    assert!(matches!(refused, Error::Malformed(_)), "{refused}");
    assert_eq!(
        design::by_owner(&db, owner).await.unwrap()[0].name,
        "Renamed board",
        "the refused save wrote nothing"
    );
}
