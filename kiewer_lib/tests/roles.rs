//! Roles and visibility (#21): who may write what on a design, through
//! `kiewer_lib::roles` and over HTTP. Gated on `DATABASE_URL` like
//! `tests/db.rs`; rows persist between runs on a dev box, so every test
//! picks its own handles.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::Error;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::design::{self, Design};
use kiewer_lib::roles::{self, Role, Visibility};
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

/// A person with a handle.
async fn person(db: &PgPool) -> (AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "someone".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("p").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    (signed_in.account_id, handle)
}

/// A new design of `owner` at `slug`, committed.
async fn new_design(db: &PgPool, owner: AccountId, slug: &str) -> Design {
    let mut tx = db.begin().await.unwrap();
    let design = design::create(&mut tx, owner, &slug.parse().unwrap(), "A design")
        .await
        .expect("a new design");
    tx.commit().await.unwrap();
    design
}

#[tokio::test]
async fn the_owner_is_the_designs_owner_column_and_a_stranger_has_no_role() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, _) = person(&db).await;
    let (stranger, _) = person(&db).await;
    let design = new_design(&db, owner, "mine").await;
    assert_eq!(
        design.visibility,
        Visibility::Public,
        "public until changed"
    );
    assert_eq!(
        roles::role_of(&db, design.id, owner).await.unwrap(),
        Some(Role::Owner)
    );
    assert_eq!(
        roles::role_of(&db, design.id, stranger).await.unwrap(),
        None
    );
    assert_eq!(
        roles::require(&db, design.id, owner, Role::Owner)
            .await
            .unwrap(),
        Role::Owner
    );
    let refused = roles::require(&db, design.id, stranger, Role::Collaborator)
        .await
        .expect_err("a stranger may not push");
    assert!(
        matches!(
            refused,
            Error::Forbidden {
                needed: Role::Collaborator
            }
        ),
        "{refused}"
    );
    assert!(refused.to_string().contains("collaborator"), "{refused}");
}

#[test_case(Role::Collaborator, Role::Collaborator, true; "a collaborator may do a collaborator's write")]
#[test_case(Role::Collaborator, Role::Admin, false; "a collaborator may not do an admin's")]
#[test_case(Role::Collaborator, Role::Owner, false; "a collaborator may not do the owner's")]
#[test_case(Role::Admin, Role::Collaborator, true; "an admin may do a collaborator's")]
#[test_case(Role::Admin, Role::Admin, true; "an admin may do an admin's")]
#[test_case(Role::Admin, Role::Owner, false; "an admin may not do the owner's")]
#[tokio::test]
async fn a_granted_role_is_checked_against_what_a_write_needs(
    granted: Role,
    needed: Role,
    allowed: bool,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, _) = person(&db).await;
    let (member, member_handle) = person(&db).await;
    let design = new_design(&db, owner, "shared").await;
    assert_eq!(
        roles::set_role(&db, design.id, &member_handle, granted)
            .await
            .unwrap(),
        roles::Changed::Done
    );
    assert_eq!(
        roles::role_of(&db, design.id, member).await.unwrap(),
        Some(granted)
    );
    let checked = roles::require(&db, design.id, member, needed).await;
    match checked {
        Ok(role) => assert!(allowed && role == granted, "{role}"),
        Err(error) => assert!(
            !allowed && matches!(error, Error::Forbidden { needed: n } if n == needed),
            "{error}"
        ),
    }
}

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
    let (account_id, handle) = person(db).await;
    let id = session::start(db, account_id).await.expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, account_id, handle)
}

/// A design of `owner` with one revision, pushed through the upload
/// route, at `/{owner}/{slug}`.
async fn uploaded(browser: &mut http::Browser, slug: &str) {
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", slug),
                Part::file(
                    "demo.kicad_sch",
                    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s")).as_bytes(),
                ),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
}

/// A push of one sheet onto the design at `path`.
async fn push_to(browser: &mut http::Browser, path: &str) -> http::Response {
    browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", path),
                Part::text("message", "another sheet"),
                Part::file(
                    "demo.kicad_sch",
                    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s")).as_bytes(),
                ),
            ],
        )
        .await
}

