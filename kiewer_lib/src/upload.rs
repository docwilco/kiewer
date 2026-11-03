//! An upload: one or more KiCad files become a revision of a design
//! (docs/v1-scope.md, Uploads). The parts of the multipart body are
//! streamed into temporary files as they arrive ([`Incoming`]), counted
//! against the per-file and per-revision caps; then [`push`] roots the
//! files at the project folder, parses each one under the parse timeout,
//! stores its bytes as a blob, and writes the tree, revision, branch and
//! `design_revision` rows in one transaction. Nothing slow happens inside
//! that transaction: the parsing and the blob writes are done before it
//! opens, and a blob a refused upload leaves behind is the collector's,
//! which is why a blob goes in through `collector::store`.
//!
//! The head an upload is built on is the one its form saw when that is
//! on the branch, or else the one [`push`] reads before the slow part;
//! the transaction holds the branch's row and compares. A head that moved in between makes no
//! revision: the upload is held for its uploader to choose
//! (`crate::pending`), since it would otherwise land on files they never
//! saw.
//!
//! What is kept: `.kicad_pcb`, `.kicad_sch` and `.kicad_pro`; anything
//! else is dropped with a line in the log, and so is what the skip list
//! names ([`skipped`]: `-backups` folders, lock files, autosaves and save
//! files, dot-directories, `__MACOSX`). A zip uploaded by itself is
//! received whole and expanded by `upload/archive.rs` under the same
//! rules, its central directory read first; a zip inside a folder is
//! another file that is not kept. A board is parsed with `kicad_core` for
//! its metadata; a schematic is parsed with it too, so a sheet KiCad 9
//! cannot read is refused naming its version; a project file is checked
//! to be JSON. All three are stored as they are.
//!
//! Rooting: the tree is rooted at the project folder (GLOSSARY) with the
//! paths under it kept, so an Explorer zip's wrapper folder goes and the
//! path of a sheet in a folder stays. More than one project in the
//! upload: the shallowest is taken and [`Pushed::warnings`] names the
//! others, for the page to show. The hourly allowance on uploads is
//! `crate::rate_limit`, checked before the rows are written.

mod archive;

use std::path::PathBuf;
use std::time::Duration;

use derive_more::Display;
use sqlx::{PgConnection, PgPool};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

use crate::account::{AccountId, Handle};
use crate::blob::{BlobHash, BlobStore};
use crate::design::{self, BranchName, Design, DesignId, Slug};
use crate::error::{Error, Refusal, Result};
use crate::pending::{self, Held, PendingId};
use crate::revision::{Metadata, RevisionDraft, RevisionHash, TreeEntry, TreeHash, TreePath};
use crate::roles::{self, Role};
use crate::{collector, rate_limit, takedown};

/// The caps an upload is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most one file may hold, uncompressed.
    pub file_bytes: u64,
    /// The most the files of one revision may hold together.
    pub revision_bytes: u64,
    /// The most an account's designs may hold, every distinct blob
    /// counted once.
    pub account_bytes: u64,
    /// How long parsing the revision's files may take in all.
    pub parse_timeout: Duration,
    /// The most entries a zip may hold, files and folders alike, counted
    /// from its bytes before the zip is opened.
    pub zip_entries: usize,
    /// The most uploads one account may make in an hour, refused ones
    /// included (`crate::rate_limit`).
    pub uploads_per_hour: u32,
}

const MB: u64 = 1024 * 1024;

impl Limits {
    /// The caps of docs/v1-scope.md: 50 MB per file, 200 MB per revision,
    /// 2 GB per account, 30 seconds of parsing, 30 revisions an hour; and
    /// 10 000 zip entries, the bound on what opening a zip holds in
    /// memory, which is this code's own number, not the scope's.
    pub const DEFAULT: Limits = Limits {
        file_bytes: 50 * MB,
        revision_bytes: 200 * MB,
        account_bytes: 2048 * MB,
        parse_timeout: Duration::from_secs(30),
        zip_entries: 10_000,
        uploads_per_hour: 30,
    };

    /// The per-file cap in whole megabytes, as the form and a refusal
    /// say it.
    #[must_use]
    pub fn file_mb(&self) -> u64 {
        self.file_bytes / MB
    }

    /// The per-revision cap in whole megabytes.
    #[must_use]
    pub fn revision_mb(&self) -> u64 {
        self.revision_bytes / MB
    }

    /// The per-account cap in whole gigabytes.
    #[must_use]
    pub fn account_gb(&self) -> u64 {
        self.account_bytes / (1024 * MB)
    }
}

/// How large the upload route lets a request body be: the revision cap
/// with room for the multipart framing and the text fields. The parts are
/// read as they arrive, so the body is refused where the read passes it,
/// with what came before already received, and the refusal is
/// `Refusal::UploadTooLarge`. Which of this limit and the revision cap
/// refuses first depends on the order of the body: once the dropped files
/// and the framing before a byte pass 1 MiB, this limit is met before the
/// kept files reach the cap, even when it is the kept files that are too
/// large.
#[expect(
    clippy::cast_possible_truncation,
    reason = "201 MiB fits a usize on every target the server builds for (64-bit)"
)]
pub const BODY_LIMIT: usize = (Limits::DEFAULT.revision_bytes + MB) as usize;

/// The three kinds of file an upload keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    /// `.kicad_pcb`: parsed for the viewer and the metadata.
    Board,
    /// `.kicad_sch`: parsed for its shape and version, then stored.
    Schematic,
    /// `.kicad_pro`: stored, the project's settings.
    Project,
}

