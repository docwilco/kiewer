//! Pending revisions (#19): an upload that finds its branch's head moved
//! is held for an hour, and the uploader chooses re-append (the parent
//! rewritten to the new head) or branch (a new branch at the old head).
//! Two uploads overlap for real here, one paused inside the blob store
//! while the other lands, and two forms open at one head; both outcomes'
//! graphs are read back, and so are expiry, the hourly allowance and who
//! may decide. The Postgres tests skip without `DATABASE_URL`, like
//! `tests/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::blob::{BlobHash, BlobStore, PutReceipt};
use kiewer_lib::design::{self, BranchName, Design};
use kiewer_lib::error::{Error, Refusal};
use kiewer_lib::pending::{self, Choice};
use kiewer_lib::revision::RevisionHash;
use kiewer_lib::roles::{self, Role};
use kiewer_lib::upload::{self, Author, BranchTarget, Incoming, Limits, Outcome, Target};
use kiewer_lib::{Result, rate_limit, session};
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use tokio::sync::Notify;

/// A sheet whose text is its own, so each upload is a new blob.
fn sheet() -> Vec<u8> {
    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s")).into_bytes()
}

/// A person with a handle.
async fn person(db: &PgPool) -> Author {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "chooser".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("h").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    Author {
        account_id: signed_in.account_id,
        handle,
    }
}

/// `person`'s browser, carrying a live session.
async fn browser(db: &PgPool, addr: SocketAddr, person: AccountId) -> http::Browser {
    let id = session::start(db, person).await.expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    browser
}

async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    (http::serve_on_free_port(state).await, dir)
}

/// One sheet pushed onto `target` through the library.
async fn pushed(
    db: &PgPool,
    blobs: &dyn BlobStore,
    author: &Author,
    target: Target,
    limits: &Limits,
) -> Result<Outcome> {
    let mut incoming = Incoming::new(*limits)?;
    let (path, kind) = Incoming::accepts("demo.kicad_sch").expect("a sheet");
    incoming.begin(path, kind).await?;
    incoming.write(&sheet()).await?;
    incoming.end().await?;
    upload::push(db, blobs, limits, author, target, Some("a sheet"), incoming).await
}

/// A design of `owner`'s with one revision on its default branch, whose
/// hash comes back with it.
async fn one_revision(
    db: &PgPool,
    blobs: &dyn BlobStore,
    owner: &Author,
) -> (Design, RevisionHash) {
    let outcome = pushed(
        db,
        blobs,
        owner,
        Target::New {
            name: Some(fresh("d")),
        },
        &Limits::DEFAULT,
    )
    .await
    .expect("the first push");
    let Outcome::Pushed(first) = outcome else {
        panic!("a new design has no head to have moved");
    };
    (first.design, first.revision)
}

/// `Target::Existing` for `design` of `owner` on `branch`.
fn onto(owner: &Author, design: &Design, branch: BranchTarget) -> Target {
    Target::Existing {
        owner: owner.handle.clone(),
        slug: design.slug.clone(),
        branch,
    }
}

/// The hashes along `branch`, newest first, each with its parent.
async fn chain(
    db: &PgPool,
    design: &Design,
    branch: &str,
) -> Vec<(RevisionHash, Option<RevisionHash>)> {
    let name: BranchName = branch.parse().expect("a branch name");
    design::history(db, design.id, &name, None)
        .await
        .expect("a history")
        .into_iter()
        .map(|entry| (entry.revision.hash, entry.revision.parent))
        .collect()
}

/// A store that holds its next `put` until told: the first upload stops
/// mid-way, with its head already read, while the second one lands.
struct Gate {
    inner: FsBlobStore,
    reached: Notify,
    open: Notify,
}

#[async_trait]
impl BlobStore for Gate {
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt> {
        self.reached.notify_one();
        self.open.notified().await;
        self.inner.put(bytes).await
    }