#[test_case(None, 403; "a stranger may not push")]
#[test_case(Some(Role::Collaborator), 303; "a collaborator pushes")]
#[test_case(Some(Role::Admin), 303; "an admin pushes")]
#[tokio::test]
async fn a_push_to_someone_elses_design_needs_the_collaborator_role(
    granted: Option<Role>,
    status: u16,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, owner_handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "shared").await;
    let path = format!("{owner_handle}/shared");
    let design = design::by_owner(&db, owner_id).await.unwrap().remove(0);

    let (mut member, _, member_handle) = signed_in(&db, addr).await;
    if let Some(role) = granted {
        roles::set_role(&db, design.id, &member_handle, role)
            .await
            .unwrap();
    }
    let form = member.get("/upload").await;
    assert_eq!(
        form.body.contains(&format!(r#"<option value="{path}">"#)),
        granted.is_some(),
        "the form offers the designs the person may push to: {}",
        form.body
    );
    let pushed = push_to(&mut member, &path).await;
    assert_eq!(pushed.status, status, "{}", pushed.body);
    if status == 303 {
        assert_eq!(pushed.header("location"), Some(format!("/{path}").as_str()));
        let page = http::request(addr, "GET", &format!("/{path}?design"), &[], "").await;
        assert!(page.body.contains("another sheet"), "{}", page.body);
        assert!(
            page.body.contains(member_handle.as_str()),
            "the member is the author: {}",
            page.body
        );
    } else {
        assert!(pushed.body.contains("collaborator"), "{}", pushed.body);
        assert_eq!(pushed.header("cache-control"), Some("private, no-store"));
    }
    let own = push_to(&mut owner, &path).await;
    assert_eq!(own.status, 303, "the owner always pushes: {}", own.body);
    let missing = push_to(&mut owner, &format!("{owner_handle}/nothing")).await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    assert!(missing.body.contains("no design"), "{}", missing.body);
}

/// The handles with each role on `design`, as the people page lists
/// them: the owner first, then admins, then collaborators, by handle.
async fn listed(db: &PgPool, design: &Design) -> Vec<(String, Role)> {
    roles::people(db, design.id)
        .await
        .unwrap()
        .into_iter()
        .map(|person| (person.handle.to_string(), person.role))
        .collect()
}

/// `handle` given `role` on `design`, as the people page's form does it.
async fn set(db: &PgPool, design: &Design, handle: &Handle, role: Role) -> roles::Changed {
    roles::set_role(db, design.id, handle, role).await.unwrap()
}

#[tokio::test]
async fn people_are_added_changed_and_removed_and_the_owner_is_exactly_one() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, owner_handle) = person(&db).await;
    let (_, admin) = person(&db).await;
    let (_, collaborator) = person(&db).await;
    let design = new_design(&db, owner, "team").await;
    assert_eq!(
        set(&db, &design, &admin, Role::Admin).await,
        roles::Changed::Done
    );
    assert_eq!(
        set(&db, &design, &collaborator, Role::Admin).await,
        roles::Changed::Done
    );
    assert_eq!(
        set(&db, &design, &collaborator, Role::Collaborator).await,
        roles::Changed::Done,
        "a role is changed by setting it again"
    );
    assert_eq!(
        set(&db, &design, &owner_handle, Role::Admin).await,
        roles::Changed::TheOwner,
        "the owner's role is not set from the people page"
    );
    let nobody: Handle = fresh("nobody").parse().unwrap();
    assert_eq!(
        set(&db, &design, &nobody, Role::Admin).await,
        roles::Changed::NoSuchPerson
    );
    let mut expected = vec![
        (owner_handle.to_string(), Role::Owner),
        (admin.to_string(), Role::Admin),
        (collaborator.to_string(), Role::Collaborator),
    ];
    assert_eq!(listed(&db, &design).await, expected);

    assert_eq!(
        roles::remove_role(&db, design.id, &admin).await.unwrap(),
        roles::Changed::Done
    );
    assert_eq!(
        roles::remove_role(&db, design.id, &admin).await.unwrap(),
        roles::Changed::Done,
        "removing someone not on the design is already done"
    );
    assert_eq!(
        roles::remove_role(&db, design.id, &owner_handle)
            .await
            .unwrap(),
        roles::Changed::TheOwner
    );
    assert_eq!(
        roles::remove_role(&db, design.id, &nobody).await.unwrap(),
        roles::Changed::NoSuchPerson
    );
    expected.remove(1);
    assert_eq!(listed(&db, &design).await, expected);

    let owner_row = sqlx::query(
        "INSERT INTO design_role (design_id, account_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(design.id)
    .bind(owner)
    .execute(&db)
    .await
    .expect_err("the schema holds no owner row: the owner is the design's column");
    assert!(owner_row.to_string().contains("check"), "{owner_row}");
}