impl FileKind {
    /// Every kind, in the order the project folder is looked for: the
    /// project file names it, else the board, else a schematic.
    pub const ALL: [FileKind; 3] = [FileKind::Project, FileKind::Board, FileKind::Schematic];

    /// The kind a path's extension names, or `None` for a file the upload
    /// drops.
    #[must_use]
    pub fn of(path: &TreePath) -> Option<Self> {
        match path.extension()?.as_str() {
            "kicad_pcb" => Some(FileKind::Board),
            "kicad_sch" => Some(FileKind::Schematic),
            "kicad_pro" => Some(FileKind::Project),
            _ => None,
        }
    }
}

/// One file of the upload, in its temporary file.
#[derive(Debug)]
struct Received {
    path: TreePath,
    kind: FileKind,
    temp: PathBuf,
    size: u64,
}

/// What the open part is.
#[derive(Debug)]
enum Writing {
    /// A file the upload keeps as it is.
    File(Received),
    /// A zip, expanded into files when it ends. Its own bytes are bounded
    /// by the per-revision cap and count towards nothing else: the files
    /// it yields are the ones the caps measure.
    Archive {
        name: String,
        temp: PathBuf,
        size: u64,
    },
}

/// A part being written.
#[derive(Debug)]
struct Open {
    writing: Writing,
    file: tokio::fs::File,
}

/// The files of one upload as they arrive: each into its own temporary
/// file under a directory of its own, removed with it. The caps are
/// checked as the bytes come, so an oversize upload is refused at the
/// first byte over, not after the transfer.
#[derive(Debug)]
pub struct Incoming {
    dir: tempfile::TempDir,
    files: Vec<Received>,
    open: Option<Open>,
    total: u64,
    /// The number of the next temporary file: one for each part and each
    /// file a zip yields.
    parts: usize,
    limits: Limits,
}

/// Why the upload skips a path (docs/v1-scope.md, Uploads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display)]
pub enum Skipped {
    /// Under a folder whose name ends in `-backups`.
    #[display("in a -backups folder")]
    Backups,
    /// Under a folder whose name starts with a dot.
    #[display("in a dot-directory")]
    DotDirectory,
    /// Under a folder named `__MACOSX`.
    #[display("in the __MACOSX folder")]
    MacOs,
    /// A file named `*.lck`.
    #[display("a lock file")]
    Lock,
    /// A file named `_autosave-*`.
    #[display("an autosave")]
    Autosave,
    /// A file named `*-save.kicad_*`.
    #[display("a save file")]
    Save,
}

/// Why `path` is one the upload skips, or `None` for one it looks at. The
/// page's script filters by the same rules to spare the transfer; this is
/// the one that decides.
#[must_use]
pub fn skipped(path: &TreePath) -> Option<Skipped> {
    let mut segments = path.as_str().split('/');
    let name = segments.next_back()?;
    for folder in segments {
        if folder.ends_with("-backups") {
            return Some(Skipped::Backups);
        }
        if folder.starts_with('.') {
            return Some(Skipped::DotDirectory);
        }
        if folder == "__MACOSX" {
            return Some(Skipped::MacOs);
        }
    }
    let lower = name.to_ascii_lowercase();
    if lower.rsplit_once('.').is_some_and(|(_, ext)| ext == "lck") {
        return Some(Skipped::Lock);
    }
    if lower.starts_with("_autosave-") {
        return Some(Skipped::Autosave);
    }
    if lower.contains("-save.kicad_") {
        return Some(Skipped::Save);
    }
    None
}

impl Incoming {
    /// An empty upload under a fresh temporary directory.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the system's temporary directory refuses one.
    pub fn new(limits: Limits) -> Result<Self> {
        Ok(Self {
            dir: tempfile::Builder::new()
                .prefix("kiewer-upload-")
                .tempdir()?,
            files: Vec::new(),
            open: None,
            total: 0,
            parts: 0,
            limits,
        })
    }

