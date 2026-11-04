//! Thumbnails (ADR 0004): every revision gets two PNGs of the composed
//! front view (front copper, paste, mask and silkscreen) fit to the
//! board, 1200 by 630 for `og:image` and 400 by 400 for lists; a
//! revision with no board and a schematic gets its root sheet instead.
//! They are drawn on the server by the viewer's own renderer on lavapipe
//! (`kiewer_viewer::headless`), so what Discord unfurls is what the
//! viewer draws, and nothing user-chosen is ever an image.
//!
//! A thumbnail is a blob: each PNG goes through the blob store's one
//! write path under its own hash, outside any tree, so the uploaded
//! files' hashes stay what they are, a second render of the same pixels
//! writes nothing, and the collector's rule becomes "a blob no tree entry
//! and no thumbnail names". The `thumbnail` row keys the two blobs by
//! revision ([`of`]); pages read the row to know whether the thumbnail
//! exists ([`rendered`]), never the store, and show a placeholder until
//! it does.
//!
//! Rendering runs inside the upload request within the render budget
//! ([`BUDGET`], [`Thumbnails::after_push`]). Past the budget, or when the
//! render fails, the revision is saved anyway and a task on the Cloud
//! Tasks queue ([`TaskQueue`]) POSTs the retry path ([`retry_path`],
//! `web/thumbnail.rs`), which renders with no budget
//! ([`Thumbnails::render`]). Giving up on the budget stops only the
//! wait: the draw already on its blocking thread runs to its end with the
//! renderer's lock held, on whatever CPU Cloud Run still gives it, and
//! its PNGs are dropped unstored. The task is what makes the thumbnail
//! land. It carries an OIDC token minted as the queue's service account
//! ([`TaskAuth`]). In development and tests the queue is [`NoTasks`] and
//! the retry path is open.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::Extensions;
use google_cloud_auth::credentials::{CacheableResource, Credentials};
use kicad_core::geometry::Bounds;
use kiewer_viewer::error::Error as RenderError;
use kiewer_viewer::headless::{Frame, Headless, Mesh};
use kiewer_viewer::render::offscreen::Offscreen;
use kiewer_viewer::scene::Scene;
use kiewer_viewer::schematic;
use kiewer_viewer::view::{Side, View, ViewState};
use serde::Deserialize;
use sqlx::PgPool;

use crate::blob::{BlobHash, BlobStore, blocking};
use crate::error::{Error, Result};
use crate::revision::{RevisionHash, TreeEntry};
use crate::sql::statement;
use crate::{collector, design};

/// The render budget: how long the upload request waits for the
/// thumbnail before saving the revision without one and queueing a
/// retry. The hand-off board renders both sizes in well under a second
/// on lavapipe; a few seconds covers a board many times its size without
/// holding the uploader's browser.
pub const BUDGET: Duration = Duration::from_secs(5);

/// The two sizes every thumbnail is rendered at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Size {
    /// 1200 by 630, the Open Graph image Discord unfurls.
    Large,
    /// 400 by 400, the history and listing pages.
    Small,
}

impl Size {
    /// Both sizes, the large one first.
    pub const ALL: [Size; 2] = [Size::Large, Size::Small];

    /// Width and height in pixels.
    #[must_use]
    pub fn pixels(self) -> (u32, u32) {
        match self {
            Size::Large => (1200, 630),
            Size::Small => (400, 400),
        }
    }

    /// The file name the route serves the size at.
    #[must_use]
    pub fn file_name(self) -> &'static str {
        match self {
            Size::Large => "1200x630.png",
            Size::Small => "400x400.png",
        }
    }

    /// The size a route's file name asks for, or `None` for any other
    /// name.
    #[must_use]
    pub fn from_file_name(name: &str) -> Option<Self> {
        Size::ALL.into_iter().find(|size| size.file_name() == name)
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (width, height) = self.pixels();
        write!(f, "{width}x{height}")
    }
}

/// The route `revision`'s thumbnail of `size` is served at, under a
/// design that reaches it: a bare hash would be a capability (ADR 0002).
/// The pixels under a revision hash never change, so the response is
/// immutable.
#[must_use]
pub fn url(handle: &str, slug: &str, revision: RevisionHash, size: Size) -> String {
    format!(
        "/{handle}/{slug}/r/{revision}/thumbnail/{}",
        size.file_name()
    )
}

