//! A zip in an upload, expanded into the upload's files (docs/v1-scope.md,
//! Uploads). The central directory is the whole decision, and it is read
//! in two steps. First its bytes are screened ([`screen`]), because
//! opening an archive is the expensive part of a hostile zip: the `zip`
//! crate holds every entry in memory, and walks the directory again for
//! each end-of-directory record it tries. Then the directory, opened:
//! which entries are KiCad files the upload keeps
//! ([`super::Incoming::accepts`]'s rule: the three kinds, the skip list),
//! that each one is stored or deflated, its declared size against the
//! per-file cap and their sum against the per-revision cap. A zip that
//! fails any of these is refused before one byte is inflated. Then only
//! the planned entries are inflated, each bounded by its declared size:
//! an entry that inflates to more than its directory says is a lie (a zip
//! bomb's), and the whole upload is refused naming it.
//!
//! Deflate alone is compiled in (`deflate-flate2`, through the flate2
//! already in the tree). Blocking I/O: the caller runs this off the
//! runtime's threads.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use zip::CompressionMethod;

use super::{FileKind, Incoming, Limits, Received};
use crate::error::{Error, Refusal, Result};
use crate::revision::TreePath;

/// The signature every central directory header starts with.
const HEADER_SIGNATURE: &[u8] = b"PK\x01\x02";

/// The signature every end-of-directory record starts with.
const END_SIGNATURE: &[u8] = b"PK\x05\x06";

/// The tail of a zip the `zip` crate searches for end-of-directory
/// records: 64 KiB of comment and the record, doubled for a margin.
const END_SPAN: u64 = 1 << 17;

/// The most end-of-directory records a zip may hold in that tail. A zip
/// has one, and every zip stored inside it near the end adds its own:
/// `zip -r` stores a small project's `-backups` zips whole, so a few dozen
/// are honest. The bound is what keeps opening cheap: a 535 KB zip of
/// 9,999 headers and 2,978 records, each naming another directory start,
/// took the 9.0.5 reader 70 seconds.
const MAX_END_RECORDS: usize = 32;

/// A refusal of the zip named `name`.
fn bad(name: &str, reason: String) -> Error {
    Error::Refused(Refusal::BadArchive {
        path: name.to_owned(),
        reason,
    })
}

/// One entry the directory says to inflate.
struct Planned {
    index: usize,
    path: TreePath,
    kind: FileKind,
    /// The uncompressed size the central directory declares.
    size: u64,
}

/// The entries of the zip at `path` (named `name` in the upload) that the
/// upload keeps, inflated into `into` as `{first}.part`, `{first + 1}.part`
/// and so on, with `total` the bytes the upload already holds: the files,
/// each with its temporary path and size. Nothing is inflated unless the
/// directory as a whole passes.
///
/// # Errors
///
/// [`Error::Refused`]: [`Refusal::BadArchive`] for bytes that are not a
/// zip, too many end-of-directory records, an encrypted entry, a
/// compression method other than deflate or stored, or an entry whose
/// inflated size differs from what it declares; [`Refusal::TooManyEntries`]
/// from the bytes, [`Refusal::FileTooLarge`] and
/// [`Refusal::RevisionTooLarge`] from the directory. [`Error::Io`] for the
/// temporary files.
pub(super) fn expand(
    path: &Path,
    name: &str,
    into: &Path,
    first: usize,
    total: u64,
    limits: &Limits,
) -> Result<Vec<Received>> {
    let mut file = BufReader::new(File::open(path)?);
    screen(&mut file, name, limits)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| bad(name, error.to_string()))?;
    let planned = plan(&archive, name, total, limits)?;
    let mut received = Vec::with_capacity(planned.len());
    for (offset, entry) in planned.into_iter().enumerate() {
        let temp = into.join(format!("{}.part", first + offset));
        inflate(&mut archive, &entry, &temp).map_err(|error| match error {
            Error::Malformed(reason) => bad(name, format!("{}: {reason}", entry.path)),
            other => other,
        })?;
        received.push(Received {
            path: entry.path,
            kind: entry.kind,
            temp,
            size: entry.size,
        });
    }
    Ok(received)
}

