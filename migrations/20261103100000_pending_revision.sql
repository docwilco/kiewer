-- Who made a branch: what "their own branches" means for a collaborator
-- (GLOSSARY, Collaborator). NULL for a branch from before this column, a
-- fork's copy, or one whose maker's account is gone: such a branch is
-- managed by an admin or the owner. The index serves the account delete's
-- SET NULL.
ALTER TABLE branch ADD COLUMN created_by bigint REFERENCES account ON DELETE SET NULL;
CREATE INDEX branch_created_by ON branch (created_by);

-- An upload whose branch head moved (ADR 0002; GLOSSARY, Pending revision):
-- the blobs and the tree are stored, and this row is the rest of what the
-- revision will be, waiting for the uploader to choose re-append or
-- branch. There is no revision row and so no hash yet: the hash covers the
-- parent, which the choice decides. base_hash is the old head, the head
-- the upload was built on; expires_at is an hour on, the last moment a
-- choice is taken.
--
-- The foreign key to branch carries the two things a branch can do to a
-- row: a rename reaches it, and a delete takes it. So a live row's branch
-- exists, and base_hash, which was on that branch's chain and a head only
-- moves forward, still is: the collector need not treat it as a root. The
-- collector calls pending::expire first, then keeps the tree of every row
-- that is left. A deleted design goes through its branches, a deleted
-- account through author_id.
CREATE TABLE pending_revision (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    design_id bigint NOT NULL,
    branch text NOT NULL,
    author_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    base_hash bytea NOT NULL REFERENCES revision,
    tree_hash bytea NOT NULL REFERENCES tree,
    message text,
    format_version integer,
    layer_count integer,
    copper_layer_count integer,
    board_width_mm double precision,
    board_height_mm double precision,
    fonts text[],
    net_count integer,
    footprint_count integer,
    warnings text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL DEFAULT now() + interval '1 hour',
    FOREIGN KEY (design_id, branch) REFERENCES branch (design_id, name)
        ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX pending_revision_branch ON pending_revision (design_id, branch);
CREATE INDEX pending_revision_expires_at ON pending_revision (expires_at);
CREATE INDEX pending_revision_tree_hash ON pending_revision (tree_hash);
CREATE INDEX pending_revision_base_hash ON pending_revision (base_hash);
-- What the hourly allowance reads (kiewer_lib::rate_limit).
CREATE INDEX pending_revision_author_created_at ON pending_revision (author_id, created_at);
