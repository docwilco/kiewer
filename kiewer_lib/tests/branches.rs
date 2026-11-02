//! The branches page (#19) over HTTP: it lists head, ordinal, age and
//! author and offers what the viewer's role allows; each role on each
//! branch action; the default branch cannot be deleted while it is the
//! default; new-branch-here writes a branch and no revision. The Postgres
//! tests skip without `DATABASE_URL`, like `tests/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::branch;
use kiewer_lib::design::{self, Design};
use kiewer_lib::roles::{self, Role};
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    (http::serve_on_free_port(state).await, dir)
}

/// A person with a handle and a live session.
struct Person {
    account: AccountId,
    handle: Handle,
    browser: http::Browser,
}

async fn person(db: &PgPool, addr: SocketAddr) -> Person {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "brancher".to_owned(),
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
    Person {
        account: signed_in.account_id,
        handle,
        browser,
    }
}

/// One sheet pushed through the form with these text fields.
async fn push(browser: &mut http::Browser, fields: &[(&str, &str)]) -> http::Response {
    let bytes = format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s")).into_bytes();
    let mut parts: Vec<Part<'_>> = fields
        .iter()
        .map(|(field, value)| Part::text(field, value))
        .collect();
    parts.push(Part::file("demo.kicad_sch", &bytes));
    browser.post_multipart("/upload", &parts).await
}

/// Everyone a branch action can be tried by, on one design with two
/// revisions on `main`, a branch `mine` the collaborator made and a
/// branch `theirs` another collaborator made.
struct Stage {
    design: Design,
    /// `/{handle}/{slug}`.
    path: String,
    owner: Person,
    admin: Person,
    collaborator: Person,
    stranger: Person,
}

#[derive(Debug, Clone, Copy)]
enum Who {
    Owner,
    Admin,
    Collaborator,
    Stranger,
}

impl Stage {
    fn person(&mut self, who: Who) -> &mut Person {
        match who {
            Who::Owner => &mut self.owner,
            Who::Admin => &mut self.admin,
            Who::Collaborator => &mut self.collaborator,
            Who::Stranger => &mut self.stranger,
        }
    }

    /// `name`'s row in the design's list.
    async fn listed(&self, db: &PgPool, name: &str) -> Option<branch::Listed> {
        branch::list(db, self.design.id)
            .await
            .expect("a list")
            .into_iter()
            .find(|branch| branch.name.as_str() == name)
    }

    /// How many revision rows the design's author has made: new-branch
    /// must leave this alone.
    async fn revisions(&self, db: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM revision WHERE author_id = $1")
            .bind(self.owner.account)
            .fetch_one(db)
            .await
            .expect("a count")
    }

    /// The design's row as it is now.
    async fn reread(&self, db: &PgPool) -> Design {
        design::find(db, self.owner.handle.as_str(), self.design.slug.as_str())
            .await
            .expect("a read")
            .expect("the design")
            .0
    }
}

async fn stage(db: &PgPool, addr: SocketAddr) -> Stage {
    let mut owner = person(db, addr).await;
    let admin = person(db, addr).await;
    let collaborator = person(db, addr).await;
    let mut other = person(db, addr).await;
    let stranger = person(db, addr).await;
    let name = fresh("d");
    let first = push(&mut owner.browser, &[("name", &name)]).await;
    assert_eq!(first.status, 303, "{}", first.body);
    let design = design::by_owner(db, owner.account)
        .await
        .expect("a list")
        .remove(0);
    for (person, role) in [
        (&admin.handle, Role::Admin),
        (&collaborator.handle, Role::Collaborator),
        (&other.handle, Role::Collaborator),
    ] {
        roles::set_role(db, design.id, person, role).await.unwrap();
    }
    let path = format!("{}/{}", owner.handle, design.slug);
    let second = push(
        &mut owner.browser,
        &[("design", &path), ("branch", "main"), ("message", "two")],
    )
    .await;
    assert_eq!(second.status, 303, "{}", second.body);
    let head = branch::list(db, design.id).await.expect("a list")[0].head;
    let mut stage = Stage {
        design,
        path: format!("/{path}"),
        owner,
        admin,
        collaborator,
        stranger,
    };
    let create = format!("{}/branches", stage.path);
    let body = |name: &str| format!("name={name}&from={head}");
    let mine = stage
        .collaborator
        .browser
        .post_form(&create, &body("mine"))
        .await;
    assert_eq!(mine.status, 303, "{}", mine.body);
    let theirs = other.browser.post_form(&create, &body("theirs")).await;
    assert_eq!(theirs.status, 303, "{}", theirs.body);
    stage
}