    /// What an upload keeps of a part named `name` (the browser's file
    /// name, or a directory upload's relative path): its path and kind,
    /// or `None` for a file that is dropped or skipped, said once in the
    /// log. A zip is neither: [`Incoming::is_archive`] tells one.
    #[must_use]
    pub fn accepts(name: &str) -> Option<(TreePath, FileKind)> {
        let path = match name.parse::<TreePath>() {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(name, %error, "upload: dropped a file whose name is not a path");
                return None;
            }
        };
        if let Some(why) = skipped(&path) {
            tracing::info!(%path, %why, "upload: skipped a file");
            return None;
        }
        if let Some(kind) = FileKind::of(&path) {
            Some((path, kind))
        } else {
            tracing::warn!(%path, "upload: dropped a file that is not a KiCad file");
            None
        }
    }

    /// Whether a part named `name` is a zip the upload expands: by its
    /// extension, any case, and uploaded by itself. A zip inside a folder
    /// is not the upload but something beside the project (KiCad writes
    /// one into `-backups` at every save and one beside the project for
    /// Archive Project), and expanding it would root the tree at an old
    /// snapshot.
    #[must_use]
    pub fn is_archive(name: &str) -> bool {
        name.parse::<TreePath>()
            .is_ok_and(|path| path.depth() == 0 && path.extension().as_deref() == Some("zip"))
    }

    /// Start receiving the file at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when its temporary file cannot be made.
    ///
    /// # Panics
    ///
    /// When a part is already open: [`Incoming::end`] closes one before
    /// the next begins.
    pub async fn begin(&mut self, path: TreePath, kind: FileKind) -> Result<()> {
        let temp = self.dir.path().join(format!("{}.part", self.parts));
        let received = Received {
            path,
            kind,
            temp,
            size: 0,
        };
        self.open_part(Writing::File(received)).await
    }

    /// Start receiving a zip named `name`, expanded when it ends.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when its temporary file cannot be made.
    ///
    /// # Panics
    ///
    /// As [`Incoming::begin`].
    pub async fn begin_archive(&mut self, name: &str) -> Result<()> {
        let temp = archive::temp_path(self.dir.path(), self.parts);
        self.open_part(Writing::Archive {
            name: name.to_owned(),
            temp,
            size: 0,
        })
        .await
    }

    async fn open_part(&mut self, writing: Writing) -> Result<()> {
        assert!(self.open.is_none(), "an upload receives one part at a time");
        let temp = match &writing {
            Writing::File(received) => &received.temp,
            Writing::Archive { temp, .. } => temp,
        };
        let file = tokio::fs::File::create(temp).await?;
        self.parts += 1;
        self.open = Some(Open { writing, file });
        Ok(())
    }

    /// The next bytes of the open part.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] when the chunk takes the file over the per-file
    /// cap or the upload over the per-revision cap (a zip's own bytes
    /// against the latter alone); [`Error::Io`] when the temporary file
    /// cannot be written.
    ///
    /// # Panics
    ///
    /// Without an open part.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        let open = self.open.as_mut().expect("a part is open");
        let len = chunk.len() as u64;
        let revision_too_large = || Refusal::RevisionTooLarge {
            limit_mb: self.limits.revision_mb(),
        };
        match &mut open.writing {
            Writing::File(received) => {
                if received.size + len > self.limits.file_bytes {
                    return Err(Refusal::FileTooLarge {
                        path: received.path.to_string(),
                        limit_mb: self.limits.file_mb(),
                    }
                    .into());
                }
                if self.total + len > self.limits.revision_bytes {
                    return Err(revision_too_large().into());
                }
                open.file.write_all(chunk).await?;
                received.size += len;
                self.total += len;
            }
            Writing::Archive { size, .. } => {
                if *size + len > self.limits.revision_bytes {
                    return Err(revision_too_large().into());
                }
                open.file.write_all(chunk).await?;
                *size += len;
            }
        }
        Ok(())
    }

    /// The open part is complete: a file joins the upload; a zip is
    /// expanded and its files do.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the temporary file cannot be flushed;
    /// [`Error::Refused`] when the zip is refused (`upload/archive.rs`).
    ///
    /// # Panics
    ///
    /// Without an open part.
    pub async fn end(&mut self) -> Result<()> {
        let mut open = self.open.take().expect("a part is open");
        open.file.flush().await?;
        drop(open.file);
        match open.writing {
            Writing::File(received) => self.files.push(received),
            Writing::Archive { name, temp, .. } => {
                let into = self.dir.path().to_owned();
                let first = self.parts;
                let total = self.total;
                let limits = self.limits;
                let expanded = tokio::task::spawn_blocking(move || {
                    archive::expand(&temp, &name, &into, first, total, &limits)
                })
                .await
                .map_err(|join| Error::Failed {
                    reason: format!("the upload's zip task did not finish: {join}"),
                })??;
                self.parts += expanded.len();
                for received in expanded {
                    self.total += received.size;
                    self.files.push(received);
                }
            }
        }
        Ok(())
    }

    /// How many files were received.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing was received.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The bytes received so far, over every file.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Who uploads: the signed-in account and its handle, the author text
/// the revision's hash covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub account_id: AccountId,
    pub handle: Handle,
}

/// Where the revision goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A new design of the author's, named `name` or after the project
    /// folder's file, with a slug from the name; the revision is the root
    /// of its default branch.
    New { name: Option<String> },
    /// The design at `/{owner}/{slug}`, which the author has at least the
    /// collaborator role on (`roles::require`), on `branch`.
    Existing {
        owner: Handle,
        slug: Slug,
        branch: BranchTarget,
    },
}

/// Which branch of an existing design a push lands on (docs/v1-scope.md,
/// Push).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchTarget {
    /// Onto the head of the design's default branch: what a form that
    /// names no branch means, and where a new design's first revision
    /// goes.
    Default,
    /// Onto the head of this branch as the upload finds it: the head
    /// read before the slow part, moved by compare-and-set.
    Head(BranchName),
    /// Onto the head of this branch, which the form the upload came from
    /// saw at `head`: a revision on the branch, the one the page showed.
    /// A head that has moved past it holds the upload
    /// (`crate::pending`). A `head` that is not on the branch says
    /// nothing about it, and this is [`BranchTarget::Head`].
    Seen {
        name: BranchName,
        head: RevisionHash,
    },
    /// A new branch named `name` at `from` (a revision the design
    /// reaches: the one the page the form was opened from showed), with
    /// this revision on top (docs/v1-scope.md, Push: a new branch starts
    /// at a revision, never from nothing).
    New {
        name: BranchName,
        from: RevisionHash,
    },
}

/// What came of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The revision landed.
    Pushed(Pushed),
    /// The branch's head moved: nothing landed, and the uploader chooses
    /// what becomes of the upload (`crate::pending`).
    Pending { design: Design, id: PendingId },
}

