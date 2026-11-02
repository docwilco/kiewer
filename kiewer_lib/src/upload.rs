//! An upload: one or more KiCad files become a revision of a design
//! (docs/v1-scope.md, Uploads). The parts of the multipart body are
//! streamed into temporary files as they arrive ([`Incoming`]), counted
//! against the per-file and per-revision caps; then [`commit`] roots the
//! files at the project folder, parses each one under the parse timeout,
//! stores its bytes as a blob, and writes the tree, revision, branch and
//! `design_revision` rows in one transaction. Nothing slow happens inside
//! that transaction: the parsing and the blob writes are done before it
//! opens, and a blob a refused upload leaves behind is the collector's.
//!
//! What is kept: `.kicad_pcb`, `.kicad_sch` and `.kicad_pro`; anything
//! else is dropped with a line in the log. A board is parsed with
//! `kicad_core` for its metadata. There is no schematic parser yet (#12),
//! so a schematic is checked to be an S-expression headed `kicad_sch` and
//! a project file to be JSON, and both are stored as they are.

use std::path::PathBuf;
use std::time::Duration;

use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

use crate::account::{AccountId, Handle};
use crate::blob::{BlobHash, BlobStore};
use crate::design::{self, Design, Slug};
use crate::error::{Error, Refusal, Result};
use crate::revision::{Metadata, RevisionDraft, RevisionHash, TreeEntry, TreePath};

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
}

const MB: u64 = 1024 * 1024;

impl Limits {
    /// The caps of docs/v1-scope.md: 50 MB per file, 200 MB per revision,
    /// 2 GB per account, 30 seconds of parsing.
    pub const DEFAULT: Limits = Limits {
        file_bytes: 50 * MB,
        revision_bytes: 200 * MB,
        account_bytes: 2048 * MB,
        parse_timeout: Duration::from_secs(30),
    };

    fn file_mb(&self) -> u64 {
        self.file_bytes / MB
    }

    fn revision_mb(&self) -> u64 {
        self.revision_bytes / MB
    }

    fn account_gb(&self) -> u64 {
        self.account_bytes / (1024 * MB)
    }
}

/// How large the upload route lets a request body be: the revision cap
/// with room for the multipart framing and the text fields. Over it, the
/// body is refused before any part is read.
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
    /// `.kicad_sch`: stored; parsing is #12.
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

/// A file being written.
#[derive(Debug)]
struct Open {
    received: Received,
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
    limits: Limits,
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
            limits,
        })
    }

    /// What an upload keeps of a part named `name` (the browser's file
    /// name, or a directory upload's relative path): its path and kind,
    /// or `None` for a file that is dropped, said once in the log.
    #[must_use]
    pub fn accepts(name: &str) -> Option<(TreePath, FileKind)> {
        let path = match name.parse::<TreePath>() {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(name, %error, "upload: dropped a file whose name is not a path");
                return None;
            }
        };
        if let Some(kind) = FileKind::of(&path) {
            Some((path, kind))
        } else {
            tracing::warn!(%path, "upload: dropped a file that is not a KiCad file");
            None
        }
    }

    /// Start receiving the file at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when its temporary file cannot be made.
    ///
    /// # Panics
    ///
    /// When a file is already open: [`Incoming::end`] closes one before
    /// the next begins.
    pub async fn begin(&mut self, path: TreePath, kind: FileKind) -> Result<()> {
        assert!(self.open.is_none(), "an upload receives one file at a time");
        let temp = self.dir.path().join(format!("{}.part", self.files.len()));
        let file = tokio::fs::File::create(&temp).await?;
        self.open = Some(Open {
            received: Received {
                path,
                kind,
                temp,
                size: 0,
            },
            file,
        });
        Ok(())
    }

    /// The next bytes of the open file.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] when the chunk takes the file over the per-file
    /// cap or the upload over the per-revision cap; [`Error::Io`] when the
    /// temporary file cannot be written.
    ///
    /// # Panics
    ///
    /// Without an open file.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        let open = self.open.as_mut().expect("a file is open");
        let len = chunk.len() as u64;
        if open.received.size + len > self.limits.file_bytes {
            return Err(Refusal::FileTooLarge {
                path: open.received.path.to_string(),
                limit_mb: self.limits.file_mb(),
            }
            .into());
        }
        if self.total + len > self.limits.revision_bytes {
            return Err(Refusal::RevisionTooLarge {
                limit_mb: self.limits.revision_mb(),
            }
            .into());
        }
        open.file.write_all(chunk).await?;
        open.received.size += len;
        self.total += len;
        Ok(())
    }

    /// The open file is complete.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the temporary file cannot be flushed.
    ///
    /// # Panics
    ///
    /// Without an open file.
    pub async fn end(&mut self) -> Result<()> {
        let mut open = self.open.take().expect("a file is open");
        open.file.flush().await?;
        drop(open.file);
        self.files.push(open.received);
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
    /// folder's file, with a slug from the name.
    New { name: Option<String> },
    /// The head of the default branch of the author's design at `slug`.
    Existing { slug: Slug },
}

