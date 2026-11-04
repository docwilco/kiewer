//! Branches and history over HTTP (#18): three uploads to `main` make a
//! chain the history page lists as rev 1 to 3, the branch and revision
//! URL forms resolve (the latter only for a revision the design
//! reaches), the banner shows off the head, a push goes to a selected or
//! a new branch, and a taken branch name is refused. The Postgres tests
//! skip without `DATABASE_URL`, like `tests/it/db.rs`.

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
            login: "historian".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("h").parse().expect("a handle");
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

/// A schematic whose text is `tag`'s alone, so each upload is a new blob.
fn sheet(tag: &str) -> Vec<u8> {
    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh(tag)).into_bytes()
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

/// The history page of `branch` of `slug`.
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

/// The text fields of one push.
type Fields<'a> = [(&'a str, &'a str)];

/// One sheet of `slug` pushed through the form with these text fields.
async fn push(browser: &mut http::Browser, slug: &str, fields: &[(&str, &str)]) -> http::Response {
    let bytes = sheet("push");
    let name = format!("{slug}.kicad_sch");
    let mut parts: Vec<Part<'_>> = fields
        .iter()
        .map(|(field, value)| Part::text(field, value))
        .collect();
    parts.push(Part::file(&name, &bytes));
    browser.post_multipart("/upload", &parts).await
}

/// Three uploads of `slug` on `main` through the form, with messages
/// `rev 1` to `rev 3`; the design's page afterwards.
/// The design page at `path`, asked to stay: the revisions here hold a
/// sheet and no board, so the bare page opens on the schematic
/// (tests/it/schematic.rs) and `design` in the query keeps it.
async fn design_page(addr: SocketAddr, path: &str) -> http::Response {
    let separator = if path.contains('?') { '&' } else { '?' };
    http::request(addr, "GET", &format!("{path}{separator}design"), &[], "").await
}

async fn chain_of_three(browser: &mut http::Browser, handle: &Handle, slug: &str) -> String {
    let path = format!("{handle}/{slug}");
    for i in 1..=3 {
        let design = if i == 1 { "" } else { path.as_str() };
        let bytes = sheet("s");
        let posted = browser
            .post_multipart(
                "/upload",
                &[
                    Part::text("design", design),
                    Part::text("name", slug),
                    Part::text("branch", "main"),
                    Part::text("message", &format!("rev {i}")),
                    Part::file(&format!("{slug}.kicad_sch"), &bytes),
                ],
            )
            .await;
        assert_eq!(posted.status, 303, "upload {i}: {}", posted.body);
        assert_eq!(
            posted.header("location"),
            Some(format!("/{handle}/{slug}").as_str())
        );
    }
    let page = design_page(browser.addr(), &format!("/{handle}/{slug}")).await;
    assert_eq!(page.status, 200, "{}", page.body);
    page.body
}

