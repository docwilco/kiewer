-- Designs and their history: blobs, trees, revisions, branches, the
-- design_revision index and the takedown list (ADR 0002; GLOSSARY, Files
-- and storage). Every hash is a SHA-256 as 32 raw bytes, computed by the
-- server from bytes it holds (kiewer_lib::revision); a text form is 64
-- lowercase hex characters, in URLs and pages only.

-- One stored file by the hash of its uncompressed bytes. The bytes live
-- in the blob store under the same hash; this row is what the collector
-- and the account quota count. size is the uncompressed length, which the
-- caps are measured in.
CREATE TABLE blob (
    hash bytea PRIMARY KEY CHECK (octet_length(hash) = 32),
    size bigint NOT NULL CHECK (size >= 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A hash the operator has refused site-wide: not accepted, not served,
-- not rendered. Separate from blob: a blocked hash may never have been
-- stored, and a block outlives the collector removing the blob.
CREATE TABLE blocked_blob (
    hash bytea PRIMARY KEY CHECK (octet_length(hash) = 32),
    reason text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A revision's map from path to blob, hashed over its sorted entries so
-- two uploads of the same files share one tree.
CREATE TABLE tree (
    hash bytea PRIMARY KEY CHECK (octet_length(hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- One file of a tree. The path is relative to the project folder, with
-- forward slashes, as kiewer_lib::revision::TreePath admits it. A blob
-- named by any entry is not removable: the collector removes entries with
-- their tree before it looks at blobs.
CREATE TABLE tree_entry (
    tree_hash bytea NOT NULL REFERENCES tree ON DELETE CASCADE,
    path text NOT NULL,
    blob_hash bytea NOT NULL REFERENCES blob,
    PRIMARY KEY (tree_hash, path)
);
CREATE INDEX tree_entry_blob_hash ON tree_entry (blob_hash);

-- One immutable snapshot, in one global table keyed by hash, not per
-- design: a fork points at existing rows and copies nothing. The hash
-- covers tree, parent, author, time and message, as git hashes a commit.
-- The author is kept as the handle text the hash covered, beside the
-- account id, so a revision outlives its author's account. created_at is
-- the time the hash covered, so it is written by the server, not
-- defaulted here. The parsed metadata (docs/v1-scope.md, Uploads) is
-- derived, outside the hash, and NULL for a revision without a board.
CREATE TABLE revision (
    hash bytea PRIMARY KEY CHECK (octet_length(hash) = 32),
    tree_hash bytea NOT NULL REFERENCES tree,
    parent_hash bytea REFERENCES revision,
    author_id bigint REFERENCES account ON DELETE SET NULL,
    author_handle text NOT NULL,
    created_at timestamptz NOT NULL,
    message text,
    format_version integer,
    layer_count integer,
    copper_layer_count integer,
    board_width_mm double precision,
    board_height_mm double precision,
    fonts text[],
    net_count integer,
    footprint_count integer
);
CREATE INDEX revision_tree_hash ON revision (tree_hash);
CREATE INDEX revision_parent_hash ON revision (parent_hash);

-- What a person owns and shares. The slug is the design's name in its
-- URL, unique per owner; the rules and renames are #22, so only the
-- uniqueness is here. Deleting the account deletes its designs
-- (docs/v1-scope.md, Storage and history).
CREATE TABLE design (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    slug text NOT NULL,
    name text NOT NULL,
    default_branch text NOT NULL DEFAULT 'main',
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_id, slug)
);

-- A named pointer to the head of one linear line of work. A push moves
-- the head by compare-and-set on the parent it was built on.
CREATE TABLE branch (
    design_id bigint NOT NULL REFERENCES design ON DELETE CASCADE,
    name text NOT NULL,
    head_hash bytea NOT NULL REFERENCES revision,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (design_id, name)
);
CREATE INDEX branch_head_hash ON branch (head_hash);

-- Which revisions a design may show: maintained in the transaction that
-- writes a push, a fork or a branch delete, and rebuildable from the
-- branches' chains. Every read of a revision is scoped by it, because a
-- bare hash would be a capability (ADR 0002).
CREATE TABLE design_revision (
    design_id bigint NOT NULL REFERENCES design ON DELETE CASCADE,
    revision_hash bytea NOT NULL REFERENCES revision,
    PRIMARY KEY (design_id, revision_hash)
);
CREATE INDEX design_revision_revision_hash ON design_revision (revision_hash);
