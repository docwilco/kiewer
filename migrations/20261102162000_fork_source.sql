-- Where a fork came from (#20; docs/v1-scope.md, Storage and history,
-- Q33 and Q55; GLOSSARY, Forked from). Additive: every column is
-- nullable and the earlier code runs unchanged against this schema, so a
-- rollback of the binary needs no down step. A fork is design, branch
-- and design_revision rows pointing at the source's revision rows (ADR
-- 0002); these columns are the only thing on the row that says it is
-- one.

-- The source design, or NULL once it is deleted (#23: a hard delete of
-- its rows, which sets this null and leaves the two text columns). The
-- handle and the slug are kept as text beside the reference for that
-- case: the forked-from line then names the source and says it is gone
-- (Q43: forks by others survive with the old handle as text). While the
-- source exists the page reads its handle and slug from it, since
-- either may have changed since the fork.
ALTER TABLE design ADD COLUMN forked_from_design_id bigint
    REFERENCES design ON DELETE SET NULL;
ALTER TABLE design ADD COLUMN forked_from_handle text;
ALTER TABLE design ADD COLUMN forked_from_slug text;
-- The revision the fork started at: the head of the source's default
-- branch when the fork was made. A reference, since the forked-from line
-- links it; SET NULL rather than a block, since the collector may remove
-- it once no branch of any design reaches it.
ALTER TABLE design ADD COLUMN forked_from_revision_hash bytea
    REFERENCES revision ON DELETE SET NULL
    CHECK (octet_length(forked_from_revision_hash) = 32);
-- The handle and the slug are what outlive the source, so they are what
-- says a design is a fork: both or neither. The two references may each
-- go null on their own.
ALTER TABLE design ADD CONSTRAINT design_fork_source_together CHECK (
    (forked_from_handle IS NULL) = (forked_from_slug IS NULL)
);
-- Behind the two ON DELETE SET NULL actions, which look the column up.
CREATE INDEX design_forked_from_design_id ON design (forked_from_design_id)
    WHERE forked_from_design_id IS NOT NULL;
CREATE INDEX design_forked_from_revision_hash ON design (forked_from_revision_hash)
    WHERE forked_from_revision_hash IS NOT NULL;