/// What a committed upload made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub design: Design,
    pub revision: RevisionHash,
    /// Whether the design is new.
    pub created: bool,
}

/// A received file with its path made relative to the project folder.
struct Rooted {
    path: TreePath,
    kind: FileKind,
    temp: PathBuf,
    size: u64,
}

/// The name of the file the project folder is found by: the `.kicad_pro`,
/// else the shallowest `.kicad_pcb`, else the shallowest `.kicad_sch`
/// (GLOSSARY, Project folder); and every file under that folder with its
/// path relative to it, in path order, shallowest first.
fn root_at_project_folder(files: Vec<Received>) -> (String, Vec<Rooted>) {
    let depth = |path: &TreePath| path.as_str().matches('/').count();
    let anchor = FileKind::ALL
        .into_iter()
        .find_map(|kind| {
            files
                .iter()
                .filter(|file| file.kind == kind)
                .min_by_key(|file| (depth(&file.path), file.path.clone()))
        })
        .expect("root_at_project_folder is called with at least one file");
    let folder = anchor
        .path
        .as_str()
        .rsplit_once('/')
        .map_or(String::new(), |(folder, _)| format!("{folder}/"));
    let stem = anchor
        .path
        .file_name()
        .rsplit_once('.')
        .map_or(anchor.path.file_name(), |(stem, _)| stem)
        .to_owned();
    let mut rooted: Vec<Rooted> = files
        .into_iter()
        .filter_map(|file| {
            let Some(relative) = file.path.as_str().strip_prefix(&folder) else {
                tracing::warn!(path = %file.path, %folder, "upload: dropped a file outside the project folder");
                return None;
            };
            let path = relative
                .parse()
                .expect("a suffix of a path at a segment boundary is a path");
            Some(Rooted {
                path,
                kind: file.kind,
                temp: file.temp,
                size: file.size,
            })
        })
        .collect();
    rooted.sort_by(|a, b| (depth(&a.path), &a.path).cmp(&(depth(&b.path), &b.path)));
    (stem, rooted)
}

