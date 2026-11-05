//! The collector (#23, ADR 0002): revisions no branch reaches go, then
//! trees and blobs no revision names, anything younger than 24 hours
//! stays, a second run finds nothing, a run cut short leaves work the
//! next one finds, and a concurrent upload of the same bytes survives.
//! After those, expired sessions and day-old accounts with no handle go
//! (#31), each by an index range.
//! A run acts on the whole database, so the tests here run on a database
//! of their own, `DATABASE_URL`'s name plus `_collector`, which no other
//! module writes to (#107); there they take turns, and each ages only its
//! own rows to bring them inside what a run removes. The Postgres tests
//! skip without `DATABASE_URL`, like `tests/it/db.rs`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Auth, Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::blob::{BlobHash, BlobStore, PutReceipt};
use kiewer_lib::cache::NoPurge;
use kiewer_lib::collector::{self, Collected};
use kiewer_lib::revision::{RevisionHash, TreeEntry};
use kiewer_lib::session::{self, SessionId};
use kiewer_lib::thumbnail::{NoTasks, TaskAuth, Thumbnails};
use kiewer_lib::web::AppState;
use kiewer_lib::{Error, Result, design};
use kiewer_test_support::db::{fresh, pool_of_its_own};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;
use tokio::sync::{Mutex, Notify};

/// The suffix of this module's own database (#107), named once: a test
/// that misspelt it would run on a database of its own, outside `TURN`.
const OWN_DATABASE: &str = "collector";

/// A run sees every test's rows and drains every test's removal queue
/// into its own store, so one runs at a time.
static TURN: Mutex<()> = Mutex::const_new(());

/// The server over `db` with a filesystem store under cargo's scratch
/// directory, and the store for a test to read and to run the collector
/// on.
async fn server(db: PgPool) -> (SocketAddr, Arc<FsBlobStore>, TempDir) {
    let dir = scratch_dir!();
    let blobs = Arc::new(FsBlobStore::new(dir.path()));
    let state = http::state(db, blobs.clone());
    (http::serve_on_free_port(state).await, blobs, dir)
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "collected".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("c").parse().expect("a handle");
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

/// A board whose text is its own, so each upload is a new blob.
fn board() -> Vec<u8> {
    format!(
        "(kicad_pcb (version 20241229) (generator \"{}\") (layers (0 \"F.Cu\" signal)))",
        fresh("board")
    )
    .into_bytes()
}

/// `bytes` pushed as a board with these form fields.
async fn push(browser: &mut http::Browser, fields: &[(&str, &str)], bytes: &[u8]) {
    let mut parts: Vec<Part<'_>> = fields
        .iter()
        .map(|(field, value)| Part::text(field, value))
        .collect();
    parts.push(Part::file("board.kicad_pcb", bytes));
    let posted = browser.post_multipart("/upload", &parts).await;
    assert_eq!(posted.status, 303, "{}", posted.body);
}

/// The head of `main` of `/{handle}/{slug}`.
async fn head(db: &PgPool, handle: &Handle, slug: &str) -> RevisionHash {
    let bytes: Vec<u8> = sqlx::query_scalar(
        "SELECT b.head_hash FROM branch b
         JOIN design d ON d.id = b.design_id
         JOIN account a ON a.id = d.owner_id
         WHERE a.handle = $1 AND d.slug = $2 AND b.name = 'main'",
    )
    .bind(handle.as_str())
    .bind(slug)
    .fetch_one(db)
    .await
    .expect("the branch's row");
    RevisionHash::from_bytes(&bytes).expect("a hash")
}

/// `revisions`, their trees and their blobs, made older than the grace.
async fn age(db: &PgPool, revisions: &[RevisionHash]) {
    let hashes: Vec<Vec<u8>> = revisions
        .iter()
        .map(|hash| hash.as_bytes().to_vec())
        .collect();
    for statement in [
        "UPDATE revision SET created_at = created_at - interval '25 hours' WHERE hash = ANY($1)",
        "UPDATE tree SET claimed_at = now() - interval '25 hours'
         WHERE hash IN (SELECT tree_hash FROM revision WHERE hash = ANY($1))",
        "UPDATE blob SET claimed_at = now() - interval '25 hours'
         WHERE hash IN (SELECT te.blob_hash FROM revision r
                        JOIN tree_entry te ON te.tree_hash = r.tree_hash
                        WHERE r.hash = ANY($1))",
    ] {
        sqlx::query(statement)
            .bind(&hashes)
            .execute(db)
            .await
            .expect("aged");
    }
}

/// A blob row made older than the grace.
async fn age_blob(db: &PgPool, hash: &BlobHash) {
    sqlx::query("UPDATE blob SET claimed_at = now() - interval '25 hours' WHERE hash = $1")
        .bind(hash.as_bytes())
        .execute(db)
        .await
        .expect("aged");
}

async fn has_revision(db: &PgPool, hash: RevisionHash) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM revision WHERE hash = $1)")
        .bind(hash.as_bytes())
        .fetch_one(db)
        .await
        .expect("a read")
}