/// What a push made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    pub design: Design,
    /// The branch the revision is the head of.
    pub branch: BranchName,
    pub revision: RevisionHash,
    /// Whether the design is new.
    pub created: bool,
    /// What the uploader should know about what was kept: the other
    /// projects when the upload held more than one, and the files left
    /// out for sitting outside the project folder. Empty for a plain
    /// upload. Plain sentences, for the page.
    pub warnings: Vec<String>,
}

/// An upload rooted at its project folder.
struct Rooted {
    /// The name, without its extension, of the file the project folder
    /// was found by: a new design's name when none was given.
    stem: String,
    /// Every file under the folder with its path relative to it, in path
    /// order, shallowest first.
    files: Vec<Received>,
    /// As [`Pushed::warnings`].
    warnings: Vec<String>,
}

/// The upload rooted at the folder of the file the project folder is
/// found by: the `.kicad_pro`, else the shallowest `.kicad_pcb`, else
/// the shallowest `.kicad_sch` (GLOSSARY, Project folder), the shallowest
/// of several of that kind, and the first in path order at a depth. The
/// other project files or boards are other projects: a warning names
/// them, and another names what was left out for sitting outside the
/// folder. Other schematics are not: one project has many sheets.
fn root_at_project_folder(files: Vec<Received>) -> Rooted {
    let depth = TreePath::depth;
    let anchor = FileKind::ALL
        .into_iter()
        .find_map(|kind| {
            files
                .iter()
                .filter(|file| file.kind == kind)
                .min_by_key(|file| (depth(&file.path), file.path.clone()))
        })
        .expect("root_at_project_folder is called with at least one file");
    let anchor_kind = anchor.kind;
    let anchor_path = anchor.path.clone();
    let folder = anchor_path
        .as_str()
        .rsplit_once('/')
        .map_or(String::new(), |(folder, _)| format!("{folder}/"));
    let stem = anchor_path
        .file_name()
        .rsplit_once('.')
        .map_or(anchor_path.file_name(), |(stem, _)| stem)
        .to_owned();
    let others: Vec<String> = files
        .iter()
        .filter(|file| anchor_kind != FileKind::Schematic && file.kind == anchor_kind)
        .filter(|file| file.path != anchor_path)
        .map(|file| file.path.to_string())
        .collect();
    let mut outside = Vec::new();
    let mut rooted: Vec<Received> = files
        .into_iter()
        .filter_map(|file| {
            let Some(relative) = file.path.as_str().strip_prefix(&folder) else {
                tracing::warn!(path = %file.path, %folder, "upload: dropped a file outside the project folder");
                outside.push(file.path.to_string());
                return None;
            };
            let path = relative
                .parse()
                .expect("a suffix of a path at a segment boundary is a path");
            Some(Received { path, ..file })
        })
        .collect();
    rooted.sort_by(|a, b| (depth(&a.path), &a.path).cmp(&(depth(&b.path), &b.path)));
    let mut warnings = Vec::new();
    if !others.is_empty() {
        warnings.push(format!(
            "the upload holds more than one project: {anchor_path} was taken as the project; the others are {}",
            others.join(", ")
        ));
    }
    if !outside.is_empty() {
        let folder = folder.trim_end_matches('/');
        warnings.push(format!(
            "left out, outside the project folder {folder}: {}",
            outside.join(", ")
        ));
    }
    Rooted {
        stem,
        files: rooted,
        warnings,
    }
}

/// A file's bytes checked as its kind says, off the runtime's threads;
/// the board's metadata when it is one.
fn parse(kind: FileKind, path: &TreePath, bytes: &[u8]) -> Result<Option<Metadata>> {
    let malformed = |error: String| {
        Error::Refused(Refusal::DoesNotParse {
            path: path.to_string(),
            error,
        })
    };
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("not UTF-8 text".to_owned()))?;
    match kind {
        FileKind::Board => {
            let board =
                kicad_core::parse_board(text).map_err(|error| malformed(error.to_string()))?;
            Ok(Some(Metadata::of(&board)))
        }
        FileKind::Schematic => {
            kicad_core::parse_schematic(text).map_err(|error| malformed(error.to_string()))?;
            Ok(None)
        }
        FileKind::Project => {
            serde_json::from_str::<serde_json::Value>(text)
                .map_err(|error| malformed(format!("not a KiCad project file: {error}")))?;
            Ok(None)
        }
    }
}

