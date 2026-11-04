//! The design settings form over HTTP (#22): the owner saves a
//! description that the page shows rendered, renames the design and
//! moves its slug, the old slug answers 301 until a new design takes it,
//! a taken slug is 409 with the page and the reason, and anyone else is
//! turned away. The Postgres tests skip without `DATABASE_URL`, like
//! `tests/it/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;

const SCHEMATIC: &[u8] = b"(kicad_sch (version 20250114))";

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory and no sign-in provider.
async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    (http::serve_on_free_port(state).await, dir)
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
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
    let id = session::start(db, signed_in.account_id)
        .await
        .expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, signed_in.account_id, handle)
}

/// A design of the browser's person named `name`, with one revision, at
/// the slug the name suggests.
async fn upload(browser: &mut http::Browser, name: &str) -> String {
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", name),
                Part::file("sheet.kicad_sch", SCHEMATIC),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    posted
        .header("location")
        .expect("the design's page")
        .to_owned()
}

/// The settings form's body.
fn form(name: &str, slug: &str, description: &str) -> String {
    let encode = |text: &str| {
        text.bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    char::from(b).to_string()
                }
                b' ' => "+".to_owned(),
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    format!(
        "name={}&slug={}&description={}",
        encode(name),
        encode(slug),
        encode(description)
    )
}

#[tokio::test]
async fn the_owner_saves_a_description_that_the_page_shows_rendered() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let design_path = upload(&mut browser, "Demo board").await;
    assert_eq!(design_path, format!("/{handle}/demo-board"));

    let page = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains(r#"<details id="settings" hidden>"#),
        "the block is in the page for everyone, hidden: {}",
        page.body
    );
    assert!(
        page.body
            .contains(&format!(r#"action="{design_path}/settings""#)),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains(r#"<input name="name" value="Demo board""#),
        "{}",
        page.body
    );
    assert!(!page.body.contains(r#"id="description""#), "{}", page.body);

    let saved = browser
        .post_form(
            &format!("{design_path}/settings"),
            &form(
                "Demo board",
                "demo-board",
                "## About\n\n<b>raw</b> [bad](javascript:1) [good](https://example.com) ![pic](https://x/y.png)",
            ),
        )
        .await;
    assert_eq!(saved.status, 303, "{}", saved.body);
    assert_eq!(saved.header("location"), Some(design_path.as_str()));
    assert_eq!(saved.header("cache-control"), Some("private, no-store"));

    let page = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    // The rendered description alone: the form's textarea below it
    // echoes the Markdown back, javascript link and all, escaped.
    let (_, rest) = page
        .body
        .split_once(r#"<div id="description">"#)
        .expect("the description renders");
    let (rendered, _) = rest.split_once("</div>").expect("the div closes");
    assert!(
        rendered.starts_with("<h2>About</h2>"),
        "the description renders as HTML: {rendered}"
    );
    assert!(
        rendered.contains(r#"<a href="https://example.com" rel="nofollow noopener">good</a>"#),
        "{rendered}"
    );
    assert!(
        !rendered.contains("<b>") && !rendered.contains("javascript") && !rendered.contains("<img"),
        "raw HTML, the javascript link and the image are gone: {rendered}"
    );
    assert!(
        rendered.contains("bad") && rendered.contains("pic"),
        "the link's text and the alt text stay: {rendered}"
    );
    assert!(
        page.body.contains("## About\n\n&#60;b&#62;raw&#60;/b&#62;"),
        "the form shows the Markdown back, escaped: {}",
        page.body
    );
    let html: String = sqlx::query_scalar(
        "SELECT d.description_html FROM design d JOIN account a ON a.id = d.owner_id
         WHERE a.handle = $1 AND d.slug = 'demo-board'",
    )
    .bind(handle.as_str())
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(
        html.starts_with("<h2>About</h2>"),
        "cached in the row: {html}"
    );
}

#[tokio::test]
async fn a_stranger_and_another_person_cannot_save() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, _, _) = signed_in(&db, addr).await;
    let design_path = upload(&mut owner, "Guarded").await;
    let settings_path = format!("{design_path}/settings");
    let body = form("Taken over", "guarded", "mine now");

    let stranger = http::request(addr, "POST", &settings_path, &[], &body).await;
    assert_eq!(stranger.status, 303, "sent home: {}", stranger.body);
    assert_eq!(stranger.header("location"), Some("/"));

    let (mut other, _, _) = signed_in(&db, addr).await;
    let refused = other.post_form(&settings_path, &body).await;
    assert_eq!(refused.status, 403, "{}", refused.body);

    let page = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    assert!(
        page.body.contains("<title>Guarded - kiewer</title>") && !page.body.contains("mine now"),
        "nothing changed: {}",
        page.body
    );
    let nowhere = other
        .post_form(&format!("/{}/none/settings", fresh("h")), &body)
        .await;
    assert_eq!(nowhere.status, 404, "{}", nowhere.body);
}

#[tokio::test]
async fn a_moved_slug_redirects_until_a_new_design_takes_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let old_path = upload(&mut browser, "Mover").await;
    assert_eq!(old_path, format!("/{handle}/mover"));
    let new_path = format!("/{handle}/moved");

    let saved = browser
        .post_form(
            &format!("{old_path}/settings"),
            &form("Mover, moved", "moved", ""),
        )
        .await;
    assert_eq!(saved.status, 303, "{}", saved.body);
    assert_eq!(
        saved.header("location"),
        Some(new_path.as_str()),
        "the browser goes to the new slug"
    );
    let page = http::request(addr, "GET", &format!("{new_path}?design"), &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("<title>Mover, moved - kiewer</title>"),
        "renamed: {}",
        page.body
    );
    let redirected = http::request(addr, "GET", &old_path, &[], "").await;
    assert_eq!(redirected.status, 301, "{}", redirected.body);
    assert_eq!(redirected.header("location"), Some(new_path.as_str()));

    // A new design named so that its slug is the old one takes it over.
    let reused = upload(&mut browser, "Mover").await;
    assert_eq!(reused, old_path, "the old slug is free for a new design");
    let page = http::request(addr, "GET", &format!("{old_path}?design"), &[], "").await;
    assert_eq!(
        page.status, 200,
        "the new design, no redirect: {}",
        page.body
    );
    assert!(
        page.body.contains("<title>Mover - kiewer</title>"),
        "{}",
        page.body
    );

    // The moved design cannot move onto a slug a design of the owner's has.
    let taken = browser
        .post_form(
            &format!("{new_path}/settings"),
            &form("Mover, moved", "mover", ""),
        )
        .await;
    assert_eq!(taken.status, 409, "{}", taken.body);
    assert_eq!(taken.header("cache-control"), Some("private, no-store"));
    assert!(
        taken.body.contains(r#"<details id="settings" open>"#)
            && taken.body.contains("already have a design at mover"),
        "the page again with the block open on the reason: {}",
        taken.body
    );
    assert!(
        taken.body.contains(r#"<input name="slug" value="mover""#),
        "the attempt stays in the form: {}",
        taken.body
    );

    let malformed = browser
        .post_form(&format!("{new_path}/settings"), &form("", "Bad Slug", ""))
        .await;
    assert_eq!(malformed.status, 422, "{}", malformed.body);
    assert!(
        malformed
            .body
            .contains("lowercase letters, digits and hyphens"),
        "{}",
        malformed.body
    );
}
