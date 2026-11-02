-- Roles on a design and its visibility (docs/v1-scope.md, Accounts and
-- access; GLOSSARY, People). The owner is design.owner_id, which is NOT
-- NULL and one column: that is the one-owner invariant, in the schema. A
-- transfer moves the column (kiewer_lib::roles). The other two roles are
-- rows here, one per person per design; the owner never has a row, which
-- the code path that grants a role refuses, and a transfer removes the
-- new owner's row in the same transaction.

CREATE TABLE design_role (
    design_id bigint NOT NULL REFERENCES design ON DELETE CASCADE,
    account_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    role text NOT NULL CHECK (role IN ('admin', 'collaborator')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (design_id, account_id)
);
-- The upload form's listing of the designs a person may push to, and
-- the index behind account_id's ON DELETE CASCADE.
CREATE INDEX design_role_account_id ON design_role (account_id);

-- Public is in every listing; unlisted opens by its link and appears in
-- none. Authorisation is on writes only, so nothing else reads this.
ALTER TABLE design ADD COLUMN visibility text NOT NULL DEFAULT 'public'
    CHECK (visibility IN ('public', 'unlisted'));

-- The home page's recent public designs, newest push first: the default
-- branch's head moved most recently. Bounded by its LIMIT, so the walk
-- stops at the first page of public designs.
CREATE INDEX branch_updated_at ON branch (updated_at DESC);