    async fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        self.inner.get(hash).await
    }

    async fn get_gzip(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        self.inner.get_gzip(hash).await
    }

    async fn exists(&self, hash: &BlobHash) -> Result<bool> {
        self.inner.exists(hash).await
    }
}

/// Two uploads to one branch that truly overlap, the slower one paused
/// after it read the head: the quick one lands, the slow one is held.
/// The design, the old head, the new head and the held upload's id.
async fn overlapping_uploads(
    db: &PgPool,
    dir: &TempDir,
    owner: &Author,
) -> (Design, RevisionHash, RevisionHash, pending::PendingId) {
    let store = FsBlobStore::new(dir.path());
    let (design, base) = one_revision(db, &store, owner).await;
    let gate = Gate {
        inner: FsBlobStore::new(dir.path()),
        reached: Notify::new(),
        open: Notify::new(),
    };
    let slow = pushed(
        db,
        &gate,
        owner,
        onto(owner, &design, BranchTarget::Default),
        &Limits::DEFAULT,
    );
    let quick = async {
        gate.reached.notified().await;
        let landed = pushed(
            db,
            &store,
            owner,
            onto(owner, &design, BranchTarget::Default),
            &Limits::DEFAULT,
        )
        .await
        .expect("the quick push");
        gate.open.notify_one();
        landed
    };
    let (slow, quick) = tokio::join!(slow, quick);
    let Outcome::Pushed(quick) = quick else {
        panic!("the quick push found nothing moved");
    };
    let Outcome::Pending { id, .. } = slow.expect("the slow push") else {
        panic!("the slow push's head moved: it is held, not landed");
    };
    (design, base, quick.revision, id)
}

#[tokio::test]
async fn the_second_of_two_overlapping_uploads_is_held_and_nothing_of_it_is_on_the_branch() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (design, base, head, id) = overlapping_uploads(&db, &dir, &owner).await;
    assert_eq!(
        chain(&db, &design, "main").await,
        vec![(head, Some(base)), (base, None)],
        "the branch holds the quick push alone"
    );
    let waiting = pending::find(&db, id, owner.account_id)
        .await
        .unwrap()
        .expect("held for its uploader");
    assert_eq!(waiting.branch.as_str(), "main");
    assert_eq!(waiting.base.hash, base);
    assert_eq!(waiting.head.hash, head);
    assert_eq!((waiting.base.ordinal, waiting.head.ordinal), (1, 2));
    assert_eq!(waiting.message.as_deref(), Some("a sheet"));
}

#[tokio::test]
async fn re_append_rewrites_the_parent_to_the_new_head() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (design, base, head, id) = overlapping_uploads(&db, &dir, &owner).await;
    let decided = pending::decide(&db, &Limits::DEFAULT, &owner, id, Choice::Reappend)
        .await
        .unwrap()
        .expect("still pending");
    assert_eq!(decided.branch.as_str(), "main");
    assert_eq!(
        chain(&db, &design, "main").await,
        vec![
            (decided.revision, Some(head)),
            (head, Some(base)),
            (base, None)
        ],
    );
    assert!(
        design::reaches(&db, design.id, decided.revision)
            .await
            .unwrap()
    );
    assert!(
        pending::find(&db, id, owner.account_id)
            .await
            .unwrap()
            .is_none(),
        "a decided upload is pending no more"
    );
    let again = pending::decide(&db, &Limits::DEFAULT, &owner, id, Choice::Reappend)
        .await
        .unwrap();
    assert!(again.is_none(), "it is decided once");
}

#[tokio::test]
async fn branch_starts_a_new_branch_at_the_old_head_and_leaves_the_old_branch_alone() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (design, base, head, id) = overlapping_uploads(&db, &dir, &owner).await;
    let name: BranchName = "side-line".parse().unwrap();
    let decided = pending::decide(
        &db,
        &Limits::DEFAULT,
        &owner,
        id,
        Choice::Branch(name.clone()),
    )
    .await
    .unwrap()
    .expect("still pending");
    assert_eq!(decided.branch, name);
    assert_eq!(
        chain(&db, &design, "side-line").await,
        vec![(decided.revision, Some(base)), (base, None)],
    );
    assert_eq!(
        chain(&db, &design, "main").await,
        vec![(head, Some(base)), (base, None)],
    );
}

