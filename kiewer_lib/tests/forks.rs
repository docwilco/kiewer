//! Forks over HTTP (#20): a signed-in person forks a design into their
//! own account, taking its default branch or all its branches; the fork
//! points at the existing revision rows and writes no revision, tree or
//! blob row; its history shows the original authors; the forked-from
//! line links the source, or says it is gone; a revision reachable only
//! from a branch the fork did not take is 404 under the fork's URLs. The
//! Postgres tests skip without `DATABASE_URL`, like `tests/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::design;
use kiewer_lib::roles::{self, Visibility};
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;

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
            login: "forker".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("f").parse().expect("a handle");
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

/// A board whose text is its own, so each upload is a new blob. A board
/// and not a schematic: a revision with a board is what opens on the
/// design page, which carries the forked-from line and the fork form.
fn board() -> Vec<u8> {
    format!(
        "(kicad_pcb (version 20241229) (generator \"{}\") (layers (0 \"F.Cu\" signal)))",
        fresh("board")
    )
    .into_bytes()
}

/// One board pushed through the upload form with these text fields.
async fn push(browser: &mut http::Browser, fields: &[(&str, &str)]) -> http::Response {
    let bytes = board();
    let mut parts: Vec<Part<'_>> = fields
        .iter()
        .map(|(field, value)| Part::text(field, value))
        .collect();
    parts.push(Part::file("board.kicad_pcb", &bytes));
    browser.post_multipart("/upload", &parts).await
}

/// The revision hashes a page links as `/{handle}/{slug}/r/{hash}`, in
/// page order, each once.
fn linked_revisions(body: &str, handle: &Handle, slug: &str) -> Vec<String> {
    let prefix = format!("href=\"/{handle}/{slug}/r/");
    let mut hashes: Vec<String> = Vec::new();
    for (start, _) in body.match_indices(&prefix) {
        let hash = &body[start + prefix.len()..start + prefix.len() + 64];
        if !hashes.iter().any(|known| known == hash) {
            hashes.push(hash.to_owned());
        }
    }
    hashes
}

/// The history page of `branch` of `/{handle}/{slug}`.
async fn history_page(
    addr: SocketAddr,
    handle: &Handle,
    slug: &str,
    branch: &str,
) -> http::Response {
    http::request(
        addr,
        "GET",
        &format!("/{handle}/{slug}/b/{branch}/history"),
        &[],
        "",
    )
    .await
}

/// A design of the browser's person at `slug` with two branches: `main`
/// with rev 1 and rev 2, and `side` starting at rev 1 with one revision
/// of its own on top. The hashes of main's chain (newest first) and of
/// the revision only `side` reaches.
async fn two_branches(
    browser: &mut http::Browser,
    handle: &Handle,
    slug: &str,
) -> (Vec<String>, String) {
    let path = format!("{handle}/{slug}");
    for (design, message) in [("", "rev 1"), (path.as_str(), "rev 2")] {
        let posted = push(
            browser,
            &[
                ("design", design),
                ("name", slug),
                ("branch", "main"),
                ("message", message),
            ],
        )
        .await;
        assert_eq!(posted.status, 303, "{message}: {}", posted.body);
    }
    let main = history_page(browser.addr(), handle, slug, "main").await;
    let on_main = linked_revisions(&main.body, handle, slug);
    assert_eq!(on_main.len(), 2);
    let first = on_main[1].clone();
    let posted = push(
        browser,
        &[
            ("design", &path),
            ("branch", ""),
            ("new_branch", "side"),
            ("from", &first),
            ("message", "on the side"),
        ],
    )
    .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let side = history_page(browser.addr(), handle, slug, "side").await;
    let on_side = linked_revisions(&side.body, handle, slug);
    assert_eq!(on_side.len(), 2, "{}", side.body);
    (on_main, on_side[0].clone())
}