/// The files of `incoming` as a revision of the author's design at
/// `target`, with `message`: rooted, parsed, stored, and the rows written
/// in one transaction; or, when the branch's head moved on the way,
/// held for the uploader's choice ([`Outcome::Pending`]).
///
/// # Errors
///
/// [`Error::Refused`] for every reason the uploader can act on
/// ([`Refusal`]), the hourly allowance among them; [`Error::Forbidden`]
/// for a push to a design the author has no role on; [`Error::Io`],
/// [`Error::Db`] and the blob store's own errors otherwise.
pub async fn push(
    db: &PgPool,
    blobs: &dyn BlobStore,
    limits: &Limits,
    author: &Author,
    target: Target,
    message: Option<&str>,
    incoming: Incoming,
) -> Result<Outcome> {
    if incoming.is_empty() {
        return Err(Refusal::NoKicadFile.into());
    }
    // Before the parse and the blob writes, which are the expensive part;
    // on the pool, a statement of its own: the count is a bound on bursts,
    // not a ledger, and the window moves with the clock.
    rate_limit::check(db, author.account_id, limits.uploads_per_hour).await?;
    let started_at = head_now(db, &target).await?;
    let Incoming { dir, files, .. } = incoming;
    let Rooted {
        stem,
        files: rooted,
        warnings,
    } = root_at_project_folder(files);
    if let Some(pair) = rooted.windows(2).find(|pair| pair[0].path == pair[1].path) {
        return Err(Refusal::DoesNotParse {
            path: pair[0].path.to_string(),
            error: "appears twice in the upload".to_owned(),
        }
        .into());
    }
    let (entries, metadata) = parse_and_store(db, blobs, limits, rooted).await?;
    // The temporary files have served; the directory goes with them.
    drop(dir);
    let rows = Rows {
        author,
        target,
        started_at,
        stem,
        message,
        entries: &entries,
        metadata: metadata.as_ref(),
        warnings,
    };
    write_rows(db, limits, rows).await
}

/// The head of the branch the upload at `target` goes to, read before
/// the parse and the blob writes, so an upload that lands meanwhile is
/// one this upload did not build on. `None` for a new design, a new
/// branch, or a design or branch that is not there, which the transaction
/// refuses or makes. One read on the pool: it decides nothing until the
/// transaction compares it.
async fn head_now(db: &PgPool, target: &Target) -> Result<Option<RevisionHash>> {
    let Target::Existing {
        owner,
        slug,
        branch,
    } = target
    else {
        return Ok(None);
    };
    let name = match branch {
        BranchTarget::New { .. } => return Ok(None),
        BranchTarget::Head(name) | BranchTarget::Seen { name, .. } => Some(name.as_str()),
        BranchTarget::Default => None,
    };
    let Some(design) = design::find_at(db, owner, slug).await? else {
        return Ok(None);
    };
    let name = name.unwrap_or(&design.default_branch);
    design::head(&mut *db.acquire().await?, design.id, name).await
}

/// Each rooted file read, parsed under what is left of the timeout,
/// checked against the block list and stored as a blob: the tree's
/// entries, and the metadata of the first board in path order (the
/// shallowest), the one the page shows.
async fn parse_and_store(
    db: &PgPool,
    blobs: &dyn BlobStore,
    limits: &Limits,
    rooted: Vec<Received>,
) -> Result<(Vec<TreeEntry>, Option<Metadata>)> {
    let deadline = Instant::now() + limits.parse_timeout;
    let mut entries = Vec::with_capacity(rooted.len());
    let mut metadata: Option<Metadata> = None;
    for file in rooted {
        let bytes = tokio::fs::read(&file.temp).await?;
        let (bytes, parsed) = {
            let path = file.path.clone();
            let kind = file.kind;
            let parsing = tokio::task::spawn_blocking(move || {
                let parsed = parse(kind, &path, &bytes);
                (bytes, parsed)
            });
            match tokio::time::timeout_at(deadline, parsing).await {
                Ok(joined) => joined.map_err(|join| Error::Failed {
                    reason: format!("the upload's parse task did not finish: {join}"),
                })?,
                Err(_) => {
                    return Err(Refusal::ParseTimeout {
                        seconds: limits.parse_timeout.as_secs(),
                    }
                    .into());
                }
            }
        };
        let parsed = parsed?;
        if file.kind == FileKind::Board && metadata.is_none() {
            metadata = parsed;
        }
        let hash = BlobHash::of(&bytes);
        if design::is_blocked(db, &hash).await? {
            return Err(Refusal::Blocked {
                path: file.path.to_string(),
            }
            .into());
        }
        collector::store(db, blobs, &bytes).await?;
        entries.push(TreeEntry {
            path: file.path,
            blob: hash,
            size: file.size,
        });
    }
    Ok((entries, metadata))
}

/// What the transaction writes, gathered.
struct Rows<'a> {
    author: &'a Author,
    target: Target,
    /// The branch's head as the upload started ([`head_now`]).
    started_at: Option<RevisionHash>,
    /// The project folder's file name without its extension: a new
    /// design's name when none was given.
    stem: String,
    message: Option<&'a str>,
    entries: &'a [TreeEntry],
    metadata: Option<&'a Metadata>,
    warnings: Vec<String>,
}

