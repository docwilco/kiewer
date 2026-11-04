//! The history tables (ADR 0002) through `kiewer_lib::design`: the
//! migration's tables and constraints, one revision written as the upload
//! writes it, a second on top by compare-and-set, and the reads a design
//! page makes. Gated on `DATABASE_URL` like `tests/it/db.rs`; rows persist
//! between runs on a dev box, so every test picks its own handle.

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::design::{self, At, BranchName, DEFAULT_BRANCH, Design, Slug};
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

/// One revision of `files` with `message` and `metadata` on `design`'s
/// default branch, as `upload::push` writes it: every row in one
/// transaction, the branch moved from `parent` by compare-and-set.
async fn push(
    db: &PgPool,
    author: (AccountId, &Handle),
    design: &Design,
    parent: Option<RevisionHash>,
    files: &[TreeEntry],
    message: Option<&str>,
    metadata: Option<&Metadata>,
) -> Result<RevisionHash> {
    write(
        db,
        author,
        Push {
            design,
            branch: &design.default_branch,
            parent,
            expected: parent,
            files,
            message,
            metadata,
        },
    )
    .await
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
    assert_eq!(shown.branch.as_str(), DEFAULT_BRANCH);
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

/// One push as `upload::push` writes it: the revision of `files` on
/// `branch` of `design`, built on `parent`, the head moved from
/// `expected` by compare-and-set (`None` makes the branch).
struct Push<'a> {
    design: &'a Design,
    branch: &'a str,
    parent: Option<RevisionHash>,
    expected: Option<RevisionHash>,
    files: &'a [TreeEntry],
    message: Option<&'a str>,
    metadata: Option<&'a Metadata>,
}

/// `push` written in one transaction.
async fn write(db: &PgPool, author: (AccountId, &Handle), push: Push<'_>) -> Result<RevisionHash> {
    let mut tx = db.begin().await?;
    for file in push.files {
        design::record_blob(&mut tx, &file.blob, file.size).await?;
    }
    let tree = design::record_tree(&mut tx, push.files).await?;
    let time = design::now_text(&mut tx).await?;
    let draft = RevisionDraft {
        tree,
        parent: push.parent,
        author: author.1,
        time: &time,
        message: push.message,
    };
    let hash = design::record_revision(&mut tx, &draft, author.0, push.metadata).await?;
    design::set_head(
        &mut tx,
        push.design.id,
        push.branch,
        push.expected,
        hash,
        author.0,
    )
    .await?;
    design::record_design_revision(&mut tx, push.design.id, hash).await?;
    tx.commit().await?;
    Ok(hash)
}

/// One revision of `files` on `branch`, built on `parent`, which is also
/// the head the compare-and-set expects (`None` makes the branch).
async fn push_to(
    db: &PgPool,
    author: (AccountId, &Handle),
    design: &Design,
    branch: &str,
    parent: Option<RevisionHash>,
    files: &[TreeEntry],
    message: Option<&str>,
) -> Result<RevisionHash> {
    write(
        db,
        author,
        Push {
            design,
            branch,
            parent,
            expected: parent,
            files,
            message,
            metadata: None,
        },
    )
    .await
}

/// `/r/{hash}` with the branch the link came from.
fn revision_at(hash: RevisionHash, branch: Option<&BranchName>) -> At<'_> {
    At::Revision { hash, branch }
}

/// `n` revisions on `main`, in order, each of one board of its own with
/// the message `rev {ordinal}`.
async fn chain(
    db: &PgPool,
    author: (AccountId, &Handle),
    design: &Design,
    n: u8,
) -> Vec<RevisionHash> {
    let mut hashes = Vec::new();
    let mut parent = None;
    for i in 0..n {
        let board = entry("demo.kicad_pcb", &[b'(', b'a' + i, b')']);
        let message = format!("rev {}", i + 1);
        let hash = push_to(
            db,
            author,
            design,
            DEFAULT_BRANCH,
            parent,
            std::slice::from_ref(&board),
            Some(&message),
        )
        .await
        .expect("a revision");
        hashes.push(hash);
        parent = Some(hash);
    }
    hashes
}