#[tokio::test]
async fn a_taken_name_leaves_the_upload_pending_for_another_try() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (_, _, _, id) = overlapping_uploads(&db, &dir, &owner).await;
    let taken = pending::decide(
        &db,
        &Limits::DEFAULT,
        &owner,
        id,
        Choice::Branch("main".parse().unwrap()),
    )
    .await
    .expect_err("main is taken");
    assert!(
        matches!(taken, Error::Refused(Refusal::BranchTaken { ref branch }) if branch == "main"),
        "{taken}"
    );
    assert!(
        pending::find(&db, id, owner.account_id)
            .await
            .unwrap()
            .is_some(),
        "the refused choice wrote nothing"
    );
}

#[tokio::test]
async fn an_upload_nobody_decides_on_expires_and_the_expiry_leaves_the_rest() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (_, _, _, lapsed) = overlapping_uploads(&db, &dir, &owner).await;
    let (_, _, _, live) = overlapping_uploads(&db, &dir, &owner).await;
    sqlx::query(
        "UPDATE pending_revision SET expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(lapsed)
    .execute(&db)
    .await
    .unwrap();

    assert!(
        pending::find(&db, lapsed, owner.account_id)
            .await
            .unwrap()
            .is_none(),
        "past its hour it is gone for its uploader, swept or not"
    );
    let late = pending::decide(&db, &Limits::DEFAULT, &owner, lapsed, Choice::Reappend)
        .await
        .unwrap();
    assert!(late.is_none(), "too late to decide");

    assert!(pending::expire(&db).await.unwrap() >= 1);
    let rows: Vec<pending::PendingId> =
        sqlx::query_scalar("SELECT id FROM pending_revision WHERE id = ANY($1)")
            .bind([lapsed, live])
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(rows, [live], "the sweep takes the lapsed row alone");
}

#[tokio::test]
async fn a_held_upload_spends_the_allowance_once_whatever_comes_of_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    // Three so far: the first revision, the quick push, the held one.
    let (_, _, _, id) = overlapping_uploads(&db, &dir, &owner).await;
    rate_limit::check(&db, owner.account_id, 4)
        .await
        .expect("three of four used");
    rate_limit::check(&db, owner.account_id, 3)
        .await
        .expect_err("the held upload counts while it waits");
    pending::decide(&db, &Limits::DEFAULT, &owner, id, Choice::Reappend)
        .await
        .unwrap()
        .expect("still pending");
    rate_limit::check(&db, owner.account_id, 4)
        .await
        .expect("the decision is the same upload, not a fourth");
    rate_limit::check(&db, owner.account_id, 3)
        .await
        .expect_err("and it still counts");
}

#[tokio::test]
async fn the_decision_itself_is_not_held_back_by_the_allowance() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (_, _, _, id) = overlapping_uploads(&db, &dir, &owner).await;
    let used_up = Limits {
        uploads_per_hour: 1,
        ..Limits::DEFAULT
    };
    let decided = pending::decide(&db, &used_up, &owner, id, Choice::Reappend)
        .await
        .unwrap();
    assert!(
        decided.is_some(),
        "an hour's allowance was spent on the upload"
    );
}

