# The upload page expands zips; the server takes none

A zip's own bytes were capped at 200 MB on the server, so a project zipped with its `.git`, `-backups` or 3D models was refused even when the KiCad files in it would fit. On Cloud Run, the server's temporary files live in the instance's 512 MiB of memory, so raising that cap means more memory or a temporary store that is not RAM. The upload page now expands a zip itself: it reads the central directory with `File.slice`, checks the caps and the skip list, inflates only the kept entries (store and deflate, through `DecompressionStream`), and sends them as plain files with their paths, exactly as a folder drop does. The server refuses any zip part, and its zip reader and the `zip` crate go. A zip that is encrypted, has zip64 records or uses another method is denied on the page, and so is a zip sent with scripts off. Decided in triage on 2026-11-03, built as #83.

## Considered options

- A server reader with a higher cap: needs a new number plus more memory, or a temporary store that is not RAM, and keeps a parser of untrusted bytes on the server.
- Accept the cap and say on the form that a zip counts whole: no work, but the refusal stays for every project zipped with its history.
- Keep the server reader as a fallback for clients other than the page: nothing but the page uploads, and the fallback would keep #77 and #78 alive.

## Consequences

An upload without script can no longer send a zip; a folder or single files still work. Files leave the page uncompressed. Whether uploads move to gzip parts on the wire is #104's question.