/// The rows of the revision, as one outcome, the blobs' rows being
/// there from `collector::store`: the design (held, or made), the quota
/// against them, the tree, then either the revision on the branch's head
/// with the head moved by compare-and-set and the design's index of its
/// revisions, or, when the head is no longer the one the upload was
/// built on, the pending row in their place.
async fn write_rows(db: &PgPool, limits: &Limits, rows: Rows<'_>) -> Result<Outcome> {
    let Rows {
        author,
        target,
        started_at,
        stem,
        message,
        entries,
        metadata,
        warnings,
    } = rows;
    let mut tx = db.begin().await?;
    let (design, branch, created) = match target {
        Target::Existing {
            owner,
            slug,
            branch,
        } => {
            let design = design::lock_at(&mut tx, &owner, &slug)
                .await?
                .ok_or_else(|| {
                    Error::Refused(Refusal::NoSuchDesign {
                        slug: format!("{owner}/{slug}"),
                    })
                })?;
            // A collaborator may push to a branch and make one (GLOSSARY,
            // Collaborator): one role covers every branch target.
            roles::require(&mut *tx, design.id, author.account_id, Role::Collaborator).await?;
            (design, branch, false)
        }
        Target::New { name } => {
            let name = name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or(stem);
            let slug = Slug::suggest(&name);
            let design = design::create(&mut tx, author.account_id, &slug, &name).await?;
            (design, BranchTarget::Default, true)
        }
    };
    let hashes: Vec<BlobHash> = entries.iter().map(|entry| entry.blob).collect();
    check_account_cap(&mut tx, limits, author.account_id, &hashes).await?;
    let tree = design::record_tree(&mut tx, entries).await?;
    // A block made since the file passed the check in parse_and_store.
    takedown::refuse_blocked(&mut *tx, tree).await?;
    let message = message.map(str::trim).filter(|message| !message.is_empty());
    let (branch, parent, expected) =
        match resolve_branch(&mut tx, &design, branch, started_at).await? {
            Resolved::Land {
                branch,
                parent,
                expected,
            } => (branch, parent, expected),
            Resolved::Hold { branch, base } => {
                let held = Held {
                    design: design.id,
                    branch: &branch,
                    author: author.account_id,
                    base,
                    tree,
                    message,
                    metadata,
                    warnings: &warnings,
                };
                let id = pending::hold(&mut tx, &held).await?;
                tx.commit().await?;
                tracing::info!(design = %design.slug, %branch, %id, "upload held: the head moved");
                return Ok(Outcome::Pending { design, id });
            }
        };
    let revision = land(
        &mut tx,
        &Landing {
            design: design.id,
            branch: &branch,
            parent,
            expected,
            tree,
            author,
            message,
            metadata,
        },
    )
    .await?;
    tx.commit().await?;
    tracing::info!(
        design = %design.slug,
        %branch,
        %revision,
        files = entries.len(),
        created,
        "revision pushed"
    );
    Ok(Outcome::Pushed(Pushed {
        design,
        branch,
        revision,
        created,
        warnings,
    }))
}

/// Refused when `blobs` would take `account` over its cap, within the
/// transaction that will name them.
pub(crate) async fn check_account_cap(
    tx: &mut PgConnection,
    limits: &Limits,
    account: AccountId,
    blobs: &[BlobHash],
) -> Result<()> {
    let (used, added) = design::account_bytes(tx, account, blobs).await?;
    if used + added > limits.account_bytes {
        return Err(Refusal::AccountFull {
            limit_gb: limits.account_gb(),
        }
        .into());
    }
    Ok(())
}

/// A revision going onto a branch.
pub(crate) struct Landing<'a> {
    pub design: DesignId,
    pub branch: &'a BranchName,
    pub parent: Option<RevisionHash>,
    /// What the compare-and-set expects the head to be; `None` makes the
    /// branch.
    pub expected: Option<RevisionHash>,
    pub tree: TreeHash,
    pub author: &'a Author,
    pub message: Option<&'a str>,
    pub metadata: Option<&'a Metadata>,
}

/// The revision's row, stamped now, its branch's head moved to it, and
/// the design's index of its revisions, within the caller's transaction:
/// what a push and a pending revision's decision both end in.
pub(crate) async fn land(tx: &mut PgConnection, landing: &Landing<'_>) -> Result<RevisionHash> {
    let author = landing.author;
    let time = design::now_text(tx).await?;
    let draft = RevisionDraft {
        tree: landing.tree,
        parent: landing.parent,
        author: &author.handle,
        time: &time,
        message: landing.message,
    };
    let revision = design::record_revision(tx, &draft, author.account_id, landing.metadata).await?;
    design::set_head(
        tx,
        landing.design,
        landing.branch.as_str(),
        landing.expected,
        revision,
        author.account_id,
    )
    .await?;
    design::record_design_revision(tx, landing.design, revision).await?;
    Ok(revision)
}

/// Where a push goes, as its transaction finds things.
enum Resolved {
    /// Onto `branch`, with `parent` as the new revision's, the
    /// compare-and-set expecting `expected`.
    Land {
        branch: BranchName,
        parent: Option<RevisionHash>,
        expected: Option<RevisionHash>,
    },
    /// Nowhere yet: `branch` has moved on from `base`.
    Hold {
        branch: BranchName,
        base: RevisionHash,
    },
}

