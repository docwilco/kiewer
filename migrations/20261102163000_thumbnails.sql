-- Thumbnails (ADR 0004): the two PNGs the server renders of a revision,
-- 1200 by 630 for og:image and 400 by 400 for lists. Each PNG is a blob
-- under its own hash, outside any tree, written through the store's one
-- path; this row keys the two by revision, and it is what a page reads
-- to know whether the thumbnail exists (a missing row is the
-- placeholder). A revision's thumbnail goes with the revision; the blob
-- rows stay until the collector finds no tree entry and no thumbnail
-- naming them, which the two indexes answer.
CREATE TABLE thumbnail (
    revision_hash bytea PRIMARY KEY REFERENCES revision ON DELETE CASCADE,
    large_hash bytea NOT NULL REFERENCES blob,
    small_hash bytea NOT NULL REFERENCES blob,
    rendered_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX thumbnail_large_hash ON thumbnail (large_hash);
CREATE INDEX thumbnail_small_hash ON thumbnail (small_hash);