/// The path the retry task POSTs to render a revision's thumbnail. A
/// bare hash, unlike [`url`]: it reads nothing back, and a production
/// server takes it only with the queue's token ([`TaskAuth`]), so it
/// grants nobody a view of the revision.
#[must_use]
pub fn retry_path(revision: RevisionHash) -> String {
    format!("/api/thumbnails/{revision}")
}

/// A revision's thumbnails, once rendered: the blob of each size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thumbnail {
    pub large: BlobHash,
    pub small: BlobHash,
}

impl Thumbnail {
    /// The blob holding the PNG of `size`.
    #[must_use]
    pub fn blob(&self, size: Size) -> BlobHash {
        match size {
            Size::Large => self.large,
            Size::Small => self.small,
        }
    }
}

/// The thumbnail of `revision`, or `None` while it is not rendered or
/// may not be shown (the `SHOWABLE` fragment, `crate::sql`).
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn of(db: &PgPool, revision: RevisionHash) -> Result<Option<Thumbnail>> {
    // One read on the pool: a page's or the thumbnail route's lookup,
    // part of no write; by primary key.
    let row = statement!(query!(
        [
            "SELECT t.large_hash, t.small_hash FROM thumbnail t
         WHERE t.revision_hash = $1 AND " SHOWABLE
        ],
        revision.as_bytes()
    ))
    .fetch_optional(db)
    .await?;
    row.map(|row| {
        Ok(Thumbnail {
            large: BlobHash::from_bytes(&row.large_hash)?,
            small: BlobHash::from_bytes(&row.small_hash)?,
        })
    })
    .transpose()
}

/// Which of `revisions` have a thumbnail that may be shown (the
/// `SHOWABLE` fragment, `crate::sql`): what a listing page asks once for
/// every row it shows.
///
/// # Errors
///
/// [`Error::Db`] when Postgres refuses; [`Error::Malformed`] when a row
/// holds a hash that is not 32 bytes, which the schema forbids.
pub async fn rendered(db: &PgPool, revisions: &[RevisionHash]) -> Result<HashSet<RevisionHash>> {
    // One read on the pool: a listing's lookup, part of no write; the
    // primary key for each hash given, nothing scanned.
    let hashes: Vec<Vec<u8>> = revisions
        .iter()
        .map(|hash| hash.as_bytes().to_vec())
        .collect();
    let rows: Vec<Vec<u8>> = statement!(query_scalar!(
        [
            "SELECT t.revision_hash FROM thumbnail t
         WHERE t.revision_hash = ANY($1) AND " SHOWABLE
        ],
        &hashes
    ))
    .fetch_all(db)
    .await?;
    rows.iter()
        .map(|bytes| RevisionHash::from_bytes(bytes))
        .collect()
}

/// Where a retry goes when the render runs past the budget: the Cloud
/// Tasks queue in production ([`CloudTasks`]), nowhere in development
/// and tests ([`NoTasks`]). Object-safe, so the server holds an
/// `Arc<dyn TaskQueue>` and the environment picks the implementation.
#[async_trait]
pub trait TaskQueue: Send + Sync {
    /// Queue a render of `revision`'s thumbnail.
    ///
    /// # Errors
    ///
    /// When the queue refuses the task.
    async fn enqueue(&self, revision: RevisionHash) -> Result<()>;
}

/// No queue: the retry is left to whoever POSTs the retry path by hand,
/// said once in the log. Development and tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTasks;

#[async_trait]
impl TaskQueue for NoTasks {
    async fn enqueue(&self, revision: RevisionHash) -> Result<()> {
        tracing::warn!(
            %revision,
            path = %retry_path(revision),
            "thumbnail: no task queue in this environment; POST the path to render it"
        );
        Ok(())
    }
}

/// The Cloud Tasks queue (infra/run.tf): one HTTP task per retry, a POST
/// of the retry path under the public URL carrying an OIDC token minted
/// as the runtime service account with the public URL as its audience,
/// which is what [`TaskAuth::Oidc`] checks. The API call is one JSON POST
/// with a bearer token from the process's credentials (the metadata
/// server on Cloud Run, application-default credentials elsewhere).
pub struct CloudTasks {
    /// The queue as the API names it, `projects/…/locations/…/queues/…`.
    queue: String,
    /// The origin the task POSTs under, and its token's audience.
    public_url: String,
    /// The service account the token is minted as.
    account: String,
    credentials: Credentials,
    http: reqwest::Client,
}