/// What a person may try on a branch.
#[derive(Debug, Clone, Copy)]
enum Action {
    Rename,
    Delete,
    SetDefault,
}

impl Action {
    async fn on(self, person: &mut Person, path: &str, branch: &str) -> http::Response {
        let (verb, body) = match self {
            Action::Rename => ("rename", "name=renamed"),
            Action::Delete => ("delete", ""),
            Action::SetDefault => ("default", ""),
        };
        person
            .browser
            .post_form(&format!("{path}/b/{branch}/{verb}"), body)
            .await
    }
}

#[test_case(Who::Stranger, Action::Rename, "mine", 403; "a stranger renames nothing")]
#[test_case(Who::Stranger, Action::Delete, "mine", 403; "a stranger deletes nothing")]
#[test_case(Who::Stranger, Action::SetDefault, "mine", 403; "a stranger sets no default")]
#[test_case(Who::Collaborator, Action::Rename, "mine", 303; "a collaborator renames their own")]
#[test_case(Who::Collaborator, Action::Delete, "mine", 303; "a collaborator deletes their own")]
#[test_case(Who::Collaborator, Action::SetDefault, "mine", 403; "a collaborator sets no default")]
#[test_case(Who::Collaborator, Action::Rename, "theirs", 403; "a collaborator leaves another's name")]
#[test_case(Who::Collaborator, Action::Delete, "theirs", 403; "a collaborator leaves another's branch")]
#[test_case(Who::Collaborator, Action::Rename, "main", 403; "a collaborator leaves the default's name")]
#[test_case(Who::Collaborator, Action::Delete, "main", 409; "a collaborator meets the default refusal")]
#[test_case(Who::Admin, Action::Rename, "theirs", 303; "an admin renames any branch")]
#[test_case(Who::Admin, Action::Delete, "theirs", 303; "an admin deletes any branch")]
#[test_case(Who::Admin, Action::SetDefault, "theirs", 303; "an admin sets the default")]
#[test_case(Who::Admin, Action::Rename, "main", 303; "an admin renames the default")]
#[test_case(Who::Admin, Action::Delete, "main", 409; "an admin meets the default refusal")]
#[test_case(Who::Owner, Action::Rename, "theirs", 303; "the owner renames any branch")]
#[test_case(Who::Owner, Action::Delete, "theirs", 303; "the owner deletes any branch")]
#[test_case(Who::Owner, Action::SetDefault, "theirs", 303; "the owner sets the default")]
#[test_case(Who::Owner, Action::Rename, "main", 303; "the owner renames the default")]
#[test_case(Who::Owner, Action::Delete, "main", 409; "the owner meets the default refusal")]
#[tokio::test]
async fn each_role_on_each_branch_action(who: Who, action: Action, branch: &str, status: u16) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let path = stage.path.clone();
    let before = stage.listed(&db, branch).await.expect("the branch");
    let answer = action.on(stage.person(who), &path, branch).await;
    assert_eq!(answer.status, status, "{}", answer.body);
    let after = stage.listed(&db, branch).await;
    let design = stage.reread(&db).await;
    if status != 303 {
        assert_eq!(after, Some(before), "a refused change changes nothing");
        assert_eq!(design.default_branch, "main");
        return;
    }
    match action {
        Action::Rename => {
            assert_eq!(after, None, "the old name is gone");
            let renamed = stage.listed(&db, "renamed").await.expect("the new name");
            assert_eq!(renamed.head, before.head);
            let default = if branch == "main" { "renamed" } else { "main" };
            assert_eq!(design.default_branch, default);
        }
        Action::Delete => assert_eq!(after, None),
        Action::SetDefault => {
            assert_eq!(after, Some(before));
            assert_eq!(design.default_branch, branch);
        }
    }
}

