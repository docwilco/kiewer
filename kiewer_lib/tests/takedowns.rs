//! Takedowns (#23): a hash in `blocked_blob` is found in the trees that
//! name it. The Postgres tests skip without `DATABASE_URL`, like
//! `tests/db.rs`.

use kiewer_lib::blob::BlobHash;
use kiewer_lib::design;
use kiewer_lib::revision::{TreeEntry, TreeHash, TreePath};
use kiewer_lib::takedown;
use kiewer_test_support::db::{fresh, pool_from_env};
use sqlx::PgPool;

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
