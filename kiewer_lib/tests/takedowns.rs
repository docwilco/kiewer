//! Takedowns (#23): a hash in `blocked_blob` is found in the trees that
//! name it, an upload holding it fails, even one that was under way when
//! the block arrived, both routes that serve it answer 410 and a brief
//! cache lifetime, the schematic page leaves it unread, and no thumbnail
//! is drawn from it. The Postgres tests skip without `DATABASE_URL`, like
//! `tests/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use kiewer_lib::Result;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::blob::{BlobHash, BlobStore, PutReceipt};
use kiewer_lib::cache::Policy;
use kiewer_lib::revision::{RevisionHash, TreeEntry, TreeHash, TreePath};
use kiewer_lib::{design, session, takedown};
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

/// A tree of the files `(path, content)` with its blob rows, the content
/// made unique so no earlier run's block applies.
async fn tree(db: &PgPool, files: &[(&str, &str)]) -> (TreeHash, Vec<TreeEntry>) {
    let entries: Vec<TreeEntry> = files
        .iter()
        .map(|(path, content)| TreeEntry {
            path: path.parse().expect("a path"),
            blob: BlobHash::of(fresh(content).as_bytes()),
            size: 1,
        })
        .collect();
    let mut tx = db.begin().await.expect("begin");
    for entry in &entries {
        design::record_blob(&mut tx, &entry.blob, entry.size)
            .await
            .expect("a blob row");
    }
    let hash = design::record_tree(&mut tx, &entries)
        .await
        .expect("a tree");
    tx.commit().await.expect("commit");
    (hash, entries)
}

async fn block(db: &PgPool, hash: &BlobHash) {
    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(hash.as_bytes())
        .execute(db)
        .await
        .expect("a block");
}

#[tokio::test]
async fn a_blocked_blob_is_found_in_its_tree_by_path() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (hash, entries) = tree(
        &db,
        &[
            ("a.kicad_sch", "a"),
            ("b/b.kicad_sch", "b"),
            ("c.kicad_pcb", "c"),
        ],
    )
    .await;
    assert_eq!(
        takedown::blocked_in(&db, hash).await.expect("a read"),
        None,
        "nothing is blocked yet"
    );

    block(&db, &entries[2].blob).await;
    block(&db, &entries[1].blob).await;
    let found = takedown::blocked_in(&db, hash).await.expect("a read");
    assert_eq!(
        found.as_ref().map(TreePath::as_str),
        Some("b/b.kicad_sch"),
        "the first blocked file by path"
    );
}

#[tokio::test]
async fn a_tree_read_runs_inside_the_writers_transaction() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (hash, entries) = tree(&db, &[("a.kicad_sch", "a")]).await;
    let mut tx = db.begin().await.expect("begin");
    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(entries[0].blob.as_bytes())
        .execute(&mut *tx)
        .await
        .expect("a block");
    let found = takedown::blocked_in(&mut *tx, hash).await.expect("a read");
    assert!(found.is_some(), "the transaction sees its own block");
    tx.rollback().await.expect("rollback");
}

const ROOT: &[u8] = include_bytes!("../../kicad_core/tests/fixtures/sheet_tree/hier.kicad_sch");
const POWER: &[u8] = include_bytes!("../../kicad_core/tests/fixtures/sheet_tree/power.kicad_sch");
const IO: &[u8] = include_bytes!("../../kicad_core/tests/fixtures/sheet_tree/sub/io.kicad_sch");

/// The server over `db` with a filesystem store under cargo's scratch
/// directory.
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
            login: "blocked".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("t").parse().expect("a handle");
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

/// A sheet whose text is its own.
fn sheet() -> Vec<u8> {
    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("sch")).into_bytes()
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

#[tokio::test]
async fn an_upload_holding_a_blocked_file_fails_and_writes_nothing() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let (clean, blocked) = (sheet(), sheet());
    block(&db, &BlobHash::of(&blocked)).await;

    let refused = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "refused"),
                Part::file("a.kicad_sch", &clean),
                Part::file("b.kicad_sch", &blocked),
            ],
        )
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(refused.body.contains("b.kicad_sch"), "{}", refused.body);
    assert!(refused.body.contains("does not accept"), "{}", refused.body);
    let (status, _) = http::get(addr, &format!("/{handle}/refused")).await;
    assert_eq!(status, 404, "no design came of it");
}

/// A store that lets the operator's block land just after it has
/// written the file, the moment past the upload's first check.
struct BlockedMidway {
    inner: FsBlobStore,
    db: PgPool,
}

