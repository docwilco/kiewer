-- A design's description and the redirect an old slug leaves (#22;
-- docs/v1-scope.md, Pages and URLs, Q48 and Q56; Accounts and access,
-- Q29). Both additive: the earlier code runs unchanged against this
-- schema, so a rollback of the binary needs no down step.

-- The description as written (Markdown) and as shown (the HTML
-- kiewer_lib::description renders from it at every edit, through its
-- allow-list). Both kept: the page reads the HTML as it is, the form
-- reads the Markdown back. Existing designs start empty.
ALTER TABLE design ADD COLUMN description text NOT NULL DEFAULT '';
ALTER TABLE design ADD COLUMN description_html text NOT NULL DEFAULT '';

-- A slug a design of the owner's was at before its slug changed:
-- /{handle}/{slug} answers 301 to the design's slug now. The row names
-- the design, not the new slug, so a design moved twice is reached from
-- either old slug. A slug of an owner is a design's or a redirect, never
-- both: the transaction that creates a design at a slug or moves one
-- there deletes the redirect first (kiewer_lib::slug::release), which is
-- how reusing an old slug ends its redirect. The redirect goes with the
-- design and with the account.
CREATE TABLE slug_redirect (
    owner_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    slug text NOT NULL,
    design_id bigint NOT NULL REFERENCES design ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_id, slug)
);
CREATE INDEX slug_redirect_design_id ON slug_redirect (design_id);
