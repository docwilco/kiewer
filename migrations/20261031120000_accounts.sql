-- Accounts, the provider identities that sign in to them, and sessions
-- (docs/v1-scope.md, Accounts and access; GLOSSARY, People).

-- One person. The handle is NULL from the first sign-in until the person
-- picks one on the welcome page; the CHECK is the format's floor (1 to 39
-- of lowercase letters, digits and hyphens, no hyphen at either end), and
-- the reserved words are kiewer_lib::account::RESERVED, in code so the list
-- grows without a migration. UNIQUE is what makes a handle a name.
CREATE TABLE account (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    handle text UNIQUE CHECK (handle ~ '^[a-z0-9]([a-z0-9-]{0,37}[a-z0-9])?$'),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- One sign-in method of an account: the provider's own id for the person,
-- never their email or login, which both change hands. One account has
-- several; a provider id belongs to one account. provider_login is what
-- the provider showed at the last sign-in: it prefills the handle and
-- names the identity in settings.
CREATE TABLE identity (
    provider text NOT NULL,
    provider_user_id text NOT NULL,
    account_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    provider_login text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, provider_user_id)
);
CREATE INDEX identity_account_id ON identity (account_id);

-- A signed-in browser. The row holds the SHA-256 of the cookie's random
-- id, so a read of the table yields no usable cookie. expires_at slides on
-- use (kiewer_lib::session); a row past it is dead and is swept when its
-- account next signs in.
CREATE TABLE session (
    id_hash bytea PRIMARY KEY CHECK (octet_length(id_hash) = 32),
    account_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);
CREATE INDEX session_account_id ON session (account_id);