#[async_trait]
impl BlobStore for BlockedMidway {
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt> {
        let receipt = self.inner.put(bytes).await?;
        block(&self.db, &receipt.hash).await;
        Ok(receipt)
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
        self.inner.delete(hash).await
    }
}

#[tokio::test]
async fn a_block_made_during_the_upload_still_stops_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs = Arc::new(BlockedMidway {
        inner: FsBlobStore::new(dir.path()),
        db: db.clone(),
    });
    let addr = http::serve_on_free_port(http::state(db.clone(), blobs)).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let refused = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "midway"),
                Part::file("a.kicad_sch", &sheet()),
            ],
        )
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(refused.body.contains("a.kicad_sch"), "{}", refused.body);
    let (status, _) = http::get(addr, &format!("/{handle}/midway")).await;
    assert_eq!(status, 404, "the transaction rolled back");
}

#[test_case("/blob/{hash}"; "the blob route")]
#[test_case("/{handle}/gone/r/{revision}/f/gone.kicad_sch"; "the file route")]
#[tokio::test]
async fn a_route_serving_a_blocked_blob_answers_410_and_a_short_lifetime(template: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let bytes = sheet();
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "gone"),
                Part::file("gone.kicad_sch", &bytes),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let revision = head(&db, &handle, "gone").await;
    let hash = BlobHash::of(&bytes);
    let route = template
        .replace("{hash}", &hash.to_string())
        .replace("{handle}", handle.as_str())
        .replace("{revision}", &revision.to_string());
    let (status, _) = http::get(addr, &route).await;
    assert_eq!(status, 200, "it serves the file first");

    block(&db, &hash).await;
    let gone = http::request(addr, "GET", &route, &[], "").await;
    assert_eq!(gone.status, 410, "{}", gone.body);
    assert_eq!(gone.header("cache-control"), Some(Policy::NotFound.value()));
    assert!(gone.body.contains("taken down"), "{}", gone.body);
}

#[tokio::test]
async fn a_blob_nobody_stored_stays_a_404() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db).await;
    let (status, _) = http::get(addr, &format!("/blob/{}", BlobHash::of(b"never"))).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_block_on_a_hash_nobody_stored_answers_410_too() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let hash = BlobHash::of(fresh("never").as_bytes());
    block(&db, &hash).await;
    let (status, _) = http::get(addr, &format!("/blob/{hash}")).await;
    assert_eq!(status, 410, "a block outlives the blob");
}

#[tokio::test]
async fn no_thumbnail_is_drawn_from_a_blocked_file() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let bytes = sheet();
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "undrawn"),
                Part::file("undrawn.kicad_sch", &bytes),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let revision = head(&db, &handle, "undrawn").await;
    sqlx::query("DELETE FROM thumbnail WHERE revision_hash = $1")
        .bind(revision.as_bytes())
        .execute(&db)
        .await
        .expect("no thumbnail yet");
    block(&db, &BlobHash::of(&bytes)).await;

    let retried = http::request(
        addr,
        "POST",
        &format!("/api/thumbnails/{revision}"),
        &[],
        "",
    )
    .await;
    assert_eq!(retried.status, 200, "{}", retried.body);
    assert_eq!(retried.body, "nothing to render\n");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM thumbnail WHERE revision_hash = $1")
        .bind(revision.as_bytes())
        .fetch_one(&db)
        .await
        .expect("a read");
    assert_eq!(rows, 0, "none was drawn");
}

#[tokio::test]
async fn the_schematic_page_leaves_a_blocked_sheet_out_of_its_tree() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::file("hier.kicad_sch", ROOT),
                Part::file("power.kicad_sch", POWER),
                Part::file("sub/io.kicad_sch", IO),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let power = BlobHash::of(POWER);
    let path = format!("/{handle}/hier/schematic");
    let page = http::request(addr, "GET", &path, &[], "").await;
    assert!(
        page.body.contains(&format!("/blob/{power}")),
        "{}",
        page.body
    );

    // A block of a shared fixture would stop the other tests' uploads of
    // it, so it lasts as long as this one look.
    block(&db, &power).await;
    let page = http::request(addr, "GET", &path, &[], "").await;
    sqlx::query("DELETE FROM blocked_blob WHERE hash = $1")
        .bind(power.as_bytes())
        .execute(&db)
        .await
        .expect("the block lifted");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        !page.body.contains(&format!("/blob/{power}")),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(&format!("/blob/{}", BlobHash::of(ROOT))),
        "the rest of the tree stands: {}",
        page.body
    );
}