/// A person with a handle and a design that has no revision yet: the
/// handle and the slug.
async fn person_with_empty_design(db: &PgPool) -> (Handle, String) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "empty".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("e").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    let mut tx = db.begin().await.unwrap();
    design::create(
        &mut tx,
        signed_in.account_id,
        &"empty".parse().unwrap(),
        "Empty",
    )
    .await
    .expect("a new design");
    tx.commit().await.unwrap();
    (handle, "empty".to_owned())
}

/// `POST /{handle}/{slug}/fork` with the branch choice `take`.
async fn fork(
    browser: &mut http::Browser,
    handle: &Handle,
    slug: &str,
    take: &str,
) -> http::Response {
    browser
        .post_form(&format!("/{handle}/{slug}/fork"), &format!("take={take}"))
        .await
}

/// The id, the slug and the creation time of the design at
/// `/{handle}/{slug}`.
async fn design_row(db: &PgPool, handle: &Handle, slug: &str) -> (i64, String, String) {
    sqlx::query_as(
        "SELECT d.id, d.slug, d.created_at::text FROM design d
         JOIN account a ON a.id = d.owner_id WHERE a.handle = $1 AND d.slug = $2",
    )
    .bind(handle.as_str())
    .bind(slug)
    .fetch_one(db)
    .await
    .expect("the design's row")
}

/// The fork at `/{handle}/{slug}` wrote design, branch and
/// `design_revision` rows and nothing else: every revision it reaches
/// was in the source's index already, and no revision, tree or blob row
/// the fork reaches is as young as the fork (the creation times are
/// Postgres's, read in the same database).
async fn assert_fork_wrote_no_content_rows(
    db: &PgPool,
    source: i64,
    fork_handle: &Handle,
    fork_slug: &str,
) {
    let (fork, _, _) = design_row(db, fork_handle, fork_slug).await;
    let foreign: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM design_revision f
         WHERE f.design_id = $1
           AND NOT EXISTS (SELECT 1 FROM design_revision s
                           WHERE s.design_id = $2 AND s.revision_hash = f.revision_hash)",
    )
    .bind(fork)
    .bind(source)
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(
        foreign, 0,
        "every revision of the fork is one of the source's"
    );
    let young: (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM design_revision dr JOIN revision r ON r.hash = dr.revision_hash
            WHERE dr.design_id = $1 AND r.created_at >= d.created_at),
           (SELECT count(*) FROM design_revision dr JOIN revision r ON r.hash = dr.revision_hash
            JOIN tree t ON t.hash = r.tree_hash
            WHERE dr.design_id = $1 AND t.created_at >= d.created_at),
           (SELECT count(*) FROM design_revision dr JOIN revision r ON r.hash = dr.revision_hash
            JOIN tree_entry te ON te.tree_hash = r.tree_hash JOIN blob b ON b.hash = te.blob_hash
            WHERE dr.design_id = $1 AND b.created_at >= d.created_at)
         FROM design d WHERE d.id = $1",
    )
    .bind(fork)
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(
        young,
        (0, 0, 0),
        "no revision, tree or blob row is as young as the fork"
    );
    let heads: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branch f
         WHERE f.design_id = $1
           AND NOT EXISTS (SELECT 1 FROM branch s
                           WHERE s.design_id = $2 AND s.name = f.name AND s.head_hash = f.head_hash)",
    )
    .bind(fork)
    .bind(source)
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(
        heads, 0,
        "every branch of the fork is a source branch at its head"
    );
}