/// What the zip's bytes say before it is opened, the stream left at its
/// start. Opening costs at most the headers times the end records, so
/// both are bounded here. The `zip` crate takes an entry only at the
/// header signature and an end record only at its own, so a count of the
/// signatures holds whatever the records claim. A count can run high, on
/// a stored zip inside the zip or on a signature by chance in compressed
/// bytes (about once in 4 GB), far under either limit.
fn screen(file: &mut (impl Read + Seek), name: &str, limits: &Limits) -> Result<()> {
    let count = count_signature(file, HEADER_SIGNATURE)?;
    if count > limits.zip_entries {
        return Err(Refusal::TooManyEntries {
            path: name.to_owned(),
            count,
            limit: limits.zip_entries,
        }
        .into());
    }
    let length = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(length.saturating_sub(END_SPAN)))?;
    let ends = count_signature(file, END_SIGNATURE)?;
    if ends > MAX_END_RECORDS {
        return Err(bad(
            name,
            format!(
                "holds {ends} end-of-directory records, more than the {MAX_END_RECORDS} the server tries"
            ),
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

/// How many times `signature` occurs in what is left of `bytes`.
fn count_signature(bytes: &mut impl Read, signature: &[u8]) -> std::io::Result<usize> {
    let mut buffer = vec![0; 1 << 20];
    let mut count = 0;
    // All but one of its bytes are carried between reads, so a signature
    // across two reads is seen; those alone cannot match, so none counts
    // twice.
    let mut carried = 0;
    loop {
        let read = bytes.read(&mut buffer[carried..])?;
        if read == 0 {
            return Ok(count);
        }
        let filled = carried + read;
        count += buffer[..filled]
            .windows(signature.len())
            .filter(|window| *window == signature)
            .count();
        carried = filled.min(signature.len() - 1);
        buffer.copy_within(filled - carried..filled, 0);
    }
}

/// The directory's verdict: which entries to inflate, after the caps pass
/// on what the directory declares.
fn plan<R: Read + Seek>(
    archive: &zip::ZipArchive<R>,
    name: &str,
    total: u64,
    limits: &Limits,
) -> Result<Vec<Planned>> {
    let mut planned = Vec::new();
    let mut sum = total;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_data(index)
            .map_err(|error| bad(name, error.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let Ok(entry_name) = entry.name() else {
            tracing::warn!(
                zip = name,
                index,
                "upload: dropped a zip entry whose name is not text"
            );
            continue;
        };
        // Windows PowerShell's Compress-Archive writes backslashes, which
        // a tree's path refuses.
        let Some((path, kind)) = Incoming::accepts(&entry_name.replace('\\', "/")) else {
            continue;
        };
        if entry.encrypted() {
            return Err(bad(name, format!("{path} is encrypted")));
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(bad(
                name,
                format!("{path} is compressed with neither deflate nor stored"),
            ));
        }
        let size = entry.size();
        if size > limits.file_bytes {
            return Err(Refusal::FileTooLarge {
                path: path.to_string(),
                limit_mb: limits.file_mb(),
            }
            .into());
        }
        sum = sum.saturating_add(size);
        if sum > limits.revision_bytes {
            return Err(Refusal::RevisionTooLarge {
                limit_mb: limits.revision_mb(),
            }
            .into());
        }
        planned.push(Planned {
            index,
            path,
            kind,
            size,
        });
    }
    Ok(planned)
}

/// One planned entry inflated into `temp`, bounded by its declared size.
///
/// # Errors
///
/// [`Error::Malformed`] for what the zip crate refuses (the compression
/// method, a damaged stream, a CRC that does not match) and for an entry
/// that holds more than its directory entry says; [`Error::Io`] for the
/// temporary file.
fn inflate<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    entry: &Planned,
    temp: &Path,
) -> Result<()> {
    let reader = archive
        .by_index(entry.index)
        .map_err(|error| Error::Malformed(error.to_string()))?;
    // One byte past the declared size is enough to tell a lie from the
    // truth; the inflater never runs further than that. The crate's own
    // reader stops there too (its CRC reader's size limit) and says so
    // first; this bound is the one this module answers for.
    let mut bounded = reader.take(entry.size.saturating_add(1));
    let mut bytes = Vec::new();
    let read = bounded
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Malformed(error.to_string()))? as u64;
    if read > entry.size {
        return Err(Error::Malformed(format!(
            "inflates to more than the {} bytes its directory entry declares",
            entry.size
        )));
    }
    if read < entry.size {
        return Err(Error::Malformed(format!(
            "holds {read} bytes where its directory entry declares {}",
            entry.size
        )));
    }
    std::fs::write(temp, &bytes)?;
    Ok(())
}

/// The temporary file an archive is received into.
pub(super) fn temp_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("{index}.zip"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use test_case::test_case;

    use super::*;

    const READ: usize = 1 << 20;

    #[test_case(0; "at the start")]
    #[test_case(READ - 4; "the last bytes of the first read")]
    #[test_case(READ - 3; "across the reads by one byte")]
    #[test_case(READ - 2; "across the reads by two")]
    #[test_case(READ - 1; "across the reads by three")]
    #[test_case(READ; "the first bytes of the second read")]
    fn a_signature_counts_once_wherever_the_reads_fall(at: usize) {
        let mut bytes = vec![0; READ + 16];
        bytes[at..at + 4].copy_from_slice(HEADER_SIGNATURE);
        let mut bytes = Cursor::new(bytes);
        assert_eq!(count_signature(&mut bytes, HEADER_SIGNATURE).unwrap(), 1);
    }

    #[test]
    fn a_part_of_a_signature_is_not_one() {
        let mut bytes = Cursor::new(b"PK\x01");
        assert_eq!(count_signature(&mut bytes, HEADER_SIGNATURE).unwrap(), 0);
    }
}