async fn has_blob_row(db: &PgPool, hash: &BlobHash) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blob WHERE hash = $1)")
        .bind(hash.as_bytes())
        .fetch_one(db)
        .await
        .expect("a read")
}

async fn tree_of(db: &PgPool, revision: RevisionHash) -> Vec<u8> {
    sqlx::query_scalar("SELECT tree_hash FROM revision WHERE hash = $1")
        .bind(revision.as_bytes())
        .fetch_one(db)
        .await
        .expect("a revision")
}

async fn has_tree(db: &PgPool, tree: &[u8]) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tree WHERE hash = $1)")
        .bind(tree)
        .fetch_one(db)
        .await
        .expect("a read")
}

/// A first sign-in that goes no further: an account with no handle, and
/// the provider id that signs in to it.
async fn sign_up(db: &PgPool) -> (AccountId, ProviderUser) {
    let user = ProviderUser {
        id: fresh("gh"),
        login: "swept".to_owned(),
        email: None,
    };
    let signed_in = account::sign_in(db, Provider::GitHub, &user)
        .await
        .expect("sign in");
    (signed_in.account_id, user)
}

/// `account` made older than the grace.
async fn age_account(db: &PgPool, account: AccountId) {
    sqlx::query("UPDATE account SET created_at = created_at - interval '25 hours' WHERE id = $1")
        .bind(account)
        .execute(db)
        .await
        .expect("aged");
}

/// `id`'s row past its expiry. The row is keyed by the SHA-256 of the
/// id's text form, as `tests/it/accounts.rs` finds it.
async fn expire_session(db: &PgPool, id: SessionId) {
    sqlx::query(
        "UPDATE session SET expires_at = now() - interval '1 second' WHERE id_hash = sha256($1)",
    )
    .bind(id.to_string().into_bytes())
    .execute(db)
    .await
    .expect("expired");
}

async fn has_session(db: &PgPool, id: SessionId) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE id_hash = sha256($1))")
        .bind(id.to_string().into_bytes())
        .fetch_one(db)
        .await
        .expect("a read")
}

async fn has_account(db: &PgPool, account: AccountId) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM account WHERE id = $1)")
        .bind(account)
        .fetch_one(db)
        .await
        .expect("a read")
}

/// One run that must succeed.
async fn run(db: &PgPool, blobs: &dyn BlobStore) -> Collected {
    collector::run(db, blobs).await.expect("a run")
}

/// `POST /{handle}/{slug}/delete` by its owner, the name typed.
async fn delete_design(browser: &mut http::Browser, handle: &Handle, slug: &str, name: &str) {
    let deleted = browser
        .post_form(
            &format!("/{handle}/{slug}/delete"),
            &format!("confirm={name}"),
        )
        .await;
    assert_eq!(deleted.status, 303, "{}", deleted.body);
}