#[tokio::test]
async fn only_the_uploader_decides_and_only_while_they_hold_a_role() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let store = FsBlobStore::new(dir.path());
    let owner = person(&db).await;
    let collaborator = person(&db).await;
    let (design, base) = one_revision(&db, &store, &owner).await;
    roles::set_role(&db, design.id, &collaborator.handle, Role::Collaborator)
        .await
        .unwrap();
    pushed(
        &db,
        &store,
        &owner,
        onto(&owner, &design, BranchTarget::Default),
        &Limits::DEFAULT,
    )
    .await
    .expect("the head moves");
    let waiting = pushed(
        &db,
        &store,
        &collaborator,
        onto(
            &owner,
            &design,
            BranchTarget::Seen {
                name: "main".parse().unwrap(),
                head: base,
            },
        ),
        &Limits::DEFAULT,
    )
    .await
    .expect("a push built on the old head");
    let Outcome::Pending { id, .. } = waiting else {
        panic!("the head it saw is not the head now");
    };

    assert!(
        pending::find(&db, id, owner.account_id)
            .await
            .unwrap()
            .is_none(),
        "not even the owner sees another person's upload"
    );
    let stranger = pending::decide(&db, &Limits::DEFAULT, &owner, id, Choice::Reappend)
        .await
        .unwrap();
    assert!(stranger.is_none());

    roles::remove_role(&db, design.id, &collaborator.handle)
        .await
        .unwrap();
    let removed = pending::decide(&db, &Limits::DEFAULT, &collaborator, id, Choice::Reappend)
        .await
        .expect_err("no role any more");
    assert!(matches!(removed, Error::Forbidden { .. }), "{removed}");
}

#[tokio::test]
async fn the_account_cap_is_met_again_at_the_decision() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (_, _, _, id) = overlapping_uploads(&db, &dir, &owner).await;
    let full = Limits {
        account_bytes: 1,
        ..Limits::DEFAULT
    };
    let refused = pending::decide(&db, &full, &owner, id, Choice::Reappend)
        .await
        .expect_err("over the cap");
    assert!(
        matches!(refused, Error::Refused(Refusal::AccountFull { .. })),
        "{refused}"
    );
}

#[tokio::test]
async fn a_file_taken_down_during_the_hour_is_refused_at_the_decision() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let owner = person(&db).await;
    let (_, _, _, id) = overlapping_uploads(&db, &dir, &owner).await;
    sqlx::query(
        "INSERT INTO blocked_blob (hash, reason)
         SELECT te.blob_hash, 'a test' FROM pending_revision p
         JOIN tree_entry te ON te.tree_hash = p.tree_hash
         WHERE p.id = $1",
    )
    .bind(id)
    .execute(&db)
    .await
    .unwrap();
    let refused = pending::decide(&db, &Limits::DEFAULT, &owner, id, Choice::Reappend)
        .await
        .expect_err("blocked");
    assert!(
        matches!(refused, Error::Refused(Refusal::Blocked { ref path }) if path == "demo.kicad_sch"),
        "{refused}"
    );
}

/// The location the form's POST answers with, for a push with these
/// text fields.
async fn posted(
    browser: &mut http::Browser,
    design: &str,
    from: &str,
    from_branch: &str,
    bytes: &[u8],
) -> http::Response {
    browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", design),
                Part::text("branch", "main"),
                Part::text("from", from),
                Part::text("from_branch", from_branch),
                Part::text("message", "from the form"),
                Part::file("demo.kicad_sch", bytes),
            ],
        )
        .await
}