#[tokio::test]
async fn the_page_lists_each_branch_and_offers_what_the_role_allows() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let url = format!("{}/branches", stage.path);
    let owner_handle = stage.owner.handle.to_string();
    let path = stage.path.clone();
    let main = stage.listed(&db, "main").await.expect("main");
    assert_eq!(main.ordinal, 2);
    assert_eq!(main.author, owner_handle);

    let (status, open) = http::get(addr, &url).await;
    assert_eq!(status, 200, "anyone may read it");
    for shown in ["mine", "theirs", "(default)", "rev 2", &main.head.short()] {
        assert!(open.contains(shown), "{shown}: {open}");
    }
    assert!(open.contains("just now"), "the age: {open}");
    assert!(open.contains(&owner_handle), "the author: {open}");
    assert!(!open.contains("<form"), "nothing to do signed out: {open}");

    let rename = |branch: &str| format!(r#"action="{path}/b/{branch}/rename""#);
    let delete = |branch: &str| format!(r#"action="{path}/b/{branch}/delete""#);
    let default = |branch: &str| format!(r#"action="{path}/b/{branch}/default""#);

    let page = stage.collaborator.browser.get(&url).await;
    assert!(page.body.contains(&rename("mine")), "{}", page.body);
    assert!(page.body.contains(&delete("mine")), "{}", page.body);
    assert!(!page.body.contains(&default("mine")), "{}", page.body);
    assert!(!page.body.contains(&rename("theirs")), "{}", page.body);
    assert!(!page.body.contains(&rename("main")), "{}", page.body);
    assert!(page.body.contains("New branch"), "{}", page.body);

    let page = stage.admin.browser.get(&url).await;
    assert!(page.body.contains(&rename("theirs")), "{}", page.body);
    assert!(page.body.contains(&default("theirs")), "{}", page.body);
    assert!(page.body.contains(&rename("main")), "{}", page.body);
    assert!(!page.body.contains(&delete("main")), "{}", page.body);

    let page = stage.stranger.browser.get(&url).await;
    assert!(!page.body.contains("<form"), "no role: {}", page.body);
}

#[tokio::test]
async fn a_refused_name_comes_back_as_the_page_with_the_reason() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let path = stage.path.clone();
    let taken = stage
        .owner
        .browser
        .post_form(&format!("{path}/b/mine/rename"), "name=theirs")
        .await;
    assert_eq!(taken.status, 409, "{}", taken.body);
    assert!(
        taken.body.contains("already has a branch theirs"),
        "{}",
        taken.body
    );
    let bad = stage
        .owner
        .browser
        .post_form(&format!("{path}/b/mine/rename"), "name=a%2Fb")
        .await;
    assert_eq!(bad.status, 400, "{}", bad.body);
    assert!(bad.body.contains("may use only"), "{}", bad.body);
    let default = stage
        .owner
        .browser
        .post_form(&format!("{path}/b/main/delete"), "")
        .await;
    assert_eq!(default.status, 409, "{}", default.body);
    assert!(
        default.body.contains("make another branch the default"),
        "{}",
        default.body
    );
    let missing = stage
        .owner
        .browser
        .post_form(&format!("{path}/b/nothing/delete"), "")
        .await;
    assert_eq!(missing.status, 404, "{}", missing.body);
}

#[tokio::test]
async fn the_default_branch_can_go_once_another_is_the_default() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let path = stage.path.clone();
    let moved = Action::SetDefault
        .on(&mut stage.owner, &path, "theirs")
        .await;
    assert_eq!(moved.status, 303, "{}", moved.body);
    let gone = Action::Delete.on(&mut stage.owner, &path, "main").await;
    assert_eq!(gone.status, 303, "{}", gone.body);
    assert_eq!(http::get(addr, &format!("{path}?design")).await.0, 200);
    let refused = Action::Delete.on(&mut stage.owner, &path, "theirs").await;
    assert_eq!(refused.status, 409, "the new default: {}", refused.body);
}