#[tokio::test]
async fn deleting_a_design_a_fork_shares_keeps_the_shared_revisions_and_removes_the_rest() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (addr, blobs, _dir) = server(db.clone()).await;
    let (mut author, _, author_handle) = signed_in(&db, addr).await;
    let (mut forker, _, forker_handle) = signed_in(&db, addr).await;
    let path = format!("{author_handle}/shared");

    let first_board = board();
    let second_board = board();
    let only_the_source = board();
    push(&mut author, &[("name", "shared")], &first_board).await;
    let first = head(&db, &author_handle, "shared").await;
    push(
        &mut author,
        &[("design", &path), ("branch", "main")],
        &second_board,
    )
    .await;
    let second = head(&db, &author_handle, "shared").await;
    let made = forker
        .post_form(&format!("/{path}/fork"), "take=default")
        .await;
    assert_eq!(made.status, 303, "{}", made.body);
    push(
        &mut author,
        &[("design", &path), ("branch", "main")],
        &only_the_source,
    )
    .await;
    let third = head(&db, &author_handle, "shared").await;
    let third_tree = tree_of(&db, third).await;
    let third_blob = BlobHash::of(&only_the_source);

    delete_design(&mut author, &author_handle, "shared", "shared").await;
    assert!(
        has_revision(&db, third).await,
        "a design's delete leaves its revisions to the collector"
    );
    age(&db, &[first, second, third]).await;
    let collected = run(&db, blobs.as_ref()).await;
    assert!(collected.revisions >= 1, "{collected}");

    for shared in [first, second] {
        assert!(has_revision(&db, shared).await, "the fork reaches it");
    }
    assert!(!has_revision(&db, third).await, "no branch reaches it");
    assert!(!has_tree(&db, &third_tree).await, "no revision names it");
    assert!(!has_blob_row(&db, &third_blob).await, "no tree names it");
    assert!(
        !blobs.exists(&third_blob).await.expect("exists"),
        "the object went with the row"
    );

    let page = http::request(addr, "GET", &format!("/{forker_handle}/shared"), &[], "").await;
    assert_eq!(page.status, 200, "the fork stands: {}", page.body);
    assert!(
        page.body
            .contains(&format!("Forked from {author_handle}/shared (deleted)")),
        "{}",
        page.body
    );
    let (status, body) = http::get(addr, &format!("/blob/{}", BlobHash::of(&second_board))).await;
    assert_eq!(status, 200, "the shared bytes are served");
    assert_eq!(body.as_bytes(), second_board);
    let (status, _) = http::get(
        addr,
        &format!("/{forker_handle}/shared/r/{first}/f/board.kicad_pcb"),
    )
    .await;
    assert_eq!(status, 200, "an older shared revision still downloads");
}

#[tokio::test]
async fn anything_younger_than_a_day_stays_with_the_chain_under_it() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (addr, blobs, _dir) = server(db.clone()).await;
    let (mut author, _, handle) = signed_in(&db, addr).await;
    let older = board();
    let newer = board();
    push(&mut author, &[("name", "young")], &older).await;
    let parent = head(&db, &handle, "young").await;
    let path = format!("{handle}/young");
    push(
        &mut author,
        &[("design", &path), ("branch", "main")],
        &newer,
    )
    .await;
    let child = head(&db, &handle, "young").await;
    delete_design(&mut author, &handle, "young", "young").await;

    // Everything is young: the design is gone and nothing else is.
    run(&db, blobs.as_ref()).await;
    assert!(has_revision(&db, parent).await && has_revision(&db, child).await);
    assert!(blobs.exists(&BlobHash::of(&newer)).await.expect("exists"));

    // The parent is old, but a young child still names it.
    age(&db, &[parent]).await;
    run(&db, blobs.as_ref()).await;
    assert!(has_revision(&db, parent).await, "its child keeps it");
    assert!(has_blob_row(&db, &BlobHash::of(&older)).await);

    age(&db, &[child]).await;
    run(&db, blobs.as_ref()).await;
    assert!(!has_revision(&db, child).await && !has_revision(&db, parent).await);
    assert!(!blobs.exists(&BlobHash::of(&newer)).await.expect("exists"));
    assert!(!blobs.exists(&BlobHash::of(&older)).await.expect("exists"));
}

#[tokio::test]
async fn a_second_run_finds_nothing() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (addr, blobs, _dir) = server(db.clone()).await;
    let (mut author, _, handle) = signed_in(&db, addr).await;
    push(&mut author, &[("name", "once")], &board()).await;
    let revision = head(&db, &handle, "once").await;
    delete_design(&mut author, &handle, "once", "once").await;
    age(&db, &[revision]).await;
    let (abandoned, _) = sign_up(&db).await;
    let lapsed = session::start(&db, abandoned).await.expect("a session");
    expire_session(&db, lapsed).await;
    age_account(&db, abandoned).await;

    let first = run(&db, blobs.as_ref()).await;
    assert!(
        first.revisions >= 1 && first.blobs >= 1 && first.sessions >= 1 && first.sign_ups >= 1,
        "{first}"
    );
    assert_eq!(run(&db, blobs.as_ref()).await, Collected::default());
}