#[tokio::test]
async fn the_recent_list_holds_the_last_ten_and_a_hinted_branch_is_the_context() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "twelve", "Twelve").await;
    let author = (owner, &handle);
    let hashes = chain(&db, author, &design, 12).await;
    let head = design::show(&db, handle.as_str(), "twelve")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.ordinal, 12);
    let ordinals: Vec<u32> = head.recent.iter().map(|entry| entry.ordinal).collect();
    assert_eq!(
        ordinals,
        (3..=12).rev().collect::<Vec<u32>>(),
        "the last ten"
    );
    assert_eq!(head.recent[0].revision.hash, hashes[11]);
    let first = design::show_at(
        &db,
        handle.as_str(),
        "twelve",
        At::Revision {
            hash: hashes[0],
            branch: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!((first.ordinal, first.behind), (1, 11));
    assert!(
        !first.recent.iter().any(|entry| entry.ordinal == 1),
        "the revision shown is older than the last ten"
    );

    // A branch at the third revision, with one on top; the third named
    // by hash is main's unless the link said which branch it came from.
    let side: BranchName = "side".parse().unwrap();
    let on_side = write(
        &db,
        author,
        Push {
            design: &design,
            branch: side.as_str(),
            parent: Some(hashes[2]),
            expected: None,
            files: &[entry("demo.kicad_pcb", b"(side)")],
            message: None,
            metadata: None,
        },
    )
    .await
    .unwrap();
    let at = |branch| revision_at(hashes[2], branch);
    let unhinted = design::show_at(&db, handle.as_str(), "twelve", at(None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unhinted.branch.as_str(), DEFAULT_BRANCH);
    assert_eq!((unhinted.ordinal, unhinted.behind), (3, 9));
    let hinted = design::show_at(&db, handle.as_str(), "twelve", at(Some(&side)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hinted.branch, side);
    assert_eq!((hinted.ordinal, hinted.behind), (3, 1));
    assert_eq!(hinted.head, on_side);
    let elsewhere: BranchName = "elsewhere".parse().unwrap();
    let no_such = design::show_at(&db, handle.as_str(), "twelve", at(Some(&elsewhere)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        no_such.branch.as_str(),
        DEFAULT_BRANCH,
        "a hint naming a branch that does not reach the revision is ignored"
    );
}

/// Three files, each a revision of its own: a chain of three.
fn three_boards() -> [TreeEntry; 3] {
    [
        entry("demo.kicad_pcb", b"(kicad_pcb (version 20241229) (a))"),
        entry("demo.kicad_pcb", b"(kicad_pcb (version 20241229) (b))"),
        entry("demo.kicad_pcb", b"(kicad_pcb (version 20241229) (c))"),
    ]
}

#[tokio::test]
async fn a_chain_of_three_has_ordinals_along_the_branch_and_a_history_newest_first() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "chain", "Chain").await;
    let author = (owner, &handle);
    let main: BranchName = DEFAULT_BRANCH.parse().unwrap();
    let hashes = chain(&db, author, &design, 3).await;
    let [first, second, third] = [hashes[0], hashes[1], hashes[2]];

    let history = design::history(&db, design.id, &main, None)
        .await
        .expect("read");
    let listed: Vec<(u32, RevisionHash, Option<&str>)> = history
        .iter()
        .map(|entry| {
            (
                entry.ordinal,
                entry.revision.hash,
                entry.revision.message.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (3, third, Some("rev 3")),
            (2, second, Some("rev 2")),
            (1, first, Some("rev 1"))
        ],
        "newest first, ordinals counted from the root"
    );
    let last_two = design::history(&db, design.id, &main, Some(2))
        .await
        .unwrap();
    assert_eq!(last_two.len(), 2);
    assert_eq!(last_two[0].ordinal, 3, "a limit keeps the ordinals");
    assert!(
        design::history(&db, design.id, &"nothing".parse().unwrap(), None)
            .await
            .unwrap()
            .is_empty(),
        "a branch that does not exist has no history"
    );

    // The head by the branch's name, and by the design's default.
    let at_branch = design::show_at(&db, handle.as_str(), "chain", At::Branch(&main))
        .await
        .unwrap()
        .expect("the branch");
    assert_eq!(at_branch.revision.hash, third);
    assert_eq!((at_branch.ordinal, at_branch.behind), (3, 0));
    assert_eq!(at_branch.branch.as_str(), DEFAULT_BRANCH);
    assert_eq!(at_branch.head, third);
    assert_eq!(
        at_branch
            .branches
            .iter()
            .map(|branch| branch.name.as_str())
            .collect::<Vec<_>>(),
        [DEFAULT_BRANCH]
    );
    assert_eq!(
        at_branch
            .recent
            .iter()
            .map(|entry| entry.ordinal)
            .collect::<Vec<_>>(),
        [3, 2, 1],
        "the last ten along the branch, newest first"
    );
    let by_default = design::show(&db, handle.as_str(), "chain")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_default.revision.hash, third);
    assert_eq!(by_default.behind, 0);

    // An older revision by hash: its ordinal and its distance behind.
    let old = design::show_at(
        &db,
        handle.as_str(),
        "chain",
        At::Revision {
            hash: first,
            branch: None,
        },
    )
    .await
    .unwrap()
    .expect("the design reaches it");
    assert_eq!(old.revision.hash, first);
    assert_eq!((old.ordinal, old.behind), (1, 2));
    assert_eq!(old.branch.as_str(), DEFAULT_BRANCH);
    assert_eq!(old.head, third, "the head of the branch in context");
    assert_eq!(old.files.len(), 1);
}

#[tokio::test]
async fn a_revision_the_design_does_not_reach_and_a_branch_it_lacks_are_not_shown() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "chain", "Chain").await;
    let boards = three_boards();
    push_to(
        &db,
        (owner, &handle),
        &design,
        DEFAULT_BRANCH,
        None,
        &boards[1..2],
        None,
    )
    .await
    .unwrap();

    // Another design's revision, and nobody's.
    let (other_owner, other_handle) = person(&db).await;
    let other = new_design(&db, other_owner, "chain", "Another chain").await;
    let elsewhere = push_to(
        &db,
        (other_owner, &other_handle),
        &other,
        DEFAULT_BRANCH,
        None,
        &boards[..1],
        Some("someone else's"),
    )
    .await
    .unwrap();
    assert!(
        design::show_at(
            &db,
            handle.as_str(),
            "chain",
            At::Revision {
                hash: elsewhere,
                branch: None
            }
        )
        .await
        .unwrap()
        .is_none(),
        "another design's revision is not this design's"
    );
    assert!(
        design::show_at(
            &db,
            handle.as_str(),
            "chain",
            At::Revision {
                hash: RevisionHash::from_bytes(&[7u8; 32]).unwrap(),
                branch: None
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        design::show_at(
            &db,
            handle.as_str(),
            "chain",
            At::Branch(&"nothing".parse().unwrap())
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn a_new_branch_starts_at_a_revision_and_a_taken_name_is_refused() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "fork", "Fork").await;
    let author = (owner, &handle);
    let experiment: BranchName = "experiment".parse().unwrap();
    let boards = three_boards();
    let hashes = chain(&db, author, &design, 2).await;
    let (first, second) = (hashes[0], hashes[1]);

    // A branch at the first revision, with a third on top of it: a chain
    // of two along the new branch, which shares its root with main.
    let third = write(
        &db,
        author,
        Push {
            design: &design,
            branch: "experiment",
            parent: Some(first),
            expected: None,
            files: &boards[2..],
            message: Some("on a branch"),
            metadata: None,
        },
    )
    .await
    .expect("a new branch");

    let on_branch = design::show_at(&db, handle.as_str(), "fork", At::Branch(&experiment))
        .await
        .unwrap()
        .expect("the branch");
    assert_eq!(on_branch.revision.hash, third);
    assert_eq!((on_branch.ordinal, on_branch.behind), (2, 0));
    assert_eq!(
        on_branch
            .branches
            .iter()
            .map(|branch| (branch.name.as_str(), branch.head))
            .collect::<Vec<_>>(),
        [("experiment", third), (DEFAULT_BRANCH, second)],
        "every branch with its head, by name"
    );
    let shared_root = design::show_at(
        &db,
        handle.as_str(),
        "fork",
        At::Revision {
            hash: first,
            branch: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        shared_root.branch.as_str(),
        DEFAULT_BRANCH,
        "a revision on several branches is shown in the default branch's context"
    );
    assert_eq!((shared_root.ordinal, shared_root.behind), (1, 1));
    let history = design::history(&db, design.id, &experiment, None)
        .await
        .unwrap();
    assert_eq!(
        history
            .iter()
            .map(|entry| (entry.ordinal, entry.revision.hash))
            .collect::<Vec<_>>(),
        [(2, third), (1, first)]
    );

    // The name is taken: the compare-and-set that makes a branch fails.
    let mut tx = db.begin().await.unwrap();
    let taken = design::set_head(&mut tx, design.id, "experiment", None, second, owner)
        .await
        .expect_err("the branch exists");
    assert!(
        matches!(taken, Error::Refused(Refusal::BranchTaken { ref branch }) if branch == "experiment"),
        "{taken}"
    );
    drop(tx);
    assert!(design::reaches(&db, design.id, third).await.unwrap());
    assert!(
        !design::reaches(
            &db,
            design.id,
            RevisionHash::from_bytes(&[9u8; 32]).unwrap()
        )
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn a_push_whose_head_moved_under_it_is_refused_at_the_compare_and_set() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, handle) = person(&db).await;
    let design = new_design(&db, owner, "race", "Race").await;
    let author = (owner, &handle);
    let boards = three_boards();
    let first = push_to(
        &db,
        author,
        &design,
        DEFAULT_BRANCH,
        None,
        &boards[..1],
        None,
    )
    .await
    .unwrap();

    // The slow upload reads the head, then a quick one lands on it.
    let mut slow = db.begin().await.unwrap();
    let seen = design::head(&mut slow, design.id, DEFAULT_BRANCH)
        .await
        .unwrap();
    assert_eq!(seen, Some(first));
    let quick = push_to(
        &db,
        author,
        &design,
        DEFAULT_BRANCH,
        Some(first),
        &boards[1..2],
        Some("quick"),
    )
    .await
    .expect("the quick push lands");

    design::record_blob(&mut slow, &boards[2].blob, boards[2].size)
        .await
        .unwrap();
    let tree = design::record_tree(&mut slow, &boards[2..]).await.unwrap();
    let time = design::now_text(&mut slow).await.unwrap();
    let draft = RevisionDraft {
        tree,
        parent: seen,
        author: &handle,
        time: &time,
        message: Some("slow"),
    };
    let stale = design::record_revision(&mut slow, &draft, owner, None)
        .await
        .unwrap();
    let moved = design::set_head(&mut slow, design.id, DEFAULT_BRANCH, seen, stale, owner)
        .await
        .expect_err("the head moved");
    assert!(
        matches!(moved, Error::Refused(Refusal::HeadMoved { ref branch }) if branch == DEFAULT_BRANCH),
        "{moved}"
    );
    drop(slow);
    let shown = design::show(&db, handle.as_str(), "race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown.revision.hash, quick, "the quick push is the head");
    assert_eq!(shown.ordinal, 2);
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