impl fmt::Debug for CloudTasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudTasks")
            .field("queue", &self.queue)
            .field("public_url", &self.public_url)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl CloudTasks {
    /// A client on `queue`, whose tasks POST `public_url` as `account`.
    ///
    /// # Errors
    ///
    /// [`Error::Failed`] when the process has no credentials to call the
    /// API with, so a server without any fails at start, not at the
    /// first retry.
    pub fn new(queue: &str, public_url: &str, account: &str) -> Result<Self> {
        let credentials = google_cloud_auth::credentials::Builder::default()
            .build()
            .map_err(|source| Error::Failed {
                reason: format!("the Cloud Tasks client for {queue}: no credentials: {source}"),
            })?;
        Ok(Self {
            queue: queue.to_owned(),
            public_url: public_url.trim_end_matches('/').to_owned(),
            account: account.to_owned(),
            credentials,
            http: reqwest::Client::new(),
        })
    }

    /// The task for `revision` as the API takes it: an HTTP POST of the
    /// retry path with an OIDC token for the public URL.
    #[must_use]
    pub fn task(&self, revision: RevisionHash) -> serde_json::Value {
        serde_json::json!({
            "task": {
                "httpRequest": {
                    "httpMethod": "POST",
                    "url": format!("{}{}", self.public_url, retry_path(revision)),
                    "oidcToken": {
                        "serviceAccountEmail": self.account,
                        "audience": self.public_url,
                    },
                },
            },
        })
    }
}

#[async_trait]
impl TaskQueue for CloudTasks {
    async fn enqueue(&self, revision: RevisionHash) -> Result<()> {
        let failed = |reason: String| Error::Failed {
            reason: format!("queueing the thumbnail task for {revision}: {reason}"),
        };
        let headers = self
            .credentials
            .headers(Extensions::new())
            .await
            .map_err(|source| failed(format!("no credentials: {source}")))?;
        let CacheableResource::New { data: headers, .. } = headers else {
            return Err(failed(
                "the credentials gave no headers without an entity tag".to_owned(),
            ));
        };
        let response = self
            .http
            .post(format!(
                "https://cloudtasks.googleapis.com/v2/{}/tasks",
                self.queue
            ))
            .headers(headers)
            .json(&self.task(revision))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(failed(format!("Cloud Tasks answered {status}: {body}")));
        }
        tracing::info!(%revision, queue = %self.queue, "thumbnail: retry queued");
        Ok(())
    }
}

/// Google's endpoint that validates an ID token and answers its claims.
/// One HTTPS call per retry, which is rare; a local check against
/// Google's published keys is the later step. It answers 400 for a token
/// it refuses.
const TOKENINFO: &str = "https://oauth2.googleapis.com/tokeninfo";

/// What the server's internal routes ask of their caller, the thumbnail
/// retry and the collector: the token Cloud Tasks or Cloud Scheduler
/// mints as the runtime service account in production, nothing in
/// development and tests.
#[derive(Debug, Clone)]
pub enum TaskAuth {
    /// Anyone may POST an internal route. Development and tests, over
    /// `http`: nothing else can reach a dev box's server.
    Open,
    /// Nobody may: a deployment without a queue has no token to check,
    /// and a render or a collector run is too much to give strangers.
    Closed,
    /// A bearer token that Google's token endpoint says is an ID token
    /// for `audience` issued to `account`.
    Oidc {
        tokeninfo: String,
        account: String,
        audience: String,
        http: reqwest::Client,
    },
}

/// The claims the token endpoint answers, the three the check reads.
#[derive(Debug, Deserialize)]
struct TokenInfo {
    aud: String,
    email: Option<String>,
    /// `"true"` as text for an ID token, which is how the endpoint
    /// writes it; taken as a boolean as well.
    #[serde(default)]
    email_verified: serde_json::Value,
}

impl TokenInfo {
    /// Whether the endpoint says the email is verified, as `"true"` or
    /// `true`; anything else is not.
    fn verified(&self) -> bool {
        match &self.email_verified {
            serde_json::Value::Bool(verified) => *verified,
            serde_json::Value::String(text) => text == "true",
            _ => false,
        }
    }
}