#[tokio::test]
async fn an_expired_session_goes_though_its_account_never_signs_in_again() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs = FsBlobStore::new(dir.path());
    let (account, _) = sign_up(&db).await;
    let handle: Handle = fresh("c").parse().expect("a handle");
    account::choose_handle(&db, account, &handle)
        .await
        .expect("choose");
    let expired = session::start(&db, account).await.expect("a session");
    let live = session::start(&db, account).await.expect("a session");
    expire_session(&db, expired).await;

    let collected = run(&db, &blobs).await;

    assert!(collected.sessions >= 1, "{collected}");
    assert!(!has_session(&db, expired).await);
    assert!(has_session(&db, live).await);
    assert!(has_account(&db, account).await);
}

#[tokio::test]
async fn a_sign_up_with_no_handle_goes_after_a_day_with_its_identities() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs = FsBlobStore::new(dir.path());
    let (abandoned, user) = sign_up(&db).await;
    let abandoned_session = session::start(&db, abandoned).await.expect("a session");
    age_account(&db, abandoned).await;
    let (young, _) = sign_up(&db).await;
    let (named, _) = sign_up(&db).await;
    let handle: Handle = fresh("c").parse().expect("a handle");
    account::choose_handle(&db, named, &handle)
        .await
        .expect("choose");
    age_account(&db, named).await;

    let collected = run(&db, &blobs).await;

    assert!(collected.sign_ups >= 1, "{collected}");
    assert!(!has_account(&db, abandoned).await);
    assert!(!has_session(&db, abandoned_session).await);
    assert!(has_account(&db, young).await, "younger than the grace");
    assert!(has_account(&db, named).await, "it chose a handle");
    // The identity went with the account, so the same person's next
    // sign-in makes a fresh one.
    let again = account::sign_in(&db, Provider::GitHub, &user)
        .await
        .expect("sign in");
    assert!(again.is_new);
    assert_ne!(again.account_id, abandoned);
}

/// A sweep of the run, planned with sequential scans priced out: on
/// tables this small the planner scans whenever it may, so a plan that
/// still scans is one with no index to read. `grace` binds the
/// statement's `$1`, for the one that has it.
#[test_case(collector::EXPIRED_SESSIONS, None, "session_expires_at" ; "expired sessions")]
#[test_case(collector::OLD_SIGN_UPS, Some(collector::GRACE_HOURS), "account_handle_key" ; "old sign-ups")]
#[tokio::test]
async fn a_sweep_reads_its_index(statement: &str, grace: Option<i32>, index: &str) {
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let mut tx = db.begin().await.expect("a transaction");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .expect("set");
    // AssertSqlSafe: `statement` is one of the crate's own constants.
    let explain = format!("EXPLAIN {statement}");
    let mut query = sqlx::query_scalar(sqlx::AssertSqlSafe(explain));
    if let Some(grace) = grace {
        query = query.bind(grace);
    }
    let lines: Vec<String> = query.fetch_all(&mut *tx).await.expect("a plan");
    let plan = lines.join("\n");
    assert!(!plan.contains("Seq Scan"), "{plan}");
    assert!(plan.contains(index), "{plan}");
}

/// The blob of an aged revision no branch reaches, as a tree entry.
fn entry(path: &str, content: &[u8]) -> TreeEntry {
    TreeEntry {
        path: path.parse().expect("a path"),
        blob: BlobHash::of(content),
        size: content.len() as u64,
    }
}

#[tokio::test]
async fn a_blob_a_thumbnail_names_stays_until_the_revision_goes() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (addr, blobs, _dir) = server(db.clone()).await;
    let (mut author, _, handle) = signed_in(&db, addr).await;
    push(&mut author, &[("name", "pictured")], &board()).await;
    let revision = head(&db, &handle, "pictured").await;
    sqlx::query("DELETE FROM thumbnail WHERE revision_hash = $1")
        .bind(revision.as_bytes())
        .execute(&db)
        .await
        .expect("no thumbnail of its own");
    let (large, small) = (fresh("large").into_bytes(), fresh("small").into_bytes());
    let large = collector::store(&db, blobs.as_ref(), &large)
        .await
        .expect("a store")
        .hash;
    let small = collector::store(&db, blobs.as_ref(), &small)
        .await
        .expect("a store")
        .hash;
    sqlx::query(
        "INSERT INTO thumbnail (revision_hash, large_hash, small_hash) VALUES ($1, $2, $3)",
    )
    .bind(revision.as_bytes())
    .bind(large.as_bytes())
    .bind(small.as_bytes())
    .execute(&db)
    .await
    .expect("a thumbnail row");
    age_blob(&db, &large).await;
    age_blob(&db, &small).await;

    run(&db, blobs.as_ref()).await;
    assert!(has_blob_row(&db, &large).await, "named by the thumbnail");
    assert!(blobs.exists(&small).await.expect("exists"));

    delete_design(&mut author, &handle, "pictured", "pictured").await;
    age(&db, &[revision]).await;
    run(&db, blobs.as_ref()).await;
    assert!(!has_blob_row(&db, &large).await, "the revision took it");
    assert!(!blobs.exists(&small).await.expect("exists"));
}

