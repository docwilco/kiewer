//! The history tables (ADR 0002) through `kiewer_lib::design`: the
//! migration's tables and constraints, one revision written as the upload
//! writes it, a second on top by compare-and-set, and the reads a design
//! page makes. Gated on `DATABASE_URL` like `tests/db.rs`; rows persist
//! between runs on a dev box, so every test picks its own handle.

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::design::{self, DEFAULT_BRANCH, Design, Slug};
use kiewer_lib::error::Refusal;
use kiewer_lib::revision::{Metadata, RevisionDraft, RevisionHash, TreeEntry};
use kiewer_lib::{Error, Result};
use kiewer_test_support::db::{fresh, pool_from_env};
use sqlx::PgPool;

#[tokio::test]
async fn the_history_tables_exist_and_hold_32_byte_hashes_only() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name IN
           ('blob', 'blocked_blob', 'branch', 'design', 'design_revision', 'revision', 'tree', 'tree_entry')
         ORDER BY table_name",
    )
    .fetch_all(&db)
    .await
    .expect("information_schema answers");
    assert_eq!(
        tables,
        [
            "blob",
            "blocked_blob",
            "branch",
            "design",
            "design_revision",
            "revision",
            "tree",
            "tree_entry"
        ]
    );
    let short = sqlx::query("INSERT INTO blob (hash, size) VALUES ($1, 1)")
        .bind(vec![0u8; 31])
        .execute(&db)
        .await
        .expect_err("a 31-byte hash is refused");
    assert!(short.to_string().contains("check"), "{short}");
}

/// A person with a handle: the owner and author of what the test writes.
async fn person(db: &PgPool) -> (AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "designer".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("d").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    (signed_in.account_id, handle)
}

fn entry(path: &str, bytes: &[u8]) -> TreeEntry {
    TreeEntry {
        path: path.parse().expect("a path"),
        blob: BlobHash::of(bytes),
        size: bytes.len() as u64,
    }
}

fn metadata() -> Metadata {
    Metadata {
        format_version: 20_241_229,
        layer_count: 4,
        copper_layer_count: 2,
        board_size: Some((50.0, 25.5)),
        fonts: vec!["Newstroke".to_owned()],
        net_count: 7,
        footprint_count: 3,
    }
}

/// One revision of `files` with `message` on `design`'s default branch,
/// as `upload::commit` writes it: every row in one transaction, the
/// branch moved from `parent` by compare-and-set.
async fn push(
    db: &PgPool,
    author: (AccountId, &Handle),
    design: &Design,
    parent: Option<RevisionHash>,
    files: &[TreeEntry],
    message: Option<&str>,
    metadata: Option<&Metadata>,
) -> Result<RevisionHash> {
    let mut tx = db.begin().await?;
    for file in files {
        design::record_blob(&mut tx, &file.blob, file.size).await?;
    }
    let tree = design::record_tree(&mut tx, files).await?;
    let time = design::now_text(&mut tx).await?;
    let draft = RevisionDraft {
        tree,
        parent,
        author: author.1,
        time: &time,
        message,
    };
    let hash = design::record_revision(&mut tx, &draft, author.0, metadata).await?;
    design::set_head(&mut tx, design.id, &design.default_branch, parent, hash).await?;
    design::record_design_revision(&mut tx, design.id, hash).await?;
    tx.commit().await?;
    Ok(hash)
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

/// The two files every test here pushes first.
fn demo_files() -> [TreeEntry; 2] {
    [
        entry("demo.kicad_pcb", b"(kicad_pcb (version 20241229))"),
        entry("demo.kicad_sch", b"(kicad_sch (version 20250114))"),
    ]
}

#[tokio::test]
async fn a_design_gets_a_first_revision_and_the_page_reads_the_head() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "demo", "Demo board").await;
    assert_eq!(design.default_branch, DEFAULT_BRANCH);
    assert_eq!(
        design::by_owner(&db, owner).await.unwrap(),
        std::slice::from_ref(&design),
        "the owner's list has it"
    );

    let files = demo_files();
    let first = push(
        &db,
        (owner, &handle),
        &design,
        None,
        &files,
        None,
        Some(&metadata()),
    )
    .await
    .expect("the first revision");

    let shown = design::show(&db, handle.as_str(), "demo")
        .await
        .expect("read")
        .expect("the design has a head");
    assert_eq!(shown.design, design);
    assert_eq!(shown.owner, handle);
    assert_eq!(shown.branch, DEFAULT_BRANCH);
    assert_eq!(shown.revision.hash, first);
    assert_eq!(shown.revision.parent, None);
    assert_eq!(shown.revision.author, handle.as_str());
    assert_eq!(shown.revision.message, None);
    assert_eq!(shown.revision.metadata.as_ref(), Some(&metadata()));
    assert_eq!(shown.files, files, "the entries come back in path order");
    assert_eq!(
        shown.board().map(|entry| entry.path.as_str()),
        Some("demo.kicad_pcb")
    );
    assert_eq!(
        shown.schematic().map(|entry| entry.path.as_str()),
        Some("demo.kicad_sch"),
        "the sheet named like the board is the root sheet"
    );
    let at = &shown.revision.created_at;
    assert!(
        at.len() == 20 && at.as_bytes()[10] == b'T' && at.ends_with('Z'),
        "ISO 8601 UTC to the second: {at}"
    );
}

