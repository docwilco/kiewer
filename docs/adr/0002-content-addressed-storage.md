# Content-addressed storage with a global revision table

Designs need revisions, branches and forks, and KiCad files do not merge, so we take git's data model without git's tooling: blobs in GCS under the SHA-256 of their bytes, stored gzip-compressed; trees and revisions as Postgres rows hashed as git hashes commits (tree, parent, author, time, message); branches as (design, name, head) rows. Real git repositories were rejected because Cloud Run has no durable filesystem and GCS FUSE has no locking; gists were rejected because only GitHub accounts could upload and GitHub could edit or delete the files outside us.

Revision rows live in one global table keyed by hash, not per design. A fork is branch rows pointing at existing revisions and copies nothing; two uploads of the same files are two revisions sharing one tree. Reads are scoped by a `design_revision` index (design, revision), maintained on push, fork and branch delete and rebuildable from branches; no route accepts a revision hash without a design, because a bare hash would be a capability. Deleting a design is a hard delete of its rows; the nightly collector removes revisions no branch reaches, then trees and blobs no revision names, leaving rows younger than 24 hours alone, a tree or a blob counting from the last upload to name it. A `blocked_blob` table is the takedown path, since forks keep content a deleted design once held.

## Consequences

- The server computes every hash from bytes it holds; clients never send hashes on write.
- A content hash is an existence oracle. Acceptable while every design is public; private designs need design-scoped blob routes or signed tokens before they ship.
- Branches are linear. A push whose head moved is a pending revision with a choice: re-append onto the new head, or a new branch at the old one. Nothing merges.