/// A tree of one fresh file, older than the grace: its hash and blob.
async fn old_tree(db: &PgPool, blobs: &dyn BlobStore) -> (Vec<u8>, BlobHash) {
    let content = fresh("held").into_bytes();
    collector::store(db, blobs, &content)
        .await
        .expect("a store");
    let entries = [entry("held.kicad_sch", &content)];
    let mut tx = db.begin().await.expect("begin");
    let tree = design::record_tree(&mut tx, &entries)
        .await
        .expect("a tree");
    tx.commit().await.expect("commit");
    sqlx::query("UPDATE tree SET claimed_at = now() - interval '25 hours' WHERE hash = $1")
        .bind(tree.as_bytes())
        .execute(db)
        .await
        .expect("aged");
    age_blob(db, &entries[0].blob).await;
    (tree.as_bytes().to_vec(), entries[0].blob)
}

#[tokio::test]
async fn a_pending_revision_keeps_its_tree_and_blobs_until_its_hour_is_up() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (addr, blobs, _dir) = server(db.clone()).await;
    let (mut author, author_id, handle) = signed_in(&db, addr).await;
    push(&mut author, &[("name", "held")], &board()).await;
    let base = head(&db, &handle, "held").await;
    let design_id: i64 = sqlx::query_scalar("SELECT id FROM design WHERE owner_id = $1")
        .bind(author_id)
        .fetch_one(&db)
        .await
        .expect("the design");
    let (tree, blob) = old_tree(&db, blobs.as_ref()).await;
    let pending: i64 = sqlx::query_scalar(
        "INSERT INTO pending_revision (design_id, branch, author_id, base_hash, tree_hash)
         VALUES ($1, 'main', $2, $3, $4) RETURNING id",
    )
    .bind(design_id)
    .bind(author_id)
    .bind(base.as_bytes())
    .bind(&tree)
    .fetch_one(&db)
    .await
    .expect("a pending revision");

    run(&db, blobs.as_ref()).await;
    assert!(has_tree(&db, &tree).await, "the pending revision names it");
    assert!(has_blob_row(&db, &blob).await);
    assert!(blobs.exists(&blob).await.expect("exists"));

    sqlx::query("UPDATE pending_revision SET expires_at = now() WHERE id = $1")
        .bind(pending)
        .execute(&db)
        .await
        .expect("its hour up");
    let collected = run(&db, blobs.as_ref()).await;
    assert!(collected.pending >= 1, "{collected}");
    assert!(!has_tree(&db, &tree).await, "a lapsed row keeps nothing");
    assert!(!has_blob_row(&db, &blob).await);
    assert!(!blobs.exists(&blob).await.expect("exists"));
}

/// A store that does as the store it wraps does, but for its `delete`,
/// which `Gate` holds or fails.
struct Gated {
    inner: Arc<FsBlobStore>,
    gate: Gate,
}

enum Gate {
    /// Deletes fail.
    Refuses,
    /// A delete says it has started and waits to be let go.
    Holds {
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
}

#[async_trait]
impl BlobStore for Gated {
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt> {
        self.inner.put(bytes).await
    }

    async fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        self.inner.get(hash).await
    }

    async fn exists(&self, hash: &BlobHash) -> Result<bool> {
        self.inner.exists(hash).await
    }

    async fn get_gzip(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        self.inner.get_gzip(hash).await
    }