impl TaskAuth {
    /// The internal routes of a server with no queue at `public_url`:
    /// [`TaskAuth::Closed`] over `https`, which is every deployment,
    /// else [`TaskAuth::Open`], a dev box.
    #[must_use]
    pub fn without_queue(public_url: &str) -> Self {
        if public_url.starts_with("https://") {
            TaskAuth::Closed
        } else {
            TaskAuth::Open
        }
    }

    /// The production check: a token for `audience` minted as `account`.
    #[must_use]
    pub fn oidc(account: &str, audience: &str) -> Self {
        Self::oidc_at(TOKENINFO, account, audience)
    }

    /// The same against the token endpoint at `tokeninfo`: a test's fake.
    #[must_use]
    pub fn oidc_at(tokeninfo: &str, account: &str, audience: &str) -> Self {
        TaskAuth::Oidc {
            tokeninfo: tokeninfo.to_owned(),
            account: account.to_owned(),
            audience: audience.trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// Whether a caller presenting `bearer` (the token after `Bearer `,
    /// or none) may call an internal route.
    ///
    /// # Errors
    ///
    /// [`Error::Http`] when the token endpoint cannot be reached and
    /// [`Error::Failed`] when it answers anything but a verdict: the
    /// check did not happen, which is no refusal of the token. A token
    /// it refuses is `false`.
    pub async fn allows(&self, bearer: Option<&str>) -> Result<bool> {
        match self {
            TaskAuth::Open => Ok(true),
            TaskAuth::Closed => Ok(false),
            TaskAuth::Oidc {
                tokeninfo,
                account,
                audience,
                http,
            } => {
                let Some(token) = bearer else {
                    return Ok(false);
                };
                // The token is in the URL, and a reqwest error's text
                // holds its URL, so neither error keeps it for the log.
                let response = http
                    .get(tokeninfo)
                    .query(&[("id_token", token)])
                    .send()
                    .await
                    .map_err(reqwest::Error::without_url)?;
                let status = response.status();
                if status == reqwest::StatusCode::BAD_REQUEST {
                    tracing::warn!("an internal route's token was refused");
                    return Ok(false);
                }
                if !status.is_success() {
                    return Err(Error::Failed {
                        reason: format!("the token endpoint answered {status}"),
                    });
                }
                let claims: TokenInfo =
                    response.json().await.map_err(reqwest::Error::without_url)?;
                Ok(claims.aud == *audience
                    && claims.email.as_deref() == Some(account)
                    && claims.verified())
            }
        }
    }
}

/// What a render found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Both sizes were drawn and stored.
    Rendered,
    /// The revision had its thumbnail already.
    AlreadyRendered,
    /// The revision holds neither a board nor a sheet to draw, the file
    /// has nothing on it to draw, or the file is blocked.
    NothingToRender,
    /// No revision has this hash.
    NoSuchRevision,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Outcome::Rendered => "rendered",
            Outcome::AlreadyRendered => "already rendered",
            Outcome::NothingToRender => "nothing to render",
            Outcome::NoSuchRevision => "no such revision",
        })
    }
}

/// The file a thumbnail is drawn from, as text.
enum Source {
    Board(String),
    Sheet(String),
}

/// Both PNGs of one render.
struct Pngs {
    large: Vec<u8>,
    small: Vec<u8>,
}

/// The headless renderer, made on the first render and kept: the
/// device and the pipelines are the cost, a frame is cheap. Used only
/// off the runtime's threads ([`blocking`]), behind a lock since one
/// device draws one frame at a time.
struct Renderer {
    headless: Mutex<Option<Headless>>,
}

/// The number, from 1, of the composed front view among `views`. Its
/// place moves with the copper layers before it: view 5 on a four-layer
/// board like the hand-off's, view 3 on a two-layer one.
fn front_view(views: &[View]) -> usize {
    views
        .iter()
        .position(|view| view.as_built && view.top == Side::Front)
        .map_or(1, |index| index + 1)
}

impl Renderer {
    fn new() -> Self {
        Self {
            headless: Mutex::new(None),
        }
    }