#[tokio::test]
async fn three_uploads_to_main_make_a_chain_the_history_page_lists_as_rev_1_to_3() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let page = chain_of_three(&mut browser, &handle, "chain").await;
    assert!(page.contains("rev 3"), "the head is rev 3: {page}");
    assert!(
        !page.contains(r#"id="off-head""#),
        "no banner at the head: {page}"
    );
    assert!(
        page.contains(&format!(r#"href="/{handle}/chain/b/main/history""#)),
        "the summary links the history: {page}"
    );

    let history = history_page(addr, &handle, "chain", "main").await;
    assert_eq!(history.status, 200, "{}", history.body);
    let body = &history.body;
    let positions: Vec<usize> = ["rev 3", "rev 2", "rev 1"]
        .iter()
        .map(|text| {
            body.find(text)
                .unwrap_or_else(|| panic!("{text} in {body}"))
        })
        .collect();
    assert!(
        positions[0] < positions[1] && positions[1] < positions[2],
        "newest first: {body}"
    );
    assert!(body.contains(handle.as_str()), "the author: {body}");
    assert!(
        body.contains(r#"class="thumb placeholder""#),
        "a thumbnail's place: {body}"
    );
    assert!(body.contains("<time datetime="), "the time: {body}");
    let hashes = linked_revisions(body, &handle, "chain");
    assert_eq!(hashes.len(), 3, "each revision links its page: {body}");
    for hash in &hashes {
        assert!(body.contains(&hash[..8]), "the short hash is shown: {body}");
    }
    assert_design_links(body, &handle, "chain");
}

/// Every link into the design on `body` has one of the settled shapes
/// (docs/v1-scope.md, URLs): the page, its people and branches pages (the latter opened at
/// a revision by `?from={hash}`), `/b/{branch}`, `/b/{branch}/history`
/// `/r/{hash}` or `/r/{hash}/f/{path}` with at most a `?b={branch}` hint; so no ordinal is in
/// any URL.
fn assert_design_links(body: &str, handle: &Handle, slug: &str) {
    let prefix = format!("href=\"/{handle}/{slug}");
    let is_hash = |text: &str| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit());
    let is_name = |text: &str| !text.is_empty() && !text.bytes().all(|b| b.is_ascii_digit());
    for (start, _) in body.match_indices(&prefix) {
        let rest = &body[start + prefix.len()..];
        let path = &rest[..rest.find('"').expect("the attribute closes")];
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let segments: Vec<&str> = path.split('/').skip(1).collect();
        let settled = match segments.as_slice() {
            [] | ["schematic" | "people" | "branches"] => true,
            ["b", name] | ["b", name, "history"] => is_name(name),
            ["r", hash] | ["r", hash, "f", _, ..] => is_hash(hash),
            _ => false,
        };
        assert!(settled, "an unsettled URL shape: {path}");
        let at_revision = path == "/branches" && query.strip_prefix("from=").is_some_and(is_hash);
        assert!(
            query.is_empty() || query.strip_prefix("b=").is_some_and(is_name) || at_revision,
            "a query that is not the branch hint: {path}?{query}"
        );
    }
}

#[tokio::test]
async fn the_branch_and_revision_urls_resolve_and_an_unreached_revision_is_404() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let page = chain_of_three(&mut browser, &handle, "urls").await;
    let history = history_page(addr, &handle, "urls", "main").await;
    let hashes = linked_revisions(&history.body, &handle, "urls");
    let (third, first) = (&hashes[0], &hashes[2]);

    let at_branch = design_page(addr, &format!("/{handle}/urls/b/main")).await;
    assert_eq!(at_branch.status, 200, "{}", at_branch.body);
    assert_eq!(
        at_branch.body, page,
        "the branch URL is the design page at the head"
    );
    assert!(
        page.contains(&format!(r#"title="{third}""#)),
        "the head is the third: {page}"
    );

    let at_first = design_page(addr, &format!("/{handle}/urls/r/{first}")).await;
    assert_eq!(at_first.status, 200, "{}", at_first.body);
    let body = &at_first.body;
    assert!(body.contains(&format!(r#"title="{first}""#)), "{body}");
    assert!(body.contains("rev 1"), "{body}");
    assert!(
        body.contains(r#"id="off-head""#) && body.contains("2 behind the head"),
        "the banner names the distance: {body}"
    );
    assert!(
        body.contains(&format!(r#"href="/{handle}/urls""#)),
        "the banner links the head: {body}"
    );
    assert!(
        body.contains(&format!(r#"href="/{handle}/urls/branches?from={first}""#)),
        "the banner links a new branch here: {body}"
    );
    assert!(
        body.contains(&format!(
            r#"<option value="/{handle}/urls/r/{first}" selected>"#
        )),
        "the revision dropdown has it selected: {body}"
    );
    assert!(
        body.contains(r#"<option value="/"#) && body.contains(r"selected>main</option>"),
        "the branch dropdown has main selected: {body}"
    );

    // Another design's revision is not this one's; nor is nonsense.
    let (mut other, _, other_handle) = signed_in(&db, addr).await;
    chain_of_three(&mut other, &other_handle, "urls").await;
    let elsewhere = history_page(addr, &other_handle, "urls", "main").await;
    let theirs = linked_revisions(&elsewhere.body, &other_handle, "urls");
    let foreign = &theirs[0];
    let (status, _) = http::get(addr, &format!("/{handle}/urls/r/{foreign}")).await;
    assert_eq!(status, 404, "a revision the design does not reach");
    let reached = design_page(addr, &format!("/{other_handle}/urls/r/{foreign}")).await;
    assert_eq!(reached.status, 200, "but its own design reaches it");
    let (status, _) = http::get(addr, &format!("/{handle}/urls/r/not-a-hash")).await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, &format!("/{handle}/urls/r/{}", "0".repeat(64))).await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, &format!("/{handle}/urls/b/nothing")).await;
    assert_eq!(status, 404, "a branch the design does not have");
    let (status, _) = http::get(addr, &format!("/{handle}/urls/b/nothing/history")).await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, &format!("/{handle}/nothing/b/main/history")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_form_pushes_to_the_selected_branch_or_a_new_one_and_refuses_a_taken_name() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    chain_of_three(&mut browser, &handle, "branchy").await;
    let branchy = format!("{handle}/branchy");
    let history = history_page(addr, &handle, "branchy", "main").await;
    let hashes = linked_revisions(&history.body, &handle, "branchy");
    let first = &hashes[2];

    // The form opened from the first revision's page: the design and
    // main preselected, the revision carried for a new branch.
    let form = browser
        .get(&format!(
            "/upload?design={handle}/branchy&branch=main&from={first}"
        ))
        .await;
    assert_eq!(form.status, 200, "{}", form.body);
    for expected in [
        format!(r#"<option value="{handle}/branchy" selected>"#),
        r#"<option value="main" selected>main</option>"#.to_owned(),
        r#"<option value="">A new branch</option>"#.to_owned(),
        format!(r#"name="from" value="{first}""#),
    ] {
        assert!(form.body.contains(&expected), "{expected}: {}", form.body);
    }
    let new_branch_form = browser
        .get(&format!(
            "/upload?design={handle}/branchy&branch=&from={first}"
        ))
        .await;
    assert!(
        new_branch_form
            .body
            .contains(r#"<option value="" selected>A new branch</option>"#),
        "opened for a new branch: {}",
        new_branch_form.body
    );

    // A push to a new branch at the first revision: a chain of two there.
    let posted = push(
        &mut browser,
        "branchy",
        &[
            ("design", &branchy),
            ("branch", ""),
            ("new_branch", "experiment"),
            ("from", first),
            ("message", "on a branch"),
        ],
    )
    .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    assert_eq!(
        posted.header("location"),
        Some(format!("/{handle}/branchy/b/experiment").as_str()),
        "the browser goes to the new branch's head"
    );
    let at_branch = design_page(addr, &format!("/{handle}/branchy/b/experiment")).await;
    assert_eq!(at_branch.status, 200, "{}", at_branch.body);
    assert!(at_branch.body.contains("on a branch"), "{}", at_branch.body);
    assert!(
        at_branch.body.contains(r"selected>experiment</option>")
            && at_branch.body.contains(">main</option>"),
        "both branches in the dropdown: {}",
        at_branch.body
    );
    let branch_history = history_page(addr, &handle, "branchy", "experiment").await;
    let on_branch = linked_revisions(&branch_history.body, &handle, "branchy");
    assert_eq!(on_branch.len(), 2, "{}", branch_history.body);
    assert_eq!(&on_branch[1], first, "rooted at the first revision");
    assert!(
        branch_history.body.contains("rev 2") && !branch_history.body.contains("rev 3"),
        "ordinals along this branch: {}",
        branch_history.body
    );
    let main_history = history_page(addr, &handle, "branchy", "main").await;
    assert_eq!(
        linked_revisions(&main_history.body, &handle, "branchy"),
        hashes,
        "main is as it was"
    );
    let form = browser
        .get(&format!(
            "/upload?design={handle}/branchy&branch=experiment"
        ))
        .await;
    assert!(
        form.body
            .contains(r#"<option value="experiment" selected>experiment</option>"#),
        "the new branch is offered: {}",
        form.body
    );
}

#[tokio::test]
async fn a_push_to_a_taken_or_missing_branch_is_refused_and_one_onto_the_head_lands() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    chain_of_three(&mut browser, &handle, "branchy").await;
    let branchy = format!("{handle}/branchy");
    let history = history_page(addr, &handle, "branchy", "main").await;
    let hashes = linked_revisions(&history.body, &handle, "branchy");
    let head = &hashes[0];
    let new_branch = [
        ("design", branchy.as_str()),
        ("branch", ""),
        ("new_branch", "experiment"),
        ("from", head),
    ];
    let posted = push(&mut browser, "branchy", &new_branch).await;
    assert_eq!(posted.status, 303, "a branch at the head: {}", posted.body);

    // The name is taken; a nameless new branch; a new branch from
    // nothing; a branch that is not there; a revision the design does
    // not reach.
    let zeros = "0".repeat(64);
    let refused: [(&Fields<'_>, u16, &str); 5] = [
        (&new_branch, 409, "already has a branch experiment"),
        (
            &[("design", &branchy), ("branch", ""), ("new_branch", "")],
            400,
            "needs a name",
        ),
        (
            &[
                ("design", &branchy),
                ("branch", ""),
                ("new_branch", "rootless"),
            ],
            400,
            "starts at a revision",
        ),
        (
            &[("design", &branchy), ("branch", "nothing")],
            404,
            "no branch nothing",
        ),
        (
            &[
                ("design", &branchy),
                ("branch", ""),
                ("new_branch", "elsewhere"),
                ("from", &zeros),
            ],
            404,
            "no revision",
        ),
    ];
    for (fields, status, reason) in refused {
        let answer = push(&mut browser, "branchy", fields).await;
        assert_eq!(answer.status, status, "{fields:?}: {}", answer.body);
        assert!(answer.body.contains(reason), "{reason}: {}", answer.body);
    }

    // Onto the selected branch: the head moves, the chain grows to four.
    let onto_main = push(
        &mut browser,
        "branchy",
        &[
            ("design", &branchy),
            ("branch", "main"),
            ("message", "rev 4"),
        ],
    )
    .await;
    assert_eq!(onto_main.status, 303, "{}", onto_main.body);
    assert_eq!(
        onto_main.header("location"),
        Some(format!("/{handle}/branchy").as_str()),
        "the default branch's head is the design's page"
    );
    let main_history = history_page(addr, &handle, "branchy", "main").await;
    assert!(main_history.body.contains("rev 4"), "{}", main_history.body);
    assert_eq!(
        linked_revisions(&main_history.body, &handle, "branchy").len(),
        4
    );
}

#[tokio::test]
async fn a_link_from_a_branch_keeps_its_context_and_the_banner_names_that_branch() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    chain_of_three(&mut browser, &handle, "ctx").await;
    let ctx = format!("{handle}/ctx");
    let main_history = history_page(addr, &handle, "ctx", "main").await;
    let first = linked_revisions(&main_history.body, &handle, "ctx")[2].clone();
    // Two revisions on a branch at the first: the branch is first, a, b.
    for (i, (branch, new_branch, from)) in [("", "side", first.as_str()), ("side", "", "")]
        .into_iter()
        .enumerate()
    {
        let message = format!("side {i}");
        let fields = [
            ("design", ctx.as_str()),
            ("branch", branch),
            ("new_branch", new_branch),
            ("from", from),
            ("message", &message),
        ];
        let posted = push(&mut browser, "ctx", &fields).await;
        assert_eq!(posted.status, 303, "{}", posted.body);
    }
    let side_history = history_page(addr, &handle, "ctx", "side").await;
    let on_side = linked_revisions(&side_history.body, &handle, "ctx");
    assert_eq!(on_side.len(), 3, "{}", side_history.body);
    let (a, root) = (&on_side[1], &on_side[2]);
    assert_eq!(root, &first);
    assert!(
        side_history
            .body
            .contains(&format!(r#"href="/{handle}/ctx/r/{a}?b=side""#)),
        "a link from the branch's history carries the branch: {}",
        side_history.body
    );
    assert_design_links(&side_history.body, &handle, "ctx");

    // Followed, the link opens in the branch's context, with the banner
    // naming the branch; without the hint the shared root is main's.
    let hinted = design_page(addr, &format!("/{handle}/ctx/r/{a}?b=side")).await;
    assert_eq!(hinted.status, 200, "{}", hinted.body);
    assert!(
        hinted
            .body
            .contains("This is rev 2 of side, 1 behind the head."),
        "{}",
        hinted.body
    );
    assert!(
        hinted.body.contains(r"selected>side</option>")
            && hinted
                .body
                .contains(&format!(r#"href="/{handle}/ctx/b/side""#)),
        "the dropdown and the banner's head link are the branch's: {}",
        hinted.body
    );
    assert_design_links(&hinted.body, &handle, "ctx");
    let bare = design_page(addr, &format!("/{handle}/ctx/r/{root}")).await;
    assert!(
        bare.body
            .contains("This is rev 1 of main, 2 behind the head."),
        "{}",
        bare.body
    );
    let hinted_root = design_page(addr, &format!("/{handle}/ctx/r/{root}?b=side")).await;
    assert!(
        hinted_root
            .body
            .contains("This is rev 1 of side, 2 behind the head."),
        "{}",
        hinted_root.body
    );
    let wrong_hint = design_page(addr, &format!("/{handle}/ctx/r/{root}?b=nothing")).await;
    assert_eq!(wrong_hint.status, 200);
    assert!(
        wrong_hint.body.contains("rev 1 of main"),
        "a hint naming no branch of the design is ignored: {}",
        wrong_hint.body
    );
}