#[tokio::test]
async fn a_transfer_moves_the_owner_and_demotes_the_previous_one_to_admin() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, owner_handle) = person(&db).await;
    let (heir, heir_handle) = person(&db).await;
    let design = new_design(&db, owner, "estate").await;
    roles::set_role(&db, design.id, &heir_handle, Role::Collaborator)
        .await
        .unwrap();

    assert_eq!(
        roles::transfer(&db, design.id, &owner_handle)
            .await
            .unwrap(),
        roles::Changed::TheOwner,
        "a transfer to the owner moves nothing"
    );
    assert_eq!(
        roles::transfer(&db, design.id, &"nobody-at-all".parse().unwrap())
            .await
            .unwrap(),
        roles::Changed::NoSuchPerson
    );
    assert_eq!(
        roles::transfer(&db, design.id, &heir_handle).await.unwrap(),
        roles::Changed::Done
    );
    assert_eq!(
        listed(&db, &design).await,
        [
            (heir_handle.to_string(), Role::Owner),
            (owner_handle.to_string(), Role::Admin),
        ],
        "the heir owns, the previous owner is an admin, the heir's old row is gone"
    );
    assert_eq!(
        roles::role_of(&db, design.id, heir).await.unwrap(),
        Some(Role::Owner)
    );
    assert_eq!(
        roles::role_of(&db, design.id, owner).await.unwrap(),
        Some(Role::Admin)
    );
    assert!(
        design::show(&db, owner_handle.as_str(), "estate")
            .await
            .unwrap()
            .is_none()
            && design::find_owned(
                &mut db.acquire().await.unwrap(),
                heir,
                &"estate".parse().unwrap()
            )
            .await
            .unwrap()
            .is_some(),
        "the design's URL moved under the new owner's handle"
    );

    // Back the other way is refused while the previous owner has a design
    // at the slug already.
    new_design(&db, owner, "estate").await;
    assert_eq!(
        roles::transfer(&db, design.id, &owner_handle)
            .await
            .unwrap(),
        roles::Changed::SlugTaken
    );
    assert_eq!(
        listed(&db, &design).await[0],
        (heir_handle.to_string(), Role::Owner),
        "the refused transfer moved nothing"
    );
}

/// A design of `owner` with one revision, its people page's URL, and a
/// browser for `granted` on it (a stranger for `None`).
async fn people_page(
    db: &PgPool,
    addr: SocketAddr,
    granted: Option<Role>,
) -> (http::Browser, Handle, String, Design) {
    let (mut owner, owner_id, owner_handle) = signed_in(db, addr).await;
    uploaded(&mut owner, "team").await;
    let design = design::by_owner(db, owner_id).await.unwrap().remove(0);
    let url = format!("/{owner_handle}/team/people");
    let browser = match granted {
        Some(Role::Owner) => owner,
        Some(role) => {
            let (member, _, member_handle) = signed_in(db, addr).await;
            roles::set_role(db, design.id, &member_handle, role)
                .await
                .unwrap();
            member
        }
        None => {
            let (stranger, _, _) = signed_in(db, addr).await;
            stranger
        }
    };
    (browser, owner_handle, url, design)
}