    /// Draw `source` at both sizes, after `delay` with the renderer held
    /// ([`Thumbnails::with_render_delay`]).
    fn render(&self, source: &Source, delay: Duration) -> Result<Pngs> {
        // A panic in an earlier render poisons the lock and leaves the
        // device in an unknown state: it is made again.
        let mut guard = match self.headless.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = None;
                guard
            }
        };
        if guard.is_none() {
            *guard = Some(Headless::new()?);
        }
        let headless = guard.as_mut().expect("filled just above");
        std::thread::sleep(delay);
        match source {
            Source::Board(text) => {
                let board = kicad_core::parse_board(text).map_err(RenderError::Board)?;
                let scene = Scene::build(&board).ok_or(RenderError::NothingToDraw)?;
                let state = ViewState::new(front_view(&scene.views));
                both(scene.bounds, |frame| {
                    Ok(headless.render(&scene, &state, frame)?.to_png()?)
                })
            }
            Source::Sheet(text) => {
                let sheet = kicad_core::parse_schematic(text).map_err(RenderError::Schematic)?;
                let scene =
                    schematic::build(&sheet, Offscreen::FORMAT).ok_or(RenderError::EmptySheet)?;
                let mesh = Mesh::from(&scene);
                let clear = schematic::clear_color(Offscreen::FORMAT);
                both(scene.bounds, |frame| {
                    Ok(headless
                        .render_mesh(mesh, clear, &scene.draws, frame)?
                        .to_png()?)
                })
            }
        }
    }
}

/// `bounds` fit to each size and drawn by `draw`, the large one first.
fn both(bounds: Bounds, mut draw: impl FnMut(&Frame) -> Result<Vec<u8>>) -> Result<Pngs> {
    let mut at = |size: Size| {
        let (width, height) = size.pixels();
        draw(&Frame::fit(width, height, bounds))
    };
    Ok(Pngs {
        large: at(Size::Large)?,
        small: at(Size::Small)?,
    })
}

/// The thumbnail service: the renderer, the budget, the queue a retry
/// goes to and what the retry path asks of its caller.
pub struct Thumbnails {
    budget: Duration,
    render_delay: Duration,
    tasks: Arc<dyn TaskQueue>,
    auth: TaskAuth,
    renderer: Arc<Renderer>,
}

