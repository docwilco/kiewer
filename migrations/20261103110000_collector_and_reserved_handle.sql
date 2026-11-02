-- What the collector and account delete need (#23; ADR 0002;
-- docs/v1-scope.md, Storage and history, Q35 and Q43). Additive: the
-- earlier code runs unchanged against this schema, so a rollback of the
-- binary needs no down step.

-- When an upload last named the row: the collector's 24-hour grace counts
-- from it, not from created_at, since a row an upload is reusing is as
-- good as new. kiewer_lib::collector says how this keeps a concurrent
-- upload safe. Existing rows start from the migration.
ALTER TABLE blob ADD COLUMN claimed_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE tree ADD COLUMN claimed_at timestamptz NOT NULL DEFAULT now();

-- A blob whose row the collector removed and whose object is still in the
-- store: written with the row's delete, removed once the object is gone,
-- so a crash between the two leaves this row for the next run. An upload
-- of the same bytes removes it, or waits while the collector holds it.
-- Empty between runs.
CREATE TABLE blob_removal (
    hash bytea PRIMARY KEY CHECK (octet_length(hash) = 32)
);

-- A handle a deleted account held, which nobody may take until
-- reserved_until (kiewer_lib::account::choose_handle reads it, and any
-- later way of changing a handle must too). A row past its time refuses
-- nothing and stays, replaced if the handle is given up again; the handle
-- is public in the revisions it authored anyway.
CREATE TABLE reserved_handle (
    handle text PRIMARY KEY,
    reserved_until timestamptz NOT NULL
);