#[test_case(None, 403; "a stranger")]
#[test_case(Some(Role::Collaborator), 403; "a collaborator")]
#[test_case(Some(Role::Admin), 200; "an admin")]
#[test_case(Some(Role::Owner), 200; "the owner")]
#[tokio::test]
async fn the_people_page_is_for_admins_and_the_owner(granted: Option<Role>, status: u16) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, owner_handle, url, _) = people_page(&db, addr, granted).await;
    let page = browser.get(&url).await;
    assert_eq!(page.status, status, "{}", page.body);
    assert_eq!(page.header("cache-control"), Some("private, no-store"));
    if status != 200 {
        assert!(page.body.contains("admin"), "{}", page.body);
        return;
    }
    assert!(
        page.body
            .contains(&format!(r#"<a href="/{owner_handle}">{owner_handle}</a>"#)),
        "the owner is listed: {}",
        page.body
    );
    assert!(page.body.contains("Add a person"), "{}", page.body);
    assert_eq!(
        page.body.contains("Transfer ownership") && page.body.contains("Visibility"),
        granted == Some(Role::Owner),
        "the owner's forms are the owner's: {}",
        page.body
    );
    let signed_out = http::request(addr, "GET", &url, &[], "").await;
    assert_eq!(
        signed_out.status, 303,
        "a stranger without a session goes home"
    );
}

#[test_case(None, 403; "a stranger may not")]
#[test_case(Some(Role::Collaborator), 403; "a collaborator may not")]
#[test_case(Some(Role::Admin), 303; "an admin may")]
#[test_case(Some(Role::Owner), 303; "the owner may")]
#[tokio::test]
async fn adding_changing_and_removing_a_person_needs_the_admin_role(
    granted: Option<Role>,
    status: u16,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, owner_handle, url, design) = people_page(&db, addr, granted).await;
    let (_, newcomer) = person(&db).await;
    let added = browser
        .post_form(
            &format!("{url}/role"),
            &format!("handle={newcomer}&role=collaborator"),
        )
        .await;
    assert_eq!(added.status, status, "{}", added.body);
    if status != 303 {
        assert!(
            roles::people(&db, design.id)
                .await
                .unwrap()
                .iter()
                .all(|person| person.handle != newcomer),
            "nothing was added"
        );
        return;
    }
    assert_eq!(added.header("location"), Some(url.as_str()));
    let page = browser.get(&url).await;
    assert!(
        page.body.contains(&format!(
            r#"<input type="hidden" name="handle" value="{newcomer}"><select name="role"><option value="collaborator" selected>"#
        )),
        "the newcomer is listed with their role selected: {}",
        page.body
    );
    let changed = browser
        .post_form(
            &format!("{url}/role"),
            &format!("handle={newcomer}&role=admin"),
        )
        .await;
    assert_eq!(changed.status, 303, "{}", changed.body);
    assert!(
        roles::people(&db, design.id)
            .await
            .unwrap()
            .iter()
            .any(|person| person.handle == newcomer && person.role == Role::Admin)
    );

    let owner_role = browser
        .post_form(
            &format!("{url}/role"),
            &format!("handle={owner_handle}&role=admin"),
        )
        .await;
    assert_eq!(owner_role.status, 409, "{}", owner_role.body);
    assert!(
        owner_role.body.contains("is the owner"),
        "{}",
        owner_role.body
    );
    let nobody = browser
        .post_form(&format!("{url}/role"), "handle=nobody-at-all&role=admin")
        .await;
    assert_eq!(nobody.status, 404, "{}", nobody.body);
    let bad_role = browser
        .post_form(
            &format!("{url}/role"),
            &format!("handle={newcomer}&role=owner"),
        )
        .await;
    assert_eq!(
        bad_role.status, 422,
        "the owner is not granted: {}",
        bad_role.body
    );
    let bad_handle = browser
        .post_form(&format!("{url}/role"), "handle=Not%20A%20Handle&role=admin")
        .await;
    assert_eq!(bad_handle.status, 422, "{}", bad_handle.body);

    let removed = browser
        .post_form(&format!("{url}/remove"), &format!("handle={newcomer}"))
        .await;
    assert_eq!(removed.status, 303, "{}", removed.body);
    assert!(
        roles::people(&db, design.id)
            .await
            .unwrap()
            .iter()
            .all(|person| person.handle != newcomer)
    );
    let owner_removed = browser
        .post_form(&format!("{url}/remove"), &format!("handle={owner_handle}"))
        .await;
    assert_eq!(owner_removed.status, 409, "{}", owner_removed.body);
}

#[test_case(None, 403; "a stranger may not")]
#[test_case(Some(Role::Collaborator), 403; "a collaborator may not")]
#[test_case(Some(Role::Admin), 403; "an admin may not")]
#[test_case(Some(Role::Owner), 303; "the owner may")]
#[tokio::test]
async fn visibility_and_transfer_are_the_owners(granted: Option<Role>, status: u16) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, owner_handle, url, design) = people_page(&db, addr, granted).await;
    let design_path = format!("/{owner_handle}/team");

    let unlisted = browser
        .post_form(&format!("{url}/visibility"), "visibility=unlisted")
        .await;
    assert_eq!(unlisted.status, status, "{}", unlisted.body);
    let page = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    assert_eq!(page.status, 200, "an unlisted design opens by its link");
    let expected = if status == 303 { "unlisted" } else { "public" };
    assert!(
        page.body.contains(&format!(
            r#"<p id="people">{expected} · <a href="{url}">people</a></p>"#
        )),
        "the design page says {expected} and links the people page: {}",
        page.body
    );
    if status == 303 {
        let bad = browser
            .post_form(&format!("{url}/visibility"), "visibility=secret")
            .await;
        assert_eq!(bad.status, 422, "{}", bad.body);
    }

    let (_, heir) = person(&db).await;
    let transferred = browser
        .post_form(&format!("{url}/transfer"), &format!("handle={heir}"))
        .await;
    assert_eq!(transferred.status, status, "{}", transferred.body);
    let people = listed(&db, &design).await;
    if status == 303 {
        assert_eq!(
            transferred.header("location"),
            Some(format!("/{heir}/team/people").as_str()),
            "the people page moved with the design"
        );
        assert_eq!(
            people,
            [
                (heir.to_string(), Role::Owner),
                (owner_handle.to_string(), Role::Admin)
            ]
        );
        let moved = http::request(addr, "GET", &format!("/{heir}/team?design"), &[], "").await;
        assert_eq!(moved.status, 200, "the design is at the heir's URL");
        let old = http::request(addr, "GET", &design_path, &[], "").await;
        assert_eq!(old.status, 404);
        let again = browser.get(&url).await;
        assert_eq!(
            again.status, 404,
            "the old URL's people page is gone with it"
        );
        let as_admin = browser.get(&format!("/{heir}/team/people")).await;
        assert_eq!(
            as_admin.status, 200,
            "the previous owner, now an admin, still manages people"
        );
        assert!(
            !as_admin.body.contains("Transfer ownership"),
            "{}",
            as_admin.body
        );
    } else {
        assert_eq!(people[0], (owner_handle.to_string(), Role::Owner));
    }
}