impl fmt::Debug for Thumbnails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thumbnails")
            .field("budget", &self.budget)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl Thumbnails {
    /// The service with retries on `tasks`, the default budget and an
    /// open retry path.
    #[must_use]
    pub fn new(tasks: Arc<dyn TaskQueue>) -> Self {
        Self {
            budget: BUDGET,
            render_delay: Duration::ZERO,
            tasks,
            auth: TaskAuth::Open,
            renderer: Arc::new(Renderer::new()),
        }
    }

    /// The same with another budget.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// The same with every render held for `delay` on its blocking
    /// thread with the renderer locked: how a test makes a render slow.
    #[must_use]
    pub fn with_render_delay(mut self, delay: Duration) -> Self {
        self.render_delay = delay;
        self
    }

    /// The same with the retry path behind `auth`.
    #[must_use]
    pub fn with_auth(mut self, auth: TaskAuth) -> Self {
        self.auth = auth;
        self
    }

    /// Whether a caller of an internal route presenting `bearer` may
    /// run it ([`TaskAuth::allows`]).
    ///
    /// # Errors
    ///
    /// [`TaskAuth::allows`]'s.
    pub async fn allows_task(&self, bearer: Option<&str>) -> Result<bool> {
        self.auth.allows(bearer).await
    }

    /// The hook at the end of a successful push: render within the
    /// budget, else queue a retry, as for a render that fails, which may
    /// be a store or a device having a bad moment. Never fails the
    /// upload: whatever goes wrong is in the log and the revision is
    /// saved without a thumbnail.
    pub async fn after_push(&self, db: &PgPool, blobs: &dyn BlobStore, revision: RevisionHash) {
        let started = Instant::now();
        match tokio::time::timeout(self.budget, self.render(db, blobs, revision)).await {
            Ok(Ok(outcome)) => {
                tracing::info!(%revision, %outcome, elapsed = ?started.elapsed(), "thumbnail");
                return;
            }
            Ok(Err(error)) => {
                tracing::error!(%revision, %error, "thumbnail: the render failed; a retry is queued");
            }
            Err(_) => {
                tracing::warn!(
                    %revision,
                    budget = ?self.budget,
                    "thumbnail: past the render budget; a retry is queued"
                );
            }
        }
        if let Err(error) = self.tasks.enqueue(revision).await {
            tracing::error!(%revision, %error, "thumbnail: the retry could not be queued");
        }
    }

    /// Render `revision`'s thumbnail with no budget and store it: what
    /// the retry path does, and what [`Thumbnails::after_push`] does
    /// within the budget. Draws the revision's board (the one the design
    /// page shows), else its root sheet; a revision with neither, with
    /// a blocked file (the `SHOWABLE` fragment would hide the result), or whose file
    /// has nothing on it gets none.
    ///
    /// # Errors
    ///
    /// [`Error::Render`] when the renderer has no Vulkan driver or the
    /// file will not draw; [`Error::Failed`] when the store lacks the
    /// file's blob; the store's and Postgres's own.
    pub async fn render(
        &self,
        db: &PgPool,
        blobs: &dyn BlobStore,
        revision: RevisionHash,
    ) -> Result<Outcome> {
        if of(db, revision).await?.is_some() {
            return Ok(Outcome::AlreadyRendered);
        }
        let Some(files) = design::files_of(db, revision).await? else {
            return Ok(Outcome::NoSuchRevision);
        };
        for file in &files {
            if design::is_blocked(db, &file.blob).await? {
                tracing::warn!(%revision, path = %file.path, "thumbnail: a file is blocked; none drawn");
                return Ok(Outcome::NothingToRender);
            }
        }
        let (entry, as_source): (&TreeEntry, fn(String) -> Source) =
            match (design::board_in(&files), design::root_sheet_in(&files)) {
                (Some(board), _) => (board, Source::Board),
                (None, Some(sheet)) => (sheet, Source::Sheet),
                (None, None) => return Ok(Outcome::NothingToRender),
            };
        let Some(bytes) = blobs.get(&entry.blob).await? else {
            return Err(Error::Failed {
                reason: format!(
                    "the store has no blob {} for {} of revision {revision}",
                    entry.blob, entry.path
                ),
            });
        };
        let text = String::from_utf8(bytes).map_err(|_| Error::Failed {
            reason: format!("{} of revision {revision} is not UTF-8 text", entry.path),
        })?;
        let source = as_source(text);
        let renderer = Arc::clone(&self.renderer);
        let delay = self.render_delay;
        let pngs = match blocking(move || renderer.render(&source, delay)).await {
            Ok(pngs) => pngs,
            // A board or a sheet with nothing on it: an answer, not a
            // failure, so the placeholder stays and the queue stops.
            Err(Error::Render(RenderError::NothingToDraw | RenderError::EmptySheet)) => {
                return Ok(Outcome::NothingToRender);
            }
            Err(error) => return Err(error),
        };
        store(db, blobs, revision, &pngs).await?;
        Ok(Outcome::Rendered)
    }
}