#[tokio::test]
async fn two_forms_open_at_one_head_second_gets_the_choice_page_and_decides_there() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, dir) = server(db.clone()).await;
    let store = FsBlobStore::new(dir.path());
    let owner = person(&db).await;
    let (design, base) = one_revision(&db, &store, &owner).await;
    let path = format!("{}/{}", owner.handle, design.slug);
    let base_text = base.to_string();
    let mut browser = browser(&db, addr, owner.account_id).await;

    let first = posted(&mut browser, &path, &base_text, "main", &sheet()).await;
    assert_eq!(first.status, 303, "{}", first.body);
    assert_eq!(first.header("location"), Some(format!("/{path}").as_str()));

    let second = posted(&mut browser, &path, &base_text, "main", &sheet()).await;
    assert_eq!(second.status, 303, "{}", second.body);
    let choice_url = second.header("location").expect("to the choice").to_owned();
    assert!(
        choice_url.starts_with(&format!("/{path}/pending/")),
        "{choice_url}"
    );

    let page = browser.get(&choice_url).await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("from the form"), "{}", page.body);
    assert!(
        page.body.contains(&base.short()),
        "the page names the old head: {}",
        page.body
    );
    assert!(
        page.body
            .contains(&format!(r#"action="{choice_url}/reappend""#)),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains(&format!(r#"action="{choice_url}/branch""#)),
        "{}",
        page.body
    );

    let refused = browser
        .post_form(&format!("{choice_url}/branch"), "name=.no")
        .await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(
        refused.body.contains("may not start with"),
        "the page again with the reason: {}",
        refused.body
    );

    let decided = browser
        .post_form(&format!("{choice_url}/branch"), "name=mine")
        .await;
    assert_eq!(decided.status, 303, "{}", decided.body);
    assert_eq!(
        decided.header("location"),
        Some(format!("/{path}/b/mine").as_str())
    );
    assert_eq!(
        browser.get(&choice_url).await.status,
        404,
        "a decided upload has no page"
    );
    assert_eq!(chain(&db, &design, "mine").await[1].0, base);
}

#[tokio::test]
async fn a_form_opened_on_another_branch_does_not_hold_the_upload() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, dir) = server(db.clone()).await;
    let store = FsBlobStore::new(dir.path());
    let owner = person(&db).await;
    let (design, base) = one_revision(&db, &store, &owner).await;
    let path = format!("{}/{}", owner.handle, design.slug);
    let mut browser = browser(&db, addr, owner.account_id).await;
    pushed(
        &db,
        &store,
        &owner,
        onto(&owner, &design, BranchTarget::Default),
        &Limits::DEFAULT,
    )
    .await
    .expect("the head moves");
    // The page the form was opened from showed another branch's revision;
    // the person then picked main in the selector.
    let other = posted(&mut browser, &path, &base.to_string(), "other", &sheet()).await;
    assert_eq!(other.status, 303, "{}", other.body);
    assert_eq!(other.header("location"), Some(format!("/{path}").as_str()));
}

#[tokio::test]
async fn a_rename_reaches_a_held_upload_and_a_delete_takes_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let store = FsBlobStore::new(dir.path());
    let owner = person(&db).await;
    let (design, base) = one_revision(&db, &store, &owner).await;
    let doomed: BranchName = "doomed".parse().unwrap();
    kiewer_lib::branch::create_at(&db, owner.account_id, design.id, &doomed, base)
        .await
        .unwrap();
    let on_doomed = BranchTarget::Head(doomed.clone());
    pushed(
        &db,
        &store,
        &owner,
        onto(&owner, &design, on_doomed),
        &Limits::DEFAULT,
    )
    .await
    .expect("the head moves");
    let seen = BranchTarget::Seen {
        name: doomed.clone(),
        head: base,
    };
    let waiting = pushed(
        &db,
        &store,
        &owner,
        onto(&owner, &design, seen),
        &Limits::DEFAULT,
    )
    .await
    .unwrap();
    let Outcome::Pending { id, .. } = waiting else {
        panic!("held");
    };

    let renamed: BranchName = "kept".parse().unwrap();
    kiewer_lib::branch::rename(&db, owner.account_id, design.id, &doomed, &renamed)
        .await
        .unwrap();
    let found = pending::find(&db, id, owner.account_id)
        .await
        .unwrap()
        .expect("the rename kept it");
    assert_eq!(found.branch, renamed);

    kiewer_lib::branch::delete(&db, owner.account_id, design.id, &renamed)
        .await
        .unwrap();
    assert!(
        pending::find(&db, id, owner.account_id)
            .await
            .unwrap()
            .is_none(),
        "its branch is gone"
    );
}