#[tokio::test]
async fn a_fork_of_the_default_branch_takes_main_alone_and_the_side_branch_is_404() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    let (on_main, only_on_side) = two_branches(&mut author, &author_handle, "chain").await;
    let (source, _, _) = design_row(&db, &author_handle, "chain").await;

    let (mut forker, _, forker_handle) = signed_in(&db, addr).await;
    let made = fork(&mut forker, &author_handle, "chain", "default").await;
    assert_eq!(made.status, 303, "{}", made.body);
    let fork_path = format!("/{forker_handle}/chain");
    assert_eq!(
        made.header("location"),
        Some(fork_path.as_str()),
        "the browser goes to the fork's page, under the forker's handle at the source's slug"
    );
    assert_eq!(made.header("cache-control"), Some("private, no-store"));
    assert_fork_wrote_no_content_rows(&db, source, &forker_handle, "chain").await;

    let page = http::request(addr, "GET", &fork_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains(&format!(r#"title="{}""#, on_main[0])),
        "the fork's main is at the source's head: {}",
        page.body
    );
    assert!(
        page.body.contains(&format!("by {author_handle} at")),
        "the revision keeps its author: {}",
        page.body
    );
    assert!(
        page.body.contains(">main</option>") && !page.body.contains(">side</option>"),
        "main alone in the branch dropdown: {}",
        page.body
    );
    // The forked-from line links the source design and the revision.
    assert!(
        page.body.contains(&format!(
            r#"Forked from <a href="/{author_handle}/chain">{author_handle}/chain</a>"#
        )),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(&format!(
            r#"<a href="/{author_handle}/chain/r/{}"><code title="{}">{}</code></a>"#,
            on_main[0],
            on_main[0],
            &on_main[0][..8]
        )),
        "the revision link: {}",
        page.body
    );
    // The fork's history is the source's chain, by its original author.
    let history = history_page(addr, &forker_handle, "chain", "main").await;
    assert_eq!(history.status, 200, "{}", history.body);
    assert_eq!(
        linked_revisions(&history.body, &forker_handle, "chain"),
        on_main,
        "{}",
        history.body
    );
    assert!(
        history.body.contains(&format!("<td>{author_handle}</td>"))
            && !history.body.contains(&format!("<td>{forker_handle}</td>")),
        "the original author in the author column, not the forker: {}",
        history.body
    );
    // The branch not taken, and the revision only it reaches, are 404.
    let (status, _) = http::get(addr, &format!("{fork_path}/b/side")).await;
    assert_eq!(status, 404, "the branch not taken");
    let (status, _) = http::get(addr, &format!("{fork_path}/b/side/history")).await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, &format!("{fork_path}/r/{only_on_side}")).await;
    assert_eq!(status, 404, "a revision only the branch not taken reaches");
    let (status, _) = http::get(addr, &format!("{fork_path}/r/{}", on_main[1])).await;
    assert_eq!(status, 200, "a revision main reaches");
    let (status, _) = http::get(addr, &format!("/{author_handle}/chain/r/{only_on_side}")).await;
    assert_eq!(status, 200, "the source still reaches it");
}

#[tokio::test]
async fn a_fork_of_all_branches_takes_both_and_reaches_the_side_revision() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    let (on_main, only_on_side) = two_branches(&mut author, &author_handle, "both").await;
    let (source, _, _) = design_row(&db, &author_handle, "both").await;

    let (mut forker, _, forker_handle) = signed_in(&db, addr).await;
    let made = fork(&mut forker, &author_handle, "both", "all").await;
    assert_eq!(made.status, 303, "{}", made.body);
    let fork_path = format!("/{forker_handle}/both");
    assert_eq!(made.header("location"), Some(fork_path.as_str()));
    assert_fork_wrote_no_content_rows(&db, source, &forker_handle, "both").await;

    let page = http::request(addr, "GET", &fork_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains(">main</option>") && page.body.contains(">side</option>"),
        "both branches: {}",
        page.body
    );
    let at_side = http::request(addr, "GET", &format!("{fork_path}/b/side"), &[], "").await;
    assert_eq!(at_side.status, 200, "{}", at_side.body);
    assert!(
        at_side.body.contains("on the side"),
        "the side branch's head: {}",
        at_side.body
    );
    let side_history = history_page(addr, &forker_handle, "both", "side").await;
    assert_eq!(
        linked_revisions(&side_history.body, &forker_handle, "both"),
        vec![only_on_side.clone(), on_main[1].clone()],
        "the side chain, rooted at rev 1: {}",
        side_history.body
    );
    let (status, _) = http::get(addr, &format!("{fork_path}/r/{only_on_side}")).await;
    assert_eq!(status, 200, "the fork reaches the side revision");
    let fork_revisions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM design_revision WHERE design_id = $1")
            .bind(design_row(&db, &forker_handle, "both").await.0)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(fork_revisions, 3, "rev 1, rev 2 and the side revision");
}