#[tokio::test]
async fn a_second_revision_goes_on_top_and_a_stale_parent_is_refused() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "demo", "Demo board").await;
    let files = demo_files();
    let first = push(&db, (owner, &handle), &design, None, &files, None, None)
        .await
        .expect("the first revision");

    // A changed board, the same schematic, a message.
    let changed = [
        entry(
            "demo.kicad_pcb",
            b"(kicad_pcb (version 20241229) (general))",
        ),
        files[1].clone(),
    ];
    let second = push(
        &db,
        (owner, &handle),
        &design,
        Some(first),
        &changed,
        Some("moved a via"),
        None,
    )
    .await
    .expect("the second revision");
    assert_ne!(second, first);
    let shown = design::show(&db, handle.as_str(), "demo")
        .await
        .unwrap()
        .expect("still a head");
    assert_eq!(shown.revision.hash, second);
    assert_eq!(shown.revision.parent, Some(first));
    assert_eq!(shown.revision.message.as_deref(), Some("moved a via"));
    assert_eq!(shown.revision.metadata, None, "no board was parsed");
    assert_eq!(shown.files.len(), 2);

    // Both revisions are the design's; a stale parent is refused.
    let listed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM design_revision WHERE design_id = $1")
            .bind(design.id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(listed, 2);
    let stale = push(
        &db,
        (owner, &handle),
        &design,
        Some(first),
        &files,
        Some("built on the old head"),
        None,
    )
    .await
    .expect_err("the head moved");
    assert!(
        matches!(
            stale,
            Error::Refused(Refusal::HeadMoved { ref branch }) if branch == DEFAULT_BRANCH
        ),
        "{stale}"
    );
    assert_eq!(
        design::show(&db, handle.as_str(), "demo")
            .await
            .unwrap()
            .unwrap()
            .revision
            .hash,
        second,
        "the refused push wrote nothing"
    );
}

#[tokio::test]
async fn a_slug_is_unique_per_owner_and_an_unknown_design_is_none() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let (other, _) = person(&db).await;
    let slug: Slug = "twice".parse().unwrap();
    let mut tx = db.begin().await.unwrap();
    design::create(&mut tx, owner, &slug, "Twice")
        .await
        .expect("the first");
    tx.commit().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    let again = design::create(&mut tx, owner, &slug, "Twice again")
        .await
        .expect_err("the owner has it");
    assert!(
        matches!(again, Error::Refused(Refusal::SlugTaken { ref slug }) if slug == "twice"),
        "{again}"
    );
    drop(tx);
    let mut tx = db.begin().await.unwrap();
    design::create(&mut tx, other, &slug, "Someone else's twice")
        .await
        .expect("another owner may use the same slug");
    assert!(
        design::find_owned(&mut tx, owner, &slug)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        design::find_owned(&mut tx, owner, &"nothing".parse().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    assert!(
        design::show(&db, handle.as_str(), "nothing")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        design::show(&db, handle.as_str(), "twice")
            .await
            .unwrap()
            .is_none(),
        "a design whose branch has no head yet shows nothing"
    );
}

#[tokio::test]
async fn an_accounts_bytes_count_each_blob_once_and_only_what_its_designs_reach() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "bytes", "Bytes").await;
    let shared = fresh("shared");
    let files = [
        entry("a.kicad_pcb", shared.as_bytes()),
        entry("b.kicad_sch", fresh("b").as_bytes()),
    ];
    push(&db, (owner, &handle), &design, None, &files, None, None)
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    let (used, added) = design::account_bytes(
        &mut tx,
        owner,
        &[files[0].blob, BlobHash::of(b"not stored")],
    )
    .await
    .unwrap();
    assert_eq!(used, files[0].size + files[1].size);
    assert_eq!(added, 0, "a blob the account reaches adds nothing");
    let fresh_bytes = fresh("new");
    design::record_blob(&mut tx, &BlobHash::of(fresh_bytes.as_bytes()), 1000)
        .await
        .unwrap();
    let (used_again, added) = design::account_bytes(
        &mut tx,
        owner,
        &[files[0].blob, BlobHash::of(fresh_bytes.as_bytes())],
    )
    .await
    .unwrap();
    assert_eq!(
        used_again, used,
        "a stored blob no design names is not the account's"
    );
    assert_eq!(added, 1000);
    drop(tx);
}

#[tokio::test]
async fn a_blocked_blob_is_known() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let hash = BlobHash::of(fresh("blocked").as_bytes());
    assert!(!design::is_blocked(&db, &hash).await.unwrap());
    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(hash.as_bytes())
        .execute(&db)
        .await
        .unwrap();
    assert!(design::is_blocked(&db, &hash).await.unwrap());
}