/// The two PNGs into the store, then their rows and the thumbnail row.
async fn store(
    db: &PgPool,
    blobs: &dyn BlobStore,
    revision: RevisionHash,
    pngs: &Pngs,
) -> Result<()> {
    // The store writes are the slow part and idempotent (content
    // addressed), and each claims its blob's row, so the rows are there
    // for this one statement, which is the outcome. A thumbnail already
    // there (two retries racing) is left as it is.
    let large = collector::store(db, blobs, &pngs.large).await?.hash;
    let small = collector::store(db, blobs, &pngs.small).await?.hash;
    sqlx::query!(
        "INSERT INTO thumbnail (revision_hash, large_hash, small_hash) VALUES ($1, $2, $3)
         ON CONFLICT (revision_hash) DO NOTHING",
        revision.as_bytes(),
        large.as_bytes(),
        small.as_bytes()
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kiewer_viewer::view::Stack;
    use test_case::test_case;

    use super::*;

    #[test_case("1200x630.png", Some(Size::Large); "the Open Graph size")]
    #[test_case("400x400.png", Some(Size::Small); "the list size")]
    #[test_case("1200x630", None; "without the extension")]
    #[test_case("640x480.png", None; "a size nobody renders")]
    fn a_size_is_named_by_its_file(name: &str, size: Option<Size>) {
        assert_eq!(Size::from_file_name(name), size);
    }

    #[test_case(&["F.Cu", "B.Cu"], 3; "two copper layers")]
    #[test_case(&["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"], 5; "four, the hand-off's view 5")]
    fn the_composed_front_view_follows_the_copper_layers(copper: &[&str], view: usize) {
        let stack = Stack {
            copper: copper.iter().map(ToString::to_string).collect(),
        };
        let views = stack.views();
        assert_eq!(front_view(&views), view);
        assert_eq!(
            views[view - 1].title,
            "F.Cu + F.Paste + F.Mask + F.SilkS",
            "the hand-off's title for it"
        );
    }

    #[test_case("https://kiewer.example", true; "a deployment")]
    #[test_case("http://localhost:8090", false; "a dev box")]
    fn a_server_without_a_queue_closes_the_retry_path_over_https(public_url: &str, closed: bool) {
        let auth = TaskAuth::without_queue(public_url);
        assert_eq!(matches!(auth, TaskAuth::Closed), closed);
    }

    #[test]
    fn the_sizes_are_the_adrs() {
        assert_eq!(Size::Large.pixels(), (1200, 630));
        assert_eq!(Size::Small.pixels(), (400, 400));
        assert_eq!(Size::Large.to_string(), "1200x630");
        assert_eq!(BUDGET, Duration::from_secs(5));
    }

    #[test]
    fn the_routes_are_under_the_revision_hash() {
        let hash: RevisionHash = "ab".repeat(32).parse().unwrap();
        assert_eq!(
            url("alice", "demo", hash, Size::Small),
            format!("/alice/demo/r/{hash}/thumbnail/400x400.png")
        );
        assert_eq!(retry_path(hash), format!("/api/thumbnails/{hash}"));
    }

    #[test]
    fn a_thumbnail_names_its_blob_by_size() {
        let thumbnail = Thumbnail {
            large: BlobHash::of(b"large"),
            small: BlobHash::of(b"small"),
        };
        assert_eq!(thumbnail.blob(Size::Large), BlobHash::of(b"large"));
        assert_eq!(thumbnail.blob(Size::Small), BlobHash::of(b"small"));
    }

    #[tokio::test]
    async fn the_open_auth_allows_anyone_and_the_oidc_one_needs_a_token() {
        let open = TaskAuth::Open;
        assert!(open.allows(None).await.unwrap());
        let oidc = TaskAuth::oidc(
            "run@example.iam.gserviceaccount.com",
            "https://kiewer.example/",
        );
        // Without a token the endpoint is never asked.
        assert!(!oidc.allows(None).await.unwrap());
        let TaskAuth::Oidc { audience, .. } = &oidc else {
            panic!("oidc");
        };
        assert_eq!(audience, "https://kiewer.example", "no trailing slash");
    }

    #[test_case(r#"{"aud":"a","email_verified":"true"}"#, true; "the endpoint's text true")]
    #[test_case(r#"{"aud":"a","email_verified":true}"#, true; "a real true")]
    #[test_case(r#"{"aud":"a","email_verified":"false"}"#, false; "the endpoint's text false")]
    #[test_case(r#"{"aud":"a","email_verified":""}"#, false; "an empty string")]
    #[test_case(r#"{"aud":"a","email_verified":"yes"}"#, false; "any other text")]
    #[test_case(r#"{"aud":"a","email_verified":1}"#, false; "a number")]
    fn only_a_true_claim_counts_as_verified(json: &str, verified: bool) {
        let claims: TokenInfo = serde_json::from_str(json).unwrap();
        assert_eq!(claims.verified(), verified);
    }

    #[test]
    fn the_token_claims_read_the_endpoints_text_booleans() {
        let claims: TokenInfo = serde_json::from_str(
            r#"{"aud":"https://kiewer.example","email":"run@x","email_verified":"true","exp":"1"}"#,
        )
        .unwrap();
        assert_eq!(claims.aud, "https://kiewer.example");
        assert_eq!(claims.email.as_deref(), Some("run@x"));
        assert_eq!(
            claims.email_verified,
            serde_json::Value::String("true".to_owned())
        );
        let bare: TokenInfo = serde_json::from_str(r#"{"aud":"a"}"#).unwrap();
        assert_eq!(bare.email, None);
        assert_eq!(bare.email_verified, serde_json::Value::Null);
        assert!(
            !bare.verified(),
            "a token without the claim is not verified"
        );
    }
}