    async fn delete(&self, hash: &BlobHash) -> Result<()> {
        match &self.gate {
            Gate::Refuses => Err(Error::Failed {
                reason: "the store is down".to_owned(),
            }),
            Gate::Holds { started, release } => {
                started.notify_one();
                release.notified().await;
                self.inner.delete(hash).await
            }
        }
    }
}

/// `bytes` stored and their row left with nothing naming it, past the
/// grace: what a refused upload leaves.
async fn orphan(db: &PgPool, blobs: &dyn BlobStore, bytes: &[u8]) -> BlobHash {
    let hash = collector::store(db, blobs, bytes)
        .await
        .expect("a store")
        .hash;
    age_blob(db, &hash).await;
    hash
}

#[tokio::test]
async fn a_failed_delete_leaves_the_object_for_the_next_run() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (_addr, blobs, _dir) = server(db.clone()).await;
    let bytes = fresh("orphan").into_bytes();
    let hash = orphan(&db, blobs.as_ref(), &bytes).await;

    let down = Gated {
        inner: blobs.clone(),
        gate: Gate::Refuses,
    };
    let failed = collector::run(&db, &down).await;
    assert!(failed.is_err(), "the store's failure is the run's");
    assert!(!has_blob_row(&db, &hash).await, "the row went first");
    assert!(blobs.exists(&hash).await.expect("exists"));

    let collected = run(&db, blobs.as_ref()).await;
    assert!(collected.objects >= 1, "{collected}");
    assert!(!blobs.exists(&hash).await.expect("exists"));
}

#[tokio::test]
async fn an_upload_of_the_same_bytes_before_the_run_keeps_the_blob() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (_addr, blobs, _dir) = server(db.clone()).await;
    let bytes = fresh("orphan").into_bytes();
    let hash = orphan(&db, blobs.as_ref(), &bytes).await;

    collector::store(&db, blobs.as_ref(), &bytes)
        .await
        .expect("a store");
    run(&db, blobs.as_ref()).await;
    assert!(has_blob_row(&db, &hash).await, "the claim is fresh");
    assert!(blobs.exists(&hash).await.expect("exists"));
}

#[tokio::test]
async fn an_upload_after_the_row_went_takes_the_removal_back() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (_addr, blobs, _dir) = server(db.clone()).await;
    let bytes = fresh("orphan").into_bytes();
    let hash = orphan(&db, blobs.as_ref(), &bytes).await;
    // A run that stopped after the row: the store refused the delete.
    let down = Gated {
        inner: blobs.clone(),
        gate: Gate::Refuses,
    };
    assert!(collector::run(&db, &down).await.is_err());
    assert!(!has_blob_row(&db, &hash).await);

    collector::store(&db, blobs.as_ref(), &bytes)
        .await
        .expect("a store");
    run(&db, blobs.as_ref()).await;
    assert!(has_blob_row(&db, &hash).await);
    assert!(
        blobs.exists(&hash).await.expect("exists"),
        "the upload's claim took the object out of the queue"
    );
}