/// The branch a push lands on, the new revision's parent, and what the
/// compare-and-set expects the head to be: the head for a push onto a
/// branch, its row held so nothing moves it now, nothing for a branch
/// being made (whose first revision's parent is where it starts). Or,
/// when the head has moved on from what the push was built on, the
/// upload is held. That is the head its form saw, else `started_at`; one
/// not on the branch is no evidence of anything (a form opened elsewhere,
/// a branch deleted and made again), and the next is asked. Within the
/// upload's transaction, after the design is held.
async fn resolve_branch(
    tx: &mut PgConnection,
    design: &Design,
    branch: BranchTarget,
    started_at: Option<RevisionHash>,
) -> Result<Resolved> {
    match branch {
        BranchTarget::Default | BranchTarget::Head(_) | BranchTarget::Seen { .. } => {
            let (name, seen) = match branch {
                BranchTarget::Head(name) => (name, None),
                BranchTarget::Seen { name, head } => (name, Some(head)),
                _ => (BranchName::from_row(design.default_branch.clone()), None),
            };
            let head = design::lock_head(tx, design.id, name.as_str()).await?;
            // The default branch of a design with no revision yet (a new
            // design, or one whose first push was refused after the row)
            // is made by this push.
            if head.is_none() && name.as_str() != design.default_branch {
                return Err(Refusal::NoSuchBranch {
                    branch: name.to_string(),
                }
                .into());
            }
            for base in seen.into_iter().chain(started_at) {
                if Some(base) == head {
                    break;
                }
                if head.is_some() && design::on_branch(tx, design.id, name.as_str(), base).await? {
                    return Ok(Resolved::Hold { branch: name, base });
                }
            }
            Ok(Resolved::Land {
                branch: name,
                parent: head,
                expected: head,
            })
        }
        BranchTarget::New { name, from } => {
            if !design::reaches(&mut *tx, design.id, from).await? {
                return Err(Refusal::NoSuchRevision {
                    hash: from.to_string(),
                }
                .into());
            }
            Ok(Resolved::Land {
                branch: name,
                parent: Some(from),
                expected: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use kiewer_assert::assert_is_not_empty;
    use test_case::test_case;

    use super::*;

    #[test_case("board.kicad_pcb", Some(FileKind::Board); "a board")]
    #[test_case("Board.KICAD_SCH", Some(FileKind::Schematic); "a schematic, any case")]
    #[test_case("proj/board.kicad_pro", Some(FileKind::Project); "a project file in a folder")]
    #[test_case("board.kicad_prl", None; "the local settings file is dropped")]
    #[test_case("readme.md", None; "a readme is dropped")]
    #[test_case("board", None; "no extension")]
    fn a_file_is_kept_by_its_extension(name: &str, kind: Option<FileKind>) {
        assert_eq!(Incoming::accepts(name).map(|(_, kind)| kind), kind);
    }

    #[test_case("../x.kicad_pcb"; "a parent segment")]
    #[test_case(""; "empty")]
    fn a_name_that_is_not_a_path_is_dropped(name: &str) {
        assert!(Incoming::accepts(name).is_none());
    }

    #[test_case("demo/demo-backups/demo-2026-11-02.kicad_pcb", Some(Skipped::Backups); "a backups folder")]
    #[test_case("demo/.git/x.kicad_sch", Some(Skipped::DotDirectory); "a dot-directory")]
    #[test_case("__MACOSX/demo/._demo.kicad_pcb", Some(Skipped::MacOs); "the Finder's folder")]
    #[test_case("demo/~demo.kicad_pcb.lck", Some(Skipped::Lock); "a lock file")]
    #[test_case("demo/demo.kicad_pro.lck", Some(Skipped::Lock); "a lock file without the tilde")]
    #[test_case("demo/_autosave-demo.kicad_sch", Some(Skipped::Autosave); "an autosave")]
    #[test_case("demo/demo-save.kicad_pcb", Some(Skipped::Save); "a save file")]
    #[test_case("demo/demo.kicad_pcb", None; "the board itself")]
    #[test_case("demo-backups.kicad_sch", None; "a file named like the folder")]
    #[test_case("demo/.hidden.kicad_sch", None; "a dot-file is not a dot-directory")]
    fn the_skip_list_names_why(path: &str, why: Option<Skipped>) {
        let path: TreePath = path.parse().unwrap();
        assert_eq!(skipped(&path), why);
        assert_eq!(
            Incoming::accepts(path.as_str()).is_some(),
            why.is_none(),
            "accepts follows the list"
        );
    }

    #[test_case("demo.zip", true; "a zip")]
    #[test_case("Demo.ZIP", true; "any case")]
    #[test_case("demo.kicad_pcb", false; "a board")]
    #[test_case("zip", false; "no extension")]
    #[test_case("demo/demo-backups/demo-2026-11-02_120000.zip", false; "KiCad's backup zip in a dropped folder")]
    #[test_case("demo/.cache/demo.zip", false; "a zip in a dot-directory")]
    #[test_case("demo/demo-archive.zip", false; "a zip in a dropped folder, not the upload itself")]
    fn a_zip_is_a_part_of_its_own_with_the_extension(name: &str, is_archive: bool) {
        assert_eq!(Incoming::is_archive(name), is_archive);
    }

    fn received(path: &str) -> Received {
        let path: TreePath = path.parse().unwrap();
        Received {
            kind: FileKind::of(&path).unwrap(),
            path,
            temp: PathBuf::new(),
            size: 1,
        }
    }

    fn paths(rooted: &[Received]) -> Vec<&str> {
        rooted.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn the_tree_is_rooted_at_the_project_files_folder() {
        let rooted = root_at_project_folder(vec![
            received("home/me/demo/demo.kicad_sch"),
            received("home/me/demo/sub/power.kicad_sch"),
            received("home/me/demo/demo.kicad_pro"),
            received("home/me/demo/demo.kicad_pcb"),
            received("home/me/other/other.kicad_pcb"),
        ]);
        assert_eq!(rooted.stem, "demo");
        assert_eq!(
            paths(&rooted.files),
            [
                "demo.kicad_pcb",
                "demo.kicad_pro",
                "demo.kicad_sch",
                "sub/power.kicad_sch"
            ],
            "shallowest first, then by path; the file outside the folder is dropped"
        );
        assert_eq!(
            rooted.warnings,
            ["left out, outside the project folder home/me/demo: home/me/other/other.kicad_pcb"],
            "a board beside a project file is not another project, but it is left out"
        );
    }

    #[test_case(&["a/b/board.kicad_pcb", "a/b/board.kicad_sch"], "board", &["board.kicad_pcb", "board.kicad_sch"], &[]; "without a project file the board roots the tree")]
    #[test_case(&["deep/er/sheet.kicad_sch", "top.kicad_sch"], "top", &["top.kicad_sch", "deep/er/sheet.kicad_sch"], &[]; "without a board the shallowest schematic does")]
    #[test_case(&["midi8knob/midi8knob.kicad_pro", "midi8knob/midi8knob.kicad_pcb", "midi8knob/midi8knob.kicad_sch", "midi8knob/sub/power.kicad_sch"], "midi8knob", &["midi8knob.kicad_pcb", "midi8knob.kicad_pro", "midi8knob.kicad_sch", "sub/power.kicad_sch"], &[]; "an Explorer zip's wrapper folder is stripped and the sheet paths kept")]
    #[test_case(&["b/deep/b.kicad_pro", "b/deep/b.kicad_pcb", "a/a.kicad_pro", "a/a.kicad_pcb"], "a", &["a.kicad_pcb", "a.kicad_pro"], &["the upload holds more than one project: a/a.kicad_pro was taken as the project; the others are b/deep/b.kicad_pro", "left out, outside the project folder a: b/deep/b.kicad_pro, b/deep/b.kicad_pcb"]; "two projects: the shallowest, and a warning naming the other")]
    #[test_case(&["x/b.kicad_pro", "x/a.kicad_pro"], "a", &["a.kicad_pro", "b.kicad_pro"], &["the upload holds more than one project: x/a.kicad_pro was taken as the project; the others are x/b.kicad_pro"]; "two project files in one folder: the first by path, the other kept as a file and named")]
    #[test_case(&["top.kicad_pro", "sub/inner.kicad_pro", "sub/inner.kicad_sch"], "top", &["top.kicad_pro", "sub/inner.kicad_pro", "sub/inner.kicad_sch"], &["the upload holds more than one project: top.kicad_pro was taken as the project; the others are sub/inner.kicad_pro"]; "a project folder inside the project folder is kept under it")]
    #[test_case(&["one/one.kicad_pcb", "two/two.kicad_pcb"], "one", &["one.kicad_pcb"], &["the upload holds more than one project: one/one.kicad_pcb was taken as the project; the others are two/two.kicad_pcb", "left out, outside the project folder one: two/two.kicad_pcb"]; "two boards and no project file: the boards are the projects")]
    fn the_anchor_falls_back_from_project_to_board_to_schematic(
        uploaded: &[&str],
        stem: &str,
        rooted_paths: &[&str],
        warnings: &[&str],
    ) {
        let rooted = root_at_project_folder(uploaded.iter().map(|path| received(path)).collect());
        assert_eq!(rooted.stem, stem);
        assert_eq!(paths(&rooted.files), rooted_paths);
        assert_eq!(rooted.warnings, warnings);
    }

    #[test]
    fn the_hand_off_files_pass_their_shape_checks() {
        let path: TreePath = "midi8knob.kicad_pcb".parse().unwrap();
        let board = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pcb");
        let metadata = parse(FileKind::Board, &path, board)
            .expect("the board parses")
            .expect("a board has metadata");
        assert_eq!(metadata.footprint_count, 78);
        let schematic = include_bytes!("../../viewer-drop/board/midi8knob.kicad_sch");
        assert_eq!(parse(FileKind::Schematic, &path, schematic).unwrap(), None);
        let project = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pro");
        assert_eq!(parse(FileKind::Project, &path, project).unwrap(), None);
    }

    #[test_case(FileKind::Board, b"(kicad_pcb (version 20241229) (layers (0 \"F.Cu\" signal))" ; "an unbalanced board")]
    #[test_case(FileKind::Board, b"(kicad_pcb (version 20211014) (layers (0 \"F.Cu\" signal)))" ; "a KiCad 6 board, which names the version")]
    #[test_case(FileKind::Schematic, b"(kicad_pcb (version 20241229))" ; "a board where a schematic was named")]
    #[test_case(FileKind::Schematic, b"not an s-expression" ; "text that is no s-expression")]
    #[test_case(FileKind::Project, b"{ not json" ; "a project file that is not JSON")]
    #[test_case(FileKind::Project, b"\xff\xfe" ; "bytes that are not UTF-8")]
    fn a_malformed_file_is_refused_naming_it(kind: FileKind, bytes: &[u8]) {
        let path: TreePath = "x.file".parse().unwrap();
        let err = parse(kind, &path, bytes).expect_err("refused");
        match err {
            Error::Refused(Refusal::DoesNotParse { path, error }) => {
                assert_eq!(path, "x.file");
                assert_is_not_empty!(error);
            }
            other => panic!("not a refusal: {other}"),
        }
    }

    #[test]
    fn the_body_limit_leaves_room_for_the_framing() {
        assert!(BODY_LIMIT as u64 > Limits::DEFAULT.revision_bytes);
    }

    #[test]
    fn the_default_caps_are_the_scopes() {
        assert_eq!(Limits::DEFAULT.file_mb(), 50);
        assert_eq!(Limits::DEFAULT.revision_mb(), 200);
        assert_eq!(Limits::DEFAULT.account_gb(), 2);
        assert_eq!(Limits::DEFAULT.parse_timeout, Duration::from_secs(30));
        assert_eq!(Limits::DEFAULT.uploads_per_hour, 30);
        assert_eq!(Limits::DEFAULT.zip_entries, 10_000);
    }
}