#[tokio::test]
async fn a_renamed_default_keeps_the_design_page_and_moves_the_branch_url() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let path = stage.path.clone();
    let renamed = Action::Rename.on(&mut stage.owner, &path, "main").await;
    assert_eq!(renamed.status, 303, "{}", renamed.body);
    assert_eq!(http::get(addr, &format!("{path}?design")).await.0, 200);
    let design_path = path.trim_start_matches('/');
    let onto = push(
        &mut stage.owner.browser,
        &[("design", design_path), ("branch", "renamed")],
    )
    .await;
    assert_eq!(onto.status, 303, "a push under the new name: {}", onto.body);
    assert_eq!(
        stage.listed(&db, "renamed").await.expect("renamed").ordinal,
        3
    );
    assert_eq!(
        http::get(addr, &format!("{path}/b/main")).await.0,
        404,
        "the old name is no URL"
    );
}

#[tokio::test]
async fn a_deleted_branch_takes_its_revisions_out_of_the_design_and_leaves_shared_ones() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let path = stage.path.clone();
    let design_path = path.trim_start_matches('/').to_owned();
    let base = stage.listed(&db, "mine").await.expect("mine").head;
    let pushed = push(
        &mut stage.collaborator.browser,
        &[("design", &design_path), ("branch", "mine")],
    )
    .await;
    assert_eq!(pushed.status, 303, "{}", pushed.body);
    let only_here = stage.listed(&db, "mine").await.expect("mine").head;
    assert_ne!(only_here, base);

    let gone = Action::Delete
        .on(&mut stage.collaborator, &path, "mine")
        .await;
    assert_eq!(gone.status, 303, "{}", gone.body);

    assert!(
        !design::reaches(&db, stage.design.id, only_here)
            .await
            .unwrap(),
        "a revision only that branch reached left the index"
    );
    assert!(
        design::reaches(&db, stage.design.id, base).await.unwrap(),
        "one another branch reaches stays"
    );
    assert_eq!(
        http::get(addr, &format!("{path}/r/{only_here}")).await.0,
        404
    );
    let row: i64 = sqlx::query_scalar("SELECT count(*) FROM revision WHERE hash = $1")
        .bind(only_here.as_bytes())
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(row, 1, "the row itself is the collector's");
}

#[tokio::test]
async fn new_branch_here_points_at_the_revision_and_writes_no_revision() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stage = stage(&db, addr).await;
    let path = stage.path.clone();
    let name: design::BranchName = "main".parse().unwrap();
    let chain = design::history(&db, stage.design.id, &name, None)
        .await
        .unwrap();
    let old = chain[1].revision.hash;

    // The off-head banner links to the page opened for that revision.
    let (_, banner) = http::get(addr, &format!("{path}/r/{old}?design")).await;
    let link = format!(r#"href="{path}/branches?from={old}""#);
    assert!(banner.contains(&link), "{banner}");
    let page = stage
        .collaborator
        .browser
        .get(&format!("{path}/branches?from={old}"))
        .await;
    assert!(
        page.body.contains(&format!("revision {}", old.short())),
        "{}",
        page.body
    );

    let revisions = stage.revisions(&db).await;
    let made = stage
        .collaborator
        .browser
        .post_form(
            &format!("{path}/branches"),
            &format!("name=here&from={old}"),
        )
        .await;
    assert_eq!(made.status, 303, "{}", made.body);
    let here = stage.listed(&db, "here").await.expect("the branch");
    assert_eq!(here.head, old);
    assert_eq!(here.ordinal, 1);
    assert_eq!(here.created_by, Some(stage.collaborator.account));
    assert_eq!(stage.revisions(&db).await, revisions, "no new revision row");

    let taken = stage
        .collaborator
        .browser
        .post_form(
            &format!("{path}/branches"),
            &format!("name=here&from={old}"),
        )
        .await;
    assert_eq!(taken.status, 409, "{}", taken.body);
    let zeros = "0".repeat(64);
    let elsewhere = stage
        .collaborator
        .browser
        .post_form(&format!("{path}/branches"), &format!("name=x&from={zeros}"))
        .await;
    assert_eq!(elsewhere.status, 404, "{}", elsewhere.body);
    let stranger = stage
        .stranger
        .browser
        .post_form(&format!("{path}/branches"), &format!("name=x&from={old}"))
        .await;
    assert_eq!(stranger.status, 403, "{}", stranger.body);
}