#[tokio::test]
async fn the_home_page_and_a_persons_page_list_public_designs_and_never_unlisted_ones() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "shown").await;
    uploaded(&mut owner, "hidden").await;
    let hidden = design::by_owner(&db, owner_id)
        .await
        .unwrap()
        .into_iter()
        .find(|design| design.slug.as_str() == "hidden")
        .expect("the hidden design");
    roles::set_visibility(&db, hidden.id, Visibility::Unlisted)
        .await
        .unwrap();
    // A design without a revision has no page to open, so no listing
    // names it.
    new_design(&db, owner_id, "empty").await;

    let shown = format!(r#"href="/{handle}/shown""#);
    let hidden = format!(r#"href="/{handle}/hidden""#);
    let empty = format!(r#"href="/{handle}/empty""#);
    let home = http::request(addr, "GET", "/", &[], "").await;
    assert_eq!(home.status, 200, "{}", home.body);
    assert!(home.body.contains("<title>kiewer</title>"), "{}", home.body);
    assert!(home.body.contains("Recent designs"), "{}", home.body);
    assert!(home.body.contains(&shown), "{}", home.body);
    assert!(!home.body.contains(&hidden), "unlisted: {}", home.body);
    assert!(!home.body.contains(&empty), "no revision: {}", home.body);
    assert_eq!(
        home.header("cache-control"),
        Some(kiewer_lib::cache::Policy::Page.value()),
        "the same for everyone, so cacheable: {}",
        home.body
    );

    let theirs = http::request(addr, "GET", &format!("/{handle}"), &[], "").await;
    assert_eq!(theirs.status, 200, "{}", theirs.body);
    assert!(
        theirs
            .body
            .contains(&format!("<title>{handle} - kiewer</title>")),
        "{}",
        theirs.body
    );
    assert!(theirs.body.contains(&shown), "{}", theirs.body);
    assert!(!theirs.body.contains(&hidden), "unlisted: {}", theirs.body);
    assert!(
        !theirs.body.contains(&empty),
        "no revision: {}",
        theirs.body
    );
    let own = owner.get(&format!("/{handle}")).await;
    assert!(
        !own.body.contains(&hidden),
        "not even to the owner, who finds it on the upload form: {}",
        own.body
    );

    let opened = http::request(addr, "GET", &format!("/{handle}/hidden?design"), &[], "").await;
    assert_eq!(opened.status, 200, "an unlisted design opens by its link");

    // A person with a handle and nothing uploaded has a page too.
    let (_, nobody) = person(&db).await;
    let bare = http::request(addr, "GET", &format!("/{nobody}"), &[], "").await;
    assert_eq!(bare.status, 200, "{}", bare.body);
    assert!(bare.body.contains("No public designs yet"), "{}", bare.body);
    let (status, _) = http::get(addr, "/nobody-has-this-handle").await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, "/Not%20A%20Handle").await;
    assert_eq!(status, 404);
}

#[test_case("/about", 200; "the about page")]
#[test_case("/upload", 303; "the upload form, which sends a stranger home")]
#[test_case("/settings", 303; "the settings page, the same")]
#[test_case("/viewer", 200; "the hand-off viewer")]
#[test_case("/healthz", 200; "the health route")]
#[tokio::test]
async fn static_routes_keep_priority_over_a_persons_page(path: &str, status: u16) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db).await;
    let (got, body) = http::get(addr, path).await;
    assert_eq!(got, status, "{path}: {body}");
    assert!(
        !body.contains("No public designs"),
        "{path} is not a person: {body}"
    );
}