#[tokio::test]
async fn forking_ones_own_design_takes_the_next_free_slug() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, handle) = signed_in(&db, addr).await;
    two_branches(&mut author, &handle, "mine").await;
    let (source, _, _) = design_row(&db, &handle, "mine").await;

    let first = fork(&mut author, &handle, "mine", "default").await;
    assert_eq!(first.status, 303, "{}", first.body);
    assert_eq!(
        first.header("location"),
        Some(format!("/{handle}/mine-2").as_str()),
        "the slug is taken by the source, so the fork is at the next free one"
    );
    assert_fork_wrote_no_content_rows(&db, source, &handle, "mine-2").await;
    let second = fork(&mut author, &handle, "mine", "default").await;
    assert_eq!(
        second.header("location"),
        Some(format!("/{handle}/mine-3").as_str()),
        "{}",
        second.body
    );
    // A fork of the fork: its source is the fork, not the original, and
    // its slug numbers on from the fork's.
    let of_fork = fork(&mut author, &handle, "mine-2", "default").await;
    assert_eq!(
        of_fork.header("location"),
        Some(format!("/{handle}/mine-2-2").as_str()),
        "{}",
        of_fork.body
    );
    let page = http::request(addr, "GET", &format!("/{handle}/mine-2-2"), &[], "").await;
    assert!(
        page.body.contains(&format!(
            r#"Forked from <a href="/{handle}/mine-2">{handle}/mine-2</a>"#
        )),
        "{}",
        page.body
    );
}

#[tokio::test]
async fn a_stranger_forks_an_unlisted_design_by_its_link_and_the_fork_is_unlisted_too() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    two_branches(&mut author, &author_handle, "quiet").await;
    let source = design::find(&db, author_handle.as_str(), "quiet")
        .await
        .unwrap()
        .expect("the source")
        .0;
    roles::set_visibility(&db, source.id, Visibility::Unlisted)
        .await
        .unwrap();

    let (mut stranger, _, stranger_handle) = signed_in(&db, addr).await;
    let made = fork(&mut stranger, &author_handle, "quiet", "all").await;
    assert_eq!(made.status, 303, "{}", made.body);
    let fork = design::find(&db, stranger_handle.as_str(), "quiet")
        .await
        .unwrap()
        .expect("the fork")
        .0;
    assert_eq!(fork.visibility, Visibility::Unlisted);
    assert_eq!(fork.name, source.name);
    let (status, body) = http::get(addr, &format!("/{stranger_handle}")).await;
    assert_eq!(status, 200);
    assert!(
        !body.contains("/quiet"),
        "an unlisted fork is in no listing: {body}"
    );
}