#[tokio::test]
async fn an_upload_of_bytes_being_deleted_waits_and_writes_them_again() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (_addr, blobs, _dir) = server(db.clone()).await;
    let bytes = fresh("orphan").into_bytes();
    let hash = orphan(&db, blobs.as_ref(), &bytes).await;

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let held = Arc::new(Gated {
        inner: blobs.clone(),
        gate: Gate::Holds {
            started: started.clone(),
            release: release.clone(),
        },
    });
    let collecting = {
        let (db, held) = (db.clone(), held.clone());
        tokio::spawn(async move { collector::run(&db, held.as_ref()).await })
    };
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the run reaches the store's delete");

    let uploading = {
        let (db, blobs, bytes) = (db.clone(), blobs.clone(), bytes.clone());
        tokio::spawn(async move { collector::store(&db, blobs.as_ref(), &bytes).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !uploading.is_finished(),
        "the claim waits for the object's delete"
    );

    release.notify_one();
    collecting.await.expect("the task").expect("the run");
    uploading.await.expect("the task").expect("the store");
    assert!(has_blob_row(&db, &hash).await);
    assert_eq!(
        blobs.get(&hash).await.expect("get").as_deref(),
        Some(bytes.as_slice()),
        "the object the upload named is there"
    );
}

#[tokio::test]
async fn a_tree_an_upload_names_again_is_left_alone() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let (_addr, blobs, _dir) = server(db.clone()).await;
    let content = fresh("sheet").into_bytes();
    collector::store(&db, blobs.as_ref(), &content)
        .await
        .expect("a store");
    let entries = [entry("a.kicad_sch", &content)];
    let mut tx = db.begin().await.expect("begin");
    let tree = design::record_tree(&mut tx, &entries)
        .await
        .expect("a tree");
    tx.commit().await.expect("commit");
    sqlx::query("UPDATE tree SET claimed_at = now() - interval '25 hours' WHERE hash = $1")
        .bind(tree.as_bytes())
        .execute(&db)
        .await
        .expect("aged");
    age_blob(&db, &entries[0].blob).await;

    let mut tx = db.begin().await.expect("begin");
    let again = design::record_tree(&mut tx, &entries)
        .await
        .expect("the tree again");
    tx.commit().await.expect("commit");
    assert_eq!(again, tree);
    run(&db, blobs.as_ref()).await;
    assert!(has_tree(&db, tree.as_bytes()).await, "claimed afresh");
    assert!(
        has_blob_row(&db, &entries[0].blob).await,
        "a tree entry names it"
    );

    sqlx::query("UPDATE tree SET claimed_at = now() - interval '25 hours' WHERE hash = $1")
        .bind(tree.as_bytes())
        .execute(&db)
        .await
        .expect("aged");
    run(&db, blobs.as_ref()).await;
    assert!(
        !has_tree(&db, tree.as_bytes()).await,
        "no revision names it"
    );
    assert!(!has_blob_row(&db, &entries[0].blob).await);
}

/// The service account and the audience the fake's good token carries.
const ACCOUNT: &str = "kiewer-run@p.iam.gserviceaccount.com";
const AUDIENCE: &str = "https://kiewer.example";

/// The fake of Google's token endpoint: one token it vouches for.
async fn tokeninfo(Query(query): Query<HashMap<String, String>>) -> axum::response::Response {
    if query.get("id_token").map(String::as_str) == Some("good") {
        Json(serde_json::json!({
            "aud": AUDIENCE, "email": ACCOUNT, "email_verified": "true",
        }))
        .into_response()
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

/// The server's state with its internal routes behind `auth`.
fn state_behind(db: PgPool, blobs: Arc<dyn BlobStore>, auth: TaskAuth) -> AppState {
    AppState::new(
        db,
        blobs,
        Arc::new(Auth::new(Vec::new(), "https://kiewer.example").expect("an auth")),
        Arc::new(Thumbnails::new(Arc::new(NoTasks)).with_auth(auth)),
        Arc::new(NoPurge),
    )
}

#[tokio::test]
async fn the_route_runs_for_the_tasks_token_and_refuses_anyone_else() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(dir.path()));

    let (listener, endpoint) = http::bind_free_port().await;
    let router = Router::new().route("/tokeninfo", get(tokeninfo));
    tokio::spawn(async move { axum::serve(listener, router).await });
    let auth = TaskAuth::oidc_at(&format!("http://{endpoint}/tokeninfo"), ACCOUNT, AUDIENCE);
    let addr = http::serve_on_free_port(state_behind(db.clone(), blobs.clone(), auth)).await;

    let anonymous = http::request(addr, "POST", "/api/collector", &[], "").await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);
    let refused = http::request(
        addr,
        "POST",
        "/api/collector",
        &[("Authorization", "Bearer forged")],
        "",
    )
    .await;
    assert_eq!(refused.status, 401, "{}", refused.body);
    let ran = http::request(
        addr,
        "POST",
        "/api/collector",
        &[("Authorization", "Bearer good")],
        "",
    )
    .await;
    assert_eq!(ran.status, 200, "{}", ran.body);
    assert!(ran.body.starts_with("removed "), "{}", ran.body);
    assert_eq!(
        ran.header("cache-control"),
        Some("private, no-store"),
        "no cache keeps a run's answer"
    );
}

#[tokio::test]
async fn the_route_is_closed_on_a_deployment_without_the_account() {
    let dir = scratch_dir!();
    let state = state_behind(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
        TaskAuth::without_queue("https://kiewer.example"),
    );
    let addr = http::serve_on_free_port(state).await;
    let refused = http::request(
        addr,
        "POST",
        "/api/collector",
        &[("Authorization", "Bearer anything")],
        "",
    )
    .await;
    assert_eq!(refused.status, 401, "{}", refused.body);
}
