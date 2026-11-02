-- The index behind revision.author_id's ON DELETE SET NULL: without it an
-- account delete updates the author's revisions by a scan of the whole
-- table, which grows for the life of the site. Additive, so the earlier
-- code runs unchanged.
CREATE INDEX revision_author_id ON revision (author_id);