#[tokio::test]
async fn the_forked_from_line_says_so_once_the_source_is_deleted() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    two_branches(&mut author, &author_handle, "doomed").await;
    let (source, _, _) = design_row(&db, &author_handle, "doomed").await;
    let (mut forker, _, forker_handle) = signed_in(&db, addr).await;
    let made = fork(&mut forker, &author_handle, "doomed", "default").await;
    assert_eq!(made.status, 303, "{}", made.body);

    // The design's delete is #23; its hard delete of the row is what the
    // fork's reference answers to.
    sqlx::query("DELETE FROM design WHERE id = $1")
        .bind(source)
        .execute(&db)
        .await
        .unwrap();
    let page = http::request(addr, "GET", &format!("/{forker_handle}/doomed"), &[], "").await;
    assert_eq!(
        page.status, 200,
        "the fork outlives its source: {}",
        page.body
    );
    assert!(
        page.body
            .contains(&format!("Forked from {author_handle}/doomed (deleted)")),
        "{}",
        page.body
    );
    assert!(
        !page
            .body
            .contains(&format!(r#"href="/{author_handle}/doomed"#)),
        "nothing links a design that is gone: {}",
        page.body
    );
}

#[tokio::test]
async fn a_fork_needs_a_signed_in_person_and_a_source_with_a_revision() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    two_branches(&mut author, &author_handle, "guarded").await;

    let signed_out = http::request(
        addr,
        "POST",
        &format!("/{author_handle}/guarded/fork"),
        &[],
        "take=default",
    )
    .await;
    assert_eq!(signed_out.status, 303, "sent home: {}", signed_out.body);
    assert_eq!(signed_out.header("location"), Some("/"));

    let (mut forker, _, _) = signed_in(&db, addr).await;
    let nowhere = fork(&mut forker, &author_handle, "nothing", "default").await;
    assert_eq!(nowhere.status, 404, "{}", nowhere.body);
    let malformed = fork(&mut forker, &author_handle, "guarded", "some").await;
    assert_eq!(malformed.status, 422, "{}", malformed.body);

    // A design with no revision has no head to fork from.
    let (owner, empty) = person_with_empty_design(&db).await;
    let refused = fork(&mut forker, &owner, &empty, "default").await;
    assert_eq!(refused.status, 404, "{}", refused.body);

    // The page offers the form to everyone, hidden until signed in. It
    // sits in a div, since a form closes an open paragraph and a script
    // looking for it in one would find nothing; on a design that is no
    // fork the block is hidden with it, or it would be an empty band.
    let page = http::request(addr, "GET", &format!("/{author_handle}/guarded"), &[], "").await;
    assert!(
        page.body.contains(r#"<div id="fork" hidden>"#)
            && page.body.contains(&format!(
                r#"<form method="post" action="/{author_handle}/guarded/fork" hidden>"#
            ))
            && page
                .body
                .contains(r#"<option value="default">the main branch</option>"#)
            && page
                .body
                .contains(r#"<option value="all">all 2 branches</option>"#),
        "{}",
        page.body
    );
}

/// A revision with no board opens on the schematic page, which has no
/// design summary: `?design` keeps the design page, and the forked-from
/// line and the form are there.
#[tokio::test]
async fn a_schematic_only_fork_shows_its_forked_from_line_at_the_design_page() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    let bytes = format!(
        "(kicad_sch (version 20250114) (uuid \"{}\"))",
        fresh("sheet")
    )
    .into_bytes();
    let posted = author
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "sheets"),
                Part::file("sheets.kicad_sch", &bytes),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);

    let (mut forker, _, forker_handle) = signed_in(&db, addr).await;
    let made = fork(&mut forker, &author_handle, "sheets", "default").await;
    assert_eq!(made.status, 303, "{}", made.body);
    let fork_path = format!("/{forker_handle}/sheets");
    assert_eq!(made.header("location"), Some(fork_path.as_str()));

    let bare = http::request(addr, "GET", &fork_path, &[], "").await;
    assert_eq!(bare.status, 303, "it opens on its sheet: {}", bare.body);
    let page = http::request(addr, "GET", &format!("{fork_path}?design"), &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains(&format!(
            r#"Forked from <a href="/{author_handle}/sheets">{author_handle}/sheets</a>"#
        )),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(&format!(
            r#"<form method="post" action="{fork_path}/fork" hidden>"#
        )),
        "the fork can be forked: {}",
        page.body
    );
}

/// A long slug is cut to fit its number, and the next fork still finds a
/// free one: the slugs a fork avoids are all of the forker's.
#[tokio::test]
async fn a_second_fork_of_a_slug_at_the_longest_gets_a_slug_of_its_own() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut author, _, handle) = signed_in(&db, addr).await;
    let long = "a".repeat(design::Slug::MAX_LEN);
    two_branches(&mut author, &handle, &long).await;
    let cut = "a".repeat(design::Slug::MAX_LEN - 2);
    for number in 2..=3 {
        let made = fork(&mut author, &handle, &long, "default").await;
        assert_eq!(made.status, 303, "{}", made.body);
        assert_eq!(
            made.header("location"),
            Some(format!("/{handle}/{cut}-{number}").as_str()),
        );
    }
}