/// A file's bytes checked as its kind says, off the runtime's threads;
/// the board's metadata when it is one.
fn parse(kind: FileKind, path: &TreePath, bytes: &[u8]) -> Result<Option<Metadata>> {
    let malformed = |error: String| {
        Error::Refused(Refusal::Malformed {
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
            let root =
                kicad_core::sexpr::read(text).map_err(|error| malformed(error.to_string()))?;
            let head = root.head().map_err(|error| malformed(error.to_string()))?;
            if head == "kicad_sch" {
                Ok(None)
            } else {
                Err(malformed(format!(
                    "expected (kicad_sch ...), found ({head} ...)"
                )))
            }
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
/// in one transaction.
///
/// # Errors
///
/// [`Error::Refused`] for every reason the uploader can act on
/// ([`Refusal`]); [`Error::Io`], [`Error::Db`] and the blob store's own
/// errors otherwise.
pub async fn commit(
    db: &PgPool,
    blobs: &dyn BlobStore,
    limits: &Limits,
    author: &Author,
    target: Target,
    message: Option<&str>,
    incoming: Incoming,
) -> Result<Committed> {
    if incoming.is_empty() {
        return Err(Refusal::NoKicadFile.into());
    }
    let Incoming { dir, files, .. } = incoming;
    let (stem, rooted) = root_at_project_folder(files);
    if let Some(pair) = rooted.windows(2).find(|pair| pair[0].path == pair[1].path) {
        return Err(Refusal::Malformed {
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
        stem,
        message,
        entries: &entries,
        metadata: metadata.as_ref(),
    };
    write_rows(db, limits, rows).await
}

/// Each rooted file read, parsed under what is left of the timeout,
/// checked against the block list and stored as a blob: the tree's
/// entries, and the metadata of the first board in path order (the
/// shallowest), the one the page shows.
async fn parse_and_store(
    db: &PgPool,
    blobs: &dyn BlobStore,
    limits: &Limits,
    rooted: Vec<Rooted>,
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
        blobs.put(&bytes).await?;
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
    /// The project folder's file name without its extension: a new
    /// design's name when none was given.
    stem: String,
    message: Option<&'a str>,
    entries: &'a [TreeEntry],
    metadata: Option<&'a Metadata>,
}

/// The rows of the revision, as one outcome: the blobs' sizes, the design
/// (found and locked, or made), the quota against them, the tree, the
/// revision on the branch's head, the head moved by compare-and-set, and
/// the design's index of its revisions.
async fn write_rows(db: &PgPool, limits: &Limits, rows: Rows<'_>) -> Result<Committed> {
    let Rows {
        author,
        target,
        stem,
        message,
        entries,
        metadata,
    } = rows;
    let mut tx = db.begin().await?;
    for entry in entries {
        design::record_blob(&mut tx, &entry.blob, entry.size).await?;
    }
    let (design, created) = match target {
        Target::Existing { slug } => {
            let design = design::find_owned(&mut tx, author.account_id, &slug)
                .await?
                .ok_or_else(|| {
                    Error::Refused(Refusal::NoSuchDesign {
                        slug: slug.to_string(),
                    })
                })?;
            (design, false)
        }
        Target::New { name } => {
            let name = name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or(stem);
            let slug = Slug::suggest(&name);
            (
                design::create(&mut tx, author.account_id, &slug, &name).await?,
                true,
            )
        }
    };
    let hashes: Vec<BlobHash> = entries.iter().map(|entry| entry.blob).collect();
    let (used, added) = design::account_bytes(&mut tx, author.account_id, &hashes).await?;
    if used + added > limits.account_bytes {
        return Err(Refusal::AccountFull {
            limit_gb: limits.account_gb(),
        }
        .into());
    }
    let tree = design::record_tree(&mut tx, entries).await?;
    let parent = design::head(&mut tx, design.id, &design.default_branch).await?;
    let time = design::now_text(&mut tx).await?;
    let draft = RevisionDraft {
        tree,
        parent,
        author: &author.handle,
        time: &time,
        message: message.map(str::trim).filter(|message| !message.is_empty()),
    };
    let revision = design::record_revision(&mut tx, &draft, author.account_id, metadata).await?;
    design::set_head(&mut tx, design.id, &design.default_branch, parent, revision).await?;
    design::record_design_revision(&mut tx, design.id, revision).await?;
    tx.commit().await?;
    tracing::info!(
        design = %design.slug,
        %revision,
        files = entries.len(),
        created,
        "upload committed"
    );
    Ok(Committed {
        design,
        revision,
        created,
    })
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_name_that_is_not_a_path_is_dropped() {
        assert!(Incoming::accepts("../x.kicad_pcb").is_none());
        assert!(Incoming::accepts("").is_none());
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

    fn paths(rooted: &[Rooted]) -> Vec<&str> {
        rooted.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn the_tree_is_rooted_at_the_project_files_folder() {
        let (stem, rooted) = root_at_project_folder(vec![
            received("home/me/demo/demo.kicad_sch"),
            received("home/me/demo/sub/power.kicad_sch"),
            received("home/me/demo/demo.kicad_pro"),
            received("home/me/demo/demo.kicad_pcb"),
            received("home/me/other/other.kicad_pcb"),
        ]);
        assert_eq!(stem, "demo");
        assert_eq!(
            paths(&rooted),
            [
                "demo.kicad_pcb",
                "demo.kicad_pro",
                "demo.kicad_sch",
                "sub/power.kicad_sch"
            ],
            "shallowest first, then by path; the file outside the folder is dropped"
        );
    }

    #[test]
    fn without_a_project_file_the_board_roots_the_tree_and_without_a_board_the_shallowest_schematic()
     {
        let (stem, rooted) = root_at_project_folder(vec![
            received("a/b/board.kicad_pcb"),
            received("a/b/board.kicad_sch"),
        ]);
        assert_eq!(stem, "board");
        assert_eq!(paths(&rooted), ["board.kicad_pcb", "board.kicad_sch"]);
        let (stem, rooted) = root_at_project_folder(vec![
            received("deep/er/sheet.kicad_sch"),
            received("top.kicad_sch"),
        ]);
        assert_eq!(stem, "top");
        assert_eq!(paths(&rooted), ["top.kicad_sch", "deep/er/sheet.kicad_sch"]);
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
            Error::Refused(Refusal::Malformed { path, error }) => {
                assert_eq!(path, "x.file");
                assert!(!error.is_empty());
            }
            other => panic!("not a refusal: {other}"),
        }
    }

    #[test]
    fn the_body_limit_leaves_room_for_the_framing() {
        assert!(BODY_LIMIT as u64 > Limits::DEFAULT.revision_bytes);
        assert_eq!(Limits::DEFAULT.file_mb(), 50);
        assert_eq!(Limits::DEFAULT.revision_mb(), 200);
        assert_eq!(Limits::DEFAULT.account_gb(), 2);
    }
}
